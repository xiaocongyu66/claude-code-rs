//! completions.rs — MCP completions 能力（重写自 `completions.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `supports_completions`：capabilities.completions 协商；未声明的服务器
//!   永不收到 completion/complete 请求。
//! - `fetch_resource_templates_for_client`：`resources/templates/list`
//!   枚举，按服务器身份（name::scope::endpoint）做 LRU 缓存（大小 20，
//!   与 resources/list 缓存一致）；resources 子能力未声明时返回 []。
//! - `clear_resource_templates_cache`：按服务器名清除全部身份键。
//! - `complete_resource_template`：`completion/complete` 填充模板 URI 变量，
//!   任意失败降级为 []；context 仅在有前置参数时携带。
//!
//! 已知偏差：
//! - `utils/memoize.rs` 的 LRU 是同步 Fn；TS 端缓存的是 async 函数。
//!   Rust 侧改为模块内 `Mutex<HashMap>` + 手写 20 条上限（LRU 位置语义：
//!   超限时删除最早插入的键——approximation，非严格 LRU）。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::services::mcp::client::{McpClient, McpClientError};
use crate::services::mcp::types::{
    CompletionResult, ListResourceTemplatesResult, ServerCapabilities, ServerResourceTemplate,
    ScopedMcpServerConfig,
};
use crate::utils::log::log_mcp_error;

/// 每服务器模板缓存大小（对齐 TS `COMPLETIONS_CACHE_SIZE`）。
pub const COMPLETIONS_CACHE_SIZE: usize = 20;

/// 连接的最小结构视图（避免硬依赖完整 MCPServerConnection 联合）。
pub struct CompletionConnection {
    pub name: String,
    pub client: Arc<McpClient>,
    pub capabilities: Option<ServerCapabilities>,
    /// W6：config 身份（scope + endpoint 指纹）。真实连接必带；mock 视图
    /// 可省略（键退化为 displayName）。
    pub config: Option<ScopedMcpServerConfig>,
}

/// 缓存键：服务器身份而非 displayName —— 同名不同 config（project vs
/// user scope、两个 stdio 命令、不同 URL）绝不能串读模板缓存。
pub fn completions_cache_key(connection: &CompletionConnection) -> String {
    let Some(config) = &connection.config else {
        return connection.name.clone();
    };
    let endpoint = match &config.config {
        crate::services::mcp::types::McpServerConfig::SSE(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::HTTP(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::WebSocket(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::SSEIde(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::WSIde(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::ClaudeAiProxy(c) => c.url.clone(),
        crate::services::mcp::types::McpServerConfig::Stdio(c) => {
            let mut parts = vec![c.command.clone()];
            parts.extend(c.args.iter().cloned());
            parts.join(" ")
        }
        crate::services::mcp::types::McpServerConfig::Sdk(c) => c.name.clone(),
    };
    format!("{}::{}::{}", connection.name, config.scope.as_str(), endpoint)
}

/// capabilities.completions 协商判定。
pub fn supports_completions(capabilities: Option<&ServerCapabilities>) -> bool {
    capabilities.and_then(|c| c.completions.as_ref()).is_some()
}

static TEMPLATE_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, Vec<ServerResourceTemplate>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// resources/templates/list 枚举（按服务器身份缓存）。resources 子能力
/// 未声明时返回 []（与 client.ts 的 resources/list 门控一致）。
pub async fn fetch_resource_templates_for_client(
    connection: &CompletionConnection,
) -> Vec<ServerResourceTemplate> {
    let cache_key = completions_cache_key(connection);
    {
        let cache = TEMPLATE_CACHE.lock().await;
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let capabilities = connection.capabilities.as_ref();
    if !capabilities
        .and_then(|c| c.resources.as_ref())
        .is_some()
    {
        return Vec::new();
    }

    let result: Result<ListResourceTemplatesResult, _> = connection
        .client
        .request("resources/templates/list", None)
        .await
        .and_then(|value| serde_json::from_value(value).map_err(|e| McpClientError::Decode(e.to_string())));

    match result {
        Ok(parsed) => {
            let templates = parsed.resource_templates;
            if templates.is_empty() {
                return Vec::new();
            }
            let tagged = templates
                .into_iter()
                .map(|t| ServerResourceTemplate {
                    template: t,
                    server: connection.name.clone(),
                })
                .collect::<Vec<_>>();
            let mut cache = TEMPLATE_CACHE.lock().await;
            // 超限时删除最早插入的键（近似 LRU；BTreeMap 无序所以任取一
            // 个最旧——HashMap 不保证顺序，这里收集首个键）
            while cache.len() >= COMPLETIONS_CACHE_SIZE {
                let first_key = cache.keys().next().cloned();
                match first_key {
                    Some(key) => {
                        cache.remove(&key);
                    }
                    None => break,
                }
            }
            cache.insert(cache_key, tagged.clone());
            tagged
        }
        Err(e) => {
            log_mcp_error(
                &connection.name,
                format!("Failed to fetch resource templates: {e}").into(),
            );
            Vec::new()
        }
    }
}

/// 按 ServerName 清缓存（list_changed 刷新路径）。删除属于该服务器的
/// 全部身份键（`${name}::${scope}::${endpoint}`）。
pub async fn clear_resource_templates_cache(server_name: &str) {
    let mut cache = TEMPLATE_CACHE.lock().await;
    let keys: Vec<String> = cache
        .keys()
        .filter(|key| key.as_str() == server_name || key.starts_with(&format!("{server_name}::")))
        .cloned()
        .collect();
    for key in keys {
        cache.remove(&key);
    }
}

/// completions/complete：填充一个模板 URI 变量。任意失败降级为 []。
pub async fn complete_resource_template(
    connection: &CompletionConnection,
    uri: &str,
    argument_name: &str,
    argument_value: &str,
    context_arguments: Option<&HashMap<String, String>>,
) -> Vec<String> {
    if !supports_completions(connection.capabilities.as_ref()) {
        return Vec::new();
    }

    let mut params = serde_json::json!({
        "ref": { "type": "ref/resource", "uri": uri },
        "argument": { "name": argument_name, "value": argument_value },
    });
    // 官方锚点：无前置参数时整个 context 省略
    if let Some(args) = context_arguments {
        if !args.is_empty() {
            params["context"] = serde_json::json!({ "arguments": serde_json::to_value(args).unwrap_or_default() });
        }
    }

    match connection
        .client
        .request("completion/complete", Some(params))
        .await
    {
        Ok(value) => match serde_json::from_value::<CompletionResult>(value) {
            Ok(result) => result.values,
            Err(_) => Vec::new(),
        },
        Err(e) => {
            log_mcp_error(
                &connection.name,
                format!("Failed to complete resource template: {e}").into(),
            );
            Vec::new()
        }
    }
}
