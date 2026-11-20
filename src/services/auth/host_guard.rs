//! host_guard.rs — 多 auth 路由的 host 守卫（重写自
//! `src/services/auth/hostGuard.ts`）。
//!
//! 职责（对齐 TS）：
//! - `assertWorkspaceHost`：workspace API key 请求只允许发往
//!   `api.anthropic.com`（防凭证泄漏到第三方 host）。
//! - `assertSubscriptionBaseUrl`：订阅 OAuth 请求只允许
//!   `api.anthropic.com`（允许测试/预发中变化的 BASE_API_URL 相对宽松
//!   语义不变：仍只认该 host）。
//! - `assertNoAnthropicEnvForOpenAI`：OpenAI 兼容模式与
//!   ANTHROPIC_API_KEY 同时设置时告警（不抛错，调用方决策）。
//!
//! 已知偏差：
//! - TS 用 `new URL(url).hostname`；Rust 侧用极简解析器（无 URL 依赖）：
//!   支持 `scheme://[userinfo@]host[:port]/...` 的 host 提取，非法 URL
//!   与 TS 抛出等价为 `Err`。端口被剥离（hostname 语义）。

use std::error::Error;

use crate::utils::log::log_error;

/// workspace（非订阅）端点的权威 Anthropic API host。
const WORKSPACE_API_HOST: &str = "api.anthropic.com";

/// 供守卫错误使用的简单错误类型。
#[derive(Debug)]
pub struct HostGuardError {
    pub message: String,
}

impl HostGuardError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl std::fmt::Display for HostGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for HostGuardError {}

/// 断言 `url` 指向 Anthropic workspace API host。
///
/// 在每个 workspace API key 请求前调用（agents / vaults / memory_stores
/// / skills），防止 key 被发送到第三方 host。
pub fn assert_workspace_host(url: &str) -> Result<(), HostGuardError> {
    let hostname = url_hostname(url).ok_or_else(|| {
        HostGuardError::new(format!(
            "assertWorkspaceHost: invalid URL \"{url}\". Workspace API key requests must target {WORKSPACE_API_HOST}."
        ))
    })?;

    if hostname != WORKSPACE_API_HOST {
        return Err(HostGuardError::new(format!(
            "assertWorkspaceHost: refusing to send workspace API key to non-Anthropic host \"{hostname}\". \
             Workspace API key requests must target {WORKSPACE_API_HOST}. \
             If you are using a custom base URL, workspace endpoints are only available on the Anthropic API."
        )));
    }
    Ok(())
}

/// 断言 `url` 指向订阅 OAuth 的 base URL（仍只认 api.anthropic.com）。
pub fn assert_subscription_base_url(url: &str) -> Result<(), HostGuardError> {
    let hostname = url_hostname(url).ok_or_else(|| {
        HostGuardError::new(format!(
            "assertSubscriptionBaseUrl: invalid URL \"{url}\". Subscription OAuth requests must target {WORKSPACE_API_HOST}."
        ))
    })?;

    if hostname != WORKSPACE_API_HOST {
        return Err(HostGuardError::new(format!(
            "assertSubscriptionBaseUrl: refusing subscription OAuth request to non-Anthropic host \"{hostname}\". \
             Subscription OAuth requests must target {WORKSPACE_API_HOST}."
        )));
    }
    Ok(())
}

/// OpenAI 兼容配置与 Anthropic 环境变量同时设置时告警（不抛错）。
pub fn assert_no_anthropic_env_for_open_ai() {
    let has_open_ai_mode = std::env::var("CLAUDE_CODE_USE_OPENAI").as_deref() == Ok("1")
        || std::env::var("OPENAI_API_KEY").is_ok();
    let has_anthropic_key = std::env::var("ANTHROPIC_API_KEY").is_ok();

    if has_open_ai_mode && has_anthropic_key {
        log_error(HostGuardError::new(
            "assertNoAnthropicEnvForOpenAI: Both ANTHROPIC_API_KEY and OpenAI-compat mode are set. \
             ANTHROPIC_API_KEY is for Anthropic workspace endpoints (/v1/agents, /v1/vaults, /v1/memory_stores). \
             OpenAI-compat mode routes /v1/messages to a third-party provider. \
             These are separate credential planes and will not interfere, but verify this is intentional."
                .to_string(),
        ));
    }
}

/// 极简 URL hostname 提取：`scheme://[userinfo@]host[:port]/...`。
/// 无 scheme / 空 host 返回 None（与 `new URL()` 抛错对齐）。
fn url_hostname(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // 去 userinfo
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    // 去端口
    let host = authority.split(':').next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_host_accepts_anthropic_only() {
        assert!(assert_workspace_host("https://api.anthropic.com/v1/agents").is_ok());
        let err = assert_workspace_host("https://evil.example.com/v1/agents").unwrap_err();
        assert!(err.message.contains("evil.example.com"));
        assert!(assert_workspace_host("not a url").is_err());
    }

    #[test]
    fn subscription_host_accepts_anthropic_only() {
        assert!(assert_subscription_base_url("https://api.anthropic.com/").is_ok());
        assert!(assert_subscription_base_url("https://proxy.example.com/").is_err());
    }

    #[test]
    fn strips_userinfo_and_port() {
        assert_eq!(
            url_hostname("https://user:pass@api.anthropic.com:443/v1/x").as_deref(),
            Some("api.anthropic.com")
        );
        assert!(url_hostname("/relative/path").is_none());
        assert!(url_hostname("http://").is_none());
    }
}
