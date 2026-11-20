//! claudeai.rs — claude.ai org 配置的 MCP 服务器拉取
//! （重写自 `claudeai.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 门控：`ENABLE_CLAUDEAI_MCP_SERVERS` 为 falsy 则禁用；无 OAuth
//!   access token 则返回空；缺少 `user:mcp_servers` scope 返回空。
//! - 拉取 `{BASE_API_URL}/v1/mcp_servers?limit=1000`（5s 超时，
//!   `anthropic-beta: mcp-servers-2025-12-04`）。
//! - 结果按 `normalizeNameForMCP` 去重，冲突加 `(2)`、`(3)`… 后缀。
//! - 会话级 memoize（fetch 一次）；clearClaudeAIMcpConfigsCache 清缓存 +
//!   清 MCP auth 缓存。
//! - markClaudeAiMcpConnected / hasClaudeAiMcpEverConnected：记录曾成功
//!   连接的 connector（`~/.claude.json` 的 claudeAiMcpEverConnected）。
//!
//! 已知偏差：
//! - TS 用 axios；Rust 用 reqwest。
//! - memoize 用 `Mutex<Option<…>>`（进程内一次-fetch；竞态下最多多拉一次）。
//! - analytics（logEvent）未转换 → 事件占位（debug 日志）。

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::constants::oauth::get_oauth_config;
use crate::services::mcp::config::{get_global_config, update_global_config};
use crate::services::mcp::normalization::normalize_name_for_mcp;
use crate::services::mcp::types::{
    ConfigScope, McpClaudeAIProxyServerConfig, McpServerConfig, ScopedMcpServerConfig,
};
use crate::utils::auth::get_claude_ai_oauth_tokens;
use crate::utils::debug::log_for_debugging;
use crate::utils::env_utils::is_env_defined_falsy;
use crate::utils::debug::DebugLogLevel;

const FETCH_TIMEOUT_MS: u64 = 5000;
const MCP_SERVERS_BETA_HEADER: &str = "mcp-servers-2025-12-04";

/// claude.ai MCP 服务器条目（duck-type TS `ClaudeAIMcpServer`）。
#[derive(Debug, Deserialize)]
struct ClaudeAiMcpServer {
    id: String,
    display_name: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeAiMcpServersResponse {
    data: Vec<ClaudeAiMcpServer>,
}

/// 会话级缓存（对齐 TS `memoize`：fetch 一次）。
static CACHE: once_cell::sync::Lazy<tokio::sync::Mutex<Option<HashMap<String, ScopedMcpServerConfig>>>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(None));

/// 拉取 claude.ai org 配置的 MCP 服务器（会话内 memoize）。
pub async fn fetch_claude_ai_mcp_configs_if_eligible(
) -> HashMap<String, ScopedMcpServerConfig> {
    {
        let cache = CACHE.lock().await;
        if let Some(cached) = cache.as_ref() {
            return cached.clone();
        }
    }

    let configs = fetch_inner().await;

    let mut cache = CACHE.lock().await;
    *cache = Some(configs.clone());
    configs
}

async fn fetch_inner() -> HashMap<String, ScopedMcpServerConfig> {
    if is_env_defined_falsy(std::env::var("ENABLE_CLAUDEAI_MCP_SERVERS").ok().as_deref()) {
        log_for_debugging("[claudeai-mcp] Disabled via env var", DebugLogLevel::Debug);
        return HashMap::new();
    }

    let Some(tokens) = get_claude_ai_oauth_tokens() else {
        log_for_debugging("[claudeai-mcp] No access token", DebugLogLevel::Debug);
        return HashMap::new();
    };

    // 直接检查 user:mcp_servers scope（非交互模式 isClaudeAISubscriber 会误判）
    let has_scope = tokens.scopes.iter().any(|s| s == "user:mcp_servers");
    if !has_scope {
        log_for_debugging(
            &format!(
                "[claudeai-mcp] Missing user:mcp_servers scope (scopes={})",
                if tokens.scopes.is_empty() {
                    "none".to_string()
                } else {
                    tokens.scopes.join(",")
                }
            ),
            DebugLogLevel::Debug,
        );
        return HashMap::new();
    }

    let oauth_config = match get_oauth_config() {
        Ok(config) => config,
        Err(_) => return HashMap::new(),
    };
    let url = format!("{}/v1/mcp_servers?limit=1000", oauth_config.base_api_url);

    log_for_debugging(&format!("[claudeai-mcp] Fetching from {url}"), DebugLogLevel::Debug);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(FETCH_TIMEOUT_MS))
        .build()
    {
        Ok(client) => client,
        Err(_) => return HashMap::new(),
    };

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", tokens.access_token))
        .header("Content-Type", "application/json")
        .header("anthropic-beta", MCP_SERVERS_BETA_HEADER)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(e) => {
            log_for_debugging(
                &format!("[claudeai-mcp] Fetch failed: {e}"),
                DebugLogLevel::Debug,
            );
            return HashMap::new();
        }
    };

    let body: ClaudeAiMcpServersResponse = match response.json().await {
        Ok(body) => body,
        Err(e) => {
            log_for_debugging(
                &format!("[claudeai-mcp] Fetch failed: {e}"),
                DebugLogLevel::Debug,
            );
            return HashMap::new();
        }
    };

    let mut configs = HashMap::new();
    // 已使用的规范化名（含后缀），处理 `Example Server 2` 与
    // `Example Server! (2)` 都规范化到同一名字的边缘情况
    let mut used_normalized: std::collections::HashSet<String> = std::collections::HashSet::new();

    for server in body.data {
        let base_name = format!("claude.ai {}", server.display_name);
        let mut final_name = base_name.clone();
        let mut final_normalized = normalize_name_for_mcp(&final_name);
        let mut count = 1;
        while used_normalized.contains(&final_normalized) {
            count += 1;
            final_name = format!("{base_name} ({count})");
            final_normalized = normalize_name_for_mcp(&final_name);
        }
        used_normalized.insert(final_normalized.clone());

        configs.insert(
            final_name,
            ScopedMcpServerConfig::new(
                McpServerConfig::ClaudeAiProxy(McpClaudeAIProxyServerConfig {
                    type_field: "claudeai-proxy".to_string(),
                    url: server.url,
                    id: server.id,
                }),
                ConfigScope::Claudeai,
            ),
        );
    }

    log_for_debugging(
        &format!("[claudeai-mcp] Fetched {} servers", configs.len()),
        DebugLogLevel::Debug,
    );
    configs
}

/// 清除会话级缓存（登录后调用，强制下次刷新）。
pub async fn clear_claude_ai_mcp_configs_cache() {
    {
        let mut cache = CACHE.lock().await;
        *cache = None;
    }
    // 同时清 MCP auth 缓存，使刚授权的服务器可以重连
    crate::services::mcp::client::clear_mcp_auth_cache();
}

/// 记录某 claude.ai connector 曾成功连接（幂等）。
pub fn mark_claude_ai_mcp_connected(name: &str) {
    update_global_config(|current| {
        let seen: Vec<String> = current
            .get("claudeAiMcpEverConnected")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if seen.iter().any(|n| n == name) {
            return current.clone();
        }
        let mut next = seen.clone();
        next.push(name.to_string());
        let mut updated = current.clone();
        updated["claudeAiMcpEverConnected"] = json!(next);
        updated
    });
}

/// 某 claude.ai connector 是否曾成功连接过。
pub fn has_claude_ai_mcp_ever_connected(name: &str) -> bool {
    get_global_config()
        .get("claudeAiMcpEverConnected")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().any(|v| v.as_str() == Some(name)))
        .unwrap_or(false)
}

/// claude.ai 代理 fetch 的 Authorization 头 provider（对齐 TS
/// `createClaudeAiProxyFetch`：检查 token 刷新 → 附 Bearer → 401 时
/// `handleOAuth401Error` 强刷一次）。
///
/// 返回同步求值的 header provider：每次请求前同步读当前 token。
/// 主动刷新（checkAndRefreshOAuthTokenIfNeeded）由连接流程在
/// transport 构造前调用一次。
pub fn claudeai_proxy_header_provider()
-> std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync> {
    std::sync::Arc::new(|| {
        crate::utils::auth::get_claude_ai_oauth_tokens().map(|t| t.access_token)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_name_helpers() {
        assert_eq!(normalize_name_for_mcp("claude.ai Slack"), "claude_ai_Slack");
        assert_eq!(normalize_name_for_mcp("claude.ai Slack (2)"), "claude_ai_Slack_2");
    }
}
