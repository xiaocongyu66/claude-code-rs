//! system_directories.rs — 跨平台系统目录。
//!
//! 重写自 `src/utils/systemDirectories.ts`。语义对齐：
//!
//! - 处理 Windows / macOS / Linux / WSL 差异；Windows 优先 USERPROFILE
//!   （兼容本地化文件夹名），Linux/WSL 优先 XDG 规范变量。
//! - `homedir` 用 `HOME`/`USERPROFILE` 环境变量近似（Cargo.toml 无
//!   `dirs` crate）。
//! - 可注入 env / homedir / platform 供测试。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::platform::{get_platform, Platform};

/// 系统目录表。
#[derive(Debug, Clone)]
pub struct SystemDirectories {
    pub home: PathBuf,
    pub desktop: PathBuf,
    pub documents: PathBuf,
    pub downloads: PathBuf,
}

/// `HOME` 目录近似（对齐 TS `os.homedir()`）。
///
/// Unix 用 `HOME`，Windows 用 `USERPROFILE`；都缺失时退化为空字符串
/// （TS 的 os.homedir() 在极端环境也会抛错，这里不 panic）。
pub fn home_dir() -> String {
    if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_default()
    } else {
        std::env::var("HOME").unwrap_or_default()
    }
}

fn env_get(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|v| !v.is_empty()).cloned()
}

/// 获取跨平台系统目录。
///
/// `options` 可注入 env / homedir / platform 供测试。
pub fn get_system_directories(
    env: Option<&HashMap<String, String>>,
    home: Option<&str>,
    platform: Option<Platform>,
) -> SystemDirectories {
    let platform = platform.unwrap_or_else(get_platform);
    let home_dir_value = home.map(str::to_string).unwrap_or_else(home_dir);
    let live_env: HashMap<String, String> = std::env::vars().collect();
    let env = env.unwrap_or(&live_env);

    let join_home = |name: &str| PathBuf::from(&home_dir_value).join(name);
    let defaults = SystemDirectories {
        home: PathBuf::from(&home_dir_value),
        desktop: join_home("Desktop"),
        documents: join_home("Documents"),
        downloads: join_home("Downloads"),
    };

    match platform {
        Platform::Windows => {
            // Windows：优先 USERPROFILE（处理本地化文件夹名）
            let user_profile = env_get(env, "USERPROFILE").unwrap_or_else(|| home_dir_value.clone());
            let join_profile = |name: &str| PathBuf::from(&user_profile).join(name);
            SystemDirectories {
                home: PathBuf::from(&home_dir_value),
                desktop: join_profile("Desktop"),
                documents: join_profile("Documents"),
                downloads: join_profile("Downloads"),
            }
        }
        Platform::Linux | Platform::Wsl => {
            // Linux/WSL：优先 XDG Base Directory 规范
            SystemDirectories {
                home: PathBuf::from(&home_dir_value),
                desktop: env_get(env, "XDG_DESKTOP_DIR")
                    .map(PathBuf::from)
                    .unwrap_or(defaults.desktop),
                documents: env_get(env, "XDG_DOCUMENTS_DIR")
                    .map(PathBuf::from)
                    .unwrap_or(defaults.documents),
                downloads: env_get(env, "XDG_DOWNLOAD_DIR")
                    .map(PathBuf::from)
                    .unwrap_or(defaults.downloads),
            }
        }
        Platform::Macos | Platform::Unknown => {
            if platform == Platform::Unknown {
                log_for_debugging(
                    "Unknown platform detected, using default paths",
                    DebugLogLevel::Debug,
                );
            }
            defaults
        }
    }
}
