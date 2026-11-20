//! process.rs — 进程 stdout/stderr 输出与退出助手。
//!
//! 重写自 `src/utils/process.ts`。
//!
//! - EPIPE 防护：TS 端给 stdout/stderr 挂 error 监听器并在 EPIPE 时
//!   destroy 流（`claude -p | head -1` 场景）。Rust 默认忽略 SIGPIPE、
//!   写失败返回 `ErrorKind::BrokenPipe`；本模块把输出函数写成
//!   "BrokenPipe 静默丢弃"，语义等价。
//! - exitWithError：写 stderr 后以退出码 1 结束进程，合并 entrypoint
//!   快速路径的 console.error + process.exit(1) 模式。
//! - peek_for_stdin_data：TS 在事件层竞争超时 vs 首块数据；
//!   Rust 端改为后台读线程把 stdin 全量读入全局缓冲 + 主线程
//!   轮询超时（见函数注释）。

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 注册进程输出错误处理器（Rust 端为 no-op：写失败由 write 函数内部
/// 按 BrokenPipe 静默处理，无事件层可挂监听器）。
pub fn register_process_output_error_handlers() {}

/// 写 stdout；流已关闭（BrokenPipe）时静默丢弃。
pub fn write_to_stdout(data: &str) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(data.as_bytes());
    let _ = handle.flush();
}

/// 写 stderr；流已关闭（BrokenPipe）时静默丢弃。
pub fn write_to_stderr(data: &str) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(data.as_bytes());
    let _ = handle.flush();
}

/// 写错误到 stderr 并以退出码 1 结束进程。
pub fn exit_with_error(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

// peek 读到的数据由这个全局缓冲保管，供真正的 stdin 读取者先取回，
// 避免探测线程"吃掉"调用方的首字节。
static PEEKED_STDIN: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
static PEEK_STARTED: AtomicBool = AtomicBool::new(false);

fn peeked_slot() -> &'static Mutex<Vec<u8>> {
    PEEKED_STDIN.get_or_init(|| Mutex::new(Vec::new()))
}

/// 取回（drain）peek 期间读到但未处理的 stdin 字节。
pub fn take_peeked_stdin() -> Vec<u8> {
    std::mem::take(&mut *peeked_slot().lock().unwrap_or_else(|p| p.into_inner()))
}

/// 等待类 stdin 流关闭，但若 ms 内没有任何数据到达则放弃。
/// 首块数据到达即取消超时；此后无条件等待流结束（调用方需要全部
/// 数据块而非只有第一块）。超时返回 true，正常结束返回 false。
/// -p 模式用它区分真实的管道 producing 者与继承但空闲的父进程 stdin。
///
/// 实现差异：TS 在 Node 事件层竞争 'data'/'end' 与 setTimeout，
/// 数据天然留在流里；Rust 端没有可复用的流抽象，改为后台线程把
/// stdin 全量读入全局缓冲，主线程轮询"首字节到达"标志至 ms 超时。
/// 命中数据后返回 false；读到的字节由 take_peeked_stdin() 交接。
pub fn peek_for_stdin_data(ms: u64) -> bool {
    let has_data = Arc::new(AtomicBool::new(false));
    let has_data_reader = Arc::clone(&has_data);

    if PEEK_STARTED.swap(true, Ordering::SeqCst) {
        // 已经 peek 过：重复调用直接按无数据处理
        return true;
    }

    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 8192];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    has_data_reader.store(true, Ordering::SeqCst);
                    peeked_slot()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if has_data.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    !has_data.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_reports_timeout_without_input() {
        // 无 stdin 输入时 100ms 内应返回 true（超时）
        //（若环境 stdin 已关闭，read 立即返回 0 → 同样 true）
        assert!(peek_for_stdin_data(100));
    }

    #[test]
    fn take_peeked_stdin_is_available_after_peek() {
        take_peeked_stdin();
        peek_for_stdin_data(50);
        // 无管道输入时缓冲应为空（或 EOF 立即结束）
        assert!(take_peeked_stdin().is_empty());
    }
}
