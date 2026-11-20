//! which.rs — 查找命令可执行文件的完整路径。
//!
//! 重写自 `src/utils/which.ts`。语义对齐：
//!
//! - TS 优先用 `Bun.which`（无进程派生）；Rust 无 Bun，等价走 Node 路径：
//!   Windows 用 `where.exe`（取第一个结果），POSIX 用 `which`。
//! - 失败/无输出返回 None；stdout trim 后作为路径返回。
//! - `which` 为异步（tokio::process），`which_sync` 为同步。
//!
//! 偏差：TS 的 execa 带 `shell: true`；Rust 侧显式经 `sh -c` /
//! `cmd /c` 执行以保持 shell 语义。stderr 一律忽略，非零退出不抛错。

use std::process::{Command, Stdio};

/// 异步查找命令路径（Windows 用 where.exe，POSIX 用 which）。
///
/// 买家注意：Windows 上 `where.exe` 可能返回多行（按 PATH 顺序），
/// 只取第一行（对齐 TS）。
pub async fn which(command: &str) -> Option<String> {
    if cfg!(windows) {
        let output = Command::new("cmd")
            .args(["/C", &format!("where.exe {command}")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .await
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .trim()
            .split(|c: char| c == '\n' || c == '\r')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    } else {
        let output = Command::new("sh")
            .args(["-c", &format!("which {command}")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .await
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// `which` 的同步版本。
pub fn which_sync(command: &str) -> Option<String> {
    if cfg!(windows) {
        let output = Command::new("cmd")
            .args(["/C", &format!("where.exe {command}")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .trim()
            .split(|c: char| c == '\n' || c == '\r')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    } else {
        let output = Command::new("sh")
            .args(["-c", &format!("which {command}")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
