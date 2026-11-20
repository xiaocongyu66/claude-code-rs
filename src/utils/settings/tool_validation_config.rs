//! tool_validation_config.rs — 权限规则的工具校验配置。
//!
//! 重写自 `src/utils/settings/toolValidationConfig.ts`。零依赖模块，
//! 纯数据 + 纯函数。
//!
//! 语义对齐：
//! - TS 的 `customValidation` 是 `{ [toolName]: (content) => result }`
//!   映射；Rust 侧用 `(&str, fn(&str) -> CustomValidationResult)` 静态
//!   切片承载，`get_custom_validation` 返回函数指针；
//! - 结果对象的 `error` / `suggestion` / `examples` 均为可选字段。

/// 自定义校验结果（TS 内联返回类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomValidationResult {
    pub valid: bool,
    pub error: Option<String>,
    pub suggestion: Option<String>,
    pub examples: Option<Vec<String>>,
}

/// 工具校验配置（TS `ToolValidationConfig`）。
pub struct ToolValidationConfig {
    /// 接受文件 glob 模式的工具（如 `*.ts`、`src/**`）
    pub file_pattern_tools: &'static [&'static str],
    /// 接受 bash 通配模式（任意位置 `*` 与 legacy `:*` 前缀）的工具
    pub bash_prefix_tools: &'static [&'static str],
    /// 特定工具的自定义校验规则
    pub custom_validation: &'static [(&'static str, fn(&str) -> CustomValidationResult)],
}

pub static TOOL_VALIDATION_CONFIG: ToolValidationConfig = ToolValidationConfig {
    // File pattern tools (accept *.ts, src/**, etc.)
    file_pattern_tools: &[
        "Read",
        "Write",
        "Edit",
        "Glob",
        "NotebookRead",
        "NotebookEdit",
        "Cd",
    ],
    // Bash wildcard tools (accept * anywhere, and legacy command:* syntax)
    bash_prefix_tools: &["Bash"],
    // Custom validation (only if needed)
    custom_validation: &[
        // WebSearch doesn't support wildcards or complex patterns
        ("WebSearch", validate_web_search),
        // WebFetch uses domain: prefix for hostname-based permissions
        ("WebFetch", validate_web_fetch),
    ],
};

/// WebSearch 不支持通配符。
fn validate_web_search(content: &str) -> CustomValidationResult {
    if content.contains('*') || content.contains('?') {
        return CustomValidationResult {
            valid: false,
            error: Some("WebSearch does not support wildcards".to_string()),
            suggestion: Some("Use exact search terms without * or ?".to_string()),
            examples: Some(vec![
                "WebSearch(claude ai)".to_string(),
                "WebSearch(typescript tutorial)".to_string(),
            ]),
        };
    }
    CustomValidationResult {
        valid: true,
        error: None,
        suggestion: None,
        examples: None,
    }
}

/// WebFetch 权限使用 `domain:` 前缀（主机名形式），不支持 URL。
fn validate_web_fetch(content: &str) -> CustomValidationResult {
    // Check if it's trying to use a URL format
    if content.contains("://") || content.starts_with("http") {
        return CustomValidationResult {
            valid: false,
            error: Some("WebFetch permissions use domain format, not URLs".to_string()),
            suggestion: Some("Use \"domain:hostname\" format".to_string()),
            examples: Some(vec![
                "WebFetch(domain:example.com)".to_string(),
                "WebFetch(domain:github.com)".to_string(),
            ]),
        };
    }

    // Must start with domain: prefix
    if !content.starts_with("domain:") {
        return CustomValidationResult {
            valid: false,
            error: Some(
                "WebFetch permissions must use \"domain:\" prefix".to_string(),
            ),
            suggestion: Some("Use \"domain:hostname\" format".to_string()),
            examples: Some(vec![
                "WebFetch(domain:example.com)".to_string(),
                "WebFetch(domain:*.google.com)".to_string(),
            ]),
        };
    }

    // Allow wildcards in domain patterns
    // Valid: domain:*.example.com, domain:example.*, etc.
    CustomValidationResult {
        valid: true,
        error: None,
        suggestion: None,
        examples: None,
    }
}

/// Helper to check if a tool uses file patterns
pub fn is_file_pattern_tool(tool_name: &str) -> bool {
    TOOL_VALIDATION_CONFIG
        .file_pattern_tools
        .contains(&tool_name)
}

/// Helper to check if a tool uses bash prefix patterns
pub fn is_bash_prefix_tool(tool_name: &str) -> bool {
    TOOL_VALIDATION_CONFIG.bash_prefix_tools.contains(&tool_name)
}

/// Helper to get custom validation for a tool
pub fn get_custom_validation(tool_name: &str) -> Option<fn(&str) -> CustomValidationResult> {
    TOOL_VALIDATION_CONFIG
        .custom_validation
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, f)| *f)
}
