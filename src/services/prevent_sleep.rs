//! prevent_sleep.rs — macOS 防休眠（重写自 `src/services/preventSleep.ts`）。
//!
//! 用内置 `caffeinate` 创建电源断言阻止 idle sleep，使 API 请求与工具
//! 执行期间 Mac 保持唤醒。带超时 spawn + 周期重启：Node 进程被 SIGKILL
//! 时孤儿 caffeinate 会在超时后自动退出（自愈）。
//!
//! 已知偏差：
//! - TS 用 `child_process.spawn` + `setInterval`；Rust 侧用
//!   `std::process::Command` 与独立的看门狗线程（无宿主事件循环）。
//!   看门狗线程以 50ms 粒度轮询停止标志，`stop` 时立即 join。
//! - `restart_interval.unref()` 是 Node 计时器概念，Rust 无对应物；线
//!   程为后台守护线程，进程退出时由 cleanup registry 或 OS 回收。
//! - 非 macOS 平台全部 API 为 no-op（对齐 TS 的 process.platform 门）。

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::utils::cleanup_registry::register_cleanup;
use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// caffeinate 超时（秒）：进程在此时长后自动退出。
const CAFFEINATE_TIMEOUT_SECONDS: u64 = 300;

/// 重启间隔：在 5 分钟超时前重启（4 分钟留充足余量）。
const RESTART_INTERVAL_MS: u64 = 4 * 60 * 1000;

/// 看门狗轮询粒度（停止响应延迟上限）。
const WATCHDOG_POLL_MS: u64 = 50;

static CAFFEINATE_PROCESS: Lazy<Mutex<Option<Child>>> = Lazy::new(|| Mutex::new(None));
static RESTART_HANDLE: Lazy<Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>> =
    Lazy::new(|| Mutex::new(None));
static REF_COUNT: AtomicU32 = AtomicU32::new(0);
static CLEANUP_REGISTERED: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_darwin() -> bool {
    cfg!(target_os = "macos")
}

/// 引用计数 +1；从 0 到 1 时启动防休眠。
pub fn start_prevent_sleep() {
    let previous = REF_COUNT.fetch_add(1, Ordering::SeqCst);
    if previous == 0 {
        spawn_caffeinate();
        start_restart_interval();
    }
}

/// 引用计数 -1；归 0 时允许休眠。
pub fn stop_prevent_sleep() {
    // 防下溢：>0 时才减（对齐 TS 的 if (refCount > 0)）
    loop {
        let current = REF_COUNT.load(Ordering::SeqCst);
        if current == 0 {
            break;
        }
        if REF_COUNT
            .compare_exchange_weak(current, current - 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if current - 1 == 0 {
                stop_restart_interval();
                kill_caffeinate();
            }
            break;
        }
    }
}

/// 强制停止（退出清理用），无论引用计数。
pub fn force_stop_prevent_sleep() {
    REF_COUNT.store(0, Ordering::SeqCst);
    stop_restart_interval();
    kill_caffeinate();
}

fn start_restart_interval() {
    if !is_darwin() {
        return;
    }
    let mut handle = lock(&RESTART_HANDLE);
    if handle.is_some() {
        return; // 已在运行
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let flag_clone = stop_flag.clone();
    let join = std::thread::spawn(move || {
        let interval = Duration::from_millis(RESTART_INTERVAL_MS);
        loop {
            // 等到下一次重启点（以 50ms 粒度检查停止标志）
            let deadline = Instant::now() + interval;
            while Instant::now() < deadline {
                if flag_clone.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let sleep = if remaining > Duration::from_millis(WATCHDOG_POLL_MS) {
                    Duration::from_millis(WATCHDOG_POLL_MS)
                } else {
                    remaining
                };
                std::thread::sleep(sleep);
            }
            if flag_clone.load(Ordering::SeqCst) {
                return;
            }
            // 仍需防休眠才重启（对齐 TS 的 refCount > 0 门）
            if REF_COUNT.load(Ordering::SeqCst) > 0 {
                log_for_debugging(
                    "Restarting caffeinate to maintain sleep prevention",
                    DebugLogLevel::Debug,
                );
                kill_caffeinate();
                spawn_caffeinate();
            }
        }
    });
    *handle = Some((stop_flag, join));
}

fn stop_restart_interval() {
    let handle = lock(&RESTART_HANDLE).take();
    if let Some((flag, join)) = handle {
        flag.store(true, Ordering::SeqCst);
        let _ = join.join();
    }
}

fn spawn_caffeinate() {
    if !is_darwin() {
        return;
    }
    {
        let mut process = lock(&CAFFEINATE_PROCESS);
        if process.is_some() {
            return; // 已在运行
        }
    }

    // 首次使用时注册退出清理（对齐 TS 的一次性注册）
    if !CLEANUP_REGISTERED.swap(true, Ordering::SeqCst) {
        register_cleanup(|| {
            force_stop_prevent_sleep();
        });
    }

    // -i：阻止 idle sleep（最不激进；显示器仍可休眠）
    // -t：超时秒数——SIGKILL 时的自愈退出
    let child = Command::new("caffeinate")
        .args(["-i", "-t", &CAFFEINATE_TIMEOUT_SECONDS.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            *lock(&CAFFEINATE_PROCESS) = Some(child);
            log_for_debugging("Started caffeinate to prevent sleep", DebugLogLevel::Debug);
        }
        Err(_) => {
            // 静默失败：caffeinate 不可用或 spawn 失败
            *lock(&CAFFEINATE_PROCESS) = None;
        }
    }
}

fn kill_caffeinate() {
    let mut process = lock(&CAFFEINATE_PROCESS);
    if let Some(mut child) = process.take() {
        // SIGKILL 立即终止（SIGTERM 可能延迟）
        match child.kill() {
            Ok(()) => {
                log_for_debugging("Stopped caffeinate, allowing sleep", DebugLogLevel::Debug);
            }
            Err(_) => {
                // 进程可能已退出：忽略
            }
        }
        let _ = child.wait();
    }
}
