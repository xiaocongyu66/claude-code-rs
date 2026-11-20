//! save_workspace_key.rs — workspace API key 的校验与持久化（重写自
//! `src/services/auth/saveWorkspaceKey.ts`）。
//!
//! 安全属性（对齐 TS）：
//! - 写入前校验 `sk-ant-api03-` 前缀与长度（20 下限 / 256 上限）。
//! - 错误消息绝不包含 key 本身（错误前缀展示最多 4 字符，指向官方
//!   控制台获取入口）。
//! - 写穿透缓存：`save_global_config` 后立即反映新 key。
//! - POSIX 上对配置文件 chmod 600；Windows 无 chmod，输出一次性 icacls
//!   提示日志。
//!
//! 已知偏差：
//! - `utils/config.ts` 未转换：`~/.claude.json` 的读写在本模块内
//!   duck-type（同 auth.rs 做法），含进程内写穿透缓存与原子重命名。
//! - sanitize 策略与 TS 一致：消息中的 `sk-ant-api03-\S*` → `[REDACTED]`。

use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::constants::oauth::file_suffix_for_oauth_config;
use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::log::log_error;
use crate::utils::system_directories::home_dir;

const WORKSPACE_KEY_PREFIX: &str = "sk-ant-api03-";
const MIN_KEY_LENGTH: usize = 20;
const MAX_KEY_LENGTH: usize = 256;

/// 保存失败的错误。
#[derive(Debug)]
pub struct SaveWorkspaceKeyError {
    pub message: String,
}

impl std::fmt::Display for SaveWorkspaceKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SaveWorkspaceKeyError {}

/// 校验并保存 workspace API key 到 `~/.claude.json`（write-through）。
///
/// key 为空 / 前缀错误 / 过短 / 过长时返回错误；错误消息不含 key 值。
pub fn save_workspace_key(key: &str) -> Result<(), SaveWorkspaceKeyError> {
    if key.trim().is_empty() {
        return Err(SaveWorkspaceKeyError::new(
            "Workspace API key must not be empty.".to_string(),
        ));
    }

    let trimmed = key.trim();

    if trimmed.len() < MIN_KEY_LENGTH {
        return Err(SaveWorkspaceKeyError::new(format!(
            "Workspace API key is too short ({} chars). Expected at least {} chars starting with \"{}\".",
            trimmed.len(),
            MIN_KEY_LENGTH,
            WORKSPACE_KEY_PREFIX
        )));
    }

    if trimmed.len() > MAX_KEY_LENGTH {
        return Err(SaveWorkspaceKeyError::new(format!(
            "Workspace API key is too long ({} chars). Maximum allowed length is {} chars.",
            trimmed.len(),
            MAX_KEY_LENGTH
        )));
    }

    if !trimmed.starts_with(WORKSPACE_KEY_PREFIX) {
        // 只展示实际 key 前 4 字符，避免泄漏熵
        let prefix4: String = trimmed.chars().take(4).collect();
        return Err(SaveWorkspaceKeyError::new(format!(
            "Workspace API key must start with \"{}\" (workspace key). Got prefix \"{prefix4}...\". \
             Obtain a workspace API key from https://console.anthropic.com/settings/keys.",
            WORKSPACE_KEY_PREFIX
        )));
    }

    let trimmed = trimmed.to_string();
    save_global_config(&|current| {
        let mut next = current.clone();
        next["workspaceApiKey"] = Value::String(trimmed.clone());
        next
    })
    .map_err(|err| {
        SaveWorkspaceKeyError::new(format!(
            "Failed to save workspace API key to config: {}",
            sanitize_error_message(&err)
        ))
    })?;

    try_chmod_600();
    Ok(())
}

/// 从设置中移除 workspace API key（不动 ANTHROPIC_API_KEY 环境变量）。
pub fn remove_workspace_key() -> Result<(), SaveWorkspaceKeyError> {
    save_global_config(&|current| {
        let mut next = current.clone();
        if let Value::Object(map) = &mut next {
            map.remove("workspaceApiKey");
        }
        next
    })
    .map_err(|err| {
        SaveWorkspaceKeyError::new(format!(
            "Failed to remove workspace API key: {}",
            sanitize_error_message(&err)
        ))
    })
}

/// 有效 workspace API key：ANTHROPIC_API_KEY 环境变量优先，其次
/// `~/.claude.json` 的 workspaceApiKey。两者均无返回 None。
pub fn get_effective_workspace_api_key() -> Option<String> {
    if let Ok(from_env) = std::env::var("ANTHROPIC_API_KEY") {
        let trimmed = from_env.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    read_global_config()
        .get("workspaceApiKey")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

impl SaveWorkspaceKeyError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

/// 从错误消息中剥掉一切 `sk-ant-api03-*` 形态的值（防泄漏到日志 /
/// Sentry）。
fn sanitize_error_message(error: &dyn std::error::Error) -> String {
    let message = error.to_string();
    let re = match regex::Regex::new(r"sk-ant-api03-\S*") {
        Ok(re) => re,
        Err(_) => return message,
    };
    re.replace_all(&message, "[REDACTED]").to_string()
}

fn try_chmod_600() {
    let config_path = get_global_claude_file();
    if cfg!(target_os = "windows") {
        log_error(SaveWorkspaceKeyError::new(format!(
            "[saveWorkspaceKey] Windows: chmod 600 is not supported. To protect your API key, \
             restrict access to {} via icacls or Windows ACL settings.",
            config_path.display()
        )));
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(&config_path) {
            Ok(metadata) => {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o600);
                if let Err(err) = std::fs::set_permissions(&config_path, permissions) {
                    // 非致命：记录但不抛
                    log_error(SaveWorkspaceKeyError::new(format!(
                        "[saveWorkspaceKey] Could not set chmod 600 on {}: {}",
                        config_path.display(),
                        sanitize_error_message(&err)
                    )));
                }
            }
            Err(err) => {
                log_error(SaveWorkspaceKeyError::new(format!(
                    "[saveWorkspaceKey] Could not set chmod 600 on {}: {}",
                    config_path.display(),
                    sanitize_error_message(&err)
                )));
            }
        }
    }
}

// --- 全局配置 duck-type（utils/config.ts 未转换；同 auth.rs 做法） ---

static GLOBAL_CONFIG_CACHE: Lazy<Mutex<Option<Value>>> = Lazy::new(|| Mutex::new(None));

fn get_global_claude_file() -> PathBuf {
    static CACHE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let config_home = get_claude_config_home_dir();
            let legacy = PathBuf::from(&config_home).join(".config.json");
            if legacy.exists() {
                return legacy;
            }
            let suffix = file_suffix_for_oauth_config();
            let filename = format!(".claude{suffix}.json");
            match std::env::var("CLAUDE_CONFIG_DIR") {
                Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(filename),
                _ => PathBuf::from(home_dir()).join(filename),
            }
        })
        .clone()
}

fn read_global_config() -> Value {
    if let Some(config) = GLOBAL_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        return config.clone();
    }
    let config = std::fs::read(get_global_claude_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Value::Object(Default::default()));
    *GLOBAL_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(config.clone());
    config
}

fn save_global_config(updater: &dyn Fn(&Value) -> Value) -> Result<(), std::io::Error> {
    let current = read_global_config();
    let updated = updater(&current);
    let path = get_global_claude_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&updated)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    *GLOBAL_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(updated);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_key() {
        assert!(save_workspace_key("").is_err());
        assert!(save_workspace_key("   ").is_err());
    }

    #[test]
    fn rejects_wrong_prefix_without_leaking_key() {
        let key = "sk-ant-api04-".to_string() + &"a".repeat(40);
        let err = save_workspace_key(&key).unwrap_err();
        assert!(err.message.contains("sk-ant-api03-"));
        // 错误消息中出现实际 key 熵的只有前 4 字符
        assert!(!err.message.contains(&"a".repeat(40)));
    }

    #[test]
    fn rejects_short_and_long() {
        let short = format!("{WORKSPACE_KEY_PREFIX}xyz");
        assert!(save_workspace_key(&short).is_err());
        let long = format!("{WORKSPACE_KEY_PREFIX}{}", "a".repeat(300));
        assert!(save_workspace_key(&long).is_err());
    }

    #[test]
    fn sanitize_strips_key_patterns() {
        struct FakeError(String);
        impl std::fmt::Display for FakeError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::fmt::Debug for FakeError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for FakeError {}
        let sanitized = sanitize_error_message(&FakeError(
            "boom sk-ant-api03-deadbeef tail".to_string(),
        ));
        assert_eq!(sanitized, "boom [REDACTED] tail");
    }
}
