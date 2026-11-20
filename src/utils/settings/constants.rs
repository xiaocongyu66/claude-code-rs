//! constants.rs — 设置来源相关常量与显示名工具。
//!
//! 重写自 `src/utils/settings/constants.ts`。唯一外部依赖是
//! `crate::bootstrap::state`（`SettingSource` 类型本体已随 state.rs 落地，
//! 本模块只承载 constants.ts 的数组/函数部分，并补上 `as_str`/`from_str`
//! 便利方法）。
//!
//! 语义对齐：
//! - `SETTING_SOURCES` 为有序常量切片（顺序即合并优先级，后者覆盖前者）；
//! - `getEnabledSettingSources()` 总是并入 `policySettings` 与 `flagSettings`
//!   （对齐 TS 里 `result.add('policySettings'); result.add('flagSettings')`，
//!   并保持 `allowed` 的原始相对顺序）；
//! - `parseSettingSourcesFlag` 只接受 user/project/local（TS 一致，flag 与
//!   policy 不能由 CLI 指定）。

use crate::bootstrap::state::{get_allowed_setting_sources, SettingSource};

/// 所有可能的设置来源；顺序敏感——后面的来源覆盖前面的。
pub const SETTING_SOURCES: &[SettingSource] = SettingSource::ALL;

/// settings 的 JSON Schema URL（可在 schemastore 编辑该 schema）。
pub const CLAUDE_CODE_SETTINGS_SCHEMA_URL: &str =
    "https://json.schemastore.org/claude-code-settings.json";

/// SettingSource 的字符串形式（对齐 TS 的字面量值）。
pub fn setting_source_as_str(source: SettingSource) -> &'static str {
    match source {
        SettingSource::UserSettings => "userSettings",
        SettingSource::ProjectSettings => "projectSettings",
        SettingSource::LocalSettings => "localSettings",
        SettingSource::FlagSettings => "flagSettings",
        SettingSource::PolicySettings => "policySettings",
    }
}

/// 字符串 → SettingSource（对齐 TS 字面量集合）。
pub fn setting_source_from_str(s: &str) -> Option<SettingSource> {
    match s {
        "userSettings" => Some(SettingSource::UserSettings),
        "projectSettings" => Some(SettingSource::ProjectSettings),
        "localSettings" => Some(SettingSource::LocalSettings),
        "flagSettings" => Some(SettingSource::FlagSettings),
        "policySettings" => Some(SettingSource::PolicySettings),
        _ => None,
    }
}

/// 用户可读的来源名（对齐 `getSettingSourceName`）。
pub fn get_setting_source_name(source: SettingSource) -> &'static str {
    match source {
        SettingSource::UserSettings => "user",
        SettingSource::ProjectSettings => "project",
        SettingSource::LocalSettings => "project, gitignored",
        SettingSource::FlagSettings => "cli flag",
        SettingSource::PolicySettings => "managed",
    }
}

/// 来源的短显示名（首字母大写，供 context/skills UI 使用）。
/// 额外接受 `'plugin'` / `'built-in'`。
pub fn get_source_display_name(source: &str) -> String {
    match source {
        "userSettings" => "User".to_string(),
        "projectSettings" => "Project".to_string(),
        "localSettings" => "Local".to_string(),
        "flagSettings" => "Flag".to_string(),
        "policySettings" => "Managed".to_string(),
        "plugin" => "Plugin".to_string(),
        "built-in" => "Built-in".to_string(),
        _ => source.to_string(),
    }
}

/// 来源显示名（小写、内联使用）。额外接受 `'cliArg' | 'command' | 'session'`。
pub fn get_setting_source_display_name_lowercase(source: &str) -> String {
    match source {
        "userSettings" => "user settings".to_string(),
        "projectSettings" => "shared project settings".to_string(),
        "localSettings" => "project local settings".to_string(),
        "flagSettings" => "command line arguments".to_string(),
        "policySettings" => "enterprise managed settings".to_string(),
        "cliArg" => "CLI argument".to_string(),
        "command" => "command configuration".to_string(),
        "session" => "current session".to_string(),
        _ => source.to_string(),
    }
}

/// 来源显示名（首字母大写、UI 标签使用）。
pub fn get_setting_source_display_name_capitalized(source: &str) -> String {
    match source {
        "userSettings" => "User settings".to_string(),
        "projectSettings" => "Shared project settings".to_string(),
        "localSettings" => "Project local settings".to_string(),
        "flagSettings" => "Command line arguments".to_string(),
        "policySettings" => "Enterprise managed settings".to_string(),
        "cliArg" => "CLI argument".to_string(),
        "command" => "Command configuration".to_string(),
        "session" => "Current session".to_string(),
        _ => source.to_string(),
    }
}

/// 解析 `--setting-sources` CLI flag 为 SettingSource 数组。
/// 空字符串 → 空数组；非法名字 → 错误。
pub fn parse_setting_sources_flag(flag: &str) -> Result<Vec<SettingSource>, String> {
    if flag.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    for name in flag.split(',').map(str::trim) {
        match name {
            "user" => result.push(SettingSource::UserSettings),
            "project" => result.push(SettingSource::ProjectSettings),
            "local" => result.push(SettingSource::LocalSettings),
            _ => {
                return Err(format!(
                    "Invalid setting source: {name}. Valid options are: user, project, local"
                ))
            }
        }
    }
    Ok(result)
}

/// 启用的设置来源；policy 与 flag 永远包含。
pub fn get_enabled_setting_sources() -> Vec<SettingSource> {
    let allowed = get_allowed_setting_sources();
    let mut result = Vec::with_capacity(allowed.len() + 2);
    let mut seen = Vec::with_capacity(allowed.len() + 2);
    for source in allowed.iter().chain(&[
        // Always include policy and flag settings
        SettingSource::PolicySettings,
        SettingSource::FlagSettings,
    ]) {
        if !seen.contains(source) {
            seen.push(*source);
            result.push(*source);
        }
    }
    result
}

/// 某来源是否被启用（应被加载）。
pub fn is_setting_source_enabled(source: SettingSource) -> bool {
    get_enabled_setting_sources().contains(&source)
}

/// 可编辑的设置来源（排除只读的 policySettings 与 flagSettings）。
pub fn is_editable_setting_source(source: SettingSource) -> bool {
    !matches!(
        source,
        SettingSource::PolicySettings | SettingSource::FlagSettings
    )
}

/// 权限规则可保存的来源列表（显示顺序）。
pub const SOURCES: &[SettingSource] = &[
    SettingSource::LocalSettings,
    SettingSource::ProjectSettings,
    SettingSource::UserSettings,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources_flag() {
        assert_eq!(parse_setting_sources_flag("").unwrap(), Vec::new());
        let parsed = parse_setting_sources_flag("user, project ,local").unwrap();
        assert_eq!(
            parsed,
            vec![
                SettingSource::UserSettings,
                SettingSource::ProjectSettings,
                SettingSource::LocalSettings,
            ]
        );
        assert!(parse_setting_sources_flag("user,policy").is_err());
    }

    #[test]
    fn enabled_sources_include_policy_and_flag() {
        let enabled = get_enabled_setting_sources();
        assert!(enabled.contains(&SettingSource::PolicySettings));
        assert!(enabled.contains(&SettingSource::FlagSettings));
    }

    #[test]
    fn display_names() {
        assert_eq!(get_setting_source_name(SettingSource::UserSettings), "user");
        assert_eq!(get_source_display_name("plugin"), "Plugin");
        assert_eq!(
            get_setting_source_display_name_lowercase("policySettings"),
            "enterprise managed settings"
        );
        assert_eq!(
            get_setting_source_display_name_capitalized("flagSettings"),
            "Command line arguments"
        );
    }
}
