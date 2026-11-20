//! validation_tips.rs — settings 校验错误的人类可读提示。
//!
//! 重写自 `src/utils/settings/validationTips.ts`。零业务依赖（TS 侧仅
//! 导入 zod 的 `ZodIssueCode` 类型；Rust 无 zod，issue code 改为字符串，
//! 与 zod v4 的 code 值一一对应：`invalid_type` / `invalid_value` /
//! `too_small` / `unrecognized_keys`）。
//!
//! 语义对齐：
//! - `TIP_MATCHERS` 为有序匹配器数组，取第一个匹配项（`find` 语义）；
//! - 命中后两条后处理规则：`invalid_value` + 有 `enumValues` + 无
//!   suggestion → 用枚举值生成 suggestion；无 docLink 时按 path 的
//!   第一段从 `PATH_DOC_LINKS` 补链接。

use serde_json::Value;

/// 校验提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationTip {
    pub suggestion: Option<String>,
    pub doc_link: Option<String>,
}

/// 匹配提示所需的上下文（TS `TipContext`）。
#[derive(Debug, Clone)]
pub struct TipContext {
    pub path: String,
    pub code: String,
    pub expected: Option<String>,
    pub received: Option<Value>,
    pub enum_values: Option<Vec<String>>,
    pub message: Option<String>,
    pub value: Option<Value>,
}

/// 静态提示（静态字符串，避免每次调用分配 String）。
struct StaticTip {
    suggestion: Option<&'static str>,
    doc_link: Option<&'static str>,
}


/// 模块内部的提示匹配器表（TS `TIP_MATCHERS`），顺序即优先级。
static TIP_MATCHERS: &[(fn(&TipContext) -> bool, StaticTip)] = &[
    (
        |ctx| ctx.path == "permissions.defaultMode" && ctx.code == "invalid_value",
        StaticTip {
            suggestion: Some(
                "Valid modes: \"acceptEdits\" (ask before file changes), \"plan\" (analysis only), \"bypassPermissions\" (auto-accept all), or \"default\" (standard behavior)",
            ),
            doc_link: Some("https://code.claude.com/docs/en/iam#permission-modes"),
        },
    ),
    (
        |ctx| ctx.path == "apiKeyHelper" && ctx.code == "invalid_type",
        StaticTip {
            suggestion: Some(
                "Provide a shell command that outputs your API key to stdout. The script should output only the API key. Example: \"/bin/generate_temp_api_key.sh\"",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| {
            ctx.path == "cleanupPeriodDays"
                && ctx.code == "too_small"
                && ctx.expected.as_deref() == Some("0")
        },
        StaticTip {
            suggestion: Some(
                "Must be 0 or greater. Set a positive number for days to retain transcripts (default is 30). Setting 0 disables session persistence entirely: no transcripts are written and existing transcripts are deleted at startup.",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| ctx.path.starts_with("env.") && ctx.code == "invalid_type",
        StaticTip {
            suggestion: Some(
                "Environment variables must be strings. Wrap numbers and booleans in quotes. Example: \"DEBUG\": \"true\", \"PORT\": \"3000\"",
            ),
            doc_link: Some("https://code.claude.com/docs/en/settings#environment-variables"),
        },
    ),
    (
        |ctx| {
            (ctx.path == "permissions.allow" || ctx.path == "permissions.deny")
                && ctx.code == "invalid_type"
                && ctx.expected.as_deref() == Some("array")
        },
        StaticTip {
            suggestion: Some(
                "Permission rules must be in an array. Format: [\"Tool(specifier)\"]. Examples: [\"Bash(npm run build)\", \"Edit(docs/**)\", \"Read(~/.zshrc)\"]. Use * for wildcards.",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| ctx.path.contains("hooks") && ctx.code == "invalid_type",
        StaticTip {
            // gh-31187 / CC-282: prior example showed {"matcher": {"tools": ["BashTool"]}}
            // — an object format that never existed in the schema (matcher is z.string(),
            // always has been). Users copied the tip's example and got the same validation
            // error again. See matchesPattern() in hooks.ts: matcher is exact-match,
            // pipe-separated ("Edit|Write"), or regex. Empty/"*" matches all.
            suggestion: Some(
                "Hooks use a matcher + hooks array. The matcher is a string: a tool name (\"Bash\"), pipe-separated list (\"Edit|Write\"), or empty to match all. Example: {\"PostToolUse\": [{\"matcher\": \"Edit|Write\", \"hooks\": [{\"type\": \"command\", \"command\": \"echo Done\"}]}]}",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| ctx.code == "invalid_type" && ctx.expected.as_deref() == Some("boolean"),
        StaticTip {
            suggestion: Some(
                "Use true or false without quotes. Example: \"includeCoAuthoredBy\": true",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| ctx.code == "unrecognized_keys",
        StaticTip {
            suggestion: Some(
                "Check for typos or refer to the documentation for valid fields",
            ),
            doc_link: Some("https://code.claude.com/docs/en/settings"),
        },
    ),
    (
        |ctx| ctx.code == "invalid_value" && ctx.enum_values.is_some(),
        StaticTip {
            suggestion: None,
            doc_link: None,
        },
    ),
    (
        |ctx| {
            ctx.code == "invalid_type"
                && ctx.expected.as_deref() == Some("object")
                && ctx.received.as_ref() == Some(&Value::Null)
                && ctx.path.is_empty()
        },
        StaticTip {
            suggestion: Some(
                "Check for missing commas, unmatched brackets, or trailing commas. Use a JSON validator to identify the exact syntax error.",
            ),
            doc_link: None,
        },
    ),
    (
        |ctx| ctx.path == "permissions.additionalDirectories" && ctx.code == "invalid_type",
        StaticTip {
            suggestion: Some(
                "Must be an array of directory paths. Example: [\"~/projects\", \"/tmp/workspace\"]. You can also use --add-dir flag or /add-dir command",
            ),
            doc_link: Some("https://code.claude.com/docs/en/iam#working-directories"),
        },
    ),
];

/// 按 path 第一段补文档链接（TS `PATH_DOC_LINKS`）。
static PATH_DOC_LINKS: &[(&str, &str)] = &[
    (
        "permissions",
        "https://code.claude.com/docs/en/iam#configuring-permissions",
    ),
    (
        "env",
        "https://code.claude.com/docs/en/settings#environment-variables",
    ),
    ("hooks", "https://code.claude.com/docs/en/hooks"),
];

pub fn get_validation_tip(context: &TipContext) -> Option<ValidationTip> {
    let matcher = TIP_MATCHERS
        .iter()
        .find(|(matches, _)| matches(context))?;

    let mut tip = ValidationTip {
        suggestion: matcher.1.suggestion.map(str::to_string),
        doc_link: matcher.1.doc_link.map(str::to_string),
    };

    if context.code == "invalid_value" {
        if let Some(enum_values) = &context.enum_values {
            if tip.suggestion.is_none() {
                tip.suggestion = Some(format!(
                    "Valid values: {}",
                    enum_values
                        .iter()
                        .map(|v| format!("\"{v}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    // Add documentation link based on path prefix
    if tip.doc_link.is_none() && !context.path.is_empty() {
        let path_prefix = context.path.split('.').next().unwrap_or("");
        if !path_prefix.is_empty() {
            tip.doc_link = PATH_DOC_LINKS
                .iter()
                .find(|(key, _)| *key == path_prefix)
                .map(|(_, link)| (*link).to_string());
        }
    }

    Some(tip)
}
