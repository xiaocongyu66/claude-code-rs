//! get_worktree_paths_portable.rs — 仅用 child_process 的可移植 worktree 探测。
//!
//! 重写自 `src/utils/getWorktreePathsPortable.ts`。语义对齐：
//!
//! - 用于 listSessionsImpl（SDK）等需要 worktree 路径、又不能拉入 CLI
//!   依赖链（execa → cross-spawn → which）的地方。
//! - 跑 `git worktree list --porcelain`，取 `worktree ` 前缀的行，
//!   NFC 规范化；超时 5 秒；任何失败（git 缺失、非仓库、超时）返回空。

use std::time::Duration;

use tokio::process::Command;

/// 返回 cwd 所在仓库的全部 worktree 路径；失败返回空 Vec。
pub async fn get_worktree_paths_portable(cwd: &str) -> Vec<String> {
    let output = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split('\n')
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| unicode_normalization::UnicodeNormalization::nfc(path).collect::<String>())
        .collect()
}

/// 供需要显式超时的调用方使用（对齐 TS 的 `{ cwd, timeout: 5000 }`）。
pub async fn get_worktree_paths_portable_with_timeout(
    cwd: &str,
    timeout: Duration,
) -> Vec<String> {
    let child = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };
    // 先取走 stdout 管道，这样超时后仍可通过 child 句柄 kill 子进程。
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut buf = Vec::new();
    let stdout = match tokio::time::timeout(
        timeout,
        tokio::io::AsyncReadExt::read_to_end(&mut stdout_pipe, &mut buf),
    )
    .await
    {
        Ok(Ok(_)) => {
            let _ = child.wait().await;
            String::from_utf8_lossy(&buf).into_owned()
        }
        _ => {
            // 超时或失败：尽力收掉子进程
            let _ = child.start_kill();
            return Vec::new();
        }
    };
    stdout
        .split('\n')
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| unicode_normalization::UnicodeNormalization::nfc(path).collect::<String>())
        .collect()
}
