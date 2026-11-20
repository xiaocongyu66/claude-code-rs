//! mdm/settings.rs — MDM（托管）企业策略 settings 的解析与缓存。
//!
//! 重写自 `src/utils/settings/mdm/settings.ts`。
//!
//! 语义对齐：
//! - 「first source wins」优先级：remote → HKLM/plist → managed-settings.json
//!   → HKCU（remote 层在 settings.rs 的远程缓存中处理，本模块不涉及）；
//! - MDM 读取由 raw_read.rs 的同步子进程执行，`start_mdm_settings_load`
//!   首次执行一次并缓存（TS 为 async Promise；Rust 无运行时依赖，改同步）；
//! - `refresh_mdm_settings()` 现场重读、不写缓存（供变更检测 30 分钟轮询）；
//! - `parse_command_output_as_settings` 复用 settings/validation 的权限规则
//!   过滤 + schema 解析，非法 JSON 或 schema 失败 → 空 settings + 错误列表。
//!
//! 已知差异：TS 的 startupProfiler（profileCheckpoint）与 diagLogs
//! （logForDiagnosticsNoPII）尚未转 Rust，此处以 debug 日志近似/省略。

use std::path::Path;
use std::sync::Mutex;

use regex::Regex;
use serde_json::Value;

use crate::utils::settings::managed_path::{get_managed_file_path, get_managed_settings_drop_in_dir};
use crate::utils::settings::mdm::constants::{
    WINDOWS_REGISTRY_KEY_PATH_HKCU, WINDOWS_REGISTRY_KEY_PATH_HKLM, WINDOWS_REGISTRY_VALUE_NAME,
};
use crate::utils::settings::mdm::raw_read::{fire_raw_read, get_mdm_raw_read, RawReadResult};
use crate::utils::settings::types::{parse_settings_json, SettingsJson};
use crate::utils::settings::validation::{filter_invalid_permission_rules, format_zod_error, ValidationError};
use crate::utils::slow_operations::json_parse;
use crate::utils::json_read::strip_bom;

/// MDM 读取结果（对齐 TS `MdmResult`）。
#[derive(Debug, Clone, PartialEq)]
pub struct MdmResult {
    pub settings: SettingsJson,
    pub errors: Vec<ValidationError>,
}

fn empty_result() -> MdmResult {
    MdmResult {
        settings: SettingsJson::default(),
        errors: Vec::new(),
    }
}

struct MdmState {
    mdm_cache: Option<MdmResult>,
    hkcu_cache: Option<MdmResult>,
    load_done: bool,
}

static STATE: Mutex<Option<MdmState>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut MdmState) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(|| MdmState {
        mdm_cache: None,
        hkcu_cache: None,
        load_done: false,
    });
    f(state)
}

/// 启动 MDM/HKCU 读取（尽早调用，使子进程与模块加载并行——TS 语义；
/// Rust 为同步执行）。只执行一次。
pub fn start_mdm_settings_load() {
    with_state(|state| {
        if state.load_done {
            return;
        }
        state.load_done = true;
        // 优先消费 cli 启动时触发的 raw read，否则现场重读。
        let raw = get_mdm_raw_read().unwrap_or_else(fire_raw_read);
        let (mdm, hkcu) = consume_raw_read_result(&raw);
        state.mdm_cache = Some(mdm);
        state.hkcu_cache = Some(hkcu);
    });
}

/// 等待（必要时触发）MDM 加载完成；Rust 同步实现首次调用时执行 load。
pub fn ensure_mdm_settings_loaded() {
    start_mdm_settings_load();
}

/// 读取 admin-only MDM 设置（macOS /Library/Managed Preferences、Windows
/// HKLM）。不含 HKCU（用户可写层）。
pub fn get_mdm_settings() -> MdmResult {
    with_state(|s| s.mdm_cache.clone().unwrap_or_else(empty_result))
}

/// 读取 HKCU 注册表 settings（仅 Windows，最低策略优先级）。
pub fn get_hkcu_settings() -> MdmResult {
    with_state(|s| s.hkcu_cache.clone().unwrap_or_else(empty_result))
}

/// 清空 MDM/HKCU 缓存与加载标记（下次读取重新执行）。
pub fn clear_mdm_settings_cache() {
    with_state(|s| {
        s.mdm_cache = None;
        s.hkcu_cache = None;
        s.load_done = false;
    });
}

/// 直接更新会话缓存（变更检测轮询使用）。
pub fn set_mdm_settings_cache(mdm: MdmResult, hkcu: MdmResult) {
    with_state(|s| {
        s.mdm_cache = Some(mdm);
        s.hkcu_cache = Some(hkcu);
    });
}

/// 现场重读 MDM 子进程并解析；不更新缓存（调用方决定是否应用）。
pub fn refresh_mdm_settings() -> (MdmResult, MdmResult) {
    let raw = fire_raw_read();
    consume_raw_read_result(&raw)
}

/// 把 JSON 命令输出（plutil stdout 或注册表 JSON 值）解析为 settings。
/// 非法权限规则先过滤；schema 失败 → 空 settings + 错误。
pub fn parse_command_output_as_settings(stdout: &str, source_path: &str) -> MdmResult {
    let data: Value = match json_parse(&strip_bom(stdout)) {
        Ok(v) if v.is_object() => v,
        _ => return empty_result(),
    };
    let mut data = data;
    let rule_warnings = filter_invalid_permission_rules(&mut data, source_path);
    match parse_settings_json(&data) {
        Ok(settings) => MdmResult {
            settings,
            errors: rule_warnings,
        },
        Err(issues) => {
            let errors = format_zod_error(&issues, source_path);
            MdmResult {
                settings: SettingsJson::default(),
                errors: vec![rule_warnings, errors].concat(),
            }
        }
    }
}

/// 解析 reg query stdout，提取注册表字符串值（REG_SZ / REG_EXPAND_SZ，
/// 大小写不敏感）。预期行形如 `    Settings    REG_SZ    {...}`。
pub fn parse_reg_query_stdout(stdout: &str, value_name: &str) -> Option<String> {
    let escaped = regex::escape(value_name);
    let re = Regex::new(&format!(
       "(?i)^\\s+{escaped}\\s+REG_(?:EXPAND_)?SZ\\s+(.*)$"
    ))
    .ok()?;
    for line in stdout.split(|c| c == '\n' || c == '\r') {
        if let Some(caps) = re.captures(line) {
            if let Some(m) = caps.get(1) {
                return Some(m.as_str().trim_end().to_string());
            }
        }
    }
    None
}

/// 将原始子进程输出转换为 MDM / HKCU 结果（first source wins）。
fn consume_raw_read_result(raw: &RawReadResult) -> (MdmResult, MdmResult) {
    // macOS: plist 结果（raw_read 内已按优先级取第一个）
    if let Some(stdouts) = &raw.plist_stdouts {
        if let Some(first) = stdouts.first() {
            let result = parse_command_output_as_settings(&first.stdout, &first.label);
            if result.settings_has_keys() {
                return (result, empty_result());
            }
        }
    }

    // Windows: HKLM 结果
    if let Some(hklm) = &raw.hklm_stdout {
        if let Some(json_string) = parse_reg_query_stdout(hklm, WINDOWS_REGISTRY_VALUE_NAME) {
            let result = parse_command_output_as_settings(
                &json_string,
                &format!("Registry: {}\\{}", WINDOWS_REGISTRY_KEY_PATH_HKLM, WINDOWS_REGISTRY_VALUE_NAME),
            );
            if result.settings_has_keys() {
                return (result, empty_result());
            }
        }
    }

    // 无 admin MDM —— 若存在文件托管 settings，则不回退 HKCU
    if has_managed_settings_file() {
        return (empty_result(), empty_result());
    }

    // 回退 HKCU（已在 raw read 中并行读取）
    if let Some(hkcu) = &raw.hkcu_stdout {
        if let Some(json_string) = parse_reg_query_stdout(hkcu, WINDOWS_REGISTRY_VALUE_NAME) {
            let result = parse_command_output_as_settings(
                &json_string,
                &format!("Registry: {}\\{}", WINDOWS_REGISTRY_KEY_PATH_HKCU, WINDOWS_REGISTRY_VALUE_NAME),
            );
            return (empty_result(), result);
        }
    }

    (empty_result(), empty_result())
}

impl MdmResult {
    /// settings 是否含至少一个键（对齐 TS 的 `Object.keys(settings).length > 0`；
    /// SettingsJson 序列化时跳过 None 字段，故直接判空对象）。
    fn settings_has_keys(&self) -> bool {
        serde_json::to_value(&self.settings)
            .ok()
            .and_then(|v| v.as_object().map(|o| !o.is_empty()))
            .unwrap_or(false)
    }
}

/// 文件托管 settings（managed-settings.json 或 managed-settings.d/*.json）
/// 是否存在且有内容。用于在 HKCU 回退前短路。
fn has_managed_settings_file() -> bool {
    let base = Path::new(get_managed_file_path()).join("managed-settings.json");
    if let Ok(content) = std::fs::read_to_string(&base) {
        if let Ok(data) = json_parse(&strip_bom(&content)) {
            if let Some(obj) = data.as_object() {
                if obj.values().any(|v| !v.is_null()) {
                    return true;
                }
            }
        }
    }
    let drop_in = Path::new(get_managed_settings_drop_in_dir());
    let Ok(entries) = std::fs::read_dir(drop_in) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type();
        let is_file_or_link = ft
            .as_ref()
            .map(|t| t.is_file() || t.is_symlink())
            .unwrap_or(false);
        if !is_file_or_link {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if let Ok(data) = json_parse(&strip_bom(&content)) {
                if data.is_object()
                    && data.as_object().map(|o| o.values().any(|v| !v.is_null())).unwrap_or(false)
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reg_query_stdout() {
        let stdout = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\ClaudeCode\r\n    Settings    REG_SZ    {\"model\": \"claude-4\"}\r\n";
        assert_eq!(
            parse_reg_query_stdout(stdout, "Settings"),
            Some("{\"model\": \"claude-4\"}".to_string())
        );
        // 大小写不敏感 + REG_EXPAND_SZ
        let stdout = "    settings    REG_EXPAND_SZ    {\"a\":1}";
        assert_eq!(parse_reg_query_stdout(stdout, "Settings"), Some("{\"a\":1}".to_string()));
        assert!(parse_reg_query_stdout("nothing here", "Settings").is_none());
    }

    #[test]
    fn parse_command_output_roundtrip() {
        let result = parse_command_output_as_settings(
            "{\"model\": \"claude-4\", \"permissions\": {\"allow\": [\"Bash(x)\"]}}",
            "reg-test",
        );
        assert!(result.errors.is_empty());
        assert_eq!(result.settings.model.as_deref(), Some("claude-4"));

        // 无效 schema → 空 settings + 错误
        let result = parse_command_output_as_settings("{\"model\": 42}", "reg-test");
        assert!(result.settings.model.is_none());
        assert!(!result.errors.is_empty());

        // 非 JSON 对象 → 空
        let result = parse_command_output_as_settings("[]", "reg-test");
        assert!(result.errors.is_empty());
    }

    #[test]
    fn invalid_permission_rule_filtering() {
        let result = parse_command_output_as_settings(
            "{\"permissions\": {\"allow\": [\"bash(bad)\"]}}",
            "reg-test",
        );
        // 规则被过滤 → 解析成功，但保留警告
        assert!(result.settings.permissions.is_some());
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn caches_and_clears() {
        clear_mdm_settings_cache();
        start_mdm_settings_load();
        let _ = get_mdm_settings();
        clear_mdm_settings_cache();
    }
}
