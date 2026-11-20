//! managed_env_constants.rs — 推理路由相关环境变量清单。
//!
//! 重写自 `src/utils/managedEnvConstants.ts`。
//!
//! 当 spawn env 中 CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST 为真值时，
//! 这些变量将从 settings 来源的 env 中剥离，避免宿主的路由配置被
//! 用户的 ~/.claude/settings.json 覆盖。
//!
//! @[MODEL LAUNCH]: 新模型通常无需改动 —— VERTEX_REGION_CLAUDE_* 走
//! 前缀匹配；新增 provider 或路由配置变量（endpoint、project、region、
//! auth）才需要改这里。

use std::collections::HashSet;
use std::sync::OnceLock;

/// 由宿主托管的 provider 路由/鉴权/模型默认值变量。
fn provider_managed_env_vars() -> &'static HashSet<&'static str> {
    static VARS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    VARS.get_or_init(|| {
        [
            // flag 本身 —— 宿主设置后 settings 不能反悔
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            // Provider 选择
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_GEMINI",
            // Endpoint 配置（base URL、project/resource 标识）
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
            "ANTHROPIC_FOUNDRY_BASE_URL",
            "ANTHROPIC_FOUNDRY_RESOURCE",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "GEMINI_BASE_URL",
            // Region 路由（逐模型 VERTEX_REGION_CLAUDE_* 由下面的前缀处理）
            "CLOUD_ML_REGION",
            // 鉴权
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
            "ANTHROPIC_FOUNDRY_API_KEY",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
            "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
            "GEMINI_API_KEY",
            // 模型默认值 —— 常为 provider 专属 ID 格式
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            // OpenAI provider 专属
            "OPENAI_AUTH_MODE",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "OPENAI_DEFAULT_HAIKU_MODEL",
            "OPENAI_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_HAIKU_MODEL_NAME",
            "OPENAI_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_DEFAULT_OPUS_MODEL",
            "OPENAI_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_OPUS_MODEL_NAME",
            "OPENAI_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_DEFAULT_SONNET_MODEL",
            "OPENAI_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_SONNET_MODEL_NAME",
            "OPENAI_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_SMALL_FAST_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION",
            "CLAUDE_CODE_SUBAGENT_MODEL",
            "GEMINI_MODEL",
            "GEMINI_SMALL_FAST_MODEL",
            // Gemini provider 专属 —— 与 Anthropic/OpenAI 分开
            "GEMINI_DEFAULT_HAIKU_MODEL",
            "GEMINI_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_HAIKU_MODEL_NAME",
            "GEMINI_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "GEMINI_DEFAULT_OPUS_MODEL",
            "GEMINI_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_OPUS_MODEL_NAME",
            "GEMINI_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "GEMINI_DEFAULT_SONNET_MODEL",
            "GEMINI_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_SONNET_MODEL_NAME",
            "GEMINI_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        ]
        .into_iter()
        .collect()
    })
}

/// provider 托管变量前缀：逐模型 Vertex region 覆盖，随模型发布扩展，
/// 用前缀匹配避免每次发布漂移。
const PROVIDER_MANAGED_ENV_PREFIXES: &[&str] = &["VERTEX_REGION_CLAUDE_"];

/// 变量是否属于 provider 托管集合（大小写不敏感）。
pub fn is_provider_managed_env_var(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    provider_managed_env_vars().contains(upper.as_str())
        || PROVIDER_MANAGED_ENV_PREFIXES
            .iter()
            .any(|p| upper.starts_with(p))
}

/// 可执行任意 shell 代码的危险 shell 设置项。
pub const DANGEROUS_SHELL_SETTINGS: &[&str] = &[
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
    "otelHeadersHelper",
    "statusLine",
];

/// 在 trust dialog 之前可以安全应用的安全环境变量。
///
/// 重要：这是"哪些 env var 安全"的唯一事实来源。不在此列表中的
/// env var 都视为危险，经远程托管设置设置时会触发安全对话框。
fn safe_env_vars() -> &'static HashSet<&'static str> {
    static VARS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    VARS.get_or_init(|| {
        [
            "ANTHROPIC_CUSTOM_HEADERS",
            "ANTHROPIC_CUSTOM_MODEL_OPTION",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            // OpenAI provider 专属
            "OPENAI_API_KEY",
            "OPENAI_AUTH_MODE",
            "OPENAI_BASE_URL",
            "OPENAI_DEFAULT_HAIKU_MODEL",
            "OPENAI_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_HAIKU_MODEL_NAME",
            "OPENAI_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_DEFAULT_OPUS_MODEL",
            "OPENAI_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_OPUS_MODEL_NAME",
            "OPENAI_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_DEFAULT_SONNET_MODEL",
            "OPENAI_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "OPENAI_DEFAULT_SONNET_MODEL_NAME",
            "OPENAI_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "OPENAI_ENABLE_THINKING",
            "OPENAI_MAX_TOKENS",
            "OPENAI_MODEL",
            "OPENAI_ORG_ID",
            "OPENAI_PROJECT_ID",
            "OPENAI_SMALL_FAST_MODEL",
            // Grok provider 专属
            "GROK_API_KEY",
            "GROK_BASE_URL",
            "GROK_DEFAULT_HAIKU_MODEL",
            "GROK_DEFAULT_OPUS_MODEL",
            "GROK_DEFAULT_SONNET_MODEL",
            "GROK_MODEL",
            "GROK_MODEL_MAP",
            "XAI_API_KEY",
            "ANTHROPIC_FOUNDRY_API_KEY",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "AWS_DEFAULT_REGION",
            "AWS_PROFILE",
            "AWS_REGION",
            "BASH_DEFAULT_TIMEOUT_MS",
            "BASH_MAX_OUTPUT_LENGTH",
            "BASH_MAX_TIMEOUT_MS",
            "CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR",
            "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_DISABLE_TERMINAL_TITLE",
            "CLAUDE_CODE_ENABLE_TELEMETRY",
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
            "CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
            "CLAUDE_CODE_SUBAGENT_MODEL",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_GEMINI",
            "CLAUDE_CODE_USE_GROK",
            "CLAUDE_CODE_USE_OPENAI",
            "CLAUDE_CODE_USE_VERTEX",
            "GEMINI_API_KEY",
            "GEMINI_BASE_URL",
            "GEMINI_MODEL",
            "GEMINI_SMALL_FAST_MODEL",
            "GEMINI_DEFAULT_HAIKU_MODEL",
            "GEMINI_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_HAIKU_MODEL_NAME",
            "GEMINI_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "GEMINI_DEFAULT_OPUS_MODEL",
            "GEMINI_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_OPUS_MODEL_NAME",
            "GEMINI_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "GEMINI_DEFAULT_SONNET_MODEL",
            "GEMINI_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "GEMINI_DEFAULT_SONNET_MODEL_NAME",
            "GEMINI_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "DISABLE_AUTOUPDATER",
            "DISABLE_BUG_COMMAND",
            "DISABLE_COST_WARNINGS",
            "DISABLE_ERROR_REPORTING",
            "DISABLE_FEEDBACK_COMMAND",
            "DISABLE_TELEMETRY",
            "ENABLE_SEARCH_EXTRA_TOOLS",
            "MAX_MCP_OUTPUT_TOKENS",
            "MAX_THINKING_TOKENS",
            "MCP_TIMEOUT",
            "MCP_TOOL_TIMEOUT",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_CLIENT_CERTIFICATE",
            "OTEL_EXPORTER_OTLP_METRICS_CLIENT_KEY",
            "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "OTEL_LOG_TOOL_DETAILS",
            "OTEL_LOG_USER_PROMPTS",
            "OTEL_LOGS_EXPORT_INTERVAL",
            "OTEL_LOGS_EXPORTER",
            "OTEL_METRIC_EXPORT_INTERVAL",
            "OTEL_METRICS_EXPORTER",
            "OTEL_METRICS_INCLUDE_ACCOUNT_UUID",
            "OTEL_METRICS_INCLUDE_SESSION_ID",
            "OTEL_METRICS_INCLUDE_VERSION",
            "OTEL_RESOURCE_ATTRIBUTES",
            "VERTEX_REGION_CLAUDE_3_5_HAIKU",
            "VERTEX_REGION_CLAUDE_3_5_SONNET",
            "VERTEX_REGION_CLAUDE_3_7_SONNET",
            "VERTEX_REGION_CLAUDE_4_0_OPUS",
            "VERTEX_REGION_CLAUDE_4_0_SONNET",
            "VERTEX_REGION_CLAUDE_4_1_OPUS",
            "VERTEX_REGION_CLAUDE_4_5_SONNET",
            "VERTEX_REGION_CLAUDE_4_6_SONNET",
            "VERTEX_REGION_CLAUDE_HAIKU_4_5",
        ]
        .into_iter()
        .collect()
    })
}

/// 变量是否属于"可安全应用"集合。
pub fn is_safe_env_var(key: &str) -> bool {
    safe_env_vars().contains(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_managed_matching() {
        assert!(is_provider_managed_env_var("ANTHROPIC_BASE_URL"));
        assert!(is_provider_managed_env_var("anthropic_base_url"));
        assert!(is_provider_managed_env_var(
            "VERTEX_REGION_CLAUDE_4_5_SONNET"
        ));
        assert!(!is_provider_managed_env_var("BASH_DEFAULT_TIMEOUT_MS"));
    }

    #[test]
    fn safe_vars() {
        assert!(is_safe_env_var("BASH_DEFAULT_TIMEOUT_MS"));
        // 重定向到攻击者控制服务器的变量不在安全集合
        assert!(!is_safe_env_var("ANTHROPIC_BASE_URL"));
        assert!(!is_safe_env_var("HTTPS_PROXY"));
        assert!(!is_safe_env_var("NODE_TLS_REJECT_UNAUTHORIZED"));
    }
}
