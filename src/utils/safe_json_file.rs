//! safe_json_file.rs — 本地 JSON 存储的安全读写助手。
//!
//! 重写自 `src/utils/safeJsonFile.ts`。语义对齐（照搬 TS 的三条保证）：
//!
//! 1. 原子写 —— 每次保存走 tmp + rename，崩溃不会留下截断/半写文件。
//! 2. 拒腐蚀 —— 不再解析（或不再匹配预期形状）的文件**绝不**被当作
//!   空文件被下次 read-modify-write 覆盖；而是被隔离到
//!   `<name>.corrupt-<timestamp>` 备份（rename，不做 copy+delete），
//!   记日志后抛 StoreCorruptError，让数据损失可见可恢复。
//! 3. 读改写锁 —— `with_store_lock` 用 `<name>.lock` 的 O_EXCL 创建 +
//!   超时后抢占僵死锁，保证两个并发进程不会在读写窗口里互相覆盖。
//!
//! 偏差：
//! - TS 的 `process.kill(pid, 0)` 存活性探测：Unix 用限定的 libc 替代
//!   不引入 libc crate，改用 `/proc/<pid>` 存在性检查（Linux），
//!   其他平台退化为仅 mtime 判定。
//! - `Atomics.wait` 的 5ms sleep 用 std::thread::sleep。
//! - randomBytes → rand crate。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;

use crate::utils::log::log_error;

/// 存储文件不可读时抛出；原文件已被移到 `backup_path`。
#[derive(Debug, Clone)]
pub struct StoreCorruptError {
    pub message: String,
    pub backup_path: String,
}

impl std::fmt::Display for StoreCorruptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreCorruptError {}

impl StoreCorruptError {
    pub fn new(path: &str, backup_path: &str, reason: &str) -> Self {
        Self {
            message: format!(
                "{path} is corrupt ({reason}); the original file was moved aside to {backup_path} and was NOT overwritten."
            ),
            backup_path: backup_path.to_string(),
        }
    }
}

/// 每个存储文件保留的 `.corrupt-*` 备份数；隔离是安全网，不是档案库。
const QUARANTINE_RETENTION_LIMIT: usize = 10;

/// 锁获取重试间隔（毫秒）。
const LOCK_RETRY_MS: u64 = 5;

/// 锁被视为僵死（崩溃持有者）并被下一个写者抢占的时长（毫秒）。
pub const STORE_LOCK_STALE_MS: u64 = 10_000;

/// 活锁（非僵死）仍未释放时的整体放弃时长（毫秒）。
pub const STORE_LOCK_TIMEOUT_MS: u64 = 15_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 锁文件里记录的持有者 pid；无解析 pid 或读取失败均返回 None。
fn read_lock_holder_pid(lock_path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(lock_path).ok()?;
    let trimmed = raw.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    let pid: u32 = digits.parse().ok()?;
    (pid > 0).then_some(pid)
}

/// 锁持有者的存活性探测：Linux 上用 `/proc/<pid>` 存在性检查。
/// 其他平台总返回 true（保守：不抢占，等主超时兜底）。
fn is_process_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut rng = rand::thread_rng();
    let random: [u8; 4] = rng.gen();
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let pid = std::process::id();
    PathBuf::from(format!("{}.tmp-{pid}-{hex}", path.to_string_lossy()))
}

fn quarantine_backups_for(path: &Path) -> Vec<String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = format!("{}.corrupt-", path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut backups: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| name.starts_with(&prefix))
                .collect();
            backups.sort();
            backups
        }
        // 目录在 rename 与 prune 之间消失 —— 无事可做
        Err(_) => Vec::new(),
    }
}

/// 把腐蚀的存储文件移开，让下次调用从干净状态开始，原始字节仍可恢复。
/// 返回备份路径。
///
/// 单一 rename 的官方形式：腐蚀文件被 RENAME（不做 copy+unlink）到
/// `<file>.corrupt-<Date.now()>-<pid>-<3-byte-hex>`。源已消失（ENOENT ——
/// 并发隔离已经移走）被容忍：返回计算出的备份路径，调用方的
/// StoreCorruptError 消息依然准确。
///
/// 保留：同一存储文件旁超过 QUARANTINE_RETENTION_LIMIT 份备份时，
/// 按 mtime 删掉最旧的（best effort）。
pub fn quarantine_corrupt_file(path: &Path) -> io::Result<PathBuf> {
    let mut rng = rand::thread_rng();
    let random: [u8; 3] = rng.gen();
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let pid = std::process::id();
    let backup_path = PathBuf::from(format!(
        "{}.corrupt-{}-{pid}-{hex}",
        path.to_string_lossy(),
        now_ms()
    ));

    if let Err(e) = fs::rename(path, &backup_path) {
        if e.kind() == io::ErrorKind::NotFound {
            // ENOENT：并发隔离已经把原文件移走（或被外部删除），
            // 返回意图备份路径，调用方错误消息保持准确。
            return Ok(backup_path);
        }
        return Err(e);
    }

    // 保留上限：超出则按 mtime 删最旧（best effort）
    let backups = quarantine_backups_for(path);
    if backups.len() > QUARANTINE_RETENTION_LIMIT {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut with_mtime: Vec<(String, u64)> = backups
            .iter()
            .map(|name| {
                let mtime = fs::metadata(dir.join(name))
                    .and_then(|m| m.modified())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64))
                    .unwrap_or(0);
                (name.clone(), mtime)
            })
            .collect();
        with_mtime.sort_by_key(|(_, mtime)| *mtime);
        let excess = &with_mtime[..with_mtime.len() - QUARANTINE_RETENTION_LIMIT];
        for (name, _) in excess {
            let _ = fs::remove_file(dir.join(name));
        }
    }
    Ok(backup_path)
}

/// 围绕单个存储文件的读改写建议锁。
///
/// - 以 create_new=true（O_CREAT|O_EXCL）获取 `<name>.lock`。
/// - EEXIST 时检查 mtime：超过 stale_ms → 抢占（unlink 重建），但
///   **仅当锁文件里记录的 pid 存活性探测失败时**；活着但超过 stale
///   预算的持有者保留锁（等待者等，整体超时兜底）。
/// - 无解析 pid 的锁（pid 写入前崩溃 / 外部创建）保持仅 mtime 判定。
/// - 否则 sleep LOCK_RETRY_MS 重试，超过 timeout_ms 抛错而不是
///   与卡住的写者竞态。`fn` 抛错时锁也会被释放（unlink，容忍 ENOENT）。
/// - 非重入：同一 path 不要嵌套调用。
pub fn with_store_lock<T, F>(path: &Path, options: Option<StoreLockOptions>, f: F) -> Result<T, StoreLockError>
where
    F: FnOnce() -> Result<T, StoreLockError>,
{
    let options = options.unwrap_or_default();
    let timeout_ms = options.timeout_ms.unwrap_or(STORE_LOCK_TIMEOUT_MS);
    let stale_ms = options.stale_ms.unwrap_or(STORE_LOCK_STALE_MS);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| StoreLockError::Io(e.to_string()))?;

    let lock_path = PathBuf::from(format!("{}.lock", path.to_string_lossy()));
    let deadline = now_ms() + timeout_ms;

    let mut file = loop {
        if now_ms() >= deadline {
            return Err(StoreLockError::Timeout {
                lock_path: lock_path.to_string_lossy().into_owned(),
                timeout_ms,
            });
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => break file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // 持有者在 create 与 stat 之间释放 —— 立即重试
                let Ok(meta) = fs::metadata(&lock_path) else {
                    continue;
                };
                let holder_mtime_ms = meta
                    .modified()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64))
                    .unwrap_or(0);
                if now_ms() > holder_mtime_ms + stale_ms {
                    // 僵死资格 —— 但年龄本身不证明被遗弃：仅当记录的
                    // 持有 pid 探测失败才抢占。活着但卡慢的持有者保留锁。
                    let holder_pid = read_lock_holder_pid(&lock_path);
                    if holder_pid.map_or(false, is_process_alive) {
                        std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
                    } else {
                        let _ = fs::remove_file(&lock_path);
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
                }
            }
            Err(e) => return Err(StoreLockError::Io(e.to_string())),
        }
    };

    // 建议性内容：谁持有、何时获得（人类调试用）
    let _ = writeln!(file, "{} {}", std::process::id(), now_ms());
    let result = f();
    let _ = fs::remove_file(&lock_path);
    result
}

#[derive(Debug, Clone, Default)]
pub struct StoreLockOptions {
    pub timeout_ms: Option<u64>,
    pub stale_ms: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreLockError {
    #[error("timed out acquiring store lock {lock_path} after {timeout_ms}ms — another writer appears stuck; refusing to risk a lost update")]
    Timeout {
        lock_path: String,
        timeout_ms: u64,
    },
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Corrupt(#[from] StoreCorruptError),
}

/// 原子 JSON 写：序列化、写到同目录 tmp 文件、rename 覆盖目标。
/// 读者只会看到旧文件或新文件，绝无半写状态。
pub fn write_json_file_atomic(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tmp_path_for(path);
    {
        let mut file = File::create(&tmp)?;
        let body = serde_json::to_string_pretty(value).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        file.write_all(format!("{body}\n").as_bytes())?;
        file.sync_data()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // best effort 清理 tmp
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 读取 JSON 存储文件（拒腐蚀）。
///
/// - 文件缺失 → None（真正的新存储）
/// - 解析成功（任意 JSON 值） → 解析值；形状由调用方校验
/// - 无法解析 → 隔离到 `.corrupt-<ts>` 备份并抛 StoreCorruptError
///   （读改写调用方不得把这种情况当空文件）
pub fn read_json_file_strict(path: &Path) -> Result<Option<serde_json::Value>, StoreCorruptError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // 非 ENOENT 的读失败（权限等）：传播原始错误信息
            return Err(StoreCorruptError::new(
                &path.to_string_lossy(),
                "",
                &format!("read failed: {e}"),
            ));
        }
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => Ok(Some(value)),
        Err(_) => {
            let backup_path = quarantine_corrupt_file(path);
            let backup_display = match &backup_path {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => format!("<quarantine failed: {e}>"),
            };
            let err = StoreCorruptError::new(
                &path.to_string_lossy(),
                &backup_display,
                "not valid JSON",
            );
            log_error(err.clone());
            Err(err)
        }
    }
}

/// JSON 解析成功但形状校验失败时的隔离 + 记日志 + 抛错。
pub fn reject_corrupt_json_file(path: &Path, reason: &str) -> StoreCorruptError {
    let backup_path = quarantine_corrupt_file(path);
    let backup_display = match &backup_path {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => format!("<quarantine failed: {e}>"),
    };
    let err = StoreCorruptError::new(
        &path.to_string_lossy(),
        &backup_display,
        reason,
    );
    log_error(err.clone());
    err
}
