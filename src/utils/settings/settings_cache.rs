//! settings_cache.rs — settings 管线的三层进程内缓存。
//!
//! 重写自 `src/utils/settings/settingsCache.ts`。语义对齐（TS 模块级
//! `let` / `Map` → `Mutex` 包裹的静态全局）：
//! - `session_settings_cache`：loadSettingsFromDisk 的合并结果缓存；
//! - `per_source_cache`：getSettingsForSource 的按来源缓存（None=miss，
//!   Some(None)=缓存“该来源无 settings”）；
//! - `parse_file_cache`：parseSettingsFile 的按路径缓存（磁盘读 + schema
//!   解析去重）；
//! - `plugin_settings_base`：pluginLoader 写入的最底层层级。
//!
//! `resetSettingsCache()` 一次性清空三层缓存（settings 写入、--add-dir、
//! 插件初始化、hooks 刷新、变更通知 fanOut 时调用）。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::bootstrap::state::SettingSource;
use crate::utils::settings::types::SettingsJson;
use crate::utils::settings::validation::{SettingsWithErrors, ValidationError};

/// parseSettingsFile 的缓存条目。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSettings {
    pub settings: Option<SettingsJson>,
    pub errors: Vec<ValidationError>,
}

struct Cache {
    session_settings_cache: Option<SettingsWithErrors>,
    per_source_cache: HashMap<SettingSource, Option<SettingsJson>>,
    parse_file_cache: HashMap<String, ParsedSettings>,
    plugin_settings_base: Option<Value>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(|| Cache {
        session_settings_cache: None,
        per_source_cache: HashMap::new(),
        parse_file_cache: HashMap::new(),
        plugin_settings_base: None,
    });
    f(cache)
}

pub fn get_session_settings_cache() -> Option<SettingsWithErrors> {
    with_cache(|c| c.session_settings_cache.clone())
}

pub fn set_session_settings_cache(value: SettingsWithErrors) {
    with_cache(|c| {
        c.session_settings_cache = Some(value);
    });
}

/// undefined = 缓存未命中；Some(None) = 缓存“该来源无 settings”。
pub fn get_cached_settings_for_source(source: SettingSource) -> Option<Option<SettingsJson>> {
    with_cache(|c| c.per_source_cache.get(&source).cloned())
}

pub fn set_cached_settings_for_source(source: SettingSource, value: Option<SettingsJson>) {
    with_cache(|c| {
        c.per_source_cache.insert(source, value);
    });
}

pub fn get_cached_parsed_file(path: &str) -> Option<ParsedSettings> {
    with_cache(|c| c.parse_file_cache.get(path).cloned())
}

pub fn set_cached_parsed_file(path: &str, value: ParsedSettings) {
    with_cache(|c| {
        c.parse_file_cache.insert(path.to_string(), value);
    });
}

pub fn reset_settings_cache() {
    with_cache(|c| {
        c.session_settings_cache = None;
        c.per_source_cache.clear();
        c.parse_file_cache.clear();
    });
}

/// 插件 settings 基层级：pluginLoader 加载插件后写入，loadSettingsFromDisk
/// 作为最低优先级层读取（对齐 TS 的 `Record<string, unknown>`）。
pub fn get_plugin_settings_base() -> Option<Value> {
    with_cache(|c| c.plugin_settings_base.clone())
}

pub fn set_plugin_settings_base(settings: Option<Value>) {
    with_cache(|c| {
        c.plugin_settings_base = settings;
    });
}

pub fn clear_plugin_settings_base() {
    with_cache(|c| {
        c.plugin_settings_base = None;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::settings::types::SettingsJson;

    #[test]
    fn per_source_cache_miss_and_negative_hit() {
        reset_settings_cache();
        assert_eq!(get_cached_settings_for_source(SettingSource::UserSettings), None);
        set_cached_settings_for_source(SettingSource::UserSettings, None);
        assert_eq!(
            get_cached_settings_for_source(SettingSource::UserSettings),
            Some(None)
        );
        reset_settings_cache();
        assert_eq!(get_cached_settings_for_source(SettingSource::UserSettings), None);
    }

    #[test]
    fn parse_file_cache_roundtrip() {
        reset_settings_cache();
        assert!(get_cached_parsed_file("/nonexistent").is_none());
        set_cached_parsed_file(
            "/nonexistent",
            ParsedSettings {
                settings: Some(SettingsJson::default()),
                errors: vec![],
            },
        );
        assert!(get_cached_parsed_file("/nonexistent").is_some());
        reset_settings_cache();
        assert!(get_cached_parsed_file("/nonexistent").is_none());
    }

    #[test]
    fn plugin_base_roundtrip() {
        reset_settings_cache();
        assert!(get_plugin_settings_base().is_none());
        set_plugin_settings_base(Some(serde_json::json!({"agent": "x"})));
        assert_eq!(get_plugin_settings_base(), Some(serde_json::json!({"agent": "x"})));
        clear_plugin_settings_base();
        assert!(get_plugin_settings_base().is_none());
    }
}
