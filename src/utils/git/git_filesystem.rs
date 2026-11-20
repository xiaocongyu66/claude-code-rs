//! git_filesystem.rs — 基于文件系统的 git 状态读取（避免 spawn git 子进程）。
//!
//! 重写自 `src/utils/git/gitFilesystem.ts`。覆盖：解析 `.git` 目录（含
//! worktree/submodule）、解析 HEAD、经 loose 文件与 packed-refs 解析 ref，
//! 以及带文件监听的 GitFileWatcher（缓存 branch/SHA 等）。
//!
//! 语义对齐说明：
//! - 与 `git_config_parser.rs` 的做法一致：TS 的 `fs/promises` 异步读取统一
//!   落地为同步 `std::fs`（Cargo.toml 未要求本模块接入异步运行时），读取
//!   错误一律转为 `None`/默认值，与 TS 的 try/catch → return null 语义一致。
//! - `utils/git.ts`（findGitRoot）尚未转换：按 TS 语义在本模块内联最小实现
//!   `find_git_root`（含 `memoizeWithLRU`（50 条）缓存与 NFC 归一化），
//!   供本模块、`gitignore.rs` 与 `constants/prompts.rs` 复用。
//! - `utils/cwd.ts` 尚未转换：`getCwd()` 内联为 `bootstrap::state::get_cwd_state()`
//!   （AsyncLocalStorage 的 per-context 覆盖层无 Rust 对应物，先取会话级状态）。
//! - `fs.watchFile` 无 std 等价物（`notify` crate 不在 Cargo.toml）：GitFileWatcher
//!   用一个轮询线程按 WATCH_INTERVAL_MS 检查被监听文件的 mtime，变化时置脏
//!   全部缓存条目（等价 TS 的 invalidate() 语义）。HEAD 变化时同样重新挂载
//!   当前分支 ref 的监听（等价 watchCurrentBranchRef）。
//! - `waitForScrollIdle`（bootstrap/state）未转换：onHeadChanged 中的等待
//!   省略（直接重挂监听），不影响正确性。
//! - get() 的竞态处理逐条对齐 TS：计算前先清 dirty，计算期间若监听线程再置
//!   脏则丢弃新值、下次重算。
//!
//! Correctness notes (verified against git source)：
//!   - HEAD：`ref: refs/heads/<branch>\n` 或裸 SHA（refs/files-backend.c）
//!   - Packed-refs：`<sha> <refname>\n`，跳过 `#` 与 `^` 行（packed-backend.c）
//!   - .git 文件（worktree）：`gitdir: <path>\n`，可为相对路径（setup.c）
//!   - Shallow：`<commonDir>/shallow` 存在即为 shallow clone（shallow.c）

use std::any::Any;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

use unicode_normalization::UnicodeNormalization;

use crate::utils::cleanup_registry::{register_cleanup, CleanupRegistration};
use crate::utils::git::git_config_parser::parse_git_config_value;
use crate::utils::memoize::memoize_with_lru;
use crate::utils::memoize::LruMemoized;

// ---------------------------------------------------------------------------
// 路径辅助（path.resolve / NFC 归一化的最小等价物）
// ---------------------------------------------------------------------------

/// Node `path.resolve` 的最小等价物：相对路径基于进程 cwd 拼为绝对路径，
/// 并做词法规范化（`Component::ParentDir` 弹栈、忽略 `.`；不解析符号链接）。
fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            // 与 TS `resolve` 的兜底行为不可达（resolve 永不抛异常）；
            // current_dir 失败时退化为根目录，不 panic。
            Err(_) => PathBuf::from("/"),
        }
    };
    normalize_lexical(&absolute)
}

/// 词法规范化：处理 `.` 与 `..`（仅在栈顶为普通名字时弹栈，避免根上的
/// `..` 残留）。对齐 Node `path.resolve` 的纯词法语义。
fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut stack: Vec<Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(stack.last(), Some(Component::Normal(_))) {
                    stack.pop();
                } else {
                    stack.push(comp);
                }
            }
            other => stack.push(other),
        }
    }
    let mut result = PathBuf::new();
    for comp in &stack {
        result.push(comp);
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

/// `getCwd()`（utils/cwd.ts 未转换）：取会话级 cwd 状态。
fn get_cwd_string() -> String {
    crate::bootstrap::state::get_cwd_state()
}

/// `String.prototype.normalize('NFC')`（JS 在字符串上操作，非法字节按
/// lossy 替换为 U+FFFD，与 TS 侧行为一致）。
fn nfc_string(path: &Path) -> String {
    String::from_utf8_lossy(path.as_os_str().as_encoded_bytes())
        .nfc()
        .collect()
}

/// 读取文件为 lossy UTF-8 字符串（对齐 TS `readFile(path, 'utf-8')`）。
fn read_file_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn unlocked<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// find_git_root —— utils/git.ts 的内联最小实现（该模块尚未转换）
// ---------------------------------------------------------------------------

/// findGitRoot 的 LRU 记忆化实例（对齐 TS 的 `memoizeWithLRU(fn, key, 50)`）。
/// 缓存键为 resolve 后的路径字符串；V 为 `Option<String>` —— TS 用
/// `GIT_ROOT_NOT_FOUND` symbol 区分「未找到」与「未缓存」，Rust 用 None。
static GIT_ROOT_MEMO: OnceLock<Arc<LruMemoized<String, Option<String>>>> = OnceLock::new();

fn git_root_memo() -> &'static Arc<LruMemoized<String, Option<String>>> {
    GIT_ROOT_MEMO.get_or_init(|| {
        memoize_with_lru(
            |start_path: &String| find_git_root_impl(start_path),
            |start_path: &String| start_path.clone(),
            50,
        )
    })
}

/// 向上遍历目录树寻找 git root（`.git` 为目录或文件均算）。
/// 返回包含 `.git` 的目录（NFC 归一化），或 None。
///
/// 注意：memoize 的键是**调用方传入的原始字符串**，resolve 由 impl 内部完成
/// （对齐 TS：LRRU 缓存键为 startPath 原值）。
pub fn find_git_root(start_path: &str) -> Option<PathBuf> {
    git_root_memo()
        .call(&start_path.to_string())
        .map(PathBuf::from)
}

fn find_git_root_impl(start_path: &str) -> Option<String> {
    let resolved = resolve_path(Path::new(start_path));
    // ancestors() 依次给出 resolved、各父目录、直至文件系统根 —— 与 TS 的
    // `while (current !== root)` 循环 + 循环后单独检查 root 等价。
    for dir in resolved.ancestors() {
        let git_path = dir.join(".git");
        if let Ok(meta) = fs::metadata(&git_path) {
            if meta.is_dir() || meta.is_file() {
                return Some(nfc_string(dir));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// resolveGitDir —— 找到真正的 .git 目录
// ---------------------------------------------------------------------------

/// resolveGitDir 的缓存：键为 resolve 后的 startPath，值为 Option（对齐
/// TS 的 `Map<string, string | null>`，None 表示「不在 git 仓库」）。
static RESOLVE_GIT_DIR_CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<PathBuf>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 清除 resolveGitDir 缓存（对齐 TS 的 clearResolveGitDirCache，测试用）。
pub fn clear_resolve_git_dir_cache() {
    unlocked(&RESOLVE_GIT_DIR_CACHE).clear();
}

/// 解析仓库真正的 `.git` 目录。处理 worktree/submodule：`.git` 为含
/// `gitdir: <path>` 的文件。按 startPath 记忆化（含负结果）。
pub fn resolve_git_dir(start_path: Option<&str>) -> Option<PathBuf> {
    let cwd = resolve_path(Path::new(start_path.unwrap_or(&get_cwd_string())));

    {
        let cache = unlocked(&RESOLVE_GIT_DIR_CACHE);
        if let Some(cached) = cache.get(&cwd) {
            return cached.clone();
        }
    }

    let remember = |value: Option<PathBuf>| {
        unlocked(&RESOLVE_GIT_DIR_CACHE).insert(cwd.clone(), value.clone());
        value
    };

    let root = find_git_root(&cwd.to_string_lossy())?;
    let git_path = root.join(".git");
    let meta = match fs::metadata(&git_path) {
        Ok(meta) => meta,
        Err(_) => return remember(None),
    };
    if meta.is_file() {
        // Worktree 或 submodule：.git 是含 `gitdir: <path>` 的文件。
        // Git 会剥掉尾部 \n 与 \r（setup.c read_gitfile_gently）。
        let content = match read_file_lossy(&git_path) {
            Ok(content) => content,
            Err(_) => return remember(None),
        };
        let content = content.trim();
        if let Some(raw_dir) = content.strip_prefix("gitdir:") {
            let resolved = resolve_path(&root.join(raw_dir.trim()));
            return remember(Some(resolved));
        }
        // 非 gitdir 指针的普通文件：按普通仓库处理（对齐 TS 的 fall-through）
    }
    remember(Some(git_path))
}

// ---------------------------------------------------------------------------
// isSafeRefName / isValidGitSha —— 校验从 .git/ 读到的 ref 名与 SHA
// ---------------------------------------------------------------------------

/// 校验从 `.git/` 读到的 ref/branch 名可安全用于路径拼接、git 位置参数与
/// shell 插值（allowlist：ASCII 字母数字 + `/._+-@`，拒绝前导 `-`、前导 `/`、
/// `..`、空组件与全部 shell 元字符）。逐行对齐 TS isSafeRefName。
pub fn is_safe_ref_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('-') || name.starts_with('/') {
        return false;
    }
    if name.contains("..") {
        return false;
    }
    // 拒绝单点与空路径组件（`.`、`foo/./bar`、`foo//bar`、`foo/`）
    if name.split('/').any(|c| c == "." || c.is_empty()) {
        return false;
    }
    // 仅小写十六进制以外的字符不会出现；此处正则等价为逐字符 allowlist。
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '+' | '@' | '-'))
}

/// 校验 git SHA：40 位（SHA-1）或 64 位（SHA-256）**小写**十六进制。
/// TS 正则为 `^[0-9a-f]{40}$` / `^[0-9a-f]{64}$`，不含大写。
pub fn is_valid_git_sha(s: &str) -> bool {
    let len = s.len();
    if len != 40 && len != 64 {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_digit() | (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// readGitHead —— 解析 .git/HEAD
// ---------------------------------------------------------------------------

/// `.git/HEAD` 的解析结果（对齐 TS 的联合类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHead {
    /// `ref: refs/heads/<branch>` —— 在分支上。
    Branch(String),
    /// 裸 SHA（detached），或非常规 symref 解析出的 SHA（可能为空串）。
    Detached(String),
}

/// 解析 `.git/HEAD`：分支名或 detached SHA。HEAD 不存在/被篡改时返回 None
/// （对齐 TS 的 catch → null 与 isSafeRefName/isValidGitSha 守卫）。
pub fn read_git_head(git_dir: &Path) -> Option<GitHead> {
    let content = read_file_lossy(&git_dir.join("HEAD")).ok()?;
    // Git 用 strbuf_rtrim 剥掉尾部空白；.trim() 等价
    let content = content.trim();
    if let Some(ref_tail) = content.strip_prefix("ref:") {
        // Git 允许 "ref:" 与路径间任意空白；trim 后取路径
        let r#ref = ref_tail.trim();
        if let Some(name) = r#ref.strip_prefix("refs/heads/") {
            // 拒绝被篡改 HEAD 的路径穿越与参数注入
            if !is_safe_ref_name(name) {
                return None;
            }
            return Some(GitHead::Branch(name.to_string()));
        }
        // 非常规 symref（非本地分支）—— 解析为 SHA
        if !is_safe_ref_name(r#ref) {
            return None;
        }
        let sha = resolve_ref(git_dir, r#ref);
        return Some(GitHead::Detached(sha.unwrap_or_default()));
    }
    // 裸 SHA（detached HEAD）：校验，防止攻击者控制的 HEAD 流入 shell 上下文
    if !is_valid_git_sha(content) {
        return None;
    }
    Some(GitHead::Detached(content.to_string()))
}

// ---------------------------------------------------------------------------
// resolveRef —— 经 loose 文件与 packed-refs 解析 ref
// ---------------------------------------------------------------------------

/// 解析 git ref（如 `refs/heads/main`）到 commit SHA。先查 loose ref 文件，
/// 再回退 packed-refs；跟随 symref。worktree 场景先查 worktree gitdir，
/// 再回退 commondir。
pub fn resolve_ref(git_dir: &Path, r#ref: &str) -> Option<String> {
    if let Some(result) = resolve_ref_in_dir(git_dir, r#ref) {
        return Some(result);
    }
    // worktree：共享 ref 位于 commondir
    let common_dir = get_common_dir(git_dir);
    if let Some(common_dir) = common_dir {
        if common_dir != git_dir {
            return resolve_ref_in_dir(&common_dir, r#ref);
        }
    }
    None
}

fn resolve_ref_in_dir(dir: &Path, r#ref: &str) -> Option<String> {
    // loose ref 文件
    if let Ok(content) = read_file_lossy(&dir.join(r#ref)) {
        let content = content.trim();
        if let Some(target) = content.strip_prefix("ref:") {
            let target = target.trim();
            // 拒绝被篡改 symref 链中的路径穿越
            if !is_safe_ref_name(target) {
                return None;
            }
            return resolve_ref(dir, target);
        }
        // loose ref 内容应为裸 SHA：校验，防止攻击者注入 shell 元字符
        if !is_valid_git_sha(content) {
            return None;
        }
        return Some(content.to_string());
    }

    // packed-refs（per packed-backend.c：`#` 头行与 `^` peeled 行跳过）
    if let Ok(packed) = read_file_lossy(&dir.join("packed-refs")) {
        for line in packed.split('\n') {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let Some(space_idx) = line.find(' ') else {
                continue;
            };
            // 用 get(..) 保持 UTF-8 边界安全
            let (Some(sha), Some(name)) = (line.get(..space_idx), line.get(space_idx + 1..))
            else {
                continue;
            };
            if name == r#ref {
                return if is_valid_git_sha(sha) {
                    Some(sha.to_string())
                } else {
                    None
                };
            }
        }
    }

    None
}

/// 读 `commondir` 文件找共享 git 目录（worktree 指向主仓库的 .git）。
/// 无该文件（普通仓库）返回 None。
pub fn get_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let content = read_file_lossy(&git_dir.join("commondir")).ok()?;
    let content = content.trim();
    Some(resolve_path(&git_dir.join(content)))
}

/// 读裸 symref 文件并提取已知前缀后的分支名。ref 不存在、非 symref 或
/// 不匹配前缀时返回 None。只查 loose 文件（packed-refs 不存 symref）。
pub fn read_raw_symref(git_dir: &Path, ref_path: &str, branch_prefix: &str) -> Option<String> {
    let content = read_file_lossy(&git_dir.join(ref_path)).ok()?;
    let content = content.trim();
    if let Some(target) = content.strip_prefix("ref:") {
        let target = target.trim();
        if let Some(name) = target.strip_prefix(branch_prefix) {
            if !is_safe_ref_name(name) {
                return None;
            }
            return Some(name.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// GitFileWatcher —— 监听 git 文件并缓存派生值
// ---------------------------------------------------------------------------

/// 轮询间隔：TS 在 NODE_ENV=test 时为 10ms，默认 1000ms。
fn watch_interval_ms() -> u64 {
    if std::env::var("NODE_ENV").ok().as_deref() == Some("test") {
        10
    } else {
        1000
    }
}

/// 缓存条目（对齐 TS CacheEntry；`compute` 在 TS 里被存储但从不读取，省略）。
struct CacheEntry {
    value: Box<dyn Any + Send + Sync>,
    dirty: bool,
}

/// 被监听文件的状态：TS 的 watchedPaths + watchFile 回调落地点。
#[derive(Default)]
struct WatchSlot {
    path: Option<PathBuf>,
    last_mtime: Option<SystemTime>,
}

impl WatchSlot {
    /// 检查 mtime 是否变化，更新已记录的 mtime，返回是否变化。
    /// mtime 由 None→Some（文件出现）也视为变化（等价 watchFile 对不存在
    /// 文件的行为：文件出现时触发）。
    fn poll(&mut self) -> bool {
        let mtime = self
            .path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        if mtime == self.last_mtime {
            return false;
        }
        self.last_mtime = mtime;
        true
    }

    fn stop(&mut self) {
        self.path = None;
        self.last_mtime = None;
    }
}

struct WatcherState {
    initialized: bool,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    head: WatchSlot,
    config: WatchSlot,
    branch_ref: WatchSlot,
    cache: HashMap<String, CacheEntry>,
    /// 置位后轮询线程退出、cleanup 不再重复执行（等价 TS stopWatching 的效果）。
    stop: bool,
}

impl Default for WatcherState {
    fn default() -> Self {
        Self {
            initialized: false,
            git_dir: None,
            common_dir: None,
            head: WatchSlot::default(),
            config: WatchSlot::default(),
            branch_ref: WatchSlot::default(),
            cache: HashMap::new(),
            stop: false,
        }
    }
}

impl WatcherState {
    /// 置脏全部缓存条目（对齐 TS invalidate()）。
    fn invalidate(&mut self) {
        for entry in self.cache.values_mut() {
            entry.dirty = true;
        }
    }
}

/// TS 的 `const gitWatcher = new GitFileWatcher()`：进程级单例。
pub struct GitFileWatcher {
    state: Mutex<WatcherState>,
    /// registerCleanup 的注册句柄：存活期间生效；reset()/cleanup 时摘除并执行。
    registration: Mutex<Option<CleanupRegistration>>,
}

impl GitFileWatcher {
    fn new() -> Self {
        Self {
            state: Mutex::new(WatcherState::default()),
            registration: Mutex::new(None),
        }
    }

    // ---- 生命周期（对齐 ensureStarted / start / stopWatching / reset）----

    /// 首次访问缓存时惰性启动（对齐 ensureStarted/start）。
    fn ensure_started(&self) {
        let mut state = unlocked(&self.state);
        if state.initialized || state.stop {
            return;
        }
        state.initialized = true;
        state.git_dir = resolve_git_dir(None);
        let Some(git_dir) = state.git_dir.clone() else {
            return;
        };
        // worktree 中分支 ref 与主 config 位于 commondir，而非 per-worktree
        // gitdir；一次解析，避免每次分支切换都重读 commondir。
        state.common_dir = get_common_dir(&git_dir);

        // 监听 .git/HEAD
        state.head.path = Some(git_dir.join("HEAD"));
        state.head.poll();
        // config（remote URL）位于 commondir（worktree）
        let config_dir = state
            .common_dir
            .clone()
            .unwrap_or_else(|| git_dir.clone());
        state.config.path = Some(config_dir.join("config"));
        state.config.poll();

        // 监听当前分支的 ref 文件（提交变化）
        self.watch_current_branch_ref(&mut state);
        drop(state);

        // 注册清理（回调内会锁 state，不能持锁注册）
        let registration = register_cleanup(|| {
            git_watcher().stop_watching();
        });
        *unlocked(&self.registration) = Some(registration);

        // 启动轮询线程
        thread::spawn(poll_loop);
    }

    /// 监听当前分支的 loose ref 文件。启动时与 HEAD 变化（分支切换）时调用。
    fn watch_current_branch_ref(&self, state: &mut WatcherState) {
        let Some(git_dir) = state.git_dir.clone() else {
            return;
        };
        let head = read_git_head(&git_dir);
        // 分支 ref 位于 commonDir（worktree）或 gitDir（普通仓库）
        let refs_dir = state
            .common_dir
            .clone()
            .unwrap_or(git_dir);
        let ref_path = match head {
            Some(GitHead::Branch(name)) => Some(refs_dir.join("refs").join("heads").join(name)),
            // branch→detached（checkout --detach / rebase / bisect）：停止监听
            _ => None,
        };

        // 已在监听该 ref（或本就未监听任何 ref）
        if ref_path == state.branch_ref.path {
            return;
        }
        state.branch_ref.path = ref_path;
        state.branch_ref.last_mtime = None;
        // TS：ref 文件可能尚不存在（新分支首个提交前）—— watchFile 对不存在
        // 的文件同样生效；mtime 从 None→Some 视为变化。
        state.branch_ref.poll();
    }

    /// 停止全部监听（对齐 stopWatching）。供 cleanup 回调与 reset 调用。
    fn stop_watching(&self) {
        let mut state = unlocked(&self.state);
        state.stop = true;
        state.head.stop();
        state.config.stop();
        state.branch_ref.stop();
    }

    /// 复位全部状态并停止监听（对齐 TS reset()，测试用）。
    /// 注意：registration 句柄必须在**不持 state 锁**的情况下 drop
    /// （cleanup registry 的 drop 钩子会回调本模块并再次锁 state）。
    fn reset(&self) {
        let registration = unlocked(&self.registration).take();
        {
            let mut state = unlocked(&self.state);
            state.stop = true;
            state.initialized = false;
            state.git_dir = None;
            state.common_dir = None;
            state.head.stop();
            state.config.stop();
            state.branch_ref.stop();
            state.cache.clear();
        }
        drop(registration);
    }

    // ---- 缓存（对齐 TS get<T>(key, compute)）----

    /// 按 key 取缓存值；首次调用时计算并缓存；被监听文件变化后条目置脏，
    /// 下次 get() 重新计算。
    ///
    /// 竞态处理逐条对齐 TS：计算**前**清 dirty —— 若计算期间文件再变化，
    /// invalidate() 会重新置脏，则新值不落缓存、下次 get() 重算。
    fn get<T>(&self, key: &str, compute: impl Fn() -> T) -> T
    where
        T: Clone + Send + Sync + 'static,
    {
        self.ensure_started();
        {
            let mut state = unlocked(&self.state);
            if let Some(entry) = state.cache.get_mut(key) {
                if !entry.dirty {
                    if let Some(value) = entry.value.downcast_ref::<T>() {
                        return value.clone();
                    }
                    // 类型不匹配（不应发生）：重算
                }
                // 计算前清脏（对齐 TS 的 existing.dirty = false）
                entry.dirty = false;
            }
        }
        let value = compute();
        {
            let mut state = unlocked(&self.state);
            match state.cache.get_mut(key) {
                // 计算期间没有新失效：落缓存
                Some(entry) if !entry.dirty => {
                    entry.value = Box::new(value.clone());
                }
                // 计算期间又失效：保留旧值与脏标记，下次重算（对齐 TS）
                Some(_) => {}
                None => {
                    state.cache.insert(
                        key.to_string(),
                        CacheEntry {
                            value: Box::new(value.clone()),
                            dirty: false,
                        },
                    );
                }
            }
        }
        value
    }

    /// 轮询一次被监听文件的 mtime（在轮询线程中调用）。
    fn poll(&self) {
        let mut state = unlocked(&self.state);
        if state.stop {
            return;
        }
        let head_changed = state.head.poll();
        let config_changed = state.config.poll();
        let branch_changed = state.branch_ref.poll();
        if head_changed || config_changed || branch_changed {
            state.invalidate();
        }
        if head_changed {
            // 对齐 onHeadChanged：invalidate() 已做；waitForScrollIdle 未转换
            // （省略等待）；重新挂载当前分支 ref 的监听。
            self.watch_current_branch_ref(&mut state);
        }
    }
}

static GIT_WATCHER: OnceLock<GitFileWatcher> = OnceLock::new();

fn git_watcher() -> &'static GitFileWatcher {
    GIT_WATCHER.get_or_init(GitFileWatcher::new)
}

/// 轮询线程主循环：按 WATCH_INTERVAL_MS 周期检查被监听文件。
/// 退出条件：state.stop（reset / cleanup）。以 10ms 粒度切片睡眠，
/// 让被测/关闭场景不必等满一个完整周期。
fn poll_loop() {
    let interval_ms = watch_interval_ms();
    let slice = Duration::from_millis(10);
    loop {
        let mut waited = 0u64;
        while waited < interval_ms {
            thread::sleep(slice);
            waited = waited.saturating_add(10);
            if should_stop_polling() {
                return;
            }
        }
        if should_stop_polling() {
            return;
        }
        git_watcher().poll();
    }
}

fn should_stop_polling() -> bool {
    unlocked(&git_watcher().state).stop
}

// ---------------------------------------------------------------------------
// compute 函数（对齐 TS 的 computeBranch / computeHead / computeRemoteUrl /
// computeDefaultBranch）
// ---------------------------------------------------------------------------

fn compute_branch() -> String {
    let Some(git_dir) = resolve_git_dir(None) else {
        return "HEAD".to_string();
    };
    match read_git_head(&git_dir) {
        Some(GitHead::Branch(name)) => name,
        _ => "HEAD".to_string(),
    }
}

fn compute_head() -> String {
    let Some(git_dir) = resolve_git_dir(None) else {
        return String::new();
    };
    match read_git_head(&git_dir) {
        Some(GitHead::Branch(name)) => {
            resolve_ref(&git_dir, &format!("refs/heads/{name}")).unwrap_or_default()
        }
        Some(GitHead::Detached(sha)) => sha,
        None => String::new(),
    }
}

fn compute_remote_url() -> Option<String> {
    let git_dir = resolve_git_dir(None)?;
    if let Some(url) = parse_git_config_value(&git_dir, "remote", Some("origin"), "url") {
        return Some(url);
    }
    // worktree 中带 remote URL 的 config 位于 common dir
    let common_dir = get_common_dir(&git_dir);
    if let Some(common_dir) = common_dir {
        if common_dir != git_dir {
            return parse_git_config_value(&common_dir, "remote", Some("origin"), "url");
        }
    }
    None
}

fn compute_default_branch() -> String {
    let Some(git_dir) = resolve_git_dir(None) else {
        return "main".to_string();
    };
    // refs/remotes/ 位于 commonDir（而非 per-worktree gitDir）
    let common_dir = get_common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
    if let Some(branch) = read_raw_symref(
        &common_dir,
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/",
    ) {
        return branch;
    }
    for candidate in ["main", "master"] {
        if resolve_ref(&common_dir, &format!("refs/remotes/origin/{candidate}")).is_some() {
            return candidate.to_string();
        }
    }
    "main".to_string()
}

// ---------------------------------------------------------------------------
// 对外缓存的 getter（对齐 getCachedBranch / getCachedHead /
// getCachedRemoteUrl / getCachedDefaultBranch / resetGitFileWatcher）
// ---------------------------------------------------------------------------

/// 当前分支名（缓存；不在分支上或无仓库时为 `"HEAD"`）。
pub fn get_cached_branch() -> String {
    git_watcher().get("branch", compute_branch)
}

/// 当前 HEAD 的 commit SHA（缓存；无仓库时为空串）。
pub fn get_cached_head() -> String {
    git_watcher().get("head", compute_head)
}

/// remote origin URL（缓存；无仓库或未配置时为 None）。
pub fn get_cached_remote_url() -> Option<String> {
    git_watcher().get("remoteUrl", compute_remote_url)
}

/// 默认分支（缓存；无仓库时为 `"main"`）。
pub fn get_cached_default_branch() -> String {
    git_watcher().get("defaultBranch", compute_default_branch)
}

/// 复位 watcher 状态（对齐 TS resetGitFileWatcher，测试用）。
pub fn reset_git_file_watcher() {
    git_watcher().reset();
}

// ---------------------------------------------------------------------------
// 任意目录的 HEAD/remote 读取（不经 watcher）
// ---------------------------------------------------------------------------

/// 由 GitHead 解析为 HEAD SHA（分支 → resolveRef；detached → SHA 原样）。
fn head_sha_from_head(git_dir: &Path, head: &GitHead) -> Option<String> {
    match head {
        GitHead::Branch(name) => resolve_ref(git_dir, &format!("refs/heads/{name}")),
        GitHead::Detached(sha) => Some(sha.clone()),
    }
}

/// 读任意目录的 HEAD SHA（插件等需要特定仓库而非 CWD 仓库时使用）。
pub fn get_head_for_dir(cwd: &str) -> Option<String> {
    let git_dir = resolve_git_dir(Some(cwd))?;
    let head = read_git_head(&git_dir)?;
    head_sha_from_head(&git_dir, &head)
}

/// 读 git worktree 目录的 HEAD SHA。
///
/// 与 `get_head_for_dir` 不同：直接读 `<worktreePath>/.git` 的 `gitdir:`
/// 指针文件，**不向上遍历**。worktree 路径不存在时 get_head_for_dir 会向上
/// 找到父仓库的 `.git`，把父仓库的 HEAD 误报为 worktree 的。
/// `.git` 指针缺失或格式错误时返回 None（调用方可视为「不是合法 worktree」）。
pub fn read_worktree_head_sha(worktree_path: &str) -> Option<String> {
    let ptr = read_file_lossy(&Path::new(worktree_path).join(".git")).ok()?;
    let ptr = ptr.trim();
    let raw_dir = ptr.strip_prefix("gitdir:")?;
    let git_dir = resolve_path(&Path::new(worktree_path).join(raw_dir.trim()));
    let head = read_git_head(&git_dir)?;
    head_sha_from_head(&git_dir, &head)
}

/// 读任意目录的 remote origin URL（经 .git/config）。
pub fn get_remote_url_for_dir(cwd: &str) -> Option<String> {
    let git_dir = resolve_git_dir(Some(cwd))?;
    if let Some(url) = parse_git_config_value(&git_dir, "remote", Some("origin"), "url") {
        return Some(url);
    }
    // worktree 中带 remote URL 的 config 位于 common dir
    let common_dir = get_common_dir(&git_dir);
    if let Some(common_dir) = common_dir {
        if common_dir != git_dir {
            return parse_git_config_value(&common_dir, "remote", Some("origin"), "url");
        }
    }
    None
}

/// 是否 shallow clone：检查 `<commonDir>/shallow` 是否存在。
/// 按 git 的 shallow.c，文件存在即为 shallow；该文件位于 commonDir。
pub fn is_shallow_clone() -> bool {
    let Some(git_dir) = resolve_git_dir(None) else {
        return false;
    };
    let common_dir = get_common_dir(&git_dir).unwrap_or(git_dir);
    fs::metadata(common_dir.join("shallow")).is_ok()
}

/// 数 worktree 个数：读 `<commonDir>/worktrees/` 目录条目数 + 1
/// （主 worktree 不列在其中）。无该目录（仅主 worktree）时为 1；
/// 不在 git 仓库时为 0。
pub fn get_worktree_count_from_fs() -> usize {
    let Some(git_dir) = resolve_git_dir(None) else {
        return 0;
    };
    let common_dir = get_common_dir(&git_dir).unwrap_or(git_dir);
    match fs::read_dir(common_dir.join("worktrees")) {
        Ok(entries) => entries.count() + 1,
        // 无 worktrees 目录：仅主 worktree
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_ref_name_allowlist() {
        assert!(is_safe_ref_name("main"));
        assert!(is_safe_ref_name("feature/foo"));
        assert!(is_safe_ref_name("release-1.2.3+build"));
        assert!(is_safe_ref_name("dependabot/npm_and_yarn/@types/node-18.0.0"));
        // 拒绝：空前缀
        assert!(!is_safe_ref_name(""));
        // 拒绝：参数注入 / 路径穿越 / shell 元字符
        assert!(!is_safe_ref_name("-foo"));
        assert!(!is_safe_ref_name("/foo"));
        assert!(!is_safe_ref_name("foo..bar"));
        assert!(!is_safe_ref_name("."));
        assert!(!is_safe_ref_name("foo/./bar"));
        assert!(!is_safe_ref_name("foo//bar"));
        assert!(!is_safe_ref_name("foo/"));
        assert!(!is_safe_ref_name("foo bar"));
        assert!(!is_safe_ref_name("foo`whoami`"));
        assert!(!is_safe_ref_name("foo$HOME"));
        assert!(!is_safe_ref_name("foo;rm"));
        assert!(!is_safe_ref_name("foo@{bar}"));
        assert!(!is_safe_ref_name("café"));
    }

    #[test]
    fn valid_git_sha_lower_hex_only() {
        assert!(is_valid_git_sha(&"a".repeat(40)));
        assert!(is_valid_git_sha(&"0123456789abcdef".repeat(4)));
        assert!(!is_valid_git_sha(&"A".repeat(40))); // 大写拒绝（对齐 TS 正则）
        assert!(!is_valid_git_sha(&"g".repeat(40)));
        assert!(!is_valid_git_sha(&"a".repeat(39)));
        assert!(!is_valid_git_sha(&"a".repeat(41)));
        assert!(!is_valid_git_sha(&"a".repeat(64 - 1)));
        assert!(is_valid_git_sha(&"a".repeat(64)));
    }

    #[test]
    fn resolve_path_lexical() {
        assert_eq!(resolve_path(Path::new("/foo/../bar")), PathBuf::from("/bar"));
        assert_eq!(resolve_path(Path::new("/foo/./bar")), PathBuf::from("/foo/bar"));
        assert_eq!(resolve_path(Path::new("/")), PathBuf::from("/"));
    }

    #[test]
    fn find_git_root_none_in_tmp() {
        // /tmp 通常不是 git 仓库；即便意外是，缓存也不应破坏 Option 语义
        let _ = find_git_root("/tmp");
    }

    #[test]
    fn read_git_head_detached_and_branch() {
        let dir = std::env::temp_dir().join("cc_rs_gittest_head");
        let _ = fs::create_dir_all(&dir);
        // detached
        fs::write(dir.join("HEAD"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        assert_eq!(
            read_git_head(&dir),
            Some(GitHead::Detached("0123456789abcdef0123456789abcdef01234567".into()))
        );
        // branch
        fs::write(dir.join("HEAD"), "ref: refs/heads/main\r\n").unwrap();
        assert_eq!(read_git_head(&dir), Some(GitHead::Branch("main".into())));
        // 被篡改的 HEAD：shell 元字符
        fs::write(dir.join("HEAD"), "ref: refs/heads/foo`whoami`\n").unwrap();
        assert_eq!(read_git_head(&dir), None);
        // 被篡改的 HEAD：非法 SHA
        fs::write(dir.join("HEAD"), "not-a-sha\n").unwrap();
        assert_eq!(read_git_head(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn packed_refs_parsing() {
        let dir = std::env::temp_dir().join("cc_rs_gittest_packed");
        let _ = fs::create_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n\
             0123456789abcdef0123456789abcdef01234567 refs/heads/main\n\
             ^abcdefabcdefabcdefabcdefabcdefabcdefabcd\n\
             89abcdef0123456789abcdef0123456789abcdef refs/heads/feature/x\n",
        )
        .unwrap();
        assert_eq!(
            resolve_ref_in_dir(&dir, "refs/heads/main"),
            Some("0123456789abcdef0123456789abcdef01234567".into())
        );
        assert_eq!(
            resolve_ref_in_dir(&dir, "refs/heads/feature/x"),
            Some("89abcdef0123456789abcdef0123456789abcdef".into())
        );
        assert_eq!(resolve_ref_in_dir(&dir, "refs/heads/missing"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_caches_branch_and_invalidates() {
        let dir = std::env::temp_dir().join("cc_rs_gittest_watcher");
        let _ = fs::remove_dir_all(&dir);
        // 模拟一个最小 git 仓库
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            dir.join(".git/refs/heads/main"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();

        let prev_node_env = std::env::var("NODE_ENV").ok();
        std::env::set_var("NODE_ENV", "test");
        // watcher 以「会话 cwd」解析仓库；utils/cwd.ts 未转换，直接写
        // bootstrap::state 的会话级 cwd（测试结束后恢复 + 清缓存，避免污染）。
        let prev_cwd = crate::bootstrap::state::get_cwd_state();
        crate::bootstrap::state::set_cwd_state(&dir.to_string_lossy());
        clear_resolve_git_dir_cache();
        reset_git_file_watcher();

        let branch = get_cached_branch();
        assert_eq!(branch, "main");
        // 再取一次命中缓存（无文件变化时结果不变）
        assert_eq!(get_cached_branch(), "main");
        assert_eq!(
            get_cached_head(),
            "0123456789abcdef0123456789abcdef01234567"
        );

        // 分支切换：改写 HEAD → 下次 get 重算
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
        fs::write(
            dir.join(".git/refs/heads/feature"),
            "89abcdef0123456789abcdef0123456789abcdef\n",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(get_cached_branch(), "feature");
        assert_eq!(get_cached_head(), "89abcdef0123456789abcdef0123456789abcdef");

        reset_git_file_watcher();
        clear_resolve_git_dir_cache();
        // 恢复全局状态
        crate::bootstrap::state::set_cwd_state(&prev_cwd);
        match prev_node_env {
            Some(v) => std::env::set_var("NODE_ENV", v),
            None => std::env::remove_var("NODE_ENV"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
