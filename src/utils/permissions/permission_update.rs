//! permission_update.rs — 权限更新的应用（in-memory）与持久化（settings 落盘）。
//!
//! 重写自 `src/utils/permissions/PermissionUpdate.ts`。语义对齐说明：
//!
//! - `apply_permission_update` 对齐 TS 的 `{ ...context, [ruleKind]: {...} }`
//!   展开语义——`ruleKind`（allow/deny/ask）选中 `alwaysAllowRules` /
//!   `alwaysDenyRules` / `alwaysAskRules` 三个桶之一，destination 选中桶内
//!   的来源字段；新规则**追加**（addRules）/**替换**（replaceRules）/
//!   **过滤**（removeRules）。`additional_working_directories` 用 HashMap
//!   的 insert/remove 复刻 TS Map.set/delete；
//! - `persist_permission_update` 对齐 TS 的 switch：只有
//!   userSettings/projectSettings/localSettings 支持持久化
//!   （`supports_persistence`），其余（session/cliArg）立即返回；
//! - TS 的 `jsonStringify` 日志参数用 `slow_operations::json_stringify`
//!   对齐（内容等价于 JSON.stringify 紧凑输出）；
//! - TS 的 `filesystem.ts::toPosixPath` 依赖尚未转换的 filesystem 模块；
//!   本文件只用到其「Windows 路径 → POSIX」这一纯函数行为，在下方以
//!   私有 helper 等价内联（与 windowsPaths.ts 的
//!   `windowsPathToPosixPath` 逐分支对齐）。filesystem.rs 落地后应改回
//!   引用，删除此处的重复实现；
//! - `getSettingsForSource` / `updateSettingsForSource` /
//!   `addPermissionRulesToSettings` 均使用已转换的
//!   `crate::utils::settings` 与同目录 `permissions_loader`。

use serde_json::{Map, Value, json};

use crate::bootstrap::state::SettingSource;
use crate::types::permissions::{
    PermissionBehavior, PermissionRuleSource, PermissionRuleValue, PermissionUpdate,
    PermissionUpdateDestination, ToolPermissionContext, ToolPermissionRulesBySource,
};
use crate::utils::debug::{DebugLogLevel, log_for_debugging};
use crate::utils::permissions::permission_rule_parser::{
    permission_rule_value_from_string, permission_rule_value_to_string,
};
use crate::utils::permissions::permissions_loader::add_permission_rules_to_settings;
use crate::utils::platform::{Platform, get_platform};
use crate::utils::settings::settings::{get_settings_for_source, update_settings_for_source};
use crate::utils::slow_operations::json_stringify;

// 再出口（对齐 TS `export type { AdditionalWorkingDirectory, WorkingDirectorySource }`）
pub use crate::types::permissions::{AdditionalWorkingDirectory, WorkingDirectorySource};

/// 取更新的持久化目标（所有 6 个变体都带 `destination` 字段）。
fn update_destination(update: &PermissionUpdate) -> PermissionUpdateDestination {
    match update {
        PermissionUpdate::AddRules { destination, .. }
        | PermissionUpdate::ReplaceRules { destination, .. }
        | PermissionUpdate::RemoveRules { destination, .. }
        | PermissionUpdate::SetMode { destination, .. }
        | PermissionUpdate::AddDirectories { destination, .. }
        | PermissionUpdate::RemoveDirectories { destination, .. } => *destination,
    }
}

fn destination_as_str(destination: PermissionUpdateDestination) -> &'static str {
    match destination {
        PermissionUpdateDestination::UserSettings => "userSettings",
        PermissionUpdateDestination::ProjectSettings => "projectSettings",
        PermissionUpdateDestination::LocalSettings => "localSettings",
        PermissionUpdateDestination::Session => "session",
        PermissionUpdateDestination::CliArg => "cliArg",
    }
}

fn destination_as_rule_source(destination: PermissionUpdateDestination) -> PermissionRuleSource {
    match destination {
        PermissionUpdateDestination::UserSettings => PermissionRuleSource::UserSettings,
        PermissionUpdateDestination::ProjectSettings => PermissionRuleSource::ProjectSettings,
        PermissionUpdateDestination::LocalSettings => PermissionRuleSource::LocalSettings,
        PermissionUpdateDestination::Session => PermissionRuleSource::Session,
        PermissionUpdateDestination::CliArg => PermissionRuleSource::CliArg,
    }
}

fn destination_setting_source(destination: PermissionUpdateDestination) -> Option<SettingSource> {
    match destination {
        PermissionUpdateDestination::UserSettings => Some(SettingSource::UserSettings),
        PermissionUpdateDestination::ProjectSettings => Some(SettingSource::ProjectSettings),
        PermissionUpdateDestination::LocalSettings => Some(SettingSource::LocalSettings),
        PermissionUpdateDestination::Session | PermissionUpdateDestination::CliArg => None,
    }
}

fn behavior_as_str(behavior: PermissionBehavior) -> &'static str {
    match behavior {
        PermissionBehavior::Allow => "allow",
        PermissionBehavior::Deny => "deny",
        PermissionBehavior::Ask => "ask",
    }
}

/// 按 behavior 选中对应的规则桶（TS 的 `ruleKind`）。
fn rules_bucket_mut<'a>(
    context: &'a mut ToolPermissionContext,
    behavior: PermissionBehavior,
) -> &'a mut ToolPermissionRulesBySource {
    match behavior {
        PermissionBehavior::Allow => &mut context.always_allow_rules,
        PermissionBehavior::Deny => &mut context.always_deny_rules,
        PermissionBehavior::Ask => &mut context.always_ask_rules,
    }
}

/// 按 destination 选中桶内的来源字段。destination 只可能是
/// userSettings/projectSettings/localSettings/session/cliArg，
/// 不可能是 flagSettings/policySettings/command（对齐 TS 动态键）。
fn destination_field_mut<'a>(
    rules: &'a mut ToolPermissionRulesBySource,
    destination: PermissionUpdateDestination,
) -> &'a mut Option<Vec<String>> {
    match destination {
        PermissionUpdateDestination::UserSettings => &mut rules.user_settings,
        PermissionUpdateDestination::ProjectSettings => &mut rules.project_settings,
        PermissionUpdateDestination::LocalSettings => &mut rules.local_settings,
        PermissionUpdateDestination::Session => &mut rules.session,
        PermissionUpdateDestination::CliArg => &mut rules.cli_arg,
    }
}

/// 从更新列表中抽取所有 addRules 更新的规则（TS `extractRules`）。
pub fn extract_rules(updates: Option<&[PermissionUpdate]>) -> Vec<PermissionRuleValue> {
    let Some(updates) = updates else {
        return Vec::new();
    };
    let mut rules: Vec<PermissionRuleValue> = Vec::new();
    for update in updates {
        if let PermissionUpdate::AddRules {
            rules: update_rules,
            ..
        } = update
        {
            rules.extend(update_rules.iter().cloned());
        }
    }
    rules
}

/// 是否存在 addRules 更新（TS `hasRules`）。
pub fn has_rules(updates: Option<&[PermissionUpdate]>) -> bool {
    !extract_rules(updates).is_empty()
}

/// 把单个权限更新应用到 context 并返回更新后的 context。
pub fn apply_permission_update(
    mut context: ToolPermissionContext,
    update: &PermissionUpdate,
) -> ToolPermissionContext {
    match update {
        PermissionUpdate::SetMode { mode, .. } => {
            log_for_debugging(
                &format!(
                    "Applying permission update: Setting mode to '{}'",
                    mode.as_str()
                ),
                DebugLogLevel::Debug,
            );
            context.mode = (*mode).into();
        }
        PermissionUpdate::AddRules {
            rules,
            behavior,
            destination,
        } => {
            let rule_strings: Vec<String> =
                rules.iter().map(permission_rule_value_to_string).collect();
            log_for_debugging(
                &format!(
                    "Applying permission update: Adding {} {} rule(s) to destination '{}': {}",
                    rules.len(),
                    behavior_as_str(*behavior),
                    destination_as_str(*destination),
                    json_stringify(&json!(rule_strings)),
                ),
                DebugLogLevel::Debug,
            );

            let field =
                destination_field_mut(rules_bucket_mut(&mut context, *behavior), *destination);
            let mut combined = field.clone().unwrap_or_default();
            combined.extend(rule_strings);
            *field = Some(combined);
        }
        PermissionUpdate::ReplaceRules {
            rules,
            behavior,
            destination,
        } => {
            let rule_strings: Vec<String> =
                rules.iter().map(permission_rule_value_to_string).collect();
            log_for_debugging(
                &format!(
                    "Replacing all {} rules for destination '{}' with {} rule(s): {}",
                    behavior_as_str(*behavior),
                    destination_as_str(*destination),
                    rules.len(),
                    json_stringify(&json!(rule_strings)),
                ),
                DebugLogLevel::Debug,
            );

            let field =
                destination_field_mut(rules_bucket_mut(&mut context, *behavior), *destination);
            *field = Some(rule_strings);
        }
        PermissionUpdate::AddDirectories {
            directories,
            destination,
        } => {
            log_for_debugging(
                &format!(
                    "Applying permission update: Adding {} director{} with destination '{}': {}",
                    directories.len(),
                    if directories.len() == 1 { "y" } else { "ies" },
                    destination_as_str(*destination),
                    json_stringify(&json!(directories)),
                ),
                DebugLogLevel::Debug,
            );
            for directory in directories {
                context.additional_working_directories.insert(
                    directory.clone(),
                    AdditionalWorkingDirectory {
                        path: directory.clone(),
                        source: destination_as_rule_source(*destination),
                    },
                );
            }
        }
        PermissionUpdate::RemoveRules {
            rules,
            behavior,
            destination,
        } => {
            let rule_strings: Vec<String> =
                rules.iter().map(permission_rule_value_to_string).collect();
            log_for_debugging(
                &format!(
                    "Applying permission update: Removing {} {} rule(s) from source '{}': {}",
                    rules.len(),
                    behavior_as_str(*behavior),
                    destination_as_str(*destination),
                    json_stringify(&json!(rule_strings)),
                ),
                DebugLogLevel::Debug,
            );

            let field =
                destination_field_mut(rules_bucket_mut(&mut context, *behavior), *destination);
            let existing_rules = field.clone().unwrap_or_default();
            let filtered_rules: Vec<String> = existing_rules
                .iter()
                .filter(|rule| !rule_strings.contains(rule))
                .cloned()
                .collect();
            *field = Some(filtered_rules);
        }
        PermissionUpdate::RemoveDirectories { directories, .. } => {
            log_for_debugging(
                &format!(
                    "Applying permission update: Removing {} director{}: {}",
                    directories.len(),
                    if directories.len() == 1 { "y" } else { "ies" },
                    json_stringify(&json!(directories)),
                ),
                DebugLogLevel::Debug,
            );
            for directory in directories {
                context.additional_working_directories.remove(directory);
            }
        }
    }
    context
}

/// 把多个权限更新依次应用到 context（TS `applyPermissionUpdates`）。
pub fn apply_permission_updates(
    context: ToolPermissionContext,
    updates: &[PermissionUpdate],
) -> ToolPermissionContext {
    let mut updated_context = context;
    for update in updates {
        updated_context = apply_permission_update(updated_context, update);
    }
    updated_context
}

/// destination 是否可持久化（TS `supportsPersistence`）。
pub fn supports_persistence(destination: PermissionUpdateDestination) -> bool {
    matches!(
        destination,
        PermissionUpdateDestination::LocalSettings
            | PermissionUpdateDestination::UserSettings
            | PermissionUpdateDestination::ProjectSettings
    )
}

/// 把单个权限更新持久化到对应的 settings 来源（TS `persistPermissionUpdate`）。
pub fn persist_permission_update(update: &PermissionUpdate) {
    if !supports_persistence(update_destination(update)) {
        return;
    }

    log_for_debugging(
        &format!(
            "Persisting permission update: {} to source '{}'",
            update_type_str(update),
            destination_as_str(update_destination(update)),
        ),
        DebugLogLevel::Debug,
    );

    let Some(source) = destination_setting_source(update_destination(update)) else {
        return;
    };

    match update {
        PermissionUpdate::AddRules {
            rules, behavior, ..
        } => {
            log_for_debugging(
                &format!(
                    "Persisting {} {} rule(s) to {}",
                    rules.len(),
                    behavior_as_str(*behavior),
                    destination_as_str(update_destination(update)),
                ),
                DebugLogLevel::Debug,
            );
            add_permission_rules_to_settings(rules, *behavior, source);
        }
        PermissionUpdate::AddDirectories { directories, .. } => {
            log_for_debugging(
                &format!(
                    "Persisting {} director{} to {}",
                    directories.len(),
                    if directories.len() == 1 { "y" } else { "ies" },
                    destination_as_str(update_destination(update)),
                ),
                DebugLogLevel::Debug,
            );
            let existing_dirs: Vec<String> = get_settings_for_source(source)
                .and_then(|s| s.permissions)
                .and_then(|p| p.additional_directories)
                .unwrap_or_default();

            // 追加新目录，避免重复
            let dirs_to_add: Vec<String> = directories
                .iter()
                .filter(|dir| !existing_dirs.contains(dir))
                .cloned()
                .collect();

            if !dirs_to_add.is_empty() {
                let mut updated_dirs = existing_dirs;
                updated_dirs.extend(dirs_to_add);
                let _ = update_settings_for_source(
                    source,
                    &json!({ "permissions": { "additionalDirectories": updated_dirs } }),
                );
            }
        }
        PermissionUpdate::RemoveRules {
            rules, behavior, ..
        } => {
            log_for_debugging(
                &format!(
                    "Removing {} {} rule(s) from {}",
                    rules.len(),
                    behavior_as_str(*behavior),
                    destination_as_str(update_destination(update)),
                ),
                DebugLogLevel::Debug,
            );
            let existing_rules: Vec<String> = get_settings_for_source(source)
                .and_then(|s| s.permissions)
                .and_then(|p| behavior_rules_owned(p, *behavior))
                .unwrap_or_default();

            // 经 parse→serialize 往返归一化后再比对，使 "Bash(*)" 与
            // "Bash" 匹配
            let rules_to_remove: Vec<String> =
                rules.iter().map(permission_rule_value_to_string).collect();
            let filtered_rules: Vec<String> = existing_rules
                .iter()
                .filter(|rule| {
                    let normalized =
                        permission_rule_value_to_string(&permission_rule_value_from_string(rule));
                    !rules_to_remove.contains(&normalized)
                })
                .cloned()
                .collect();

            let _ = update_settings_for_source(
                source,
                &permissions_patch(behavior_as_str(*behavior), Value::from(filtered_rules)),
            );
        }
        PermissionUpdate::RemoveDirectories { directories, .. } => {
            log_for_debugging(
                &format!(
                    "Removing {} director{} from {}",
                    directories.len(),
                    if directories.len() == 1 { "y" } else { "ies" },
                    destination_as_str(update_destination(update)),
                ),
                DebugLogLevel::Debug,
            );
            let existing_dirs: Vec<String> = get_settings_for_source(source)
                .and_then(|s| s.permissions)
                .and_then(|p| p.additional_directories)
                .unwrap_or_default();

            let filtered_dirs: Vec<String> = existing_dirs
                .iter()
                .filter(|dir| !directories.contains(dir))
                .cloned()
                .collect();

            let _ = update_settings_for_source(
                source,
                &json!({ "permissions": { "additionalDirectories": filtered_dirs } }),
            );
        }
        PermissionUpdate::SetMode { mode, .. } => {
            log_for_debugging(
                &format!(
                    "Persisting mode '{}' to {}",
                    mode.as_str(),
                    destination_as_str(update_destination(update)),
                ),
                DebugLogLevel::Debug,
            );
            let _ = update_settings_for_source(
                source,
                &json!({ "permissions": { "defaultMode": mode.as_str() } }),
            );
        }
        PermissionUpdate::ReplaceRules {
            rules, behavior, ..
        } => {
            log_for_debugging(
                &format!(
                    "Replacing all {} rules in {} with {} rule(s)",
                    behavior_as_str(*behavior),
                    destination_as_str(update_destination(update)),
                    rules.len(),
                ),
                DebugLogLevel::Debug,
            );
            let rule_strings: Vec<String> =
                rules.iter().map(permission_rule_value_to_string).collect();
            let _ = update_settings_for_source(
                source,
                &permissions_patch(behavior_as_str(*behavior), Value::from(rule_strings)),
            );
        }
    }
}

/// 构造 `{ permissions: { <key>: <value> } }` 的局部更新对象。
/// `json!` 宏不接受计算键，故 behavior 键通过本函数手工拼装
/// （与 TS 的 `{ permissions: { [update.behavior]: ... } }` 对齐）。
fn permissions_patch(key: &str, value: Value) -> Value {
    let mut permissions = Map::new();
    permissions.insert(key.to_string(), value);
    let mut root = Map::new();
    root.insert("permissions".to_string(), Value::Object(permissions));
    Value::Object(root)
}

fn update_type_str(update: &PermissionUpdate) -> &'static str {
    match update {
        PermissionUpdate::AddRules { .. } => "addRules",
        PermissionUpdate::ReplaceRules { .. } => "replaceRules",
        PermissionUpdate::RemoveRules { .. } => "removeRules",
        PermissionUpdate::SetMode { .. } => "setMode",
        PermissionUpdate::AddDirectories { .. } => "addDirectories",
        PermissionUpdate::RemoveDirectories { .. } => "removeDirectories",
    }
}

/// 从 `PermissionsSettings` 取出某 behavior 的规则数组（owned）。
fn behavior_rules_owned(
    permissions: crate::utils::settings::types::PermissionsSettings,
    behavior: PermissionBehavior,
) -> Option<Vec<String>> {
    match behavior {
        PermissionBehavior::Allow => permissions.allow,
        PermissionBehavior::Deny => permissions.deny,
        PermissionBehavior::Ask => permissions.ask,
    }
}

/// 把多个权限更新持久化（TS `persistPermissionUpdates`）。
/// 只持久化来源可持久化的更新。
pub fn persist_permission_updates(updates: &[PermissionUpdate]) {
    for update in updates {
        persist_permission_update(update);
    }
}

// ---------------------------------------------------------------------------
// toPosixPath —— filesystem.ts 未转换前的等价内联（见模块头注释）
// ---------------------------------------------------------------------------

/// 把路径转换为 POSIX 格式（对齐 filesystem.ts `toPosixPath`）。
fn to_posix_path(path: &str) -> String {
    if get_platform() == Platform::Windows {
        windows_path_to_posix_path(path)
    } else {
        path.to_string()
    }
}

/// Windows 路径 → POSIX 路径（对齐 windowsPaths.ts `windowsPathToPosixPath`）。
fn windows_path_to_posix_path(windows_path: &str) -> String {
    // UNC 路径：\\server\share -> //server/share
    if windows_path.starts_with("\\\\") {
        return windows_path.replace('\\', "/");
    }
    let chars: Vec<char> = windows_path.chars().collect();
    // 盘符路径：C:\Users\foo -> /c/Users/foo
    if chars.len() >= 3
        && chars[0].is_ascii_alphabetic()
        && chars[1] == ':'
        && (chars[2] == '/' || chars[2] == '\\')
    {
        let drive_letter = chars[0].to_ascii_lowercase();
        let rest: String = chars[2..]
            .iter()
            .map(|c| if *c == '\\' { '/' } else { *c })
            .collect();
        return format!("/{drive_letter}{rest}");
    }
    // 已是 POSIX 或相对路径——只翻转斜杠
    windows_path.replace('\\', "/")
}

/// POSIX 语义的绝对路径判定（对齐 `path.posix.isAbsolute`）。
fn is_posix_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// 为目录创建 Read 规则建议（TS `createReadRuleSuggestion`）。
///
/// destination 默认 `'session'`（见 `create_read_rule_suggestion_default`）。
/// 根目录（`/`）过于宽泛，不适合作为权限目标，返回 `None`。
pub fn create_read_rule_suggestion(
    dir_path: &str,
    destination: PermissionUpdateDestination,
) -> Option<PermissionUpdate> {
    // 转 POSIX 以做模式匹配（内部处理 Windows）
    let path_for_pattern = to_posix_path(dir_path);

    // 根目录太宽泛
    if path_for_pattern == "/" {
        return None;
    }

    // 绝对路径前置一个额外的 /，构造 //path/** 模式
    let rule_content = if is_posix_absolute(&path_for_pattern) {
        format!("/{path_for_pattern}/**")
    } else {
        format!("{path_for_pattern}/**")
    };

    Some(PermissionUpdate::AddRules {
        destination,
        rules: vec![PermissionRuleValue {
            tool_name: "Read".to_string(),
            rule_content,
        }],
        behavior: PermissionBehavior::Allow,
    })
}

/// `create_read_rule_suggestion` 的默认 destination 版本（`'session'`）。
pub fn create_read_rule_suggestion_default(dir_path: &str) -> Option<PermissionUpdate> {
    create_read_rule_suggestion(dir_path, PermissionUpdateDestination::Session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::permissions::PermissionMode;

    fn context_for_test() -> ToolPermissionContext {
        ToolPermissionContext {
            mode: PermissionMode::Default,
            additional_working_directories: Default::default(),
            always_allow_rules: Default::default(),
            always_deny_rules: Default::default(),
            always_ask_rules: Default::default(),
            is_bypass_permissions_mode_available: true,
            stripped_dangerous_rules: None,
            should_avoid_permission_prompts: None,
            await_automated_checks_before_dialog: None,
            pre_plan_mode: None,
        }
    }

    fn rule_value(tool: &str, content: Option<&str>) -> PermissionRuleValue {
        PermissionRuleValue {
            tool_name: tool.to_string(),
            rule_content: content.map(str::to_string),
        }
    }

    #[test]
    fn extract_and_has_rules() {
        let updates = vec![
            PermissionUpdate::AddRules {
                destination: PermissionUpdateDestination::Session,
                rules: vec![rule_value("Bash", Some("npm:*"))],
                behavior: PermissionBehavior::Allow,
            },
            PermissionUpdate::SetMode {
                destination: PermissionUpdateDestination::Session,
                mode: crate::types::permissions::ExternalPermissionMode::Plan,
            },
        ];
        assert!(has_rules(Some(updates.as_slice())));
        assert_eq!(
            extract_rules(Some(updates.as_slice())),
            vec![rule_value("Bash", Some("npm:*"))]
        );
        assert!(!has_rules(None));
        assert!(extract_rules(None).is_empty());
    }

    #[test]
    fn apply_add_rules_appends() {
        let ctx = context_for_test();
        let update = PermissionUpdate::AddRules {
            destination: PermissionUpdateDestination::Session,
            rules: vec![rule_value("Bash", Some("npm:*"))],
            behavior: PermissionBehavior::Allow,
        };
        let next = apply_permission_update(ctx, &update);
        assert_eq!(
            next.always_allow_rules.session.as_deref(),
            Some(["Bash(npm:*)".to_string()].as_slice())
        );

        // 再次追加同一 destination：累积
        let next = apply_permission_update(next, &update);
        assert_eq!(
            next.always_allow_rules.session.as_deref(),
            Some(["Bash(npm:*)".to_string(), "Bash(npm:*)".to_string()].as_slice())
        );
    }

    #[test]
    fn apply_replace_rules_overwrites() {
        let ctx = context_for_test();
        let updates = [
            PermissionUpdate::AddRules {
                destination: PermissionUpdateDestination::UserSettings,
                rules: vec![rule_value("Bash", None)],
                behavior: PermissionBehavior::Deny,
            },
            PermissionUpdate::ReplaceRules {
                destination: PermissionUpdateDestination::UserSettings,
                rules: vec![rule_value("Read", None)],
                behavior: PermissionBehavior::Deny,
            },
        ];
        let next = apply_permission_updates(ctx, &updates);
        assert_eq!(
            next.always_deny_rules.user_settings.as_deref(),
            Some(["Read".to_string()].as_slice())
        );
    }

    #[test]
    fn apply_remove_rules_filters() {
        let ctx = context_for_test();
        let updates = [
            PermissionUpdate::AddRules {
                destination: PermissionUpdateDestination::CliArg,
                rules: vec![rule_value("Bash", Some("npm:*")), rule_value("Read", None)],
                behavior: PermissionBehavior::Ask,
            },
            PermissionUpdate::RemoveRules {
                destination: PermissionUpdateDestination::CliArg,
                rules: vec![rule_value("Bash", Some("npm:*"))],
                behavior: PermissionBehavior::Ask,
            },
        ];
        let next = apply_permission_updates(ctx, &updates);
        assert_eq!(
            next.always_ask_rules.cli_arg.as_deref(),
            Some(["Read".to_string()].as_slice())
        );
    }

    #[test]
    fn apply_set_mode_updates_context_mode() {
        let ctx = context_for_test();
        let update = PermissionUpdate::SetMode {
            destination: PermissionUpdateDestination::Session,
            mode: crate::types::permissions::ExternalPermissionMode::AcceptEdits,
        };
        let next = apply_permission_update(ctx, &update);
        assert_eq!(next.mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn apply_add_and_remove_directories() {
        let ctx = context_for_test();
        let update = PermissionUpdate::AddDirectories {
            destination: PermissionUpdateDestination::CliArg,
            directories: vec!["/tmp/extra".to_string()],
        };
        let next = apply_permission_update(ctx, &update);
        assert!(
            next.additional_working_directories
                .contains_key("/tmp/extra")
        );

        let update = PermissionUpdate::RemoveDirectories {
            destination: PermissionUpdateDestination::CliArg,
            directories: vec!["/tmp/extra".to_string()],
        };
        let next = apply_permission_update(next, &update);
        assert!(next.additional_working_directories.is_empty());
    }

    #[test]
    fn supports_persistence_only_editable() {
        assert!(supports_persistence(
            PermissionUpdateDestination::UserSettings
        ));
        assert!(supports_persistence(
            PermissionUpdateDestination::ProjectSettings
        ));
        assert!(supports_persistence(
            PermissionUpdateDestination::LocalSettings
        ));
        assert!(!supports_persistence(PermissionUpdateDestination::Session));
        assert!(!supports_persistence(PermissionUpdateDestination::CliArg));
    }

    #[test]
    fn persist_skips_non_persistent_destinations() {
        // session/cliArg 更新直接无操作返回
        let update = PermissionUpdate::AddRules {
            destination: PermissionUpdateDestination::Session,
            rules: vec![rule_value("Bash", None)],
            behavior: PermissionBehavior::Allow,
        };
        persist_permission_update(&update);
        // 无断言：成功即未 panic
    }

    #[test]
    fn read_rule_suggestion_for_absolute_path() {
        let suggestion = create_read_rule_suggestion_default("/home/user/project");
        assert!(matches!(suggestion, Some(_)));
        let update = suggestion.unwrap();
        match update {
            PermissionUpdate::AddRules {
                rules,
                behavior,
                destination,
            } => {
                assert_eq!(behavior, PermissionBehavior::Allow);
                assert_eq!(destination, PermissionUpdateDestination::Session);
                assert_eq!(rules[0].tool_name, "Read");
                assert_eq!(
                    rules[0].rule_content.as_deref(),
                    Some("//home/user/project/**")
                );
            }
            _ => panic!("expected AddRules"),
        }
    }

    #[test]
    fn read_rule_suggestion_for_relative_path() {
        let update =
            create_read_rule_suggestion("src", PermissionUpdateDestination::UserSettings).unwrap();
        match update {
            PermissionUpdate::AddRules {
                rules, destination, ..
            } => {
                assert_eq!(rules[0].rule_content.as_deref(), Some("src/**"));
                assert_eq!(destination, PermissionUpdateDestination::UserSettings);
            }
            _ => panic!("expected AddRules"),
        }
    }

    #[test]
    fn read_rule_suggestion_root_is_none() {
        assert!(create_read_rule_suggestion_default("/").is_none());
    }

    #[test]
    fn windows_path_conversion() {
        assert_eq!(windows_path_to_posix_path("C:\\Users\\foo"), "/c/Users/foo");
        assert_eq!(
            windows_path_to_posix_path("\\\\server\\share"),
            "//server/share"
        );
        assert_eq!(windows_path_to_posix_path("a\\b"), "a/b");
        assert_eq!(windows_path_to_posix_path("C:/Users/foo"), "/c/Users/foo");
        // 非盘符冒号路径不转换盘符
        assert_eq!(windows_path_to_posix_path("foo:bar"), "foo:bar");
    }
}
