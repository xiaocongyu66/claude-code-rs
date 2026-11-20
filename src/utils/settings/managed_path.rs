//! managed_path.rs — 托管（managed）settings 的平台路径。
//!
//! 重写自 `src/utils/settings/managedPath.ts`。零业务依赖：仅平台检测 +
//! 环境变量。TS 用 lodash `memoize` 缓存；Rust 用 [`std::sync::OnceLock`]
//! （首次调用后永不变更，与 memoize 语义一致；测试如需重置见
//! [`reset_for_testing`]）。
//!
//! 语义对齐：
//! - `USER_TYPE === 'ant'` 且 `CLAUDE_CODE_MANAGED_SETTINGS_PATH` 设置时
//!   使用该覆盖路径（ant-only 测试/演示用途，外部构建中不出现）；
//! - macOS: `/Library/Application Support/ClaudeCode`；
//! - Windows: `C:\Program Files\ClaudeCode`；
//! - 其它（Linux）: `/etc/claude-code`；
//! - drop-in 目录 = `<managed>/managed-settings.d`。

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::utils::platform::get_platform;

static MANAGED_FILE_PATH: OnceLock<String> = OnceLock::new();
static DROP_IN_DIR: OnceLock<String> = OnceLock::new();

fn is_ant_user() -> bool {
    std::env::var("USER_TYPE").as_deref() == Ok("ant")
}

/// 托管 settings 目录（按平台）。
pub fn get_managed_file_path() -> &'static str {
    MANAGED_FILE_PATH.get_or_init(|| {
        // Allow override for testing/demos (Ant-only, eliminated from external builds)
        if is_ant_user() {
            if let Ok(path) = std::env::var("CLAUDE_CODE_MANAGED_SETTINGS_PATH") {
                if !path.is_empty() {
                    return path;
                }
            }
        }

        match get_platform() {
            crate::utils::platform::Platform::Macos => {
                "/Library/Application Support/ClaudeCode".to_string()
            }
            crate::utils::platform::Platform::Windows => {
                "C:\\Program Files\\ClaudeCode".to_string()
            }
            // WSL / Linux / Unknown：TS 的 default 分支
            _ => "/etc/claude-code".to_string(),
        }
    })
}

/// managed-settings.d/ drop-in 目录。
pub fn get_managed_settings_drop_in_dir() -> &'static str {
    DROP_IN_DIR.get_or_init(|| {
        PathBuf::from(get_managed_file_path())
            .join("managed-settings.d")
            .to_string_lossy()
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_default_paths() {
        // 不设置 ant 覆盖时，路径只随平台变化
        let path = get_managed_file_path();
        match get_platform() {
            crate::utils::platform::Platform::Macos => {
                assert_eq!(path, "/Library/Application Support/ClaudeCode")
            }
            crate::utils::platform::Platform::Windows => {
                assert_eq!(path, "C:\\Program Files\\ClaudeCode")
            }
            _ => assert_eq!(path, "/etc/claude-code"),
        }
    }

    #[test]
    fn drop_in_dir_is_child_of_managed_path() {
        let drop_in = get_managed_settings_drop_in_dir();
        assert!(drop_in.ends_with("managed-settings.d"));
    }
}
