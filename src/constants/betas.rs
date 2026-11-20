//! betas.rs — Anthropic API beta header 常量。
//!
//! 重写自 `src/constants/betas.ts`。TS 原文件用 `feature('FLAG')`
//! （bun:bundle 运行时开关，环境变量 `FEATURE_<FLAG>=1` 启用）和
//! `process.env.USER_TYPE` 动态决定两个 header 的值；此处用 std::env
//! 等价实现。与 TS 在模块加载时求值一次不同，这些函数每次调用读取环境
//! 变量——进程内 feature flag 不会变更，语义等价。

pub const CLAUDE_CODE_20250219_BETA_HEADER: &str = "claude-code-20250219";
pub const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
pub const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";
pub const CONTEXT_MANAGEMENT_BETA_HEADER: &str = "context-management-2025-06-27";
pub const STRUCTURED_OUTPUTS_BETA_HEADER: &str = "structured-outputs-2025-12-15";
pub const WEB_SEARCH_BETA_HEADER: &str = "web-search-2025-03-05";
// Tool search beta headers differ by provider:
// - Claude API / Foundry: advanced-tool-use-2025-11-20
// - Vertex AI / Bedrock: tool-search-tool-2025-10-19
pub const SEARCH_EXTRA_TOOLS_BETA_HEADER_1P: &str = "advanced-tool-use-2025-11-20";
pub const SEARCH_EXTRA_TOOLS_BETA_HEADER_3P: &str = "tool-search-tool-2025-10-19";
pub const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";
pub const TASK_BUDGETS_BETA_HEADER: &str = "task-budgets-2026-03-13";
pub const PROMPT_CACHING_SCOPE_BETA_HEADER: &str = "prompt-caching-scope-2026-01-05";
pub const FAST_MODE_BETA_HEADER: &str = "fast-mode-2026-02-01";
pub const REDACT_THINKING_BETA_HEADER: &str = "redact-thinking-2026-02-12";
pub const TOKEN_EFFICIENT_TOOLS_BETA_HEADER: &str = "token-efficient-tools-2026-03-28";

/// `feature('FLAG')` 的等价实现：环境变量 `FEATURE_<FLAG>` 为真值时启用。
fn feature(flag: &str) -> bool {
    std::env::var(format!("FEATURE_{flag}"))
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

/// AFK 模式 beta header，受 `feature('TRANSCRIPT_CLASSIFIER')` 控制。
pub fn afk_mode_beta_header() -> &'static str {
    if feature("TRANSCRIPT_CLASSIFIER") {
        "afk-mode-2026-01-31"
    } else {
        ""
    }
}

/// CLI 内部 beta header，仅 `USER_TYPE=ant` 时非空。
pub fn cli_internal_beta_header() -> &'static str {
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        "cli-internal-2026-02-09"
    } else {
        ""
    }
}

pub const ADVISOR_BETA_HEADER: &str = "advisor-tool-2026-03-01";

/// Bedrock only supports a limited number of beta headers and only through
/// extraBodyParams. This list maintains the beta strings that should be in
/// Bedrock extraBodyParams *and not* in Bedrock headers.
pub const BEDROCK_EXTRA_PARAMS_HEADERS: &[&str] = &[
    INTERLEAVED_THINKING_BETA_HEADER,
    CONTEXT_1M_BETA_HEADER,
    SEARCH_EXTRA_TOOLS_BETA_HEADER_3P,
];

/// Betas allowed on Vertex countTokens API.
/// Other betas will cause 400 errors.
pub const VERTEX_COUNT_TOKENS_ALLOWED_BETAS: &[&str] = &[
    CLAUDE_CODE_20250219_BETA_HEADER,
    INTERLEAVED_THINKING_BETA_HEADER,
    CONTEXT_MANAGEMENT_BETA_HEADER,
];

/// `CACHE_EDITING_BETA_HEADER`（TS 中恒为空字符串）。
pub const CACHE_EDITING_BETA_HEADER: &str = "";

/// 判断给定 beta header 是否属于 Bedrock extraBodyParams 集合。
pub fn is_bedrock_extra_params_header(header: &str) -> bool {
    BEDROCK_EXTRA_PARAMS_HEADERS.contains(&header)
}

/// 判断给定 beta header 是否被 Vertex countTokens API 接受。
pub fn is_vertex_count_tokens_allowed_beta(header: &str) -> bool {
    VERTEX_COUNT_TOKENS_ALLOWED_BETAS.contains(&header)
}
