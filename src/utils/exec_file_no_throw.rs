//! exec_file_no_throw.rs — execFile 的永不抛错封装。
//!
//! 重写自 `src/utils/execFileNoThrow.ts`。语义对齐：
//!
//! - `exec_file_no_throw(file, args, options)`：execFile，但**永远 resolve**，
//!   返回 `{ stdout, stderr, code, error? }`。
//! - `exec_file_no_throw_with_cwd`：接受显式 cwd 的核心变体。
//! - 失败语义（对齐 TS execa 的 `failed`）：
//!   - preserve_outputOnError=true（默认）：保留 stdout/stderr，code 为
//!     退出码（缺失取 1），error 为可读消息（shortMessage > signal > code）。
//!   - preserve_outputOnError=false：stdout/stderr 清空。
//!   - spawn 级失败（命令不存在等）：logError 后返回空输出 + code 1。
//! - 默认 timeout 10 分钟、maxBuffer 1MB、cwd 取 getCwd()。
//! - Windows 上 execa 自带 .bat/.cmd 兼容处理；Rust 侧偏差：直接执行
//!   目标文件，未做 PATHEXT/PATHEXT 后缀探测与 cmd /c 包装，调用方在
//!   Windows 上应传 .exe 或配合 shell:true。
//!
//! 偏差：AbortSignal → CancellationToken；异步用 tokio::process。

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::utils::abort_controller::CancellationToken;
use crate::utils::log::log_error;

const MS_IN_SECOND: u64 = 1000;
const SECONDS_IN_MINUTE: u64 = 60;
const DEFAULT_TIMEOUT_MS: u64 = 10 * SECONDS_IN_MINUTE * MS_IN_SECOND;
const DEFAULT_MAX_BUFFER: usize = 1_000_000;

/// 执行结果（对齐 TS 的 `{ stdout, stderr, code, error? }`）。
#[derive(Debug, Clone)]
pub struct ExecFileResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecFileOptions {
    pub abort_signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub preserve_output_on_error: Option<bool>,
    /// Setting useCwd=false avoids circular dependencies during initialization
    /// (getCwd() -> PersistentShell -> logEvent() -> execFileNoThrow)
    pub use_cwd: Option<bool>,
    pub env: Option<HashMap<String, String>>,
    pub stdin: Option<StdinMode>,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    Ignore,
    Inherit,
    Pipe,
}

#[derive(Debug, Clone, Default)]
pub struct ExecFileWithCwdOptions {
    pub abort_signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    pub preserve_output_on_error: Option<bool>,
    pub max_buffer: Option<usize>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub shell: Option<bool>,
    pub stdin: Option<StdinMode>,
    pub input: Option<String>,
}

/// execFileNoThrow：exec_file_no_throw_with_cwd 的便捷包装，
/// useCwd=true 时把 cwd 设为 getCwd()。
pub async fn exec_file_no_throw(
    file: &str,
    args: &[String],
    options: &ExecFileOptions,
) -> ExecFileResult {
    let cwd = if options.use_cwd.unwrap_or(true) {
        Some(crate::utils::cwd::get_cwd())
    } else {
        None
    };
    exec_file_no_throw_with_cwd(
        file,
        args,
        &ExecFileWithCwdOptions {
            abort_signal: options.abort_signal.clone(),
            timeout_ms: options.timeout_ms,
            preserve_output_on_error: options.preserve_output_on_error,
            max_buffer: Some(DEFAULT_MAX_BUFFER),
            cwd,
            env: options.env.clone(),
            shell: None,
            stdin: options.stdin,
            input: options.input.clone(),
        },
    )
    .await
}

/// 取人可读的错误消息（优先级：shortMessage > signal > code）。
fn get_error_message(short_message: Option<&str>, signal: Option<&str>, code: i32) -> String {
    if let Some(short_message) = short_message {
        return short_message.to_string();
    }
    if let Some(signal) = signal {
        return signal.to_string();
    }
    code.to_string()
}

/// execFile，但永远 resolve（不抛错）。
pub async fn exec_file_no_throw_with_cwd(
    file: &str,
    args: &[String],
    options: &ExecFileWithCwdOptions,
) -> ExecFileResult {
    let timeout = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let preserve_output = options.preserve_output_on_error.unwrap_or(true);
    let max_buffer = options.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER);

    let mut cmd = if options.shell.unwrap_or(false) {
        // execa 的 shell:true —— 经 sh -c（Windows 为 cmd /c）执行
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let shell_flag = if cfg!(windows) { "/C" } else { "-c" };
        let mut joined = shlex_like_quote(file);
        for arg in args {
            joined.push(' ');
            joined.push_str(&shlex_like_quote(arg));
        }
        let mut cmd = Command::new(shell);
        cmd.arg(shell_flag).arg(&joined);
        cmd
    } else {
        let mut cmd = Command::new(file);
        cmd.args(args);
        cmd
    };
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if let Some(cwd) = &options.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = &options.env {
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    match options.stdin.unwrap_or(StdinMode::Pipe) {
        StdinMode::Ignore => {
            cmd.stdin(std::process::Stdio::null());
        }
        StdinMode::Inherit => {
            cmd.stdin(std::process::Stdio::inherit());
        }
        StdinMode::Pipe => {
            cmd.stdin(if options.input.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            });
        }
    }

    let child_result = cmd.spawn();
    let mut child = match child_result {
        Ok(child) => child,
        Err(e) => {
            // execa 抛 ExecaError 时：logError 并返回空输出 + code 1
            log_error(anyhow::anyhow!(e));
            return ExecFileResult {
                stdout: String::new(),
                stderr: String::new(),
                code: 1,
                error: None,
            };
        }
    };

    // input 写入 stdin
    if let Some(input) = &options.input {
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            let _ = stdin.write_all(input.as_bytes()).await;
            drop(stdin);
        }
    }

    // 并发读 stdout/stderr（任务形式，避免与 wait 的 borrow 冲突）。
    // maxBuffer 截断在收集后做（对齐 execa）。
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();
    let stdout_task = tokio::spawn(read_all_to_vec(stdout_handle));
    let stderr_task = tokio::spawn(read_all_to_vec(stderr_handle));

    // 等待退出；deadline 到则 kill（try_wait 轮询是为了在超时分支里
    // 仍能访问 child —— wait() 的借用与 kill 冲突）。
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
    let mut timed_out = false;
    let mut status: Option<std::process::ExitStatus> = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill().await;
                    timed_out = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(e) => {
                log_error(anyhow::anyhow!(e));
                break;
            }
        }
    }

    // 收集 stdout/stderr（任务在子进程退出后自然完成；超时 kill 后
    // 管道关闭，read 返回 0）
    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    // abort 检查：token 已 abort 视为失败
    let aborted = options
        .abort_signal
        .as_ref()
        .map(|t| t.aborted())
        .unwrap_or(false);

    let mut stdout_bytes = stdout_bytes;
    stdout_bytes.truncate(max_buffer);
    let mut stderr_bytes = stderr_bytes;
    stderr_bytes.truncate(max_buffer);
    let stdout_string = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_string = String::from_utf8_lossy(&stderr_bytes).into_owned();

    let Some(status) = status else {
        // wait 本身失败（罕见，如信号采集错误）：当失败处理
        return ExecFileResult {
            stdout: String::new(),
            stderr: String::new(),
            code: 1,
            error: None,
        };
    };

    if status.success() && !aborted && !timed_out {
        return ExecFileResult {
            stdout: stdout_string,
            stderr: stderr_string,
            code: 0,
            error: None,
        };
    }

    let code = status.code().unwrap_or(1);
    if preserve_output {
        let signal = if timed_out {
            Some("SIGKILL".to_string())
        } else {
            signal_name(exit_signal(&status))
        };
        let short_message = if aborted {
            Some("The user aborted a request".to_string())
        } else if timed_out {
            Some(format!("Command timed out after {timeout} ms: {file} {}", args.join(" ")))
        } else if code != 0 {
            Some(format!(
                "Command failed with exit code {code}: {file} {}",
                args.join(" ")
            ))
        } else {
            None
        };
        ExecFileResult {
            stdout: stdout_string,
            stderr: stderr_string,
            code,
            error: Some(get_error_message(short_message.as_deref(), signal.as_deref(), code)),
        }
    } else {
        ExecFileResult {
            stdout: String::new(),
            stderr: String::new(),
            code,
            error: None,
        }
    }
}

/// 把子进程的 stdout/stderr 读到 Vec<u8>（None 句柄返回空）。
async fn read_all_to_vec<R>(mut handle: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    match &mut handle {
        Some(stream) => {
            let mut v = Vec::new();
            let _ = stream.read_to_end(&mut v).await;
            v
        }
        None => Vec::new(),
    }
}
    match handle {
        Some(mut stream) => {
            let mut v = Vec::new();
            let _ = stream.read_to_end(&mut v).await;
            v
        }
        None => Vec::new(),
    }
}

fn signal_name(code: Option<i32>) -> Option<String> {
    match code {
        Some(2) => Some("SIGINT".to_string()),
        Some(15) => Some("SIGTERM".to_string()),
        Some(n) => Some(format!("signal {n}")),
        None => None,
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// 简单 POSIX 引号：空或含空白/特殊字符则单引号包裹并转义内部单引号。
/// 对齐 execa 的 shell:true 拼参行为（非严格 POSIX quoting，但覆盖
/// 绝大多数 CLI 调用场景）。
fn shlex_like_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let needs_quotes = s.chars().any(|c| {
        c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '"' | '\'' | '\\' | '*' | '?' | '[' | ']' | '#' | '~' | '=' | '%')
    });
    if needs_quotes {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}
