//! ide_path_conversion.rs — IDE 通信的路径转换工具。
//!
//! 重写自 `src/utils/idePathConversion.ts`。语义对齐：
//!
//! - 处理 Claude 环境与 IDE 环境之间的路径转换（Windows IDE + WSL
//!   Claude 场景）。
//! - `to_local_path`：先匹配 `\\wsl$\<distro>\` / `\\wsl.localhost\<distro>\`
//!   UNC 前缀做 distro 校验（distro 不匹配则原样返回），再调用
//!   `wslpath -u` 转换；wslpath 失败时手工兜底（反斜杠转正斜杠 +
//!   `X:` → `/mnt/x`）。
//! - `to_ide_path`：调用 `wslpath -w`，失败原样返回。
//!
//! 偏差：TS 用 `execFileSync`；Rust 侧用 `std::process::Command`，
//! stderr 丢弃（对齐 stdio ignore）。

use std::process::Command;

use once_cell::sync::Lazy;
use regex::Regex;

static WSL_UNC_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\\\\wsl(?:\.localhost|\$)\\([^\\]+)(.*)$")
        .expect("valid wsl unc regex")
});

/// IDE 路径转换器 trait（对齐 TS IDEPathConverter）。
pub trait IdePathConverter {
    /// 把 IDE 格式路径转成本地（Claude）格式。用于从 IDE lockfile
    /// 读取 workspace folders。
    fn to_local_path(&self, ide_path: &str) -> String;
    /// 把本地格式路径转成 IDE 格式。用于向 IDE 发路径
    /// （showDiffInIDE 等）。
    fn to_ide_path(&self, local_path: &str) -> String;
}

/// Windows IDE + WSL Claude 场景的转换器。
pub struct WindowsToWslConverter {
    pub wsl_distro_name: Option<String>,
}

impl WindowsToWslConverter {
    pub fn new(wsl_distro_name: Option<String>) -> Self {
        Self { wsl_distro_name }
    }

    fn run_wslpath(arg: &str, path: &str) -> Option<String> {
        let output = Command::new("wslpath")
            .arg(arg)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()) // wslpath 把错误写到 stderr
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl IdePathConverter for WindowsToWslConverter {
    fn to_local_path(&self, windows_path: &str) -> String {
        if windows_path.is_empty() {
            return windows_path.to_string();
        }

        // 来自不同 WSL distro 的路径：wslpath 会失败，原样返回
        if let Some(distro) = &self.wsl_distro_name {
            if let Some(caps) = WSL_UNC_RE.captures(windows_path) {
                if caps.get(1).map(|m| m.as_str()) != Some(distro.as_str()) {
                    return windows_path.to_string();
                }
            }
        }

        if let Some(result) = Self::run_wslpath("-u", windows_path) {
            return result;
        }

        // wslpath 失败：手工兜底（`X:` → `/mnt/x`）
        let with_slashes = windows_path.replace('\\', "/");
        let bytes = with_slashes.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let drive = bytes[0].to_ascii_lowercase() as char;
            return format!("/mnt/{drive}{}", &with_slashes[2..]);
        }
        with_slashes
    }

    fn to_ide_path(&self, wsl_path: &str) -> String {
        if wsl_path.is_empty() {
            return wsl_path.to_string();
        }
        // wslpath 失败则原样返回
        Self::run_wslpath("-w", wsl_path).unwrap_or_else(|| wsl_path.to_string())
    }
}

/// 检查 WSL UNC 路径的 distro 是否匹配。
pub fn check_wsl_distro_match(windows_path: &str, wsl_distro_name: &str) -> bool {
    if let Some(caps) = WSL_UNC_RE.captures(windows_path) {
        return caps.get(1).map(|m| m.as_str()) == Some(wsl_distro_name);
    }
    true // 非 WSL UNC 路径，不存在 distro 不匹配
}
