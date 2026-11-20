//! exec_file_no_throw_portable.rs — 带默认值的 execSync（DEPRECATED）。
//!
//! 重写自 `src/utils/execFileNoThrowPortable.ts`（导出
//! `execSyncWithDefaults_DEPRECATED`）。语义对齐：
//!
//! - TS 的 `execaSync(command, { shell: true, reject: false, ... })`：
//!   经 shell 执行，非零退出不抛错，返回 stdout 或 null。
//! - 默认 timeout 10 分钟、maxBuffer 1MB、cwd 为 getCwd()。
//! - 调用方可能传 options 对象或旧签名 AbortSignal（Rust 侧用枚举区分）。
//! - slowLogging 包裹（`exec: <command 前 200 字符>`）。
//!
//! 偏差：AbortSignal 用 `CancellationToken` 替代；已 abort 时返回 None
//! （对齐 TS 的 throwIfAborted 抛错后 catch 返回 null）。

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

use crate::utils::abort_controller::CancellationToken;
use crate::utils::cwd;
use crate::utils::slow_operations::{slow_logging, SlowOperationGuard};

const MS_IN_SECOND: u64 = 1000;
const SECONDS_IN_MINUTE: u64 = 60;
const DEFAULT_TIMEOUT_MS: u64 = 10 * SECONDS_IN_MINUTE * MS_IN_SECOND;
const MAX_BUFFER: usize = 1_000_000;

/// 第二参数：options 对象或旧签名 AbortSignal。
pub enum ExecSyncArg {
    Options(ExecSyncOptions),
    AbortSignal(CancellationToken),
}

#[derive(Debug, Clone, Default)]
pub struct ExecSyncOptions {
    pub abort_signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub input: Option<String>,
}

/// @deprecated 用非阻塞的 async 执行替代。同步 exec 会阻塞事件循环。
///
/// 经 shell 执行 command，成功且 stdout 非空时返回 trim 后的字符串，
/// 否则返回 None（非零退出、超时、spawn 失败、stdout 为空、已 abort）。
pub fn exec_sync_with_defaults_deprecated(command: &str, arg: Option<ExecSyncArg>) -> Option<String> {
    let options: ExecSyncOptions = match arg {
        None => Default::default(),
        Some(ExecSyncArg::Options(opts)) => opts,
        Some(ExecSyncArg::AbortSignal(signal)) => ExecSyncOptions {
            abort_signal: Some(signal),
            timeout_ms: Some(DEFAULT_TIMEOUT_MS),
            input: None,
        },
    };

    let final_timeout = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

    if let Some(signal) = &options.abort_signal {
        if signal.aborted() {
            return None;
        }
    }

    let _guard: SlowOperationGuard =
        slow_logging(|| format!("exec: {}", &command[..command.len().min(200)]));

    let result = run_shell_capture(command, final_timeout, options.input.as_deref());
    let output = match result {
        Ok(output) => output,
        Err(_) => return None,
    };

    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn run_shell_capture(
    command: &str,
    timeout_ms: u64,
    input: Option<&str>,
) -> io::Result<std::process::Output> {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let shell_flag = if cfg!(windows) { "/C" } else { "-c" };
    let mut cmd = Command::new(shell);
    cmd.arg(shell_flag).arg(command);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // env: 继承当前进程环境（对齐 TS process.env）；
    // stdio: ['ignore', 'pipe', 'pipe'] —— 有 input 时 stdin 用 pipe
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.current_dir(cwd::get_cwd());

    let mut child = cmd.spawn()?;

    // stdin：有 input 则写入后闭管
    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
    } else {
        // stdio: ['ignore', 'pipe', 'pipe']
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait()? {
            Some(status) => {
                let stdout = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut v = Vec::new();
                        let _ = s.read_to_end(&mut v);
                        // maxBuffer: 1MB（对齐 TS execa 配置）
                        v.truncate(MAX_BUFFER);
                        v
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut v = Vec::new();
                        let _ = s.read_to_end(&mut v);
                        v
                    })
                    .unwrap_or_default();
                return Ok(std::process::Output { status, stdout, stderr });
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "exec timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

