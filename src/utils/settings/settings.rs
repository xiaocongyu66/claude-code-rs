//! settings.rs — settings 管线核心（加载、合并、缓存、写入）。
//!
//! 重写自 `src/utils/settings/settings.ts`。
//!
//! 语义对齐要点：
//! - 来源优先级（低 → 高）：userSettings → projectSettings → localSettings
//!   → flagSettings → policySettings；pluginSettingsBase 为最低基底层；
//! - policySettings 采用「first source wins」：remote > HKLM/plist >
//!   managed-settings.json(+drop-ins) > HKCU；
//! - 合并语义为 lodash `mergeWith` + `settingsMergeCustomizer`：数组拼接
//!   去重、对象深合并、标量后者覆盖；
//! - `updateSettingsForSource` 用 `Value::Null` 表达 TS 的 `undefined`
//!   删除语义（JSON 无法表达 undefined），数组永远整体替换；
//! - managed-settings.d/*.json 按字典序合并，后者覆盖前者。
//!
//! 已知差异（依赖模块尚未转 Rust，待下一波消除）：
//! - `profileCheckpoint`（startupProfiler）、`logForDiagnosticsNoPII`
//!   （diagLogs）省略——校验/加载逻辑不受影响；
//! - 远程托管 settings 缓存（services/remoteManagedSettings/syncCacheState.ts）
//!   以本模块内的最小缓存 + 磁盘读取实现（`<configHome>/remote-settings.json`，
//!   eligible 未决时不读）；
//! - `addFileGlobRuleToGitignore`（utils/git/gitignore.ts）以内联的最小
//!   实现近似（`.git` 存在检查 + 全局 ignore 文件追加）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::bootstrap::state::{
    get_flag_settings_inline, get_flag_settings_path, get_original_cwd, get_use_cowork_plugins,
    SettingSource,
};
use crate::utils::array::uniq;
use crate::utils::debug::log_for_debugging;
use crate::utils::debug::DebugLogLevel;
use crate::utils::env_utils::{get_claude_config_home_dir, is_env_truthy};
use crate::utils::errors::{get_errno_code, is_enoent};
use crate::utils::fs_operations::{get_fs_implementation, safe_resolve_path, BufferEncoding};
use crate::utils::json_read::strip_bom;
use crate::utils::log::log_error;
use crate::utils::platform::get_platform;
use crate::utils::settings::constants::get_enabled_setting_sources;
use crate::utils::settings::internal_writes::mark_internal_write;
use crate::utils::settings::managed_path::{
    get_managed_file_path, get_managed_settings_drop_in_dir,
};
use crate::utils::settings::mdm::settings::{
    get_hkcu_settings, get_mdm_settings, MdmResult,
};
use crate::utils::settings::settings_cache::{
    get_cached_parsed_file, get_cached_settings_for_source, get_plugin_settings_base,
    get_session_settings_cache, reset_settings_cache, set_cached_parsed_file,
    set_cached_settings_for_source, set_session_settings_cache, ParsedSettings,
};
use crate::utils::settings::types::{
    feature_enabled, known_top_level_keys, parse_settings_json, CompactionSettings, SettingsJson,
};
use crate::utils::settings::validation::{
    filter_invalid_permission_rules, format_zod_error, SettingsWithErrors, ValidationError,
};
use crate::utils::slow_operations::{clone_deep, json_parse, json_stringify_pretty};

// ---------------------------------------------------------------------------
// 远程托管 settings 缓存（syncCacheState.ts 的最小对应物）
// ---------------------------------------------------------------------------

const REMOTE_SETTINGS_FILENAME: &str = "remote-settings.json";

struct RemoteState {
    /// None = 资格未判定（对齐 TS 的 `eligible !== true` → null）。
    eligible: Option<bool>,
    cache: Option<Value>,
}

static REMOTE: Mutex<Option<RemoteState>> = Mutex::new(None);

/// 设置远程托管 settings 的资格（对齐 TS `setEligibility`）。managedEnv
/// 尚未转 Rust，默认保持未判定。
pub fn set_remote_settings_eligible(v: bool) -> bool {
    let mut guard = REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert(RemoteState {
        eligible: None,
        cache: None,
    });
    state.eligible = Some(v);
    v
}

/// 重置远程缓存（对齐 TS `resetSyncCache`）。
pub fn reset_remote_sync_cache() {
    let mut guard = REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        state.cache = None;
        state.eligible = None;
    }
}

pub fn set_remote_managed_settings_session_cache(value: Option<Value>) {
    let mut guard = REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert(RemoteState {
        eligible: None,
        cache: None,
    });
    state.cache = value;
}

fn get_remote_managed_settings_sync_from_cache() -> Option<Value> {
    let mut guard = REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert(RemoteState {
        eligible: None,
        cache: None,
    });
    if state.eligible != Some(true) {
        return None;
    }
    if state.cache.is_some() {
        return state.cache.clone();
    }
    // 首次：从磁盘加载 remote-settings.json（对齐 TS loadSettings）
    let cached = load_remote_settings_file();
    if cached.is_some() {
        state.cache = cached.clone();
        // 对齐 TS：首次可用时刷新 settings 缓存，让合并层重新读取
        reset_settings_cache();
    }
    cached
}

fn load_remote_settings_file() -> Option<Value> {
    let path = Path::new(&get_claude_config_home_dir()).join(REMOTE_SETTINGS_FILENAME);
    let content = std::fs::read_to_string(&path).ok()?;
    let data = json_parse(&strip_bom(&content)).ok()?;
    if data.is_object() {
        Some(data)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 托管 settings 文件（managed-settings.json + drop-ins）
// ---------------------------------------------------------------------------

/// managed-settings.json 的完整路径。
pub fn get_managed_settings_file_path() -> String {
    Path::new(get_managed_file_path())
        .join("managed-settings.json")
        .to_string_lossy()
        .to_string()
}

/// 加载文件托管 settings：managed-settings.json 先 merge（最低优先级），
/// managed-settings.d/*.json 按字典序 merge 在上（后者覆盖前者）。
pub fn load_managed_file_settings() -> (Option<SettingsJson>, Vec<ValidationError>) {
    let mut errors: Vec<ValidationError> = Vec::new();
    let mut merged: Value = Value::Object(Map::new());
    let mut found = false;

    let (base, base_errors) = parse_settings_file(&get_managed_settings_file_path());
    errors.extend(base_errors);
    if let Some(s) = base {
        if !settings_is_empty(&s) {
            merge_settings(&mut merged, &settings_to_value(&s));
            found = true;
        }
    }

    let drop_in_dir = get_managed_settings_drop_in_dir();
    let read = std::fs::read_dir(drop_in_dir);
    if let Ok(entries) = read {
        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let ft = entry.file_type();
            let is_file_or_link = ft
                .as_ref()
                .map(|t| t.is_file() || t.is_symlink())
                .unwrap_or(false);
            let name = entry.file_name().to_string_lossy().to_string();
            if is_file_or_link && name.ends_with(".json") && !name.starts_with('.') {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let path = Path::new(drop_in_dir).join(&name);
            let (settings, file_errors) = parse_settings_file(&path.to_string_lossy());
            errors.extend(file_errors);
            if let Some(s) = settings {
                if !settings_is_empty(&s) {
                    merge_settings(&mut merged, &settings_to_value(&s));
                    found = true;
                }
            }
        }
    } else if let Err(e) = read {
        let code = get_errno_code(&e);
        if code.as_deref() != Some("ENOENT") && code.as_deref() != Some("ENOTDIR") {
            log_error(e);
        }
    }

    if !found {
        return (None, errors);
    }
    match parse_settings_json(&merged) {
        Ok(s) => (Some(s), errors),
        Err(issues) => {
            errors.extend(format_zod_error(&issues, &get_managed_settings_file_path()));
            (None, errors)
        }
    }
}

/// 哪些文件托管 settings 来源存在（/status 显示用）。
pub fn get_managed_file_settings_presence() -> (bool, bool) {
    let (base, _) = parse_settings_file(&get_managed_settings_file_path());
    let has_base = base.map(|s| !settings_is_empty(&s)).unwrap_or(false);

    let mut has_drop_ins = false;
    let drop_in_dir = get_managed_settings_drop_in_dir();
    if let Ok(entries) = std::fs::read_dir(drop_in_dir) {
        has_drop_ins = entries.flatten().any(|entry| {
            let ft = entry.file_type();
            let is_file_or_link = ft
                .as_ref()
                .map(|t| t.is_file() || t.is_symlink())
                .unwrap_or(false);
            let name = entry.file_name().to_string_lossy().to_string();
            is_file_or_link && name.ends_with(".json") && !name.starts_with('.')
        });
    }
    (has_base, has_drop_ins)
}

/// settings 是否含至少一个键。
fn settings_is_empty(s: &SettingsJson) -> bool {
    let value = settings_to_value(s);
    value.as_object().map(|o| o.is_empty()).unwrap_or(true)
}

/// SettingsJson → Value（序列化时跳过 None 字段）。
fn settings_to_value(s: &SettingsJson) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Object(Map::new()))
}

// ---------------------------------------------------------------------------
// parseSettingsFile
// ---------------------------------------------------------------------------

/// 处理 settings 文件的文件系统错误：ENOENT（断裂符号链接/缺失）→ debug
/// 日志；其它 → logError。
fn handle_file_system_error(error: &dyn std::error::Error, path: &str) {
    if is_enoent(error) {
        log_for_debugging(
            &format!("Broken symlink or missing file encountered for settings.json at path: {path}"),
            DebugLogLevel::Debug,
        );
    } else {
        log_error(format!("{error}"));
    }
}

/// 解析 settings 文件（带按路径缓存；缓存命中时返回深拷贝，调用方可随意
/// 改动）。空文件 → 空 settings；非法 JSON / schema 失败 → None + 错误。
pub fn parse_settings_file(path: &str) -> ParsedSettings {
    if let Some(cached) = get_cached_parsed_file(path) {
        return ParsedSettings {
            settings: cached.settings.map(|s| clone_deep(&settings_to_value(&s))
                .and_then(|v| parse_settings_json(&v).ok())
                .unwrap_or_default()),
            errors: cached.errors,
        };
    }
    let result = parse_settings_file_uncached(path);
    set_cached_parsed_file(path, result.clone());
    // 首次返回也深拷贝——调用方可能在下一次缓存读取前改动结果
    ParsedSettings {
        settings: result
            .settings
            .map(|s| clone_deep(&settings_to_value(&s))
                .and_then(|v| parse_settings_json(&v).ok())
                .unwrap_or_default()),
        errors: result.errors,
    }
}

fn parse_settings_file_uncached(path: &str) -> ParsedSettings {
    let fs = get_fs_implementation();
    let resolved = safe_resolve_path(&fs, path);
    let content: String = match fs.read_file_sync(&resolved.resolved_path, BufferEncoding::Utf8) {
        Ok(c) => c,
        Err(e) => {
            handle_file_system_error(&e, path);
            return ParsedSettings {
                settings: None,
                errors: Vec::new(),
            };
        }
    };

    if content.trim().is_empty() {
        return ParsedSettings {
            settings: Some(SettingsJson::default()),
            errors: Vec::new(),
        };
    }

    // 非法 JSON → null，交由 schema 解析产生 'Invalid or malformed JSON'
    let mut data: Value = json_parse(&strip_bom(&content)).unwrap_or(Value::Null);

    // 先过滤非法权限规则，避免一条坏规则毒化整个文件
    let rule_warnings = filter_invalid_permission_rules(&mut data, path);

    match parse_settings_json(&data) {
        Ok(s) => ParsedSettings {
            settings: Some(s),
            errors: rule_warnings,
        },
        Err(issues) => ParsedSettings {
            settings: None,
            errors: vec![rule_warnings, format_zod_error(&issues, path)].concat(),
        },
    }
}

// ---------------------------------------------------------------------------
// 路径解析
// ---------------------------------------------------------------------------

/// 来源对应的文件根目录（$PROJ_DIR/.claude/settings.json → $PROJ_DIR）。
pub fn get_settings_root_path_for_source(source: SettingSource) -> String {
    match source {
        SettingSource::UserSettings => PathBuf::from(get_claude_config_home_dir())
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(get_claude_config_home_dir()))
            .to_string_lossy()
            .to_string(),
        SettingSource::PolicySettings | SettingSource::ProjectSettings | SettingSource::LocalSettings => {
            PathBuf::from(get_original_cwd())
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(get_original_cwd()))
                .to_string_lossy()
                .to_string()
        }
        SettingSource::FlagSettings => {
            if let Some(path) = get_flag_settings_path() {
                PathBuf::from(&path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&path))
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| get_original_cwd())
            } else {
                get_original_cwd()
            }
        }
    }
}

/// 用户 settings 文件名：cowork 模式下 cowork_settings.json，否则 settings.json。
fn get_user_settings_file_path() -> &'static str {
    if get_use_cowork_plugins()
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_COWORK_PLUGINS").ok().as_deref())
    {
        "cowork_settings.json"
    } else {
        "settings.json"
    }
}

/// 来源对应的 settings 文件路径（None = 无对应文件）。
pub fn get_settings_file_path_for_source(source: SettingSource) -> Option<String> {
    match source {
        SettingSource::UserSettings => Some(
            Path::new(&get_settings_root_path_for_source(source))
                .join(get_user_settings_file_path())
                .to_string_lossy()
                .to_string(),
        ),
        SettingSource::ProjectSettings | SettingSource::LocalSettings => Some(
            Path::new(&get_settings_root_path_for_source(source))
                .join(get_relative_settings_file_path_for_source(source))
                .to_string_lossy()
                .to_string(),
        ),
        SettingSource::PolicySettings => Some(get_managed_settings_file_path()),
        SettingSource::FlagSettings => get_flag_settings_path(),
    }
}

pub fn get_relative_settings_file_path_for_source(
    source: SettingSource,
) -> &'static str {
    match source {
        SettingSource::ProjectSettings => ".claude/settings.json",
        // TS 严格匹配 project/local；其它输入不可达（调用方保证）
        _ => ".claude/settings.local.json",
    }
}

// ---------------------------------------------------------------------------
// 按来源读取
// ---------------------------------------------------------------------------

/// 读取某来源的 settings（带按来源缓存）。
pub fn get_settings_for_source(source: SettingSource) -> Option<SettingsJson> {
    if let Some(cached) = get_cached_settings_for_source(source) {
        return cached;
    }
    let result = get_settings_for_source_uncached(source);
    set_cached_settings_for_source(source, result.clone());
    result
}

fn get_settings_for_source_uncached(source: SettingSource) -> Option<SettingsJson> {
    // policySettings：first source wins（remote > HKLM/plist > file > HKCU）
    if source == SettingSource::PolicySettings {
        let remote = get_remote_managed_settings_sync_from_cache();
        if let Some(remote_settings) = remote {
            if remote_settings.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                return parse_settings_json(&remote_settings).ok();
            }
        }

        let mdm_result: MdmResult = get_mdm_settings();
        if !settings_is_empty(&mdm_result.settings) {
            return Some(mdm_result.settings);
        }

        let (file_settings, _) = load_managed_file_settings();
        if let Some(file_settings) = file_settings {
            return Some(file_settings);
        }

        let hkcu = get_hkcu_settings();
        if !settings_is_empty(&hkcu.settings) {
            return Some(hkcu.settings);
        }

        return None;
    }

    let settings_file_path = get_settings_file_path_for_source(source);
    let file_settings = settings_file_path
        .as_deref()
        .and_then(|p| parse_settings_file(p).settings);

    // flagSettings：合并 SDK 设置的内联 settings
    if source == SettingSource::FlagSettings {
        if let Some(inline) = get_flag_settings_inline() {
            if let Ok(parsed) = parse_settings_json(&inline) {
                let mut base = file_settings
                    .as_ref()
                    .map(|s| settings_to_value(s))
                    .unwrap_or(Value::Object(Map::new()));
                merge_settings(&mut base, &settings_to_value(&parsed));
                return parse_settings_json(&base).ok();
            }
        }
    }

    file_settings
}

/// 最高优先级活跃策略来源（first source wins）。
pub fn get_policy_settings_origin() -> Option<&'static str> {
    let remote = get_remote_managed_settings_sync_from_cache();
    if let Some(remote_settings) = remote {
        if remote_settings.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            return Some("remote");
        }
    }

    let mdm_result = get_mdm_settings();
    if !settings_is_empty(&mdm_result.settings) {
        return match get_platform() {
            crate::utils::platform::Platform::Macos => Some("plist"),
            _ => Some("hklm"),
        };
    }

    let (file_settings, _) = load_managed_file_settings();
    if file_settings.is_some() {
        return Some("file");
    }

    let hkcu = get_hkcu_settings();
    if !settings_is_empty(&hkcu.settings) {
        return Some("hkcu");
    }

    None
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/// 把 `settings` 合并入 `source` 的现有 settings 后写入磁盘。
///
/// 删除语义：`settings` 中的 `Value::Null` 表示删除该键（TS 用 undefined）；
/// 数组整体替换（调用方负责计算最终状态）。policySettings / flagSettings
/// 只读，直接成功返回。
pub fn update_settings_for_source(source: SettingSource, settings: &Value) -> Result<(), String> {
    if source == SettingSource::PolicySettings || source == SettingSource::FlagSettings {
        return Ok(());
    }

    let Some(file_path) = get_settings_file_path_for_source(source) else {
        return Ok(());
    };

    let result = (|| -> Result<(), String> {
        // 创建目录
        if let Some(parent) = Path::new(&file_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // 绕过按来源缓存直接读盘：merge 会就地改动目标
        let mut existing = get_settings_for_source_uncached(source);

        // 校验失败时检查是否存在 JSON 语法错误
        if existing.is_none() {
            let read = std::fs::read(&file_path);
            match read {
                Ok(bytes) => {
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    match json_parse(&content) {
                        Ok(raw) if raw.is_object() => {
                            log_for_debugging(
                                &format!("Using raw settings from {file_path} due to validation failure"),
                                DebugLogLevel::Debug,
                            );
                            // 直接用原始 JSON 作 base（绕过 schema 校验）
                            let mut base = raw;
                            merge_settings_with_delete(&mut base, settings);
                            write_settings(&file_path, &base)?;
                            after_write(source, &file_path)?;
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(_) => {
                            return Err(format!(
                                "Invalid JSON syntax in settings file at {file_path}"
                            ));
                        }
                    }
                }
                Err(e) => {
                    if !is_enoent(&e) {
                        return Err(format!("Failed to read raw settings from {file_path}: {e}"));
                    }
                    // 文件不存在 → 与空 settings 合并
                }
            }
        }

        let mut base = existing
            .map(|s| settings_to_value(&s))
            .unwrap_or(Value::Object(Map::new()));
        merge_settings_with_delete(&mut base, settings);
        write_settings(&file_path, &base)?;
        after_write(source, &file_path)?;
        Ok(())
    })();

    if let Err(e) = &result {
        log_error(format!("Failed to update settings for {file_path}: {e}"));
    }
    result
}

fn write_settings(file_path: &str, value: &Value) -> Result<(), String> {
    let json = json_stringify_pretty(value, 2) + "\n";
    // 标记为内部写入，使变更检测器忽略自身的回声
    mark_internal_write(file_path);
    crate::utils::slow_operations::write_file_sync_deprecated(file_path, json.as_bytes(), true)
        .map_err(|e| format!("Failed to write settings file {file_path}: {e}"))
}

fn after_write(source: SettingSource, file_path: &str) -> Result<(), String> {
    // 使会话缓存失效
    reset_settings_cache();
    if source == SettingSource::LocalSettings {
        // local settings 加入 .gitignore（异步、容错；对齐 TS 的 fire-and-forget）
        let rel = get_relative_settings_file_path_for_source(SettingSource::LocalSettings);
        add_file_glob_rule_to_gitignore(rel, &get_original_cwd());
    }
    let _ = file_path;
    Ok(())
}

/// lodash mergeWith + settingsMergeCustomizer：数组拼接去重，对象深合并，
/// 标量后者覆盖（loadSettingsFromDisk / flagSettings 合并用）。
pub fn merge_settings(base: &mut Value, src: &Value) {
    match (base, src) {
        (Value::Object(b), Value::Object(s)) => {
            for (k, sv) in s {
                if let Some(bv) = b.get_mut(k) {
                    if bv.is_array() && sv.is_array() {
                        // 两个数组 → 拼接去重
                        let merged = merge_arrays(bv, sv);
                        *bv = merged;
                    } else if bv.is_object() && sv.is_object() {
                        merge_settings(bv, sv);
                    } else {
                        *bv = clone_deep(sv);
                    }
                } else {
                    b.insert(k.clone(), clone_deep(sv));
                }
            }
        }
        (b, s) => {
            *b = clone_deep(s);
        }
    }
}

/// updateSettingsForSource 的合并：`Null` 删除键；数组整体替换；对象深合并。
pub fn merge_settings_with_delete(base: &mut Value, src: &Value) {
    match (base, src) {
        (Value::Object(b), Value::Object(s)) => {
            for (k, sv) in s {
                if matches!(sv, Value::Null) {
                    b.remove(k);
                    continue;
                }
                if let Some(bv) = b.get_mut(k) {
                    if sv.is_array() {
                        // 数组永远替换（调用方计算最终状态）
                        *bv = clone_deep(sv);
                    } else if bv.is_object() && sv.is_object() {
                        merge_settings_with_delete(bv, sv);
                    } else {
                        *bv = clone_deep(sv);
                    }
                } else {
                    b.insert(k.clone(), clone_deep(sv));
                }
            }
        }
        (b, s) => {
            *b = clone_deep(s);
        }
    }
}

fn merge_arrays(a: &Value, b: &Value) -> Value {
    let mut items: Vec<Value> = Vec::new();
    if let Some(arr) = a.as_array() {
        items.extend(arr.iter().cloned());
    }
    if let Some(arr) = b.as_array() {
        items.extend(arr.iter().cloned());
    }
    Value::Array(uniq(items))
}

/// local settings 写入后把 `**/<filename>` 加入全局 gitignore（对齐
/// utils/git/gitignore.ts 的关键路径；git 仓库检测以 .git 存在近似，
/// 已被 ignore 时不重复添加）。
fn add_file_glob_rule_to_gitignore(filename: &str, cwd: &str) {
    let gitignore_entry = format!("**/{filename}");
    let global_gitignore_path = global_gitignore_path();
    // 不在 git 仓库 → 跳过
    if !is_in_git_repo(cwd) {
        return;
    }
    // 已存在相同条目 → 不重复添加
    if let Ok(content) = std::fs::read_to_string(&global_gitignore_path) {
        if content.contains(&gitignore_entry) {
            return;
        }
    }
    if let Some(parent) = Path::new(&global_gitignore_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let result = match std::fs::read_to_string(&global_gitignore_path) {
        Ok(content) => std::fs::write(&global_gitignore_path, format!("{content}\n{gitignore_entry}\n")),
        Err(e) if is_enoent(&e) => std::fs::write(&global_gitignore_path, format!("{gitignore_entry}\n")),
        Err(e) => {
            log_for_debugging(
                &format!("Failed to update global gitignore: {e}"),
                DebugLogLevel::Debug,
            );
            return;
        }
    };
    if let Err(e) = result {
        log_for_debugging(
            &format!("Failed to write global gitignore: {e}"),
            DebugLogLevel::Debug,
        );
    }
}

/// 全局 gitignore 路径（XDG_CONFIG_HOME/git/ignore 或 ~/.config/git/ignore）。
fn global_gitignore_path() -> String {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty());
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty());
    let base = xdg
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("git").join("ignore").to_string_lossy().to_string()
}

/// cwd 是否在 git 仓库内（向上查找 .git；gitignore.ts 的近似）。
fn is_in_git_repo(cwd: &str) -> bool {
    let mut current = PathBuf::from(cwd);
    loop {
        if current.join(".git").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// 托管 settings 键清单（日志用）
// ---------------------------------------------------------------------------

/// 列出托管 settings 的键（供日志脱敏展示）。permissions/sandbox/hooks
/// 展开一层嵌套键，其它仅返回顶层键；结果排序。
pub fn get_managed_settings_keys_for_logging(settings: &SettingsJson) -> Vec<String> {
    // 只保留 schema 已知键（对齐 TS 的 SettingsSchema().strip()）
    let value = settings_to_value(settings);
    let valid = {
        let mut stripped = Map::new();
        let known: HashSet<&str> = known_top_level_keys().into_iter().collect();
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                if known.contains(k.as_str()) {
                    stripped.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(stripped)
    };

    let keys_to_expand = ["permissions", "sandbox", "hooks"];
    let mut all_keys: Vec<String> = Vec::new();

    let permissions_nested: Vec<String> = {
        let mut v = vec![
            "allow".to_string(),
            "deny".to_string(),
            "ask".to_string(),
            "defaultMode".to_string(),
            "disableBypassPermissionsMode".to_string(),
            "additionalDirectories".to_string(),
        ];
        if feature_enabled("TRANSCRIPT_CLASSIFIER") {
            v.push("disableAutoMode".to_string());
        }
        v
    };
    let sandbox_nested: Vec<String> = vec![
        "enabled".to_string(),
        "failIfUnavailable".to_string(),
        "allowUnsandboxedCommands".to_string(),
        "network".to_string(),
        "filesystem".to_string(),
        "ignoreViolations".to_string(),
        "excludedCommands".to_string(),
        "autoAllowBashIfSandboxed".to_string(),
        "enableWeakerNestedSandbox".to_string(),
        "enableWeakerNetworkIsolation".to_string(),
        "ripgrep".to_string(),
    ];
    let hooks_nested: Vec<String> = vec![
        "PreToolUse".to_string(),
        "PostToolUse".to_string(),
        "Notification".to_string(),
        "UserPromptSubmit".to_string(),
        "SessionStart".to_string(),
        "SessionEnd".to_string(),
        "Stop".to_string(),
        "SubagentStop".to_string(),
        "PreCompact".to_string(),
        "PostCompact".to_string(),
        "TeammateIdle".to_string(),
        "TaskCreated".to_string(),
        "TaskCompleted".to_string(),
    ];

    let obj = valid.as_object();
    if let Some(obj) = obj {
        for key in obj.keys() {
            if keys_to_expand.contains(&key.as_str()) {
                if let Some(nested) = obj.get(key).and_then(|v| v.as_object()) {
                    let valid_keys: &Vec<String> = match key.as_str() {
                        "permissions" => &permissions_nested,
                        "sandbox" => &sandbox_nested,
                        "hooks" => &hooks_nested,
                        _ => continue,
                    };
                    for nested_key in nested.keys() {
                        if valid_keys.iter().any(|k| k == nested_key) {
                            all_keys.push(format!("{key}.{nested_key}"));
                        }
                    }
                }
            } else {
                all_keys.push(key.clone());
            }
        }
    }
    all_keys.sort();
    all_keys
}

// ---------------------------------------------------------------------------
// 全量加载
// ---------------------------------------------------------------------------

static IS_LOADING_SETTINGS: AtomicBool = AtomicBool::new(false);

/// 从磁盘加载全部来源（不复用缓存；带重入保护）。
fn load_settings_from_disk() -> SettingsWithErrors {
    if IS_LOADING_SETTINGS.swap(true, Ordering::SeqCst) {
        return SettingsWithErrors {
            settings: SettingsJson::default(),
            errors: Vec::new(),
        };
    }

    let result = (|| {
        // 最底层：插件 settings（只含 allowlist 键）
        let mut merged_settings: Value = Value::Object(Map::new());
        if let Some(plugin_settings) = get_plugin_settings_base() {
            merge_settings(&mut merged_settings, &plugin_settings);
        }
        let mut all_errors: Vec<ValidationError> = Vec::new();
        let mut seen_errors: HashSet<String> = HashSet::new();
        let mut seen_files: HashSet<String> = HashSet::new();

        for source in get_enabled_setting_sources() {
            // policySettings：first source wins
            if source == SettingSource::PolicySettings {
                let mut policy_settings: Option<SettingsJson> = None;
                let mut policy_errors: Vec<ValidationError> = Vec::new();

                // 1. remote（最高）
                if let Some(remote_settings) = get_remote_managed_settings_sync_from_cache() {
                    if remote_settings.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                        match parse_settings_json(&remote_settings) {
                            Ok(s) => policy_settings = Some(s),
                            Err(issues) => policy_errors
                                .extend(format_zod_error(&issues, "remote managed settings")),
                        }
                    }
                }

                // 2. admin-only MDM（HKLM / plist）
                if policy_settings.is_none() {
                    let mdm_result = get_mdm_settings();
                    if !settings_is_empty(&mdm_result.settings) {
                        policy_settings = Some(mdm_result.settings);
                    }
                    policy_errors.extend(mdm_result.errors);
                }

                // 3. managed-settings.json + drop-ins
                if policy_settings.is_none() {
                    let (settings, errors) = load_managed_file_settings();
                    if let Some(s) = settings {
                        policy_settings = Some(s);
                    }
                    policy_errors.extend(errors);
                }

                // 4. HKCU
                if policy_settings.is_none() {
                    let hkcu = get_hkcu_settings();
                    if !settings_is_empty(&hkcu.settings) {
                        policy_settings = Some(hkcu.settings);
                    }
                    policy_errors.extend(hkcu.errors);
                }

                if let Some(p) = policy_settings {
                    merge_settings(&mut merged_settings, &settings_to_value(&p));
                }
                for error in policy_errors {
                    let error_key = format!(
                        "{}:{}:{}",
                        error.file.as_deref().unwrap_or(""),
                        error.path,
                        error.message
                    );
                    if seen_errors.insert(error_key) {
                        all_errors.push(error);
                    }
                }
                continue;
            }

            if let Some(file_path) = get_settings_file_path_for_source(source) {
                let resolved = PathBuf::from(&file_path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&file_path))
                    .to_string_lossy()
                    .to_string();
                if seen_files.insert(resolved.clone()) {
                    let (settings, errors) = parse_settings_file(file_path.as_str());
                    for error in errors {
                        let error_key = format!(
                            "{}:{}:{}",
                            error.file.as_deref().unwrap_or(""),
                            error.path,
                            error.message
                        );
                        if seen_errors.insert(error_key) {
                            all_errors.push(error);
                        }
                    }
                    if let Some(s) = settings {
                        merge_settings(&mut merged_settings, &settings_to_value(&s));
                    }
                }
            }

            // flagSettings：合并 SDK 内联 settings
            if source == SettingSource::FlagSettings {
                if let Some(inline) = get_flag_settings_inline() {
                    if let Ok(parsed) = parse_settings_json(&inline) {
                        merge_settings(&mut merged_settings, &settings_to_value(&parsed));
                    }
                }
            }
        }

        let merged = parse_settings_json(&merged_settings).unwrap_or_default();
        SettingsWithErrors {
            settings: merged,
            errors: all_errors,
        }
    })();

    IS_LOADING_SETTINGS.store(false, Ordering::SeqCst);
    result
}

/// 合并全部来源的 settings（快照语义；会话级缓存）。
pub fn get_initial_settings() -> SettingsJson {
    let with_errors = get_settings_with_errors();
    with_errors.settings
}

/// 用户配置的 V2 compaction 策略（无来源定义时为 None）。
pub fn get_compaction_settings() -> Option<CompactionSettings> {
    get_initial_settings().compaction
}

/// sources 明细 + 合并结果（始终现读磁盘）。
pub struct SettingsWithSources {
    pub effective: SettingsJson,
    /// 低 → 高优先级。
    pub sources: Vec<(SettingSource, SettingsJson)>,
}

pub fn get_settings_with_sources() -> SettingsWithSources {
    // 重置缓存，让 per-source 与合并结果一致
    reset_settings_cache();
    let mut sources: Vec<(SettingSource, SettingsJson)> = Vec::new();
    for source in get_enabled_setting_sources() {
        if let Some(settings) = get_settings_for_source(source) {
            if !settings_is_empty(&settings) {
                sources.push((source, settings));
            }
        }
    }
    SettingsWithSources {
        effective: get_initial_settings(),
        sources,
    }
}

/// 合并的 settings + 全部校验错误（会话级缓存）。
pub fn get_settings_with_errors() -> SettingsWithErrors {
    if let Some(cached) = get_session_settings_cache() {
        return cached;
    }
    let result = load_settings_from_disk();
    set_session_settings_cache(result.clone());
    result
}

/// 任何可信来源（排除 projectSettings）是否已接受 bypass-permissions 对话框。
pub fn has_skip_dangerous_mode_permission_prompt() -> bool {
    [SettingSource::UserSettings, SettingSource::LocalSettings, SettingSource::FlagSettings, SettingSource::PolicySettings]
        .iter()
        .any(|s| {
            get_settings_for_source(*s)
                .and_then(|settings| settings.skip_dangerous_mode_permission_prompt)
                .unwrap_or(false)
        })
}

/// auto mode 对所有用户可用，无需 opt-in（对齐 TS 常量 true）。
pub fn has_auto_mode_opt_in() -> bool {
    true
}

/// plan mode 是否使用 auto mode 语义（默认 true；任一可信来源显式 false 则
/// 关闭）。projectSettings 被排除。
pub fn get_use_auto_mode_during_plan() -> bool {
    if !feature_enabled("TRANSCRIPT_CLASSIFIER") {
        return true;
    }
    [SettingSource::PolicySettings, SettingSource::FlagSettings, SettingSource::UserSettings, SettingSource::LocalSettings]
        .iter()
        .all(|s| {
            get_settings_for_source(*s)
                .and_then(|settings| settings.use_auto_mode_during_plan)
                .unwrap_or(true)
        })
}

/// autoMode 配置（allow / soft_deny / environment 数组拼接；ant 用户额外
/// 合并 deny 兼容别名）。无内容时返回 None。
pub fn get_auto_mode_config() -> Option<AutoModeConfig> {
    if !feature_enabled("TRANSCRIPT_CLASSIFIER") {
        return None;
    }

    let mut allow: Vec<String> = Vec::new();
    let mut soft_deny: Vec<String> = Vec::new();
    let mut environment: Vec<String> = Vec::new();

    for source in [
        SettingSource::UserSettings,
        SettingSource::LocalSettings,
        SettingSource::FlagSettings,
        SettingSource::PolicySettings,
    ] {
        let Some(settings) = get_settings_for_source(source) else {
            continue;
        };
        let Some(auto_mode) = settings.auto_mode else {
            continue;
        };
        if let Some(a) = auto_mode.allow {
            allow.extend(a);
        }
        if let Some(d) = auto_mode.soft_deny {
            soft_deny.extend(d);
        }
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            if let Some(d) = auto_mode.deny {
                soft_deny.extend(d);
            }
        }
        if let Some(e) = auto_mode.environment {
            environment.extend(e);
        }
    }

    if allow.is_empty() && soft_deny.is_empty() && environment.is_empty() {
        return None;
    }
    Some(AutoModeConfig {
        allow: if allow.is_empty() { None } else { Some(allow) },
        soft_deny: if soft_deny.is_empty() { None } else { Some(soft_deny) },
        environment: if environment.is_empty() { None } else { Some(environment) },
    })
}

/// autoMode 合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeConfig {
    pub allow: Option<Vec<String>>,
    pub soft_deny: Option<Vec<String>>,
    pub environment: Option<Vec<String>>,
}

/// 任何原始 settings 文件（不含 policy）是否含某键——用于在校验失败时仍能
/// 探测用户意图。
pub fn raw_settings_contains_key(key: &str) -> bool {
    for source in get_enabled_setting_sources() {
        if source == SettingSource::PolicySettings {
            continue;
        }
        let Some(file_path) = get_settings_file_path_for_source(source) else {
            continue;
        };
        let fs = get_fs_implementation();
        let resolved = safe_resolve_path(&fs, &file_path);
        let content: String = match fs.read_file_sync(&resolved.resolved_path, BufferEncoding::Utf8) {
            Ok(c) => c,
            Err(e) => {
                handle_file_system_error(&e, &file_path);
                continue;
            }
        };
        if content.trim().is_empty() {
            continue;
        }
        let raw = json_parse(&strip_bom(&content)).unwrap_or(Value::Null);
        if raw.is_object()
            && raw
                .as_object()
                .map(|o| o.contains_key(key))
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_settings_concats_and_dedups_arrays() {
        let mut base = json!({
            "permissions": {"allow": ["Bash(a)"]},
            "models": ["m1"],
            "scalar": "old"
        });
        merge_settings(&mut base, &json!({
            "permissions": {"allow": ["Bash(b)", "Bash(a)"], "deny": ["x"]},
            "models": ["m2"],
            "scalar": "new"
        }));
        let perms = &base["permissions"];
        assert_eq!(
            perms["allow"].as_array().unwrap(),
            &vec![json!("Bash(a)"), json!("Bash(b)")]
        );
        assert_eq!(perms["deny"].as_array().unwrap(), &vec![json!("x")]);
        assert_eq!(base["models"].as_array().unwrap(), &vec![json!("m1"), json!("m2")]);
        assert_eq!(base["scalar"], "new");
    }

    #[test]
    fn merge_with_delete_and_array_replace() {
        let mut base = json!({
            "keep": 1,
            "drop": 2,
            "list": [1, 2]
        });
        merge_settings_with_delete(&mut base, &json!({"drop": null, "list": [3]}));
        let obj = base.as_object().unwrap();
        assert!(!obj.contains_key("drop"));
        assert_eq!(base["list"].as_array().unwrap(), &vec![json!(3)]);
    }

    #[test]
    fn managed_path_helpers() {
        assert!(get_managed_settings_file_path().ends_with("managed-settings.json"));
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            get_relative_settings_file_path_for_source(SettingSource::ProjectSettings),
            ".claude/settings.json"
        );
        assert_eq!(
            get_relative_settings_file_path_for_source(SettingSource::LocalSettings),
            ".claude/settings.local.json"
        );
    }

    #[test]
    fn keys_for_logging_expand_nested() {
        let mut settings = SettingsJson::default();
        settings.model = Some("claude-4".to_string());
        let value = json!({"allow": ["Bash(x)"], "unknown": 1});
        settings.permissions = Some(crate::utils::settings::types::PermissionsSettings {
            allow: Some(vec!["Bash(x)".to_string()]),
            deny: None,
            ask: None,
            default_mode: None,
            disable_bypass_permissions_mode: None,
            disable_auto_mode: None,
            additional_directories: None,
            extra: {
                let mut m = Map::new();
                m.insert("unknown".to_string(), json!(1));
                m
            },
        });
        let keys = get_managed_settings_keys_for_logging(&settings);
        assert!(keys.contains(&"model".to_string()));
        assert!(keys.contains(&"permissions.allow".to_string()));
        // 未知嵌套键不展开
        assert!(!keys.contains(&"permissions.unknown".to_string()));
        assert_eq!(keys, {
            let mut sorted = keys.clone();
            sorted.sort();
            sorted
        });
    }

    #[test]
    fn remote_cache_defaults_none_when_not_eligible() {
        assert!(get_remote_managed_settings_sync_from_cache().is_none());
    }
}
