//! validation.rs — settings 校验错误组装与权限规则过滤。
//!
//! 重写自 `src/utils/settings/validation.ts`。zod 侧的 `ZodError` /
//! `ZodIssue` 在 Rust 里对应 [`crate::utils::settings::types::SchemaIssue`]；
//! `formatZodError` 的逐分支消息组装（invalid_value → `Invalid value.
//! Expected one of: ...`、invalid_type → `Expected X, but received Y`、
//! unrecognized_keys → `Unrecognized field(s): ...`、too_small → `Number
//! must be greater than or equal to N`）一一搬运， tip 生成复用已转换的
//! validation_tips 模块。
//!
//! `filterInvalidPermissionRules` 会 **就地修改** 原始 JSON 数据（对齐 TS
//! 的 `perms[key] = rules.filter(...)`），在校验前剔除非法权限规则，避免
//! 一条坏规则毒化整个 settings 文件。

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::utils::settings::permission_validation::validate_permission_rule;
use crate::utils::settings::schema_output::generate_settings_json_schema;
use crate::utils::settings::types::{parse_settings_json_strict, IssueCode, SchemaIssue, SettingsJson};
use crate::utils::settings::validation_tips::{get_validation_tip, TipContext, ValidationTip};
use crate::utils::slow_operations::json_parse;
use crate::utils::string_utils::plural;

static RECEIVED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"received (\w+)").expect("RECEIVED_RE is a constant regex")
});

/// 字段路径（点分表示法，如 "permissions.defaultMode"）。
pub type FieldPath = String;

/// MCP 配置错误的附加元数据（对齐 TS 的 mcpErrorMetadata；本模块只承载
/// 类型，MCP 错误聚合在 allErrors.ts —— 仍为 TS）。
#[derive(Debug, Clone, PartialEq)]
pub struct McpErrorMetadata {
    pub scope: String,
    pub server_name: Option<String>,
    pub severity: Option<String>,
}

/// settings / MCP 校验错误（对齐 TS `ValidationError`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    /// 相对文件路径。
    pub file: Option<String>,
    /// 字段路径（点分）。
    pub path: FieldPath,
    /// 人类可读错误消息。
    pub message: String,
    /// 期望值/类型。
    pub expected: Option<String>,
    /// 实际的非法值（invalid_type 时为收到的 *类型名* 字符串）。
    pub invalid_value: Option<Value>,
    /// 修复建议。
    pub suggestion: Option<String>,
    /// 相关文档链接。
    pub doc_link: Option<String>,
    /// MCP 错误元数据（仅 MCP 配置错误携带）。
    pub mcp_error_metadata: Option<McpErrorMetadata>,
}

/// 合并后的 settings + 错误集合（对齐 TS `SettingsWithErrors`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsWithErrors {
    pub settings: SettingsJson,
    pub errors: Vec<ValidationError>,
}

/// 获取未知值的类型字符串（对齐 TS `getReceivedType`）。
fn received_type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 从 zod 风格消息中提取 `received (\w+)`（对齐 `extractReceivedFromMessage`）。
fn extract_received_from_message(msg: &str) -> Option<String> {
    RECEIVED_RE
        .captures(msg)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// 数字格式化：整数去小数点（对齐 TS `String(number)`）。
fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// 将 schema issue 列表格式化为人类可读的校验错误（对齐 `formatZodError`）。
pub fn format_zod_error(issues: &[SchemaIssue], file_path: &str) -> Vec<ValidationError> {
    issues
        .iter()
        .map(|issue| {
            let path = issue.path.join(".");
            let mut message = issue.message.clone();
            let mut expected: Option<String> = None;

            let mut enum_values: Option<Vec<String>> = None;
            let mut received_value: Option<Value> = None;
            let mut invalid_value: Option<Value> = None;
            let mut expected_value: Option<String> = None;

            // 第一遍：按 issue 形状提取 expected / received（供 tip 生成）。
            match &issue.code {
                IssueCode::InvalidValue => {
                    enum_values = issue.values.as_ref().map(|vs| {
                        vs.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    });
                    expected_value = enum_values.as_ref().map(|v| v.join(" | "));
                }
                IssueCode::InvalidType => {
                    expected_value = issue.expected.clone();
                    let received_type = extract_received_from_message(&issue.message)
                        .or_else(|| issue.input.as_ref().map(|v| received_type_of(v).to_string()));
                    received_value = received_type.map(Value::String);
                    invalid_value = received_value.clone();
                }
                IssueCode::TooSmall => {
                    expected_value = issue.minimum.map(fmt_num);
                }
                IssueCode::Custom => {
                    if let Some(params) = &issue.params {
                        if let Some(received) = params.get("received") {
                            received_value = Some(received.clone());
                            invalid_value = received_value.clone();
                        }
                    }
                }
                IssueCode::UnrecognizedKeys
                | IssueCode::TooBig
                | IssueCode::Other(_) => {}
            }

            let tip: Option<ValidationTip> = get_validation_tip(&TipContext {
                path: path.clone(),
                code: issue_code_str(&issue.code),
                expected: expected_value.clone(),
                received: received_value.clone(),
                enum_values: enum_values.clone(),
                message: Some(issue.message.clone()),
                value: received_value.clone(),
            });

            // 第二遍：按 issue code 组装最终消息（对齐 formatZodError 的分支）。
            match &issue.code {
                IssueCode::InvalidValue => {
                    let quoted = enum_values
                        .map(|v| {
                            v.iter()
                                .map(|s| format!("\"{s}\""))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    expected = Some(quoted.clone());
                    message = format!("Invalid value. Expected one of: {quoted}");
                }
                IssueCode::InvalidType => {
                    let received_type = extract_received_from_message(&issue.message)
                        .or_else(|| issue.input.as_ref().map(|v| received_type_of(v).to_string()));
                    if let Some(rt) = received_type {
                        let exp = issue.expected.as_deref().unwrap_or("?");
                        if exp == "object" && rt == "null" && path.is_empty() {
                            message = "Invalid or malformed JSON".to_string();
                        } else {
                            message = format!("Expected {exp}, but received {rt}");
                        }
                    }
                }
                IssueCode::UnrecognizedKeys => {
                    let keys = issue.keys.join(", ");
                    message = format!(
                        "{}: {keys}",
                        plural(issue.keys.len() as u64, "Unrecognized field", None)
                    );
                }
                IssueCode::TooSmall => {
                    let min = issue.minimum.unwrap_or(0.0);
                    message = format!("Number must be greater than or equal to {}", fmt_num(min));
                    expected = Some(fmt_num(min));
                }
                IssueCode::TooBig | IssueCode::Custom | IssueCode::Other(_) => {}
            }

            ValidationError {
                file: Some(file_path.to_string()),
                path,
                message,
                expected,
                invalid_value,
                suggestion: tip.and_then(|t| t.suggestion),
                doc_link: tip.and_then(|t| t.doc_link),
                mcp_error_metadata: None,
            }
        })
        .collect()
}

fn issue_code_str(code: &IssueCode) -> String {
    match code {
        IssueCode::InvalidType => "invalid_type",
        IssueCode::InvalidValue => "invalid_value",
        IssueCode::UnrecognizedKeys => "unrecognized_keys",
        IssueCode::TooSmall => "too_small",
        IssueCode::TooBig => "too_big",
        IssueCode::Custom => "custom",
        IssueCode::Other(s) => s,
    }
    .to_string()
}

/// `validateSettingsFileContent` 的返回（对齐 TS 的判别联合）。
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateSettingsFileContent {
    Valid,
    Invalid { error: String, full_schema: String },
}

/// 校验 settings 文件内容是否符合 SettingsSchema（严格模式，用于文件编辑）。
pub fn validate_settings_file_content(content: &str) -> ValidateSettingsFileContent {
    let Ok(json_data) = json_parse(content) else {
        return ValidateSettingsFileContent::Invalid {
            error: "Invalid JSON: Unknown parsing error".to_string(),
            full_schema: generate_settings_json_schema(),
        };
    };

    match parse_settings_json_strict(&json_data) {
        Ok(_) => ValidateSettingsFileContent::Valid,
        Err(issues) => {
            let errors = format_zod_error(&issues, "settings");
            let error_message = "Settings validation failed:\n".to_string()
                + &errors
                    .iter()
                    .map(|err| format!("- {}: {}", err.path, err.message))
                    .collect::<Vec<_>>()
                    .join("\n");
            ValidateSettingsFileContent::Invalid {
                error: error_message,
                full_schema: generate_settings_json_schema(),
            }
        }
    }
}

/// 过滤原始 JSON 数据中的非法权限规则（schema 校验前调用），返回被移除
/// 规则的警告列表。**就地修改** `data`（对齐 TS 的 array.filter 重新赋值）。
pub fn filter_invalid_permission_rules(data: &mut Value, file_path: &str) -> Vec<ValidationError> {
    let Some(obj) = data.as_object_mut() else {
        return Vec::new();
    };
    let Some(perms_v) = obj.get_mut("permissions") else {
        return Vec::new();
    };
    let Some(perms) = perms_v.as_object_mut() else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    for key in ["allow", "deny", "ask"] {
        let Some(rules_v) = perms.get_mut(key) else {
            continue;
        };
        let Some(rules) = rules_v.as_array() else {
            continue;
        };

        let mut kept = Vec::with_capacity(rules.len());
        for rule in rules.iter() {
            let Some(rule_str) = rule.as_str() else {
                warnings.push(ValidationError {
                    file: Some(file_path.to_string()),
                    path: format!("permissions.{key}"),
                    message: format!("Non-string value in {key} array was removed"),
                    invalid_value: Some(rule.clone()),
                    expected: None,
                    suggestion: None,
                    doc_link: None,
                    mcp_error_metadata: None,
                });
                continue;
            };
            // PR-0a: 传入 behavior，使 vault 工具的 whole-tool allow 在 allow
            // 数组被拒、在 deny 数组仍可作 kill switch。
            let result = validate_permission_rule(rule_str, Some(key));
            if result.valid {
                kept.push(rule.clone());
                continue;
            }
            let mut message = format!("Invalid permission rule \"{rule_str}\" was skipped");
            if let Some(err) = &result.error {
                message.push_str(": ");
                message.push_str(err);
            }
            if let Some(suggestion) = &result.suggestion {
                message.push_str(". ");
                message.push_str(suggestion);
            }
            warnings.push(ValidationError {
                file: Some(file_path.to_string()),
                path: format!("permissions.{key}"),
                message,
                invalid_value: Some(Value::String(rule_str.to_string())),
                expected: None,
                suggestion: None,
                doc_link: None,
                mcp_error_metadata: None,
            });
        }
        perms.insert(key.to_string(), Value::Array(kept));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_invalid_value() {
        let issues = vec![SchemaIssue::invalid_value(vec!["modelType".to_string()], &["anthropic", "openai"])];
        let errs = format_zod_error(&issues, "f.json");
        assert_eq!(
            errs[0].message,
            "Invalid value. Expected one of: \"anthropic\", \"openai\""
        );
        assert_eq!(errs[0].file.as_deref(), Some("f.json"));
        assert_eq!(errs[0].path, "modelType");
    }

    #[test]
    fn formats_invalid_type_and_malformed_json() {
        let issues = vec![SchemaIssue::invalid_type(vec![], "object", &Value::Null)];
        let errs = format_zod_error(&issues, "f.json");
        assert_eq!(errs[0].message, "Invalid or malformed JSON");

        let issues = vec![SchemaIssue::invalid_type(
            vec!["model".to_string()],
            "string",
            &json!(42),
        )];
        let errs = format_zod_error(&issues, "f.json");
        assert_eq!(errs[0].message, "Expected string, but received number");
    }

    #[test]
    fn formats_unrecognized_keys_and_too_small() {
        let issues = vec![SchemaIssue::unrecognized_keys(
            vec![],
            vec!["foo".to_string(), "bar".to_string()],
        )];
        let errs = format_zod_error(&issues, "f.json");
        assert_eq!(errs[0].message, "Unrecognized fields: foo, bar");

        let issues = vec![SchemaIssue::too_small(
            vec!["cleanupPeriodDays".to_string()],
            0.0,
            &json!(-1),
        )];
        let errs = format_zod_error(&issues, "f.json");
        assert_eq!(errs[0].message, "Number must be greater than or equal to 0");
        assert_eq!(errs[0].expected.as_deref(), Some("0"));
    }

    #[test]
    fn validate_settings_content_paths() {
        let ok = validate_settings_file_content("{\"model\": \"claude-4\"}");
        assert!(matches!(ok, ValidateSettingsFileContent::Valid));

        let bad = validate_settings_file_content("{\"model\": 42}");
        match bad {
            ValidateSettingsFileContent::Invalid { error, full_schema } => {
                assert!(error.starts_with("Settings validation failed:"));
                assert!(full_schema.contains("\"$schema\""));
            }
            _ => panic!("expected invalid"),
        }

        let bad_json = validate_settings_file_content("{not json");
        match bad_json {
            ValidateSettingsFileContent::Invalid { error, .. } => {
                assert!(error.starts_with("Invalid JSON:"));
            }
            _ => panic!("expected invalid"),
        }
    }

    #[test]
    fn filters_invalid_permission_rules() {
        let mut data = json!({
            "permissions": {
                "allow": ["Bash(npm install)", "bash(bad)", 42, "VaultHttpFetch"],
                "deny": ["VaultHttpFetch"],
            }
        });
        let warnings = filter_invalid_permission_rules(&mut data, "f.json");
        // 非字符串 + 小写工具名 + allow 路径的 vault 整工具允许 → 3 条警告
        assert_eq!(warnings.len(), 3);
        let allow = data["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow, &vec![json!("Bash(npm install)")]);
        // deny 路径的整工具 VaultHttpFetch 保留（kill switch）
        assert_eq!(
            data["permissions"]["deny"].as_array().unwrap(),
            &vec![json!("VaultHttpFetch")]
        );
    }

    #[test]
    fn filter_noop_without_permissions() {
        let mut data = json!({"model": "claude-4"});
        assert!(filter_invalid_permission_rules(&mut data, "f.json").is_empty());
    }
}
