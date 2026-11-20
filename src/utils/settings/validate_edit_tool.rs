//! validate_edit_tool.rs — settings 文件编辑校验（FileEditTool 复用入口）。
//!
//! 重写自 `src/utils/settings/validateEditTool.ts`。
//!
//! 已知差异（依赖尚未转 Rust，待下一波消除）：
//! - `isClaudeSettingsPath` 来自 utils/permissions/filesystem.ts（含
//!   expandPath/normalizeCaseForComparison 等路径工具），此处内联最小等价
//!   实现：路径分隔符归一化 + 大小写不敏感后缀匹配 `.claude/settings.json`
//!   / `.claude/settings.local.json`，或与当前项目 settings 路径（含
//!   managed-settings.json 与 CLI flag 路径）完全匹配（大小写不敏感）；
//! - `ValidationResult` 来自 src/Tool.ts（TS），此处按下游消费的字段
//!   （result / message / errorCode）内联一个子集结构。
//!
//! 编辑前后内容都过 `validate_settings_file_content`：编辑前不合法则放行
//! （不阻塞用户修复），编辑后不合法才返回带完整 schema 的错误。

use crate::utils::settings::settings::get_settings_file_path_for_source;
use crate::utils::settings::validation::validate_settings_file_content;
use crate::bootstrap::state::SettingSource;

/// FileEditTool 校验失败结果（src/Tool.ts `ValidationResult` 的消费子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsEditValidationError {
    pub message: String,
    /// 对齐 TS 的 errorCode: 10。
    pub error_code: u32,
}

/// 校验 settings 文件编辑（`validateInputForSettingsFileEdit`）：
///
/// - 非 Claude settings 文件 → None（不校验）；
/// - 编辑前内容不合法 → None（允许编辑修复）；
/// - 编辑前合法、编辑后非法 → 返回错误（含完整 schema 文本）。
pub fn validate_input_for_settings_file_edit(
    file_path: &str,
    original_content: &str,
    updated_content: &str,
) -> Option<SettingsEditValidationError> {
    if !is_claude_settings_path(file_path) {
        return None;
    }

    // 编辑前内容是否符合 schema
    if !matches!(
        validate_settings_file_content(original_content),
        crate::utils::settings::validation::ValidateSettingsFileContent::Valid
    ) {
        // 编辑前即非法 → 放行编辑（不阻塞）
        return None;
    }

    // 编辑后必须仍然合法
    match validate_settings_file_content(updated_content) {
        crate::utils::settings::validation::ValidateSettingsFileContent::Valid => None,
        crate::utils::settings::validation::ValidateSettingsFileContent::Invalid { error, full_schema } => {
            Some(SettingsEditValidationError {
                message: format!(
                    "Claude Code settings.json validation failed after edit:\n{error}\n\n\
                     Full schema:\n{full_schema}\n\
                     IMPORTANT: Do not update the env unless explicitly instructed to do so."
                ),
                error_code: 10,
            })
        }
    }
}

/// `isClaudeSettingsPath`（utils/permissions/filesystem.ts）的最小等价实现。
/// 见模块头「已知差异」。
fn is_claude_settings_path(file_path: &str) -> bool {
    // 路径分隔符归一化为 '/'（对齐 TS 的 expandPath + platform sep 比较）
    let normalized = file_path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();

    if lower.ends_with("/.claude/settings.json") || lower.ends_with("/.claude/settings.local.json") {
        // 包含其它项目的 .claude/settings.json
        return true;
    }

    // 当前项目的 settings 路径（含 managed settings 与 CLI flag 路径）
    for source in [
        SettingSource::UserSettings,
        SettingSource::ProjectSettings,
        SettingSource::LocalSettings,
        SettingSource::PolicySettings,
        SettingSource::FlagSettings,
    ] {
        if let Some(settings_path) = get_settings_file_path_for_source(source) {
            let settings_norm = settings_path.replace('\\', "/").to_ascii_lowercase();
            if settings_norm == lower {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_settings_paths() {
        assert!(is_claude_settings_path("/home/u/project/.claude/settings.json"));
        assert!(is_claude_settings_path("/home/u/project/.claude/SETTINGS.LOCAL.json"));
        assert!(is_claude_settings_path("C:\\Users\\u\\.claude\\settings.json"));
        assert!(!is_claude_settings_path("/home/u/project/.claude/settings.json.bak"));
        assert!(!is_claude_settings_path("/tmp/random.json"));
    }

    #[test]
    fn allows_edit_when_before_is_invalid() {
        // 编辑前非法（model 类型错误）→ 放行
        let result = validate_input_for_settings_file_edit(
            "/p/.claude/settings.json",
            "{\"model\": 42}",
            "{\"model\": 43}",
        );
        assert!(result.is_none());
    }

    #[test]
    fn blocks_edit_that_breaks_valid_settings() {
        let result = validate_input_for_settings_file_edit(
            "/p/.claude/settings.json",
            "{\"model\": \"claude-4\"}",
            "{\"model\": 42}",
        );
        let err = result.unwrap();
        assert_eq!(err.error_code, 10);
        assert!(err.message.contains("Claude Code settings.json validation failed after edit:"));
        assert!(err.message.contains("Full schema:"));
    }

    #[test]
    fn non_settings_files_are_not_validated() {
        assert!(validate_input_for_settings_file_edit("/tmp/x.txt", "{", "{").is_none());
    }
}
