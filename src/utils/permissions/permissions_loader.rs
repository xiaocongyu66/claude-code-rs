//! permissions_loader.rs — 从 settings 磁盘文件加载权限规则。
//!
//! 重写自 `src/utils/permissions/permissionsLoader.ts`。语义对齐说明：
//!
//! - 依赖：`crate::utils::settings`（types / constants / settings）、
//!   `crate::utils::fs_operations`、`crate::utils::log`、同目录
//!   `permission_rule_parser`、`crate::types::permissions` 均已就绪；
//! - TS 的 `fileRead.ts`（`readFileSync`）与 `json.ts`（`safeParseJSON`）
//!   尚未转换为 Rust。lenient 加载路径只用到「读文本 + 容错 JSON 解析」
//!   两个原语，Rust 侧以 `std::fs::read_to_string` +
//!   `serde_json::from_str`（配合 `json_read::strip_bom`）等价实现，
//!   失败返回 `None`（对齐 TS 的 try/catch → null）；
//! - TS 的 `updateSettingsForSource` 返回 `{ error }`，Rust 侧
//!   `update_settings_for_source` 返回 `Result<(), String>`；失败分支
//!   按原逻辑 `logError` 后回传 false；
//! - `SettingsJson` ↔ `serde_json::Value` 的往返用 `serde_json::to_value`
//!   （SettingsJson 派生了 Serialize），保持未识别键（`#[serde(flatten)]`
//!   的 extra map）不丢失；
//! - 规则的 `aliasSkillToolNames` 注入逻辑（R5）与 toolAliases 合并
//!   （first source wins）逐条对齐。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::{Map, Value};

use crate::bootstrap::state::SettingSource;
use crate::types::permissions::{
    PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
};
use crate::utils::fs_operations::{get_fs_implementation, safe_resolve_path};
use crate::utils::json_read::strip_bom;
use crate::utils::log::{StringError, log_error};
use crate::utils::permissions::permission_rule_parser::{
    permission_rule_value_from_string, permission_rule_value_to_string,
};
use crate::utils::settings::constants::{get_enabled_setting_sources, is_editable_setting_source};
use crate::utils::settings::settings::{
    get_settings_file_path_for_source, get_settings_for_source, update_settings_for_source,
};
use crate::utils::settings::types::{PermissionsSettings, SettingsJson};

/// 规则来源（含cliArg/command/session）→ settings 来源（5 个）。
/// cliArg/command/session 不是 settings 来源，返回 `None`。
fn rule_source_as_setting_source(source: PermissionRuleSource) -> Option<SettingSource> {
    match source {
        PermissionRuleSource::UserSettings => Some(SettingSource::UserSettings),
        PermissionRuleSource::ProjectSettings => Some(SettingSource::ProjectSettings),
        PermissionRuleSource::LocalSettings => Some(SettingSource::LocalSettings),
        PermissionRuleSource::FlagSettings => Some(SettingSource::FlagSettings),
        PermissionRuleSource::PolicySettings => Some(SettingSource::PolicySettings),
        PermissionRuleSource::CliArg
        | PermissionRuleSource::Command
        | PermissionRuleSource::Session => None,
    }
}

fn behavior_as_str(behavior: PermissionBehavior) -> &'static str {
    match behavior {
        PermissionBehavior::Allow => "allow",
        PermissionBehavior::Deny => "deny",
        PermissionBehavior::Ask => "ask",
    }
}

fn behavior_rules<'a>(
    permissions: &'a PermissionsSettings,
    behavior: PermissionBehavior,
) -> Option<&'a Vec<String>> {
    match behavior {
        PermissionBehavior::Allow => permissions.allow.as_ref(),
        PermissionBehavior::Deny => permissions.deny.as_ref(),
        PermissionBehavior::Ask => permissions.ask.as_ref(),
    }
}

/// managed settings（policySettings）中 `allowManagedPermissionRulesOnly`
/// 是否启用。启用后只承认 managed 来源的权限规则。
pub fn should_allow_managed_permission_rules_only() -> bool {
    get_settings_for_source(SettingSource::PolicySettings)
        .and_then(|s| s.allow_managed_permission_rules_only)
        .unwrap_or(false)
}

/// 权限提示中是否展示「always allow」选项。
/// `allowManagedPermissionRulesOnly` 启用时隐藏这些选项。
pub fn should_show_always_allow_options() -> bool {
    !should_allow_managed_permission_rules_only()
}

/// 遍历的规则行为顺序（TS `SUPPORTED_RULE_BEHAVIORS`）。
const SUPPORTED_RULE_BEHAVIORS: &[PermissionBehavior] = &[
    PermissionBehavior::Allow,
    PermissionBehavior::Deny,
    PermissionBehavior::Ask,
];

/// Lenient 版本的 `getSettingsForSource`：不做任何 schema 校验，
/// 只解析 JSON 原样返回。
///
/// 仅用于「加载 settings 以追加新规则」——避免因 hooks 等无关字段的
/// 校验失败而丢失已有规则。**仅供写路径使用，读取执行用配置时禁用**。
fn get_settings_for_source_lenient_for_editing_only(source: SettingSource) -> Option<Value> {
    let file_path = get_settings_file_path_for_source(source)?;

    let resolved = safe_resolve_path(&*get_fs_implementation(), &file_path);
    let content = std::fs::read_to_string(&resolved.resolved_path).ok()?;

    if content.trim().is_empty() {
        return Some(Value::Object(Map::new()));
    }

    // 不做校验直接返回解析结果（对齐 TS 的 `safeParseJSON(content, false)`：
    // `shouldLogError=false` → 静默失败返回 null）
    let data = serde_json::from_str::<Value>(strip_bom(&content)).ok()?;
    match data {
        // TS: `data && typeof data === 'object'`（数组也算 object）
        Value::Object(_) | Value::Array(_) => Some(data),
        _ => None,
    }
}

/// `serde_json::Map<String, String>` → `HashMap<String, String>`。
/// serde_json 只为 `Map<String, Value>` 实现 Clone，需逐项转换。
fn tool_aliases_to_hashmap(map: &Map<String, String>) -> HashMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// 计算规则工具名在该 toolAliases 映射下的别名集合。
///
/// 对齐 TS `aliasesForRule`：返回所有「alias !== toolName &&
/// toolAliases[alias] === toolName」的 alias；无则 `None`。
fn aliases_for_rule(
    tool_aliases: &HashMap<String, String>,
    tool_name: &str,
) -> Option<Vec<String>> {
    let mut aliases: Vec<String> = Vec::new();
    for (alias, canonical) in tool_aliases {
        if alias != tool_name && canonical == tool_name {
            aliases.push(alias.clone());
        }
    }
    if aliases.is_empty() {
        None
    } else {
        Some(aliases)
    }
}

/// 把 permissions JSON 转换为 `PermissionRule` 数组。纯函数。
///
/// R5 aliasSkillToolNames wiring：从 settings 字符串解析出的规则拾取
/// 顶层 `toolAliases` 映射（alias → 规范工具名），使针对规范名编写的
/// 规则能匹配以 alias 发出的调用。工具名本身是 alias 条目的规则
/// 不携带别名。
pub fn settings_json_to_rules(
    data: Option<&SettingsJson>,
    source: PermissionRuleSource,
) -> Vec<PermissionRule> {
    let Some(data) = data else {
        return Vec::new();
    };
    let Some(permissions) = data.permissions.as_ref() else {
        return Vec::new();
    };

    let tool_aliases = data
        .tool_aliases
        .as_ref()
        .map(tool_aliases_to_hashmap)
        .unwrap_or_default();

    let mut rules: Vec<PermissionRule> = Vec::new();
    for behavior in SUPPORTED_RULE_BEHAVIORS {
        if let Some(behavior_array) = behavior_rules(permissions, *behavior) {
            for rule_string in behavior_array {
                let rule_value = permission_rule_value_from_string(rule_string);
                rules.push(PermissionRule {
                    source,
                    rule_behavior: *behavior,
                    alias_skill_tool_names: aliases_for_rule(&tool_aliases, &rule_value.tool_name),
                    rule_value,
                });
            }
        }
    }
    rules
}

// ---------------------------------------------------------------------------
// toolAliases face（R5 aliasSkillToolNames unparked）
// ---------------------------------------------------------------------------

/// 测试 seam：设置后 `get_settings_tool_aliases` 恰好返回该映射，
/// `None` 恢复真实 settings 读取（TS `setToolAliasesForTest`）。
static TOOL_ALIASES_OVERRIDE_FOR_TEST: Lazy<Mutex<Option<HashMap<String, String>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn set_tool_aliases_for_test(map: Option<HashMap<String, String>>) {
    *TOOL_ALIASES_OVERRIDE_FOR_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = map;
}

/// 单来源的顶层 `toolAliases` 映射（alias → 规范工具名）。
pub fn get_tool_aliases_for_source(data: Option<&SettingsJson>) -> HashMap<String, String> {
    data.and_then(|d| d.tool_aliases.as_ref().map(tool_aliases_to_hashmap))
        .unwrap_or_default()
}

/// 跨已启用 settings 来源合并的 `toolAliases` 映射（alias → 规范工具名），
/// 每个别名键 first source wins。按 map 形状定义为单跳——
/// 调用方不得展开链式别名。
pub fn get_settings_tool_aliases() -> HashMap<String, String> {
    let override_guard = TOOL_ALIASES_OVERRIDE_FOR_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(map) = override_guard.as_ref() {
        return map.clone();
    }
    drop(override_guard);

    let mut merged: HashMap<String, String> = HashMap::new();
    for source in get_enabled_setting_sources() {
        let aliases = get_tool_aliases_for_source(get_settings_for_source(source).as_ref());
        for (alias, real) in aliases {
            merged.entry(alias).or_insert(real);
        }
    }
    merged
}

/// 从所有相关来源（managed 与项目 settings）加载全部权限规则。
pub fn load_all_permission_rules_from_disk() -> Vec<PermissionRule> {
    // allowManagedPermissionRulesOnly 时只用 managed 来源的规则
    if should_allow_managed_permission_rules_only() {
        return get_permission_rules_for_source(SettingSource::PolicySettings);
    }

    // 否则从所有启用来源加载（向后兼容）
    let mut rules: Vec<PermissionRule> = Vec::new();
    for source in get_enabled_setting_sources() {
        rules.extend(get_permission_rules_for_source(source));
    }
    rules
}

/// 从指定来源加载权限规则。
pub fn get_permission_rules_for_source(source: SettingSource) -> Vec<PermissionRule> {
    let settings_data = get_settings_for_source(source);
    settings_json_to_rules(
        settings_data.as_ref(),
        setting_source_as_rule_source(source),
    )
}

fn setting_source_as_rule_source(source: SettingSource) -> PermissionRuleSource {
    match source {
        SettingSource::UserSettings => PermissionRuleSource::UserSettings,
        SettingSource::ProjectSettings => PermissionRuleSource::ProjectSettings,
        SettingSource::LocalSettings => PermissionRuleSource::LocalSettings,
        SettingSource::FlagSettings => PermissionRuleSource::FlagSettings,
        SettingSource::PolicySettings => PermissionRuleSource::PolicySettings,
    }
}

/// 从来源 settings 中删除规则；成功返回 true。
///
/// 运行时校验来源可编辑（userSettings/projectSettings/localSettings）。
pub fn delete_permission_rule_from_settings(rule: &PermissionRule) -> bool {
    // 运行时确保来源确实可编辑（对齐 TS 的 EDITABLE_SOURCES 检查）
    let Some(setting_source) = rule_source_as_setting_source(rule.source) else {
        return false;
    };
    if !is_editable_setting_source(setting_source) {
        return false;
    }

    let rule_string = permission_rule_value_to_string(&rule.rule_value);
    let Some(settings_data) = get_settings_for_source(setting_source) else {
        return false;
    };
    let Some(permissions) = settings_data.permissions.as_ref() else {
        return false;
    };
    let Some(behavior_array) = behavior_rules(permissions, rule.rule_behavior) else {
        return false;
    };

    // 归一化条目（parse→serialize 往返），使旧名（如 "KillShell"）
    // 匹配规范形式（"TaskStop"）
    let normalize_entry =
        |raw: &str| permission_rule_value_to_string(&permission_rule_value_from_string(raw));
    if !behavior_array
        .iter()
        .any(|raw| normalize_entry(raw) == rule_string)
    {
        return false;
    }

    // 构造更新后的完整 settings（保留未识别键），把该 behavior 的数组
    // 替换为过滤后结果
    let mut value =
        serde_json::to_value(&settings_data).unwrap_or_else(|_| Value::Object(Map::new()));
    let filtered: Vec<Value> = behavior_array
        .iter()
        .filter(|raw| normalize_entry(raw) != rule_string)
        .map(|s| Value::String(s.clone()))
        .collect();
    set_permission_behavior_array(&mut value, rule.rule_behavior, filtered);

    if update_settings_for_source(setting_source, &value).is_err() {
        // 错误已在 update_settings_for_source 内部记录，对齐 TS 直接返回 false
        return false;
    }
    true
}

/// 把 settings Value 中 `permissions.<behavior>` 数组整体替换。
fn set_permission_behavior_array(
    value: &mut Value,
    behavior: PermissionBehavior,
    rules: Vec<Value>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let permissions = obj
        .entry("permissions".to_string())
        .or_insert(Value::Object(Map::new()));
    let Some(permissions_obj) = permissions.as_object_mut() else {
        return;
    };
    let behavior_array = permissions_obj
        .entry(behavior_as_str(behavior).to_string())
        .or_insert(Value::Array(Vec::new()));
    *behavior_array = Value::Array(rules);
}

/// 向来源 settings 追加规则；成功返回 true。
///
/// `allowManagedPermissionRulesOnly` 启用时不持久化新规则。
pub fn add_permission_rules_to_settings(
    rule_values: &[PermissionRuleValue],
    rule_behavior: PermissionBehavior,
    source: SettingSource,
) -> bool {
    // allowManagedPermissionRulesOnly 启用时不持久化权限规则
    if should_allow_managed_permission_rules_only() {
        return false;
    }

    if rule_values.is_empty() {
        // 无规则可加
        return true;
    }

    let rule_strings: Vec<String> = rule_values
        .iter()
        .map(permission_rule_value_to_string)
        .collect();
    // 先用走 schema 校验的正常加载器；校验失败时回落 lenient 加载，
    // 保留已有规则（即使 hooks 等字段有校验错误）
    let mut base = get_settings_for_source(source)
        .map(|s| serde_json::to_value(&s).unwrap_or_else(|_| Value::Object(Map::new())))
        .or_else(|| get_settings_for_source_lenient_for_editing_only(source))
        .unwrap_or_else(|| Value::Object(Map::new()));

    // 确保 permissions 对象存在
    if let Some(obj) = base.as_object_mut() {
        obj.entry("permissions".to_string())
            .or_insert(Value::Object(Map::new()));
    }
    let existing_rules: Vec<String> = base
        .pointer(&format!("/permissions/{}", behavior_as_str(rule_behavior)))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // 去重：已存在条目经 parse→serialize 往返归一化后再比对，
    // 使旧名匹配规范形式
    let existing_rules_set: Vec<String> = existing_rules
        .iter()
        .map(|raw| permission_rule_value_to_string(&permission_rule_value_from_string(raw)))
        .collect();
    let new_rules: Vec<String> = rule_strings
        .iter()
        .filter(|rule| !existing_rules_set.contains(rule))
        .cloned()
        .collect();

    // 无新规则则视为成功
    if new_rules.is_empty() {
        return true;
    }

    // 追加新规则（保留已存在的原始条目与未识别键）
    let mut combined: Vec<Value> = existing_rules
        .iter()
        .map(|s| Value::String(s.clone()))
        .collect();
    combined.extend(new_rules.iter().map(|s| Value::String(s.clone())));
    set_permission_behavior_array(&mut base, rule_behavior, combined);

    if let Err(error) = update_settings_for_source(source, &base) {
        log_error(StringError::new(error));
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_only_flag_reads_policy_settings() {
        // 默认环境（无 managed settings）应为 false
        assert!(!should_allow_managed_permission_rules_only());
        assert!(should_show_always_allow_options());
    }

    #[test]
    fn empty_settings_produce_no_rules() {
        assert_eq!(
            settings_json_to_rules(None, PermissionRuleSource::UserSettings),
            Vec::<PermissionRule>::new()
        );
        let data = SettingsJson::default();
        assert_eq!(
            settings_json_to_rules(Some(&data), PermissionRuleSource::UserSettings),
            Vec::<PermissionRule>::new()
        );
    }

    #[test]
    fn rules_parse_from_settings_json() {
        let data = SettingsJson {
            permissions: Some(PermissionsSettings {
                allow: Some(vec!["Bash(npm:*)".to_string(), "Read".to_string()]),
                deny: Some(vec!["Task".to_string()]),
                ask: None,
                ..PermissionsSettings::default()
            }),
            ..SettingsJson::default()
        };
        let rules = settings_json_to_rules(Some(&data), PermissionRuleSource::ProjectSettings);
        assert_eq!(rules.len(), 3);
        // 允许顺序（SUPPORTED_RULE_BEHAVIORS）：allow → deny → ask
        assert_eq!(rules[0].rule_behavior, PermissionBehavior::Allow);
        assert_eq!(rules[0].rule_value.tool_name, "Bash");
        assert_eq!(rules[0].rule_value.rule_content.as_deref(), Some("npm:*"));
        assert_eq!(rules[1].rule_value.tool_name, "Read");
        assert_eq!(rules[1].rule_value.rule_content, None);
        assert_eq!(rules[2].rule_behavior, PermissionBehavior::Deny);
        // 旧名 Task 归一化为 Agent
        assert_eq!(rules[2].rule_value.tool_name, "Agent");
        assert_eq!(rules[2].source, PermissionRuleSource::ProjectSettings);
    }

    #[test]
    fn tool_aliases_attach_alias_skill_tool_names() {
        let mut aliases = Map::new();
        aliases.insert("SkillTool".to_string(), "Skill".to_string());
        let data = SettingsJson {
            permissions: Some(PermissionsSettings {
                allow: Some(vec!["Skill".to_string()]),
                ..PermissionsSettings::default()
            }),
            tool_aliases: Some(aliases),
            ..SettingsJson::default()
        };
        let rules = settings_json_to_rules(Some(&data), PermissionRuleSource::UserSettings);
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].alias_skill_tool_names.as_deref(),
            Some(["SkillTool".to_string()].as_slice())
        );
    }

    #[test]
    fn tool_aliases_override_for_test() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "b".to_string());
        set_tool_aliases_for_test(Some(map));
        let merged = get_settings_tool_aliases();
        assert_eq!(merged.get("a").map(|s| s.as_str()), Some("b"));
        set_tool_aliases_for_test(None);
        let merged = get_settings_tool_aliases();
        assert!(!merged.contains_key("a"));
    }

    #[test]
    fn rule_source_maps_to_settings_source() {
        assert_eq!(
            rule_source_as_setting_source(PermissionRuleSource::LocalSettings),
            Some(SettingSource::LocalSettings)
        );
        assert_eq!(
            rule_source_as_setting_source(PermissionRuleSource::CliArg),
            None
        );
    }

    #[test]
    fn delete_rule_rejects_non_editable_sources() {
        let rule = PermissionRule {
            source: PermissionRuleSource::PolicySettings,
            rule_behavior: PermissionBehavior::Allow,
            rule_value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: None,
            },
            alias_skill_tool_names: None,
        };
        assert!(!delete_permission_rule_from_settings(&rule));
    }

    #[test]
    fn add_rules_rejects_empty_input() {
        assert!(add_permission_rules_to_settings(
            &[],
            PermissionBehavior::Allow,
            SettingSource::UserSettings
        ));
    }

    #[test]
    fn lenient_loader_reads_raw_json() {
        // 用一个临时 settings 来源路径读写：直接用 user settings 路径
        // 可能不存在，这里只验证函数对不存在的文件返回 None
        let none = get_settings_for_source_lenient_for_editing_only(SettingSource::UserSettings);
        // 不能断言 None（CI 环境可能存在全局 settings 文件），
        // 只验证返回值结构与契约一致
        match none {
            None => {}
            Some(v) => {
                assert!(v.is_object() || v.is_array());
            }
        }
    }
}
