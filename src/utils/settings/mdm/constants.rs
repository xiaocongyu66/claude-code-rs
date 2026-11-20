//! constants.rs — MDM settings 模块的共享常量与路径构造。
//!
//! 重写自 `src/utils/settings/mdm/constants.ts`。
//!
//! 该模块零重依赖（TS 侧只导入 `os`）——mdmRawRead 与 mdmSettings 都从
//! 这里导入以避免重复。
//!
//! 语义对齐：
//! - `os.userInfo().username`：Rust 标准库无直接等价，按 USER / LOGNAME /
//!   USERNAME 环境变量顺序读取（macOS 上与 userInfo 一致）；
//! - `os.homedir()`：读 `HOME` 环境变量；
//! - `process.env.USER_TYPE === 'ant'` 调用时求值。

use std::path::PathBuf;

/// macOS preference domain for Claude Code MDM profiles.
pub const MACOS_PREFERENCE_DOMAIN: &str = "com.anthropic.claudecode";

/// Windows registry key paths for Claude Code MDM policies.
///
/// These keys live under SOFTWARE\Policies which is on the WOW64 shared key
/// list — both 32-bit and 64-bit processes see the same values without
/// redirection. Do not move these to SOFTWARE\ClaudeCode, as SOFTWARE is
/// redirected and 32-bit processes would silently read from WOW6432Node.
/// See: https://learn.microsoft.com/en-us/windows/win32/winprog64/shared-registry-keys
pub const WINDOWS_REGISTRY_KEY_PATH_HKLM: &str = "HKLM\\SOFTWARE\\Policies\\ClaudeCode";
pub const WINDOWS_REGISTRY_KEY_PATH_HKCU: &str = "HKCU\\SOFTWARE\\Policies\\ClaudeCode";

/// Windows registry value name containing the JSON settings blob.
pub const WINDOWS_REGISTRY_VALUE_NAME: &str = "Settings";

/// Path to macOS plutil binary.
pub const PLUTIL_PATH: &str = "/usr/bin/plutil";

/// Arguments for plutil to convert plist to JSON on stdout (append plist path).
pub const PLUTIL_ARGS_PREFIX: &[&str] = &["-convert", "json", "-o", "-", "--"];

/// Subprocess timeout in milliseconds.
pub const MDM_SUBPROCESS_TIMEOUT_MS: u64 = 5000;

/// macOS plist 路径条目：路径 + 来源标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacOSPlistPath {
    pub path: String,
    pub label: &'static str,
}

fn current_username() -> Option<String> {
    // 对齐 TS 的 `userInfo().username`（os 模块在 macOS 上返回当前有效
    // 用户名）；Rust 标准库无 userInfo，按常见环境变量顺序读取。
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
}

fn homedir() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Build the list of macOS plist paths in priority order (highest first).
/// Evaluates `process.env.USER_TYPE` at call time so ant-only paths are
/// included only when appropriate.
pub fn get_macos_plist_paths() -> Vec<MacOSPlistPath> {
    let username = current_username();
    let mut paths: Vec<MacOSPlistPath> = Vec::new();

    if let Some(username) = username {
        paths.push(MacOSPlistPath {
            path: format!(
                "/Library/Managed Preferences/{username}/{MACOS_PREFERENCE_DOMAIN}.plist"
            ),
            label: "per-user managed preferences",
        });
    }

    paths.push(MacOSPlistPath {
        path: format!("/Library/Managed Preferences/{MACOS_PREFERENCE_DOMAIN}.plist"),
        label: "device-level managed preferences",
    });

    // Allow user-writable preferences for local MDM testing in ant builds only.
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        if let Some(home) = homedir() {
            paths.push(MacOSPlistPath {
                path: home
                    .join("Library")
                    .join("Preferences")
                    .join(format!("{MACOS_PREFERENCE_DOMAIN}.plist"))
                    .to_string_lossy()
                    .into_owned(),
                label: "user preferences (ant-only)",
            });
        }
    }

    paths
}
