//! official_registry.rs — 官方 MCP registry 预取
//! （重写自 `officialRegistry.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `prefetch_official_mcp_urls()`：fire-and-forget 拉取
//!   `https://api.anthropic.com/mcp-registry/v0/servers?version=latest&visibility=commercial`
//!   （5s 超时），把 server.remotes[].url 规范化（去 query、去尾斜杠）后
//!   存入全局 Set。
//! - `is_official_mcp_url(normalized_url)`：registry 未加载时 fail-closed
//!   返回 false。
//! - `reset_official_mcp_urls_for_testing()`：清空缓存。
//!
//! 已知偏差：
//! - TS 用 axios；Rust 用 reqwest（Cargo.toml 已有）。
//! - `logForDebugging` / `errorMessage` 签名对齐已转换模块。

use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::services::mcp::utils::strip_url_query_and_slash;
use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// registry 响应中的服务器条目（duck-type TS `RegistryServer`）。
#[derive(Debug, Deserialize)]
struct RegistryServer {
    server: RegistryServerInfo,
}

#[derive(Debug, Deserialize)]
struct RegistryServerInfo {
    #[serde(default, rename = "remotes")]
    remotes: Vec<RegistryRemote>,
}

#[derive(Debug, Deserialize)]
struct RegistryRemote {
    url: String,
}

#[derive(Debug, Deserialize)]
struct RegistryResponse {
    servers: Vec<RegistryServer>,
}

const REGISTRY_URL: &str =
    "https://api.anthropic.com/mcp-registry/v0/servers?version=latest&visibility=commercial";
const FETCH_TIMEOUT_MS: u64 = 5000;

/// 已加载的官方 URL 集合（None = 未加载，fail-closed）。
static OFFICIAL_URLS: Lazy<Mutex<Option<Vec<String>>>> = Lazy::new(|| Mutex::new(None));

/// Fire-and-forget 预取官方 MCP registry。
pub async fn prefetch_official_mcp_urls() {
    if std::env::var("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").is_ok() {
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(FETCH_TIMEOUT_MS))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            log_for_debugging(
                &format!("Failed to fetch MCP registry: {e}"),
                DebugLogLevel::Error,
            );
            return;
        }
    };

    match client.get(REGISTRY_URL).send().await {
        Ok(response) => match response.json::<RegistryResponse>().await {
            Ok(body) => {
                let mut urls = Vec::new();
                for entry in body.servers {
                    for remote in entry.server.remotes {
                        let normalized = strip_url_query_and_slash(&remote.url);
                        if !normalized.is_empty() {
                            urls.push(normalized);
                        }
                    }
                }
                let count = urls.len();
                {
                    let mut cache = OFFICIAL_URLS.lock().unwrap_or_else(|e| e.into_inner());
                    *cache = Some(urls);
                }
                log_for_debugging(
                    &format!("[mcp-registry] Loaded {count} official MCP URLs"),
                    DebugLogLevel::Debug,
                );
            }
            Err(e) => {
                log_for_debugging(
                    &format!("Failed to fetch MCP registry: {e}"),
                    DebugLogLevel::Error,
                );
            }
        },
        Err(e) => {
            log_for_debugging(
                &format!("Failed to fetch MCP registry: {e}"),
                DebugLogLevel::Error,
            );
        }
    }
}

/// 判断（已规范化的）URL 是否在官方 registry 中。registry 未加载 → false。
pub fn is_official_mcp_url(normalized_url: &str) -> bool {
    let cache = OFFICIAL_URLS.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .as_ref()
        .map(|urls| urls.iter().any(|u| u == normalized_url))
        .unwrap_or(false)
}

/// 测试用：清空 registry 缓存。
pub fn reset_official_mcp_urls_for_testing() {
    let mut cache = OFFICIAL_URLS.lock().unwrap_or_else(|e| e.into_inner());
    *cache = None;
}
