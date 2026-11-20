//! exec_sync_wrapper.rs — 带慢操作日志的 execSync 包装（DEPRECATED）。
//!
//! 重写自 `src/utils/execSyncWrapper.ts`。语义对齐：
//!
//! - 用 `slowLogging` 包裹每次 execSync，检测慢操作。
//! - TS 的 `execSync` 走 shell（POSIX `sh -c` / Windows `cmd /c`），
//!   stdout 捕获为 Buffer 或 string（encoding 选项）。
//! - 偏差：Rust 侧用 `std::process::Command` 显式起 shell；
//!   `BufferEncoding` 只在 utf8/ascii 时把输出转成 String（对齐
//!   fs_operations 的 decode 约定），其余编码返回原始字节。
//! - 该 API 标记 DEPRECATED：同步 exec 阻塞事件循环。

use std::collections::HashMap;
use std::io::{self, Read};
use std::process::{Command, Stdio};

use crate::utils::fs_operations::BufferEncoding;
use crate::utils::slow_operations::{slow_logging, SlowOperationGuard};

/// execSync 选项（对齐 Node ExecSyncOptions 的常用子集）。
#[derive(Debug, Clone, Default)]
pub struct ExecSyncOptions {
    /// 工作目录；缺省为当前进程 cwd。
    pub cwd: Option<String>,
    /// 环境变量；缺省继承当前进程。
    pub env: Option<HashMap<String, String>>,
    /// 输出编码；None 返回原始字节。
    pub encoding: Option<BufferEncoding>,
    /// 超时毫秒；None 不超时。
    pub timeout_ms: Option<u64>,
}

/// execSync 的输出（对齐 TS 的 `Buffer | string` 联合）。
#[derive(Debug, Clone)]
pub enum ExecSyncOutput {
    Bytes(Vec<u8>),
    Text(String),
}

impl ExecSyncOutput {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ExecSyncOutput::Text(s) => Some(s),
            ExecSyncOutput::Bytes(_) => None,
        }
    }
}

fn decode(bytes: &[u8], encoding: BufferEncoding) -> ExecSyncOutput {
    match encoding {
        BufferEncoding::Utf8 | BufferEncoding::Ascii => {
            ExecSyncOutput::Text(String::from_utf8_lossy(bytes).into_owned())
        }
        // 其余编码保持原始字节（对齐 fs_operations 只支持 utf8/ascii 的约定）
        _ => ExecSyncOutput::Bytes(bytes.to_vec()),
    }
}

/// 带 slowLogging 的 execSync（DEPRECATED）。
///
/// 通过 shell 执行命令。非零退出码时返回错误（对齐 node execSync 抛错）。
pub fn exec_sync_deprecated(command: &str, options: &ExecSyncOptions) -> io::Result<ExecSyncOutput> {
    let _guard: SlowOperationGuard =
        slow_logging(|| format!("execSync: {}", &command[..command.len().min(100)]));

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let shell_flag = if cfg!(windows) { "/C" } else { "-c" };

    let mut cmd = Command::new(shell);
    cmd.arg(shell_flag).arg(command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if let Some(cwd) = &options.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = &options.env {
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    if let Some(timeout_ms) = options.timeout_ms {
        // std 的 child 超时在 wait 时由调用方实现；这里用 spawn + wait_timeout 风格
        let mut child = cmd.spawn()?;
        let output = match wait_with_timeout(&mut child, timeout_ms) {
            Ok(output) => output,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Command failed with exit code {:?}", output.status.code()),
            ));
        }
        Ok(decode(
            &output.stdout,
            options.encoding.unwrap_or(BufferEncoding::Utf8),
        ))
    } else {
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Command failed with exit code {:?}", output.status.code()),
            ));
        }
        Ok(decode(
            &output.stdout,
            options.encoding.unwrap_or(BufferEncoding::Utf8),
        ))
    }
}

/// kill + 等待的简易超时包装（不引入新依赖）。
fn wait_with_timeout(child: &mut std::process::Child, timeout_ms: u64) -> io::Result<std::process::Output> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait()? {
            Some(status) => {
                let stdout = child.stdout.take().map(|mut s| {
                    let mut v = Vec::new();
                    let _ = s.read_to_end(&mut v);
                    v
                });
                let stderr = child.stderr.take().map(|mut s| {
                    let mut v = Vec::new();
                    let _ = s.read_to_end(&mut v);
                    v
                });
                return Ok(std::process::Output {
                    status,
                    stdout: stdout.unwrap_or_default(),
                    stderr: stderr.unwrap_or_default(),
                });
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "execSync timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}
