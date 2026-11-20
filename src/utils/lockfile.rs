//! lockfile.rs — 文件锁（proper-lockfile 的 Rust 落地）。
//!
//! 重写自 `src/utils/lockfile.ts`。TS 版是 proper-lockfile 的惰性加载
//! 间接层（避免 graceful-fs 在启动期 monkey-patch fs 的 ~8ms 开销）；
//! Cargo.toml 无 proper-lockfile，Rust 侧直接用 std::fs 实现
//! 等价语义（O_EXCL 互斥创建 + stale 检测 + 重试）：
//!
//! - `lock_sync(file)`：创建 `${file}.lock`（create_new=true 互斥），
//!   成功返回 release guard，drop 时释放。锁已存在且超过 stale_ms
//!   则视作僵死锁抢占（unlink 后重建）。抢占前不校验持锁进程存活性
//!   （偏差：proper-lockfile 只按 mtime 判定 stale）。
//! - `check_locked(file)`：锁文件是否存在。
//! - `unlock(file)`：删除锁文件（容忍 ENOENT）。
//!
//! 偏差：proper-lockfile 的 update（周期 touch mtime）未实现——
//! 当前调用方持锁时间均为个位数毫秒级，stale 判定足够。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 锁参数（对齐 proper-lockfile 的 LockOptions 常用子集）。
#[derive(Debug, Clone, Copy)]
pub struct LockOptions {
    /// 锁文件被视为僵死（可抢占）的毫秒数。
    pub stale_ms: u64,
    /// 抢占重试间隔毫秒数。
    pub retry_interval_ms: u64,
    /// 整体放弃前的重试毫秒数。0 表示不重试。
    pub timeout_ms: u64,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            stale_ms: 10_000,
            retry_interval_ms: 100,
            timeout_ms: 0,
        }
    }
}

/// 锁的持有句柄；drop 时自动释放（对齐 TS 的 release 回调）。
pub struct LockGuard {
    lock_path: PathBuf,
}

impl LockGuard {
    /// 显式释放（等价 TS 的 release() 回调）。
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn lock_path_for(file: &Path) -> PathBuf {
    let mut p = file.as_os_str().to_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

fn lock_mtime_age_ms(lock_path: &Path) -> Option<Duration> {
    let meta = std::fs::metadata(lock_path).ok()?;
    let mtime = meta.modified().ok()?;
    SystemTime::now().duration_since(mtime).ok()
}

/// 尝试获取文件锁（同步）。
///
/// - 成功返回 `LockGuard`（drop 释放）。
/// - 已加锁：stale 期内返回 `Err(io::ErrorKind::WouldBlock 的 io::Error)`；
///   超过 stale_ms 则抢占（删除重建）。
/// - 传 `wait` 时按 options 重试直到 timeout_ms。
pub fn lock_sync(file: &Path, options: &LockOptions, wait: bool) -> io::Result<LockGuard> {
    let lock_path = lock_path_for(file);
    let deadline = if wait && options.timeout_ms > 0 {
        Some(SystemTime::now() + Duration::from_millis(options.timeout_ms))
    } else {
        None
    };
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                // 记录持有者提示（对齐 proper-lockfile 写 PID/mtime）。
                let _ = write_holder_hint(&mut file);
                return Ok(LockGuard { lock_path });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let stale = lock_mtime_age_ms(&lock_path)
                    .map(|age| age.as_millis() as u64 > options.stale_ms)
                    .unwrap_or(true);
                if stale {
                    // 僵死锁：抢占。竞争者可能同时抢占，删除失败/创建失败都重试。
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if let Some(deadline) = deadline {
                    if SystemTime::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "lockfile: timed out waiting for lock",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(options.retry_interval_ms));
                    continue;
                }
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "lockfile: already locked"));
            }
            Err(e) => return Err(e),
        }
    }
}

fn write_holder_hint(file: &mut File) -> io::Result<()> {
    use std::io::Write;
    writeln!(file, "{}", std::process::id())
}

/// 锁文件是否存在（对齐 proper-lockfile `check`）。
pub fn check_locked(file: &Path) -> io::Result<bool> {
    lock_path_for(file).try_exists()
}

/// 释放锁（对齐 proper-lockfile `unlock`）。容忍 ENOENT。
pub fn unlock(file: &Path) -> io::Result<()> {
    match std::fs::remove_file(lock_path_for(file)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 异步获取文件锁：重试期间让出执行权（用 tokio::time sleep）。
///
/// `timeout_ms` 为 0 时只尝试一次（失败立即返回 WouldBlock）。
pub async fn lock(file: &Path, options: &LockOptions) -> io::Result<LockGuard> {
    let deadline = if options.timeout_ms > 0 {
        Some(SystemTime::now() + Duration::from_millis(options.timeout_ms))
    } else {
        None
    };
    loop {
        match lock_sync(file, options, false) {
            Ok(guard) => return Ok(guard),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let Some(deadline) = deadline else {
                    return Err(e);
                };
                if SystemTime::now() >= deadline {
                    return Err(e);
                }
                let interval = if options.retry_interval_ms > 0 {
                    Duration::from_millis(options.retry_interval_ms)
                } else {
                    Duration::from_millis(100)
                };
                tokio::time::sleep(interval).await;
            }
            Err(e) => return Err(e),
        }
    }
}
