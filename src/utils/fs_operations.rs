//! fs_operations.rs — 可替换的文件系统抽象层。
//!
//! 重写自 `src/utils/fsOperations.ts`。这是 log.ts / debug.ts / settings 等
//! 模块的共同枢纽，语义对齐说明：
//!
//! - TS 的 `FsOperations` 是一个对象类型（供测试用 mock 注入）；Rust 侧落
//!  地为 `pub trait FsOperations` + 默认实现 `StdFs`（全部落到 `std::fs`）。
//! - TS 里 9 个 Promise 方法（stat / readdir / unlink / rmdir / rm / mkdir /
//!   readFile / rename / readFileBytes）在 Rust 侧**先做成同步**：
//!   `async-trait` 与 `futures` 均不在 Cargo.toml 中，且当前没有已转换的异
//!   步调用方；待上游（log/settings 等）落地 async 运行时接入点后再决定是
//!   否引入 async-trait 或返回 `BoxFuture`。
//! - `Buffer` → `Vec<u8>`；`fs.Stats` → `FileStats`；`fs.Dirent` →
//!   `DirEntry { name, kind }`；`BufferEncoding` 用枚举表示，**decode 仅实现
//!   utf8/ascii**（两者在 Node 里行为一致：非法字节替换为 U+FFFD，对应
//!   `String::from_utf8_lossy`）；其余 encoding 返回 `InvalidInput` 错误。
//! - `getErrnoCode` 对应 `crate::utils::errors::get_errno_code`；注意它对
//!   原始 `io::Error` 只映射了 ENOENT/EACCES/EPERM/ENOTDIR/ELOOP，**没有
//!   EEXIST**，所以 TS 里“吞掉 EEXIST”的两处（mkdir 的 Bun/Windows 只读目
//!   录 workaround、appendFileSync 的 'ax' 回退）同时用
//!   `ErrorKind::AlreadyExists` 与 `get_errno_code` 双判定。
//! - `slowLogging` 慢操作装饰在 Rust 侧省略（slowOperations 模块未转换）；
//!   TS 版每个 fs 调用前的 `using _ = slowLogging\`...\`` 无对应物。
//! - `createWriteStream`：Node 侧是惰性打开的事件流；Rust 侧直接返回
//!   `std::fs::File`（Create+Write+Truncate **立即打开**），无
//!   backpressure/事件语义。调用方当前未转换，先保证类型可用。
//! - `process.cwd()` 在 Rust 侧返回 `io::Result<String>`（TS 无 cwd 时抛
//!   异常；Rust 侧把错误显式化，不 panic）。

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use once_cell::sync::Lazy;
use unicode_normalization::UnicodeNormalization;

use crate::utils::errors;

// ---------------------------------------------------------------------------
// 辅助类型
// ---------------------------------------------------------------------------

/// TS `BufferEncoding`。decode 只实现 Utf8/Ascii（见模块头注释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferEncoding {
    Ascii,
    Utf8,
    Utf16Le,
    Latin1,
    Base64,
    Hex,
    Ucs2,
}

impl BufferEncoding {
    /// 按 Node 的大小写不敏感字符串解析（`'utf-8'` 视同 `'utf8'`）。
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ascii" => Some(Self::Ascii),
            "utf8" | "utf-8" => Some(Self::Utf8),
            "utf16le" | "utf-16le" => Some(Self::Utf16Le),
            "latin1" => Some(Self::Latin1),
            "base64" => Some(Self::Base64),
            "hex" => Some(Self::Hex),
            "ucs2" | "ucs-2" => Some(Self::Ucs2),
            _ => None,
        }
    }
}

/// `fs.Stats` 的对照物。`mtime` 为系统时间；特殊文件类型判定
/// （is_fifo/is_socket/…）来自 lstat/stat 的 `file_type`。
#[derive(Debug, Clone)]
pub struct FileStats {
    pub size: u64,
    pub mtime: SystemTime,
    file_type: fs::FileType,
}

impl FileStats {
    fn from_metadata(metadata: fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            mtime: metadata.modified()?,
            file_type: metadata.file_type(),
        })
    }

    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    #[cfg(unix)]
    pub fn is_fifo(&self) -> bool {
        std::os::unix::fs::FileTypeExt::is_fifo(&self.file_type)
    }

    #[cfg(not(unix))]
    pub fn is_fifo(&self) -> bool {
        false
    }

    #[cfg(unix)]
    pub fn is_socket(&self) -> bool {
        std::os::unix::fs::FileTypeExt::is_socket(&self.file_type)
    }

    #[cfg(not(unix))]
    pub fn is_socket(&self) -> bool {
        false
    }

    #[cfg(unix)]
    pub fn is_char_device(&self) -> bool {
        std::os::unix::fs::FileTypeExt::is_char_device(&self.file_type)
    }

    #[cfg(not(unix))]
    pub fn is_char_device(&self) -> bool {
        false
    }

    #[cfg(unix)]
    pub fn is_block_device(&self) -> bool {
        std::os::unix::fs::FileTypeExt::is_block_device(&self.file_type)
    }

    #[cfg(not(unix))]
    pub fn is_block_device(&self) -> bool {
        false
    }
}

/// 目录条目类型（`fs.Dirent` 的 kind）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    /// 底层文件系统返回 DT_UNKNOWN（与 Node 同样可能出现）。
    Unknown,
}

/// `fs.Dirent` 的对照物：名称 + 类型（不跟随符号链接）。
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

fn entry_kind(file_type: &fs::FileType) -> EntryKind {
    if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt as _;
            if file_type.is_fifo() {
                return EntryKind::Fifo;
            }
            if file_type.is_socket() {
                return EntryKind::Socket;
            }
            if file_type.is_char_device() {
                return EntryKind::CharacterDevice;
            }
            if file_type.is_block_device() {
                return EntryKind::BlockDevice;
            }
        }
        EntryKind::Unknown
    }
}

/// `rm` / `rmSync` 的选项。`None` 等价于 TS 省略 options
/// （recursive=false, force=false）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RmOptions {
    pub recursive: bool,
    pub force: bool,
}

/// `mkdir` / `mkdirSync` 的选项。mode 为 `None` 时由系统默认值
/// （0o777 & ~umask）决定——与 Node 一致。注意 TS 版的 mkdir/mkdirSync
/// **强制 recursive: true**，本实现同样强制。
#[derive(Debug, Clone, Copy, Default)]
pub struct MkdirOptions {
    pub mode: Option<u32>,
}

/// `appendFileSync` 的选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct AppendFileOptions {
    pub mode: Option<u32>,
}

/// `symlinkSync` 的 type 参数。POSIX 上无意义（仅 Windows 区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkType {
    Dir,
    File,
    /// Windows 目录连接点。`std::fs` 不支持，返回 `Unsupported` 错误。
    Junction,
}

/// `readSync` 的返回值。buffer 为 `length` 长度的零填充缓冲
/// （对应 TS `Buffer.alloc(length)`），`bytes_read` 为实际读到的字节数。
#[derive(Debug)]
pub struct ReadSyncResult {
    pub buffer: Vec<u8>,
    pub bytes_read: usize,
}

/// `readFileRange` / `tailFile` 的返回值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFileRangeResult {
    /// 以 UTF-8（非法字节替换为 U+FFFD，对齐 Node）解码出的内容。
    pub content: String,
    pub bytes_read: u64,
    pub bytes_total: u64,
}

// ---------------------------------------------------------------------------
// trait 定义
// ---------------------------------------------------------------------------

/// 简化版文件系统操作接口（对齐 TS `FsOperations`）。
///
/// 允许替换实现（如测试 mock）。所有方法在 trait 层是同步的——TS 的
/// Promise API 暂按同步落地（见模块头注释）。错误统一为 `io::Error`，
/// 需要分类时配合 `errors::get_errno_code` / `errors::is_fs_inaccessible`。
pub trait FsOperations {
    /// 获取当前工作目录（TS `process.cwd()`；Rust 侧把错误显式化）。
    fn cwd(&self) -> io::Result<String>;

    // --- 文件访问与信息 ---

    /// 路径是否存在（不抛错；语义等价 `fs.existsSync`）。
    fn exists_sync(&self, path: &str) -> bool;

    /// 获取文件属性，跟随符号链接（TS 异步 `stat`，Rust 同步）。
    fn stat(&self, path: &str) -> io::Result<FileStats>;

    /// 列出目录内容并带类型信息（TS 异步 `readdir` withFileTypes）。
    fn readdir(&self, path: &str) -> io::Result<Vec<DirEntry>>;

    /// 删除文件（TS 异步 `unlink`）。
    fn unlink(&self, path: &str) -> io::Result<()>;

    /// 删除空目录（TS 异步 `rmdir`）。
    fn rmdir(&self, path: &str) -> io::Result<()>;

    /// 删除文件/目录（TS 异步 `rm`）。recursive 时目录走
    /// `remove_dir_all`、文件走 `remove_file`；force 吞 ENOENT。
    fn rm(&self, path: &str, options: Option<&RmOptions>) -> io::Result<()>;

    /// 递归创建目录（recursive 强制为 true；吞 EEXIST——
    /// Bun/Windows 只读目录 workaround，见模块头注释）。
    fn mkdir(&self, path: &str, options: Option<&MkdirOptions>) -> io::Result<()>;

    /// 以编码读文件为字符串（TS 异步 `readFile`）。encoding 仅支持
    /// utf8/ascii，其余返回 `InvalidInput`。
    fn read_file(&self, path: &str, encoding: BufferEncoding) -> io::Result<String>;

    /// 重命名/移动（TS 异步 `rename`）。
    fn rename(&self, old_path: &str, new_path: &str) -> io::Result<()>;

    /// 获取文件属性，跟随符号链接。
    fn stat_sync(&self, path: &str) -> io::Result<FileStats>;

    /// 获取文件属性，**不**跟随符号链接。
    fn lstat_sync(&self, path: &str) -> io::Result<FileStats>;

    // --- 文件内容操作 ---

    /// 以编码读文件为字符串。
    fn read_file_sync(&self, path: &str, encoding: BufferEncoding) -> io::Result<String>;

    /// 读取原始字节。
    fn read_file_bytes_sync(&self, path: &str) -> io::Result<Vec<u8>>;

    /// 从文件起始读取至多 `length` 字节（零填充缓冲 + 实际字节数）。
    fn read_sync(&self, path: &str, length: usize) -> io::Result<ReadSyncResult>;

    /// 追加字符串。带显式 mode 时用 create_new+append 原子创建
    /// （对齐 TS 的 'ax'，避免 TOCTOU），文件已存在则回退普通追加。
    fn append_file_sync(
        &self,
        path: &str,
        data: &str,
        options: Option<&AppendFileOptions>,
    ) -> io::Result<()>;

    /// 拷贝文件（覆盖目标）。
    fn copy_file_sync(&self, src: &str, dest: &str) -> io::Result<()>;

    /// 删除文件。
    fn unlink_sync(&self, path: &str) -> io::Result<()>;

    /// 重命名/移动。
    fn rename_sync(&self, old_path: &str, new_path: &str) -> io::Result<()>;

    /// 创建硬链接。
    fn link_sync(&self, target: &str, path: &str) -> io::Result<()>;

    /// 创建符号链接。type 仅 Windows 有效；Junction 不支持。
    fn symlink_sync(
        &self,
        target: &str,
        path: &str,
        ty: Option<SymlinkType>,
    ) -> io::Result<()>;

    /// 读取符号链接目标（相对链接保持原样返回）。
    fn readlink_sync(&self, path: &str) -> io::Result<String>;

    /// 解析符号链接并返回规范路径；结果做 NFC 规范化（对齐 TS）。
    fn realpath_sync(&self, path: &str) -> io::Result<String>;

    // --- 目录操作 ---

    /// 递归创建目录（同 `mkdir`，同步入口）。
    fn mkdir_sync(&self, path: &str, options: Option<&MkdirOptions>) -> io::Result<()>;

    /// 列出目录内容（带类型信息）。
    fn readdir_sync(&self, path: &str) -> io::Result<Vec<DirEntry>>;

    /// 列出目录内容（仅文件名）。
    fn readdir_string_sync(&self, path: &str) -> io::Result<Vec<String>>;

    /// 目录是否为空（默认实现复用 `readdir_sync`，对齐 TS 的
    /// `isDirEmptySync` 用自身 `readdirSync`）。
    fn is_dir_empty_sync(&self, path: &str) -> io::Result<bool> {
        Ok(self.readdir_sync(path)?.is_empty())
    }

    /// 删除空目录。
    fn rmdir_sync(&self, path: &str) -> io::Result<()>;

    /// 删除文件/目录（同 `rm`，同步入口）。
    fn rm_sync(&self, path: &str, options: Option<&RmOptions>) -> io::Result<()>;

    /// 创建可写句柄。偏差：立即以 Create+Write+Truncate 打开，
    /// 返回 `std::fs::File`（非 Node 的惰性事件流）。
    fn create_write_stream(&self, path: &str) -> io::Result<File>;

    /// 读取原始字节。`max_bytes` 为 `None` 时读整个文件；否则只读
    /// min(文件大小, max_bytes)（对齐 TS 的截断逻辑）。
    fn read_file_bytes(&self, path: &str, max_bytes: Option<usize>) -> io::Result<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// StdFs：默认实现（全部落到 std::fs / std::os::unix::fs / std::env）
// ---------------------------------------------------------------------------

/// 默认文件系统实现：直接转发到 `std::fs`（+ unix 扩展）。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

/// 判定错误是否为 EEXIST（`errors::get_errno_code` 未映射原始
/// `io::Error` 的 EEXIST，所以双判定）。
fn is_eexist(e: &io::Error) -> bool {
    e.kind() == ErrorKind::AlreadyExists || errors::get_errno_code(e).as_deref() == Some("EEXIST")
}

/// `rm`/`rmSync` 共用实现。
fn rm_path(path: &str, recursive: bool, force: bool) -> io::Result<()> {
    let result = if recursive {
        // 用 symlink_metadata（不跟随链接）区分目录与文件：
        // 对目录用 remove_dir_all，其余（含符号链接本身）用 remove_file。
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(path),
            _ => fs::remove_file(path),
        }
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        // force: true 时忽略 ENOENT（对齐 Node `rmSync({force: true})`）。
        Err(e) if force && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `mkdir`/`mkdirSync` 共用实现：逐级创建（recursive 强制 true），吞 EEXIST。
fn mkdir_recursive(path: &str, options: Option<&MkdirOptions>) -> io::Result<()> {
    let mode = options.and_then(|o| o.mode);
    let mut current = PathBuf::new();
    for comp in Path::new(path).components() {
        current.push(comp.as_os_str());
        let result = {
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            if let Some(mode) = mode {
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
            }
            builder.create(&current)
        };
        if let Err(e) = result {
            if is_eexist(&e) {
                // 已存在（含 Bun/Windows 把只读目录误判为不存在而抛 EEXIST
                // 的 workaround，见 fsOperations.ts 注释）。
                continue;
            }
            return Err(e);
        }
    }
    Ok(())
}

fn decode_utf8(bytes: &[u8], encoding: BufferEncoding) -> io::Result<String> {
    match encoding {
        BufferEncoding::Utf8 | BufferEncoding::Ascii => {
            // Node 的 utf8/ascii 解码对非法字节产生替换字符，
            // 与 from_utf8_lossy 一致；不返回错误。
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "unsupported BufferEncoding (only utf8/ascii implemented; got {encoding:?}) — see fs_operations.rs deviation notes"
            ),
        )),
    }
}

impl FsOperations for StdFs {
    fn cwd(&self) -> io::Result<String> {
        std::env::current_dir().map(|p| p.to_string_lossy().into_owned())
    }

    fn exists_sync(&self, path: &str) -> bool {
        Path::new(path).try_exists().unwrap_or(false)
    }

    fn stat(&self, path: &str) -> io::Result<FileStats> {
        fs::metadata(path).and_then(FileStats::from_metadata)
    }

    fn readdir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        readdir_with_types(path)
    }

    fn unlink(&self, path: &str) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn rmdir(&self, path: &str) -> io::Result<()> {
        fs::remove_dir(path)
    }

    fn rm(&self, path: &str, options: Option<&RmOptions>) -> io::Result<()> {
        let opts = options.copied().unwrap_or_default();
        rm_path(path, opts.recursive, opts.force)
    }

    fn mkdir(&self, path: &str, options: Option<&MkdirOptions>) -> io::Result<()> {
        mkdir_recursive(path, options)
    }

    fn read_file(&self, path: &str, encoding: BufferEncoding) -> io::Result<String> {
        decode_utf8(&fs::read(path)?, encoding)
    }

    fn rename(&self, old_path: &str, new_path: &str) -> io::Result<()> {
        fs::rename(old_path, new_path)
    }

    fn stat_sync(&self, path: &str) -> io::Result<FileStats> {
        fs::metadata(path).and_then(FileStats::from_metadata)
    }

    fn lstat_sync(&self, path: &str) -> io::Result<FileStats> {
        fs::symlink_metadata(path).and_then(FileStats::from_metadata)
    }

    fn read_file_sync(&self, path: &str, encoding: BufferEncoding) -> io::Result<String> {
        decode_utf8(&fs::read(path)?, encoding)
    }

    fn read_file_bytes_sync(&self, path: &str) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn read_sync(&self, path: &str, length: usize) -> io::Result<ReadSyncResult> {
        let mut file = File::open(path)?;
        let mut buffer = vec![0u8; length];
        let bytes_read = file.read(&mut buffer)?;
        Ok(ReadSyncResult { buffer, bytes_read })
    }

    fn append_file_sync(
        &self,
        path: &str,
        data: &str,
        options: Option<&AppendFileOptions>,
    ) -> io::Result<()> {
        let mode = options.and_then(|o| o.mode);
        if let Some(mode) = mode {
            // 对齐 TS：显式 mode 时先用 create_new（'ax' 语义）原子创建，
            // 避免“存在性检查 → 打开”的 TOCTOU；已存在则回退普通追加。
            let mut opts = OpenOptions::new();
            opts.append(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, mode);
            match opts.open(path) {
                Ok(mut file) => return file.write_all(data.as_bytes()),
                Err(e) if is_eexist(&e) => { /* 文件已存在，回退 */ }
                Err(e) => return Err(e),
            }
        }
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        file.write_all(data.as_bytes())
    }

    fn copy_file_sync(&self, src: &str, dest: &str) -> io::Result<()> {
        fs::copy(src, dest).map(|_| ())
    }

    fn unlink_sync(&self, path: &str) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn rename_sync(&self, old_path: &str, new_path: &str) -> io::Result<()> {
        fs::rename(old_path, new_path)
    }

    fn link_sync(&self, target: &str, path: &str) -> io::Result<()> {
        fs::hard_link(target, path)
    }

    fn symlink_sync(
        &self,
        target: &str,
        path: &str,
        ty: Option<SymlinkType>,
    ) -> io::Result<()> {
        // POSIX 上 symlink 的 type 参数无意义
        #[cfg(unix)]
        {
            let _ = ty;
            return std::os::unix::fs::symlink(target, path);
        }
        #[cfg(windows)]
        {
            return match ty.unwrap_or(SymlinkType::File) {
                SymlinkType::File => std::os::windows::fs::symlink_file(target, path),
                SymlinkType::Dir => std::os::windows::fs::symlink_dir(target, path),
                SymlinkType::Junction => Err(io::Error::new(
                    ErrorKind::Unsupported,
                    "junction symlinks are not supported by StdFs",
                )),
            };
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, path, ty);
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "symlink is not supported on this platform",
            ));
        }
        // 任一平台分支已 return；此处保证类型完备（无 panic 路径）。
        #[allow(unreachable_code)]
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "symlink: no platform branch compiled",
        ))
    }

    fn readlink_sync(&self, path: &str) -> io::Result<String> {
        fs::read_link(path).map(|p| p.to_string_lossy().into_owned())
    }

    fn realpath_sync(&self, path: &str) -> io::Result<String> {
        let canonical = fs::canonicalize(path)?;
        // 对齐 TS：`.normalize('NFC')`
        Ok(canonical.to_string_lossy().nfc().collect())
    }

    fn mkdir_sync(&self, path: &str, options: Option<&MkdirOptions>) -> io::Result<()> {
        mkdir_recursive(path, options)
    }

    fn readdir_sync(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        readdir_with_types(path)
    }

    fn readdir_string_sync(&self, path: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(path)? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn rmdir_sync(&self, path: &str) -> io::Result<()> {
        fs::remove_dir(path)
    }

    fn rm_sync(&self, path: &str, options: Option<&RmOptions>) -> io::Result<()> {
        let opts = options.copied().unwrap_or_default();
        rm_path(path, opts.recursive, opts.force)
    }

    fn create_write_stream(&self, path: &str) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    fn read_file_bytes(&self, path: &str, max_bytes: Option<usize>) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        match max_bytes {
            None => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                Ok(buf)
            }
            Some(max_bytes) => {
                let size = file.metadata()?.len();
                let read_size = usize::try_from(size.min(max_bytes as u64)).unwrap_or(usize::MAX);
                let mut buf = vec![0u8; read_size];
                let mut total = 0usize;
                while total < read_size {
                    let n = file.read(&mut buf[total..])?;
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                if total < read_size {
                    buf.truncate(total);
                }
                Ok(buf)
            }
        }
    }
}

fn readdir_with_types(path: &str) -> io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        out.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: entry_kind(&entry.file_type()?),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 全局可替换实现（对齐 TS setFsImplementation / getFsImplementation / ...)
// ---------------------------------------------------------------------------

static ACTIVE_FS: Lazy<Mutex<Arc<dyn FsOperations + Send + Sync>>> =
    Lazy::new(|| Mutex::new(Arc::new(StdFs)));

/// 替换全局文件系统实现（对齐 TS `setFsImplementation`）。
/// 注意：与 TS 一致，不会自动更新 cwd。
pub fn set_fs_implementation(implementation: Arc<dyn FsOperations + Send + Sync>) {
    let mut guard = ACTIVE_FS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = implementation;
}

/// 获取当前激活的文件系统实现（对齐 TS `getFsImplementation`）。
/// 默认返回 `StdFs`；锁中毒时从残留状态恢复，不 panic。
pub fn get_fs_implementation() -> Arc<dyn FsOperations + Send + Sync> {
    let guard = ACTIVE_FS.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone()
}

/// 重置为默认的 std 实现（对齐 TS `setOriginalFsImplementation`）。
pub fn set_original_fs_implementation() {
    let mut guard = ACTIVE_FS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Arc::new(StdFs);
}

// ---------------------------------------------------------------------------
// 路径辅助（对齐 TS 用到的 nodePath 子集）
// ---------------------------------------------------------------------------

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn join_path(base: &str, child: &str) -> String {
    Path::new(base)
        .join(child)
        .to_string_lossy()
        .into_owned()
}

fn join_segments(base: &str, segments: &[String]) -> String {
    let mut out = PathBuf::from(base);
    for seg in segments {
        out.push(seg);
    }
    out.to_string_lossy().into_owned()
}

/// 词法规范化（对齐 Node `path.normalize`/`path.resolve` 的 `..` 处理：
/// 纯词法，不触碰文件系统）。RootDir/Prefix 之后的 `..` 丢弃。
fn normalize(path: &str) -> String {
    let mut comps: Vec<Component> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => {
                comps.clear();
                comps.push(comp);
            }
            Component::CurDir => {}
            Component::ParentDir => match comps.last() {
                Some(Component::Normal(_)) => {
                    comps.pop();
                }
                // RootDir/Prefix 之后的 `..` 直接丢弃（对齐 Node）
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => comps.push(comp),
            },
            Component::Normal(_) => comps.push(comp),
        }
    }
    let mut out = PathBuf::new();
    for comp in &comps {
        out.push(comp.as_os_str());
    }
    out.to_string_lossy().into_owned()
}

/// 对齐 Node `path.resolve(from, to)`：to 绝对则规范化 to；否则
/// join(from, to)，结果仍相对时以 cwd 补齐为绝对路径。
fn resolve_against(base: &str, target: &str) -> String {
    let joined = if Path::new(target).is_absolute() {
        target.to_string()
    } else {
        join_path(base, target)
    };
    let abs = if Path::new(&joined).is_absolute() {
        PathBuf::from(&joined)
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(&joined)
    };
    normalize(&abs.to_string_lossy())
}

/// 用户主目录（对齐 Node `os.homedir()` 的环境变量路径），
/// 并做 NFC 规范化（对齐 TS `homedir().normalize('NFC')`）。
/// 偏差：不读 /etc/passwd（无 libc 依赖），HOME 缺失时返回 None。
fn homedir_nfc() -> Option<String> {
    homedir_inner().map(|h| h.nfc().collect())
}

#[cfg(unix)]
fn homedir_inner() -> Option<String> {
    std::env::var("HOME").ok()
}

#[cfg(windows)]
fn homedir_inner() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            std::env::var("HOMEDRIVE")
                .ok()
                .zip(std::env::var("HOMEPATH").ok())
                .map(|(d, p)| format!("{d}{p}"))
        })
}

#[cfg(not(any(unix, windows)))]
fn homedir_inner() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// 导出的路径工具函数（对齐 TS 顶层导出）
// ---------------------------------------------------------------------------

/// `safeResolvePath` 的返回值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// 解析后的路径（失败或被阻断时为原路径）
    pub resolved_path: String,
    /// resolved_path 是否由符号链接解析得到
    pub is_symlink: bool,
    /// resolved_path 是否为规范路径（realpathSync 产物，调用方可跳过
    /// 进一步的符号链接解析）
    pub is_canonical: bool,
}

/// 安全解析文件路径（对齐 TS `safeResolvePath`）。
///
/// 策略：UNC 路径直接返回原路径（避免 Windows 网络请求）；FIFO/socket/
/// 字符/块设备在 realpath 前阻断（避免 realpath 阻塞在 FIFO 上）；
/// lstat/realpath 任一失败（ENOENT、断链、EACCES、ELOOP…）返回原路径，
/// 允许后续操作继续（如文件创建场景）。
pub fn safe_resolve_path(fs: &dyn FsOperations, file_path: &str) -> ResolvedPath {
    // 在任何文件系统访问之前阻断 UNC 路径（Windows 上避免 DNS/SMB）
    if file_path.starts_with("//") || file_path.starts_with('\\') {
        return ResolvedPath {
            resolved_path: file_path.to_string(),
            is_symlink: false,
            is_canonical: false,
        };
    }

    let stats = match fs.lstat_sync(file_path) {
        Ok(s) => s,
        Err(_) => {
            return ResolvedPath {
                resolved_path: file_path.to_string(),
                is_symlink: false,
                is_canonical: false,
            }
        }
    };
    // realpath 可能阻塞在等待写者的 FIFO 上，先拦截特殊文件
    if stats.is_fifo()
        || stats.is_socket()
        || stats.is_char_device()
        || stats.is_block_device()
    {
        return ResolvedPath {
            resolved_path: file_path.to_string(),
            is_symlink: false,
            is_canonical: false,
        };
    }

    match fs.realpath_sync(file_path) {
        Ok(resolved_path) => ResolvedPath {
            is_symlink: resolved_path != file_path,
            resolved_path,
            is_canonical: true,
        },
        Err(_) => ResolvedPath {
            resolved_path: file_path.to_string(),
            is_symlink: false,
            is_canonical: false,
        },
    }
}

/// 判断路径是否为已加载路径的重复（对齐 TS `isDuplicatePath`）。
/// 通过 realpath 判重；非重复时把解析路径加入 `loaded_paths`。
pub fn is_duplicate_path(
    fs: &dyn FsOperations,
    file_path: &str,
    loaded_paths: &mut std::collections::HashSet<String>,
) -> bool {
    let ResolvedPath { resolved_path, .. } = safe_resolve_path(fs, file_path);
    if loaded_paths.contains(&resolved_path) {
        return true;
    }
    loaded_paths.insert(resolved_path);
    false
}

/// 经由 realpathSync 解析路径“最深存在的祖先”（对齐 TS
/// `resolveDeepestExistingAncestorSync`）。
///
/// 输入路径可能不存在（新文件写入场景）；用 lstat 逐级上移找到第一个
/// 存在的组件，再 realpath 一次。能处理：存活的父目录符号链接、
/// 悬空文件符号链接、悬空父目录符号链接。
///
/// 返回拼接了不存在尾段的绝对路径；若任何已存在祖先都不是符号链接
/// （解析后等于自身），返回 `None`。
pub fn resolve_deepest_existing_ancestor_sync(
    fs: &dyn FsOperations,
    absolute_path: &str,
) -> Option<String> {
    let mut dir = absolute_path.to_string();
    let mut segments: Vec<String> = Vec::new();
    // 用 lstat（O(1)，不跟随链接）逐级上移；仅在找到存在组件后调用
    // 一次 realpath（O(深度)）。
    while dir != dirname(&dir) {
        let stats = match fs.lstat_sync(&dir) {
            Ok(s) => s,
            Err(_) => {
                // 真不存在，上移
                segments.insert(0, basename(&dir));
                dir = dirname(&dir);
                continue;
            }
        };
        if stats.is_symlink() {
            // 符号链接（存活或悬空）。先试 realpath（解析链式链接），
            // 失败则用 readlink 兜底悬空链接。
            return match fs.realpath_sync(&dir) {
                Ok(resolved) => Some(join_segments(&resolved, &segments)),
                Err(_) => {
                    // 悬空：realpath 失败但 lstat 看到了链接入口
                    let target = fs.readlink_sync(&dir).ok()?;
                    let abs_target = resolve_against(&dirname(&dir), &target);
                    Some(join_segments(&abs_target, &segments))
                }
            };
        }
        // 存在的非符号链接组件。一次 realpath 解析其祖先中的任何符号链接；
        // 若解析后仍是自身，则无符号链接，返回 None。
        return match fs.realpath_sync(&dir) {
            Ok(resolved) if resolved != dir => Some(join_segments(&resolved, &segments)),
            _ => None,
        };
    }
    None
}

/// 去重插入（保持 TS `Set` 的插入顺序语义）。
fn push_dedup(result: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, path: String) {
    if seen.insert(path.clone()) {
        result.push(path);
    }
}

/// 获取权限检查需要覆盖的全部路径（对齐 TS `getPathsForPermissionCheck`）。
///
/// 包含：原路径（`~` 展开后的）、符号链接链上的所有中间目标、最终
/// realpath 结果。安全相关：对 `/etc/passwd` 的 deny 规则必须能拦住
/// 实际位于 `/private/etc/passwd`（macOS）的文件。
///
/// 与 TS 一致：结果保留插入顺序（TS `Set` 的迭代序）；UNC 路径提前返回；
/// 链路遍历最多 40 层（防循环）；任何错误都“带着已有结果继续”。
pub fn get_paths_for_permission_check(input_path: &str) -> Vec<String> {
    // 防御性展开 `~`（工具应在 getPath() 里做，这里作为权限检查的纵深防御）
    let mut path = input_path.to_string();
    if let Some(home) = homedir_nfc() {
        if path == "~" {
            path = home;
        } else if let Some(rest) = path.strip_prefix("~/") {
            path = join_path(&home, rest);
        }
    }

    let mut result: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 总是检查原始路径
    push_dedup(&mut result, &mut seen, path.clone());

    // UNC 路径提前返回（Windows 上避免 DNS/SMB 网络请求）
    if path.starts_with("//") || path.starts_with('\\') {
        return result;
    }

    let fs = get_fs_implementation();

    // 跟随符号链接链，收集所有中间目标。如 test.txt -> /etc/passwd ->
    // /private/etc/passwd，三条路径都要检查。
    {
        let mut current_path = path.clone();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        const MAX_DEPTH: usize = 40; // 防止循环链接失控，对齐 SYMLOOP_MAX 常见值

        for _ in 0..MAX_DEPTH {
            if visited.contains(&current_path) {
                break; // 循环链接
            }
            visited.insert(current_path.clone());

            if !fs.exists_sync(&current_path) {
                // 路径不存在（新文件场景）。existsSync 跟随符号链接，
                // 所以对悬空符号链接同样走到这里：解析路径及其祖先中的
                // 符号链接，让权限检查看到真实目的地。
                if current_path == path {
                    if let Some(resolved) = resolve_deepest_existing_ancestor_sync(&*fs, &path) {
                        push_dedup(&mut result, &mut seen, resolved);
                    }
                }
                break;
            }

            let stats = match fs.lstat_sync(&current_path) {
                Ok(s) => s,
                Err(_) => break, // 遍历出错：带着已有结果继续
            };

            // 跳过会导致问题的特殊文件类型
            if stats.is_fifo()
                || stats.is_socket()
                || stats.is_char_device()
                || stats.is_block_device()
            {
                break;
            }

            if !stats.is_symlink() {
                break;
            }

            // 取直接链接目标；相对目标则相对链接所在目录解析为绝对路径
            let target = match fs.readlink_sync(&current_path) {
                Ok(t) => t,
                Err(_) => break,
            };
            let absolute_target = resolve_against(&dirname(&current_path), &target);
            push_dedup(&mut result, &mut seen, absolute_target.clone());
            current_path = absolute_target;
        }
    }

    // 最后用 realpath 补上最终路径，覆盖目录组件里残余的符号链接
    let resolved = safe_resolve_path(&*fs, &path);
    if resolved.is_symlink && resolved.resolved_path != path {
        push_dedup(&mut result, &mut seen, resolved.resolved_path);
    }

    result
}

// ---------------------------------------------------------------------------
// 范围读取（对齐 TS readFileRange / tailFile / readLinesReverse）
// ---------------------------------------------------------------------------

/// 从 `offset` 起读取至多 `max_bytes` 字节（对齐 TS `readFileRange`）。
/// 文件大小 <= offset 时返回 `None`。内容为 UTF-8 解码字符串
/// （非法字节替换为 U+FFFD）。
pub fn read_file_range(
    path: &str,
    offset: u64,
    max_bytes: usize,
) -> io::Result<Option<ReadFileRangeResult>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size <= offset {
        return Ok(None);
    }
    let bytes_to_read = usize::try_from((size - offset).min(max_bytes as u64))
        .unwrap_or(usize::MAX);
    let mut buffer = vec![0u8; bytes_to_read];
    file.seek(SeekFrom::Start(offset))?;
    let mut total_read = 0usize;
    while total_read < bytes_to_read {
        let n = file.read(&mut buffer[total_read..])?;
        if n == 0 {
            break;
        }
        total_read += n;
    }
    let content = String::from_utf8_lossy(&buffer[..total_read]).into_owned();
    Ok(Some(ReadFileRangeResult {
        content,
        bytes_read: total_read as u64,
        bytes_total: size,
    }))
}

/// 读取文件末尾 `max_bytes` 字节（对齐 TS `tailFile`）。
/// 文件小于 max_bytes 时返回整个文件；空文件返回零值结果。
pub fn tail_file(path: &str, max_bytes: usize) -> io::Result<ReadFileRangeResult> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(ReadFileRangeResult {
            content: String::new(),
            bytes_read: 0,
            bytes_total: 0,
        });
    }
    let offset = size.saturating_sub(max_bytes as u64);
    let bytes_to_read = usize::try_from(size - offset).unwrap_or(usize::MAX);
    let mut buffer = vec![0u8; bytes_to_read];
    file.seek(SeekFrom::Start(offset))?;
    let mut total_read = 0usize;
    while total_read < bytes_to_read {
        let n = file.read(&mut buffer[total_read..])?;
        if n == 0 {
            break;
        }
        total_read += n;
    }
    let content = String::from_utf8_lossy(&buffer[..total_read]).into_owned();
    Ok(ReadFileRangeResult {
        content,
        bytes_read: total_read as u64,
        bytes_total: size,
    })
}

/// 从文件末尾向前逐行产出（对齐 TS 异步生成器 `readLinesReverse`）。
///
/// 以 4KB 块回溯读取，避免整文件加载。跨块边界保留**原始字节**
/// （而非已解码字符串），避免被 4KB 边界切开的多元 UTF-8 序列损坏
/// （对 history.jsonl 这类 JSONL，损坏序列会导致解析失败丢条目）。
/// 空行（`line` 假值）被跳过——与 TS 一致。
pub struct ReverseLines {
    file: File,
    position: u64,
    /// 尚未回溯到换行符的原始字节（多字节 UTF-8 序列的跨块载体）
    remainder: Vec<u8>,
    /// 已解析待产出的行（栈式，pop 即逆序）
    pending: Vec<String>,
    /// 回溯结束后 remainder 是否已产出
    remainder_done: bool,
}

const REVERSE_LINE_CHUNK_SIZE: usize = 1024 * 4;

impl ReverseLines {
    /// 打开文件准备逆序读行。
    pub fn new(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            position: file.metadata()?.len(),
            file,
            remainder: Vec::new(),
            pending: Vec::new(),
            remainder_done: false,
        })
    }

    /// 在 position 处读取一块（position 前移），返回实际读到的字节。
    fn read_chunk(&mut self) -> Result<Vec<u8>, io::Error> {
        let chunk_size = REVERSE_LINE_CHUNK_SIZE.min(usize::try_from(self.position).unwrap_or(0));
        let mut chunk = vec![0u8; chunk_size];
        if chunk_size == 0 {
            return Ok(chunk);
        }
        self.position -= chunk_size as u64;
        self.file.seek(SeekFrom::Start(self.position))?;
        let mut filled = 0usize;
        while filled < chunk_size {
            let n = self.file.read(&mut chunk[filled..])?;
            if n == 0 {
                break; // 文件收缩等异常情况，有多少用多少
            }
            filled += n;
        }
        chunk.truncate(filled);
        Ok(chunk)
    }
}

impl Iterator for ReverseLines {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(line) = self.pending.pop() {
                return Some(Ok(line));
            }
            if self.position == 0 {
                break;
            }
            let chunk = match self.read_chunk() {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            // combined = 新块 + 上一轮的 remainder
            let mut combined = Vec::with_capacity(chunk.len() + self.remainder.len());
            combined.extend_from_slice(&chunk);
            combined.extend_from_slice(&self.remainder);

            let Some(first_newline) = combined.iter().position(|&b| b == b'\n') else {
                // 本块无换行：整块进入 remainder，继续向前回溯
                self.remainder = combined;
                continue;
            };
            // remainder = 首个换行符之前的部分（仍待回溯）
            self.remainder = combined[..first_newline].to_vec();
            // 换行符之后的部分按 '\n' 切行，逆序压栈（跳过空行）
            let tail = &combined[first_newline + 1..];
            let text = String::from_utf8_lossy(tail).into_owned();
            for line in text.split('\n').rev() {
                if !line.is_empty() {
                    self.pending.push(line.to_string());
                }
            }
            // 回到循环开头：pop 或继续读下一块
        }
        // 文件已回溯到头：产出剩余 remainder（仅一次）
        if !self.remainder_done {
            self.remainder_done = true;
            if !self.remainder.is_empty() {
                return Some(Ok(String::from_utf8_lossy(&self.remainder).into_owned()));
            }
        }
        None
    }
}

/// 便捷入口：打开文件并把行逆序收集成 `Vec`（内部仍是逐块回溯的
/// `ReverseLines`；内存占用受返回集合大小而非文件大小约束）。
pub fn read_lines_reverse(path: &str) -> io::Result<Vec<String>> {
    ReverseLines::new(path)?.collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("fsops_test_{}_{}", std::process::id(), name));
        p.to_string_lossy().into_owned()
    }

    fn write_test_file(path: &str, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn std_fs_mkdir_read_write_roundtrip() {
        let dir = temp_path("roundtrip_dir/nested");
        let fs = StdFs;
        fs.mkdir(&dir, None).unwrap();
        assert!(fs.exists_sync(&dir));

        let file = join_path(&dir, "file.txt");
        let mut out = fs.create_write_stream(&file).unwrap();
        out.write_all(b"hello").unwrap();
        drop(out);

        assert_eq!(
            fs.read_file(&file, BufferEncoding::Utf8).unwrap(),
            "hello"
        );
        assert_eq!(fs.read_file_bytes_sync(&file).unwrap(), b"hello");
        assert!(!fs.is_dir_empty_sync(&dir).unwrap());

        let stats = fs.stat_sync(&file).unwrap();
        assert!(stats.is_file() && !stats.is_dir() && stats.size == 5);

        fs.rm_sync(&dir, Some(&RmOptions { recursive: true, force: true })).unwrap();
        assert!(!fs.exists_sync(&dir));
    }

    #[test]
    fn mkdir_swallows_eexist() {
        let dir = temp_path("eexist");
        let fs = StdFs;
        fs.mkdir_sync(&dir, None).unwrap();
        // 二次创建不能报错（对齐 TS 吞 EEXIST）
        fs.mkdir_sync(&dir, None).unwrap();
        fs.rm_sync(&dir, Some(&RmOptions { recursive: true, force: true })).unwrap();
    }

    #[test]
    fn rm_force_ignores_missing() {
        let fs = StdFs;
        let missing = temp_path("does_not_exist_xyz");
        fs.rm_sync(&missing, Some(&RmOptions { recursive: false, force: true })).unwrap();
        assert!(fs.rm_sync(&missing, None).is_err()); // 无 force 必须报错
    }

    #[test]
    fn append_with_mode_then_fallback() {
        let path = temp_path("append.txt");
        let fs = StdFs;
        fs.append_file_sync(&path, "a", Some(&AppendFileOptions { mode: Some(0o600) })).unwrap();
        fs.append_file_sync(&path, "b", Some(&AppendFileOptions { mode: Some(0o600) })).unwrap();
        assert_eq!(fs.read_file_sync(&path, BufferEncoding::Utf8).unwrap(), "ab");
        fs.unlink_sync(&path).unwrap();
    }

    #[test]
    fn unsupported_encoding_errors() {
        let path = temp_path("enc.txt");
        write_test_file(&path, "x");
        let fs = StdFs;
        assert!(fs.read_file(&path, BufferEncoding::Base64).is_err());
        fs.unlink_sync(&path).unwrap();
    }

    #[test]
    fn read_sync_returns_zero_filled_buffer() {
        let path = temp_path("readsync.txt");
        write_test_file(&path, "abcdef");
        let fs = StdFs;
        let r = fs.read_sync(&path, 16).unwrap();
        assert_eq!(r.bytes_read, 6);
        assert_eq!(r.buffer.len(), 16);
        assert_eq!(&r.buffer[..6], b"abcdef");
        fs.unlink_sync(&path).unwrap();
    }

    #[test]
    fn read_file_bytes_respects_max() {
        let path = temp_path("bytes.txt");
        write_test_file(&path, "0123456789");
        let fs = StdFs;
        assert_eq!(fs.read_file_bytes(&path, Some(4)).unwrap(), b"0123");
        assert_eq!(fs.read_file_bytes(&path, None).unwrap(), b"0123456789");
        fs.unlink_sync(&path).unwrap();
    }

    #[test]
    fn range_tail_and_reverse_lines_with_multibyte() {
        let path = temp_path("range.txt");
        // 5 行，第二行含多字节 UTF-8 字符
        write_test_file(&path, "one\ntwó\nthree\nfour\nfive");
        let r = read_file_range(&path, 4, 100).unwrap().unwrap();
        assert_eq!(&r.content, "\ntwó\nthree\nfour\nfive");
        assert_eq!(r.bytes_total, 24);

        let t = tail_file(&path, 10).unwrap();
        assert!(t.content.ends_with("five"));
        assert_eq!(t.bytes_total, 24);

        let lines = read_lines_reverse(&path).unwrap();
        assert_eq!(lines, vec!["five", "four", "three", "twó", "one"]);

        let empty_range = read_file_range(&path, 1000, 10).unwrap();
        assert!(empty_range.is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reverse_lines_handles_chunks_larger_than_file() {
        let path = temp_path("small.txt");
        write_test_file(&path, "only-one-line");
        let lines = read_lines_reverse(&path).unwrap();
        assert_eq!(lines, vec!["only-one-line"]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn safe_resolve_path_nonexistent_returns_original() {
        let fs = StdFs;
        let missing = temp_path("missing_xyz");
        let r = safe_resolve_path(&fs, &missing);
        assert!(!r.is_symlink && !r.is_canonical && r.resolved_path == missing);
    }

    #[test]
    fn is_duplicate_path_dedups() {
        let fs = StdFs;
        let mut loaded: HashSet<String> = HashSet::new();
        let a = "/etc/hosts";
        assert!(!is_duplicate_path(&fs, a, &mut loaded));
        assert!(is_duplicate_path(&fs, a, &mut loaded));
    }

    #[test]
    #[cfg(unix)]
    fn permission_check_paths_follow_symlink_chain() {
        let dir = temp_path("perm_chain");
        let fs_impl = StdFs;
        fs_impl.mkdir_sync(&dir, None).unwrap();

        // target.txt <- link1 <- link2（两级链）
        let target = join_path(&dir, "target.txt");
        write_test_file(&target, "data");
        let link1 = join_path(&dir, "link1.txt");
        let link2 = join_path(&dir, "link2.txt");
        std::os::unix::fs::symlink(&target, &link1).unwrap();
        std::os::unix::fs::symlink(&link1, &link2).unwrap();

        let paths = get_paths_for_permission_check(&link2);
        // 链上每个目标 + realpath 终点都应覆盖
        assert!(paths.contains(&link2));
        assert!(paths.contains(&link1));
        assert!(paths.iter().any(|p| p.ends_with("target.txt") && !p.ends_with("link1.txt") && !p.ends_with("link2.txt")));

        fs_impl
            .rm_sync(&dir, Some(&RmOptions { recursive: true, force: true }))
            .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn permission_check_resolves_dangling_symlink_ancestor() {
        let dir = temp_path("perm_dangling");
        let fs_impl = StdFs;
        fs_impl.mkdir_sync(&dir, None).unwrap();

        // ./dangling_dir -> /nonexistent_abc，内部文件路径不存在
        let link = join_path(&dir, "dangling_dir");
        std::os::unix::fs::symlink("/nonexistent_abc_xyz", &link).unwrap();
        let inside = join_path(&link, "file.txt");

        let paths = get_paths_for_permission_check(&inside);
        // 原路径 + 解析出的悬空目标都要覆盖
        assert!(paths.contains(&inside));
        assert!(paths.iter().any(|p| p.contains("nonexistent_abc_xyz")));

        fs_impl
            .rm_sync(&dir, Some(&RmOptions { recursive: true, force: true }))
            .unwrap();
    }

    #[test]
    fn unc_paths_short_circuit() {
        let fs = StdFs;
        let r = safe_resolve_path(&fs, "//server/share/file");
        assert_eq!(r.resolved_path, "//server/share/file");
        let paths = get_paths_for_permission_check("\\\\server\\share");
        assert_eq!(paths, vec!["\\\\server\\share"]);
    }

    #[test]
    fn normalize_helpers() {
        assert_eq!(normalize("/a/b/../c"), "/a/c");
        assert_eq!(normalize("/a/./b/../../c"), "/c");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(resolve_against("/foo", "bar"), "/foo/bar");
        assert_eq!(resolve_against("/foo", "/abs/x"), "/abs/x");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(basename("/a/b"), "b");
    }

    #[test]
    fn global_fs_roundtrips() {
        let default = get_fs_implementation();
        assert!(get_fs_implementation().cwd().is_ok());
        set_original_fs_implementation();
        let again = get_fs_implementation();
        let _ = again; // 默认实现可替换、可复位即可
        let _ = default;
    }

    #[test]
    fn buffer_encoding_parse() {
        assert_eq!(BufferEncoding::from_str("utf8"), Some(BufferEncoding::Utf8));
        assert_eq!(BufferEncoding::from_str("UTF-8"), Some(BufferEncoding::Utf8));
        assert_eq!(BufferEncoding::from_str("base64"), Some(BufferEncoding::Base64));
        assert_eq!(BufferEncoding::from_str("nope"), None);
    }
}
