//! types.rs — MCP 配置模式与连接/协议类型（重写自 `types.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `ConfigScope`（local/user/project/dynamic/enterprise/claudeai/managed）
//!   与 `TransportKind`（stdio/sse/sse-ide/http/ws/ws-ide/sdk/claudeai-proxy）。
//! - 各传输的服务器配置 struct（TS 用 zod schema 校验；Rust 用 serde +
//!   手写 `parse_mcp_server_config` 做等价校验，如 `command` 非空、
//!   `authServerMetadataUrl` 必须 https、stdio 的 maxBufferSizeMb 为正整数）。
//! - `McpServerConfig` 鉴别联合：TS 用字面量 `type` 字段区分（stdio 的
//!   type 可省略以兼容旧配置）。
//! - `McpJsonConfig`（`{ mcpServers: Record<string, McpServerConfig> }`）。
//! - 服务器连接状态联合（connected/failed/needs-auth/pending/disabled）。
//! - JSON-RPC 消息、MCP 结果类型与服务器能力声明（对
//!   `@modelcontextprotocol/sdk/types.js` 按实际用到的子集建模）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

// ---------------------------------------------------------------------------
// 配置 scope / 传输类型
// ---------------------------------------------------------------------------

/// MCP 配置来源（TS `ConfigScope`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigScope {
    Local,
    User,
    Project,
    Dynamic,
    Enterprise,
    Claudeai,
    Managed,
}

impl ConfigScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigScope::Local => "local",
            ConfigScope::User => "user",
            ConfigScope::Project => "project",
            ConfigScope::Dynamic => "dynamic",
            ConfigScope::Enterprise => "enterprise",
            ConfigScope::Claudeai => "claudeai",
            ConfigScope::Managed => "managed",
        }
    }

    /// 从字符串解析；非法值返回 None（TS 的 zod enum 直接拒绝）。
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "local" => Some(ConfigScope::Local),
            "user" => Some(ConfigScope::User),
            "project" => Some(ConfigScope::Project),
            "dynamic" => Some(ConfigScope::Dynamic),
            "enterprise" => Some(ConfigScope::Enterprise),
            "claudeai" => Some(ConfigScope::Claudeai),
            "managed" => Some(ConfigScope::Managed),
            _ => None,
        }
    }
}

/// 全部合法 scope（对齐 `ConfigScopeSchema().options`）。
pub const CONFIG_SCOPE_OPTIONS: &[ConfigScope] = &[
    ConfigScope::Local,
    ConfigScope::User,
    ConfigScope::Project,
    ConfigScope::Dynamic,
    ConfigScope::Enterprise,
    ConfigScope::Claudeai,
    ConfigScope::Managed,
];

/// MCP OAuth 配置（TS `McpOAuthConfigSchema`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u32>,
    /// 必须为 https:// URL（zod `.url().startsWith('https://')`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
    /// Cross-App Access (XAA / SEP-990)：仅一个布尔标志。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xaa: Option<bool>,
}

/// stdio 服务器配置（TS `McpStdioServerConfigSchema`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStdioServerConfig {
    /// zod 中 `.optional()`，兼容省略 type 的旧配置。
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_field: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// MCP SDK 1.30 起默认 10MB 读缓冲；服务器可按 MB 覆盖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_buffer_size_mb: Option<u32>,
}

/// SSE 服务器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSSEServerConfig {
    pub type_field: String, // "sse"
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
}

/// IDE 扩展内部 SSE 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSSEIDEServerConfig {
    pub type_field: String, // "sse-ide"
    pub url: String,
    pub ide_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ide_running_in_windows: Option<bool>,
}

/// IDE 扩展内部 WebSocket 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpWebSocketIDEServerConfig {
    pub type_field: String, // "ws-ide"
    pub url: String,
    pub ide_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ide_running_in_windows: Option<bool>,
}

/// Streamable HTTP 服务器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpHTTPServerConfig {
    pub type_field: String, // "http"
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
}

/// WebSocket 服务器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpWebSocketServerConfig {
    pub type_field: String, // "ws"
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_helper: Option<String>,
}

/// SDK 进程内服务器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSdkServerConfig {
    pub type_field: String, // "sdk"
    pub name: String,
}

/// claude.ai 代理服务器配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClaudeAIProxyServerConfig {
    pub type_field: String, // "claudeai-proxy"
    pub url: String,
    pub id: String,
}

/// 服务器配置鉴别联合（TS `McpServerConfig`）。
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    SSE(McpSSEServerConfig),
    SSEIde(McpSSEIDEServerConfig),
    WSIde(McpWebSocketIDEServerConfig),
    HTTP(McpHTTPServerConfig),
    WebSocket(McpWebSocketServerConfig),
    Sdk(McpSdkServerConfig),
    ClaudeAiProxy(McpClaudeAIProxyServerConfig),
}

impl McpServerConfig {
    /// TS 端 stdio 的 `type` 可省略（向后兼容）。
    pub fn type_str(&self) -> &'static str {
        match self {
            McpServerConfig::Stdio(_) => "stdio",
            McpServerConfig::SSE(_) => "sse",
            McpServerConfig::SSEIde(_) => "sse-ide",
            McpServerConfig::WSIde(_) => "ws-ide",
            McpServerConfig::HTTP(_) => "http",
            McpServerConfig::WebSocket(_) => "ws",
            McpServerConfig::Sdk(_) => "sdk",
            McpServerConfig::ClaudeAiProxy(_) => "claudeai-proxy",
        }
    }

    /// 远程服务器的 URL（无 URL 的类型返回 None）。
    pub fn url(&self) -> Option<&str> {
        match self {
            McpServerConfig::SSE(c) => Some(&c.url),
            McpServerConfig::SSEIde(c) => Some(&c.url),
            McpServerConfig::WSIde(c) => Some(&c.url),
            McpServerConfig::HTTP(c) => Some(&c.url),
            McpServerConfig::WebSocket(c) => Some(&c.url),
            McpServerConfig::ClaudeAiProxy(c) => Some(&c.url),
            McpServerConfig::Stdio(_) | McpServerConfig::Sdk(_) => None,
        }
    }

    /// stdio 命令数组（`[command, ...args]`），非 stdio 返回 None。
    pub fn command_array(&self) -> Option<Vec<String>> {
        match self {
            McpServerConfig::Stdio(c) => {
                let mut arr = vec![c.command.clone()];
                arr.extend(c.args.iter().cloned());
                Some(arr)
            }
            _ => None,
        }
    }

    pub fn as_sse_or_http(&self) -> Option<(&str, &McpOAuthConfig)> {
        match self {
            McpServerConfig::SSE(c) => c.oauth.as_ref().map(|o| (c.url.as_str(), o)),
            McpServerConfig::HTTP(c) => c.oauth.as_ref().map(|o| (c.url.as_str(), o)),
            _ => None,
        }
    }

    /// 序列化为 JSON 值（字段顺序与 struct 声明一致；scope 由
    /// `ScopedMcpServerConfig` 单独附加）。
    pub fn to_json_value(&self) -> Value {
        match self {
            McpServerConfig::Stdio(c) => serde_json::to_value(c),
            McpServerConfig::SSE(c) => serde_json::to_value(c),
            McpServerConfig::SSEIde(c) => serde_json::to_value(c),
            McpServerConfig::WSIde(c) => serde_json::to_value(c),
            McpServerConfig::HTTP(c) => serde_json::to_value(c),
            McpServerConfig::WebSocket(c) => serde_json::to_value(c),
            McpServerConfig::Sdk(c) => serde_json::to_value(c),
            McpServerConfig::ClaudeAiProxy(c) => serde_json::to_value(c),
        }
        .unwrap_or(Value::Null)
    }
}

/// 带 scope 的服务器配置（TS `ScopedMcpServerConfig`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedMcpServerConfig {
    pub config: McpServerConfig,
    pub scope: ConfigScope,
    /// 插件来源（如 `slack@anthropic`）；渠道门控用。
    pub plugin_source: Option<String>,
}

impl ScopedMcpServerConfig {
    pub fn new(config: McpServerConfig, scope: ConfigScope) -> Self {
        Self {
            config,
            scope,
            plugin_source: None,
        }
    }
}

/// `.mcp.json` / 全局配置的根结构（TS `McpJsonConfigSchema`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpJsonConfig {
    /// key 为服务器名。`HashMap` 序列化为无序对象；写入时由调用方保证顺序。
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: HashMap<String, Value>,
}

// ---------------------------------------------------------------------------
// 配置解析错误（对齐 zod safeParse 的 issues 语义）
// ---------------------------------------------------------------------------

/// 单条 schema 校验错误（对齐 zod issue 的 path + message）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaParseError {
    /// 点分路径，如 `mcpServers.myServer.command`。
    pub path: String,
    pub message: String,
}

/// 解析 `McpServerConfig` 的结果（对齐 zod safeParse）。
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfigParse {
    pub config: Option<McpServerConfig>,
    pub errors: Vec<SchemaParseError>,
}

fn err(path: &str, message: impl Into<String>) -> SchemaParseError {
    SchemaParseError {
        path: path.to_string(),
        message: message.into(),
    }
}

/// URL 是否为 https://（大小写不敏感的前缀判定，对齐 zod startsWith）。
fn is_https(url: &str) -> bool {
    url.to_ascii_lowercase().starts_with("https://")
}

/// 粗略 URL 校验（对齐 zod `.url()`：scheme + 主机存在）。
fn looks_like_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty()
}

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn string_map_field(obj: &Map<String, Value>, key: &str) -> Option<HashMap<String, String>> {
    obj.get(key).and_then(Value::as_object).map(|map| {
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    })
}

fn parse_oauth(obj: &Map<String, Value>, path: &str) -> Result<McpOAuthConfig, Vec<SchemaParseError>> {
    let mut errors = Vec::new();
    if let Some(port) = obj.get("callbackPort").and_then(Value::as_u64) {
        if port == 0 {
            errors.push(err(&format!("{path}.callbackPort"), "must be positive"));
        }
    }
    if let Some(url) = str_field(obj, "authServerMetadataUrl") {
        if !looks_like_url(url) || !is_https(url) {
            errors.push(err(
                &format!("{path}.authServerMetadataUrl"),
                "authServerMetadataUrl must use https://",
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(McpOAuthConfig {
        client_id: str_field(obj, "clientId").map(str::to_string),
        callback_port: obj.get("callbackPort").and_then(Value::as_u64).map(|v| v as u32),
        auth_server_metadata_url: str_field(obj, "authServerMetadataUrl").map(str::to_string),
        xaa: obj.get("xaa").and_then(Value::as_bool),
    })
}

/// 从 JSON 值解析单个服务器配置（对齐 `McpServerConfigSchema`）。
pub fn parse_mcp_server_config(raw: &Value) -> McpServerConfigParse {
    let Some(obj) = raw.as_object() else {
        return McpServerConfigParse {
            config: None,
            errors: vec![err("", "Expected object")],
        };
    };

    let kind = str_field(obj, "type").unwrap_or("stdio");

    let try_build = || -> Result<McpServerConfig, Vec<SchemaParseError>> {
        match kind {
            "stdio" => {
                let command = str_field(obj, "command").map(str::to_string).ok_or_else(|| {
                    vec![err("command", "Command cannot be empty")]
                })?;
                if command.is_empty() {
                    return Err(vec![err("command", "Command cannot be empty")]);
                }
                let args = obj
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let max_buffer_size_mb = obj.get("maxBufferSizeMb").and_then(Value::as_u64);
                if let Some(mb) = max_buffer_size_mb {
                    if mb == 0 {
                        return Err(vec![err(
                            "maxBufferSizeMb",
                            "must be a positive integer",
                        )]);
                    }
                }
                Ok(McpServerConfig::Stdio(McpStdioServerConfig {
                    type_field: str_field(obj, "type").map(str::to_string),
                    command,
                    args,
                    env: string_map_field(obj, "env"),
                    max_buffer_size_mb: max_buffer_size_mb.map(|v| v as u32),
                }))
            }
            "sse" => {
                let url = str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                    vec![err("url", "Invalid input: expected string, received undefined")]
                })?;
                let oauth = if obj.contains_key("oauth") {
                    let oauth_raw = &obj["oauth"];
                    if let Some(o) = oauth_raw.as_object() {
                        Some(parse_oauth(o, "oauth")?)
                    } else {
                        return Err(vec![err("oauth", "Expected object")]);
                    }
                } else {
                    None
                };
                Ok(McpServerConfig::SSE(McpSSEServerConfig {
                    type_field: "sse".to_string(),
                    url,
                    headers: string_map_field(obj, "headers"),
                    headers_helper: str_field(obj, "headersHelper").map(str::to_string),
                    oauth,
                }))
            }
            "sse-ide" => Ok(McpServerConfig::SSEIde(McpSSEIDEServerConfig {
                type_field: "sse-ide".to_string(),
                url: str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                    vec![err("url", "Invalid input: expected string, received undefined")]
                })?,
                ide_name: str_field(obj, "ideName").map(str::to_string).ok_or_else(|| {
                    vec![err("ideName", "Invalid input: expected string, received undefined")]
                })?,
                ide_running_in_windows: obj.get("ideRunningInWindows").and_then(Value::as_bool),
            })),
            "ws-ide" => Ok(McpServerConfig::WSIde(McpWebSocketIDEServerConfig {
                type_field: "ws-ide".to_string(),
                url: str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                    vec![err("url", "Invalid input: expected string, received undefined")]
                })?,
                ide_name: str_field(obj, "ideName").map(str::to_string).ok_or_else(|| {
                    vec![err("ideName", "Invalid input: expected string, received undefined")]
                })?,
                auth_token: str_field(obj, "authToken").map(str::to_string),
                ide_running_in_windows: obj.get("ideRunningInWindows").and_then(Value::as_bool),
            })),
            "http" => {
                let url = str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                    vec![err("url", "Invalid input: expected string, received undefined")]
                })?;
                let oauth = if obj.contains_key("oauth") {
                    let oauth_raw = &obj["oauth"];
                    if let Some(o) = oauth_raw.as_object() {
                        Some(parse_oauth(o, "oauth")?)
                    } else {
                        return Err(vec![err("oauth", "Expected object")]);
                    }
                } else {
                    None
                };
                Ok(McpServerConfig::HTTP(McpHTTPServerConfig {
                    type_field: "http".to_string(),
                    url,
                    headers: string_map_field(obj, "headers"),
                    headers_helper: str_field(obj, "headersHelper").map(str::to_string),
                    oauth,
                }))
            }
            "ws" => Ok(McpServerConfig::WebSocket(McpWebSocketServerConfig {
                type_field: "ws".to_string(),
                url: str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                    vec![err("url", "Invalid input: expected string, received undefined")]
                })?,
                headers: string_map_field(obj, "headers"),
                headers_helper: str_field(obj, "headersHelper").map(str::to_string),
            })),
            "sdk" => Ok(McpServerConfig::Sdk(McpSdkServerConfig {
                type_field: "sdk".to_string(),
                name: str_field(obj, "name").map(str::to_string).ok_or_else(|| {
                    vec![err("name", "Invalid input: expected string, received undefined")]
                })?,
            })),
            "claudeai-proxy" => Ok(McpServerConfig::ClaudeAiProxy(
                McpClaudeAIProxyServerConfig {
                    type_field: "claudeai-proxy".to_string(),
                    url: str_field(obj, "url").map(str::to_string).ok_or_else(|| {
                        vec![err("url", "Invalid input: expected string, received undefined")]
                    })?,
                    id: str_field(obj, "id").map(str::to_string).ok_or_else(|| {
                        vec![err("id", "Invalid input: expected string, received undefined")]
                    })?,
                },
            )),
            other => Err(vec![err("type", format!("Invalid enum value: {other}"))]),
        }
    };

    match try_build() {
        Ok(config) => McpServerConfigParse { config: Some(config), errors: vec![] },
        Err(errors) => McpServerConfigParse { config: None, errors },
    }
}

/// 解析 `McpJsonConfig`（`{ mcpServers: {...} }`）。
pub fn parse_mcp_json_config(raw: &Value) -> Result<McpJsonConfig, Vec<SchemaParseError>> {
    let obj = raw
        .as_object()
        .ok_or_else(|| vec![err("", "Expected object")])?;
    let servers = obj
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| vec![err("mcpServers", "Expected record of server configs")])?;
    let mut mcp_servers = HashMap::new();
    for (name, cfg) in servers {
        let parsed = parse_mcp_server_config(cfg);
        match parsed.config {
            Some(_) => {
                mcp_servers.insert(name.clone(), cfg.clone());
            }
            None => {
                return Err(parsed
                    .errors
                    .into_iter()
                    .map(|e| SchemaParseError {
                        path: format!("mcpServers.{name}.{}", e.path),
                        message: e.message,
                    })
                    .collect());
            }
        }
    }
    Ok(McpJsonConfig { mcp_servers })
}

// ---------------------------------------------------------------------------
// 连接活性 / 服务器状态
// ---------------------------------------------------------------------------

/// 连接活性检查结果（healthCheck.ts 用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPServerHealth {
    pub status: McpHealthStatus,
    pub failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ping_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpHealthStatus {
    Healthy,
    Unhealthy,
}

/// 已连接服务器句柄（client.rs 的 McpClient 经 Arc 共享）。
pub type McpClientRef = std::sync::Arc<crate::services::mcp::client::McpClient>;

/// 服务器连接状态联合（TS `MCPServerConnection`）。
#[derive(Debug, Clone)]
pub enum MCPServerConnection {
    Connected(ConnectedMCPServer),
    Failed(FailedMCPServer),
    NeedsAuth(NeedsAuthMCPServer),
    Pending(PendingMCPServer),
    Disabled(DisabledMCPServer),
}

impl MCPServerConnection {
    pub fn name(&self) -> &str {
        match self {
            MCPServerConnection::Connected(c) => &c.name,
            MCPServerConnection::Failed(c) => &c.name,
            MCPServerConnection::NeedsAuth(c) => &c.name,
            MCPServerConnection::Pending(c) => &c.name,
            MCPServerConnection::Disabled(c) => &c.name,
        }
    }

    pub fn config(&self) -> &ScopedMcpServerConfig {
        match self {
            MCPServerConnection::Connected(c) => &c.config,
            MCPServerConnection::Failed(c) => &c.config,
            MCPServerConnection::NeedsAuth(c) => &c.config,
            MCPServerConnection::Pending(c) => &c.config,
            MCPServerConnection::Disabled(c) => &c.config,
        }
    }

    pub fn type_str(&self) -> &'static str {
        match self {
            MCPServerConnection::Connected(_) => "connected",
            MCPServerConnection::Failed(_) => "failed",
            MCPServerConnection::NeedsAuth(_) => "needs-auth",
            MCPServerConnection::Pending(_) => "pending",
            MCPServerConnection::Disabled(_) => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectedMCPServer {
    pub client: McpClientRef,
    pub name: String,
    pub capabilities: ServerCapabilities,
    pub server_info: Option<ServerInfo>,
    pub instructions: Option<String>,
    pub health: Option<MCPServerHealth>,
    pub config: ScopedMcpServerConfig,
}

#[derive(Debug, Clone)]
pub struct FailedMCPServer {
    pub name: String,
    pub config: ScopedMcpServerConfig,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NeedsAuthMCPServer {
    pub name: String,
    pub config: ScopedMcpServerConfig,
}

#[derive(Debug, Clone)]
pub struct PendingMCPServer {
    pub name: String,
    pub config: ScopedMcpServerConfig,
    pub reconnect_attempt: Option<u32>,
    pub max_reconnect_attempts: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct DisabledMCPServer {
    pub name: String,
    pub config: ScopedMcpServerConfig,
}

// ---------------------------------------------------------------------------
// MCP CLI 状态（/mcp 菜单序列化用）
// ---------------------------------------------------------------------------

/// `/mcp` 展示用的工具摘要（TS `SerializedTool`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedTool {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_json_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_mcp: Option<bool>,
    /// 未规范化的原始工具名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_tool_name: Option<String>,
}

/// `/mcp` 展示用的客户端摘要（TS `SerializedClient`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedClient {
    pub name: String,
    #[serde(rename = "type")]
    pub type_str: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ServerCapabilities>,
}

/// `/mcp` 展示状态（TS `MCPCliState`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPCliState {
    pub clients: Vec<SerializedClient>,
    pub configs: HashMap<String, Value>,
    pub tools: Vec<SerializedTool>,
    #[serde(default)]
    pub resources: HashMap<String, Vec<Value>>,
    /// 规范化名 → 原始名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_names: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// JSON-RPC / MCP 协议类型（对 @modelcontextprotocol/sdk/types.js 子集建模）
// ---------------------------------------------------------------------------

/// JSON-RPC 消息（请求 / 响应 / 通知）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Response(JSONRPCResponse),
    Notification(JSONRPCNotification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCResponse {
    pub jsonrpc: String,
    pub id: Value,
    /// 成功结果与错误二选一（untagged 由 result 字段是否存在区分）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JSONRPCError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JSONRPCError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// MCP 协议错误（JSON-RPC error 包装）。
#[derive(Debug, Clone, Error)]
#[error("MCP error {code}: {message}")]
pub struct McpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// 已知 MCP JSON-RPC 错误码（对 `ErrorCode` 子集）。
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// SDK "Connection closed"。
    pub const CONNECTION_CLOSED: i64 = -32000;
    /// Session not found（HTTP 404 配对）。
    pub const REQUEST_TIMEOUT: i64 = -32001;
    /// UrlElicitationRequired。
    pub const URL_ELICITATION_REQUIRED: i64 = -32042;
}

/// 服务器能力声明（`initialize` 结果的 capabilities 字段子集）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
    /// 实验性能力（如 `claude/channel`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourcesCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

impl ServerCapabilities {
    /// 渠道服务器声明 `experimental['claude/channel']`（真值即声明）。
    pub fn declares_channel(&self) -> bool {
        self.experimental
            .as_ref()
            .and_then(|e| e.get("claude/channel"))
            .map(|v| !matches!(v, Value::Null | Value::Bool(false)))
            .unwrap_or(false)
    }

    /// 声明 `experimental['claude/channel/permission']`。
    pub fn declares_channel_permission(&self) -> bool {
        self.experimental
            .as_ref()
            .and_then(|e| e.get("claude/channel/permission"))
            .map(|v| !matches!(v, Value::Null | Value::Bool(false)))
            .unwrap_or(false)
    }
}

/// `initialize` 结果中的服务器信息（TS `serverInfo`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// ---------------------------------------------------------------------------
// tools / resources / prompts / completions 结果类型
// ---------------------------------------------------------------------------

/// MCP 工具定义（`tools/list` 条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
    /// 开放给服务器的 `_meta`（如 `anthropic/searchHint`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// MCP 资源（`resources/list` 条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `resources/list` 结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListResourcesResult {
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 资源模板（`resources/templates/list` 条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceTemplate {
    pub uri_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListResourceTemplatesResult {
    #[serde(default, rename = "resourceTemplates")]
    pub resource_templates: Vec<ResourceTemplate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 带服务器名的资源（TS `ServerResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerResource {
    #[serde(flatten)]
    pub resource: Resource,
    pub server: String,
}

/// 带服务器名的资源模板（TS `ServerResourceTemplate`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerResourceTemplate {
    #[serde(flatten)]
    pub template: ResourceTemplate,
    pub server: String,
}

/// MCP prompt 定义（`prompts/list` 条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<McpPromptArgument>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListPromptsResult {
    #[serde(default)]
    pub prompts: Vec<McpPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `completion/complete` 结果（TS `CompleteResult.completion`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionResult {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

/// `tools/call` 结果内容块（TS `ContentBlock` 子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
    ResourceLink {
        name: String,
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddedResource {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// 文本资源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 二进制资源（base64）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// `tools/call` 结果（TS `CallToolResult`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

/// `prompts/get` 结果消息内容（TS `PromptMessage.content`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: ContentBlock,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetPromptResult {
    #[serde(default)]
    pub messages: Vec<PromptMessage>,
}

/// elicit 请求参数（`elicitation/create`，TS `ElicitRequestParams`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ElicitParams {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// URL 模式 elicitation（-32042 携带）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation_id: Option<String>,
}

/// elicit 响应（TS `ElicitResult`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitResult {
    pub action: ElicitAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_stdio_config_without_type() {
        let parsed = parse_mcp_server_config(&json!({
            "command": "npx",
            "args": ["-y", "server"]
        }));
        assert!(parsed.errors.is_empty());
        match parsed.config {
            Some(McpServerConfig::Stdio(c)) => {
                assert_eq!(c.command, "npx");
                assert_eq!(c.args, vec!["-y".to_string(), "server".to_string()]);
                assert!(c.env.is_none());
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn rejects_empty_command() {
        let parsed = parse_mcp_server_config(&json!({"type": "stdio", "command": ""}));
        assert!(parsed.config.is_none());
        assert_eq!(parsed.errors[0].message, "Command cannot be empty");
    }

    #[test]
    fn rejects_non_https_metadata_url() {
        let parsed = parse_mcp_server_config(&json!({
            "type": "http",
            "url": "http://example.com",
            "oauth": { "authServerMetadataUrl": "http://insecure.example.com/meta" }
        }));
        assert!(parsed.config.is_none());
        assert!(parsed.errors[0].message.contains("https"));
    }

    #[test]
    fn parses_http_config_with_oauth() {
        let parsed = parse_mcp_server_config(&json!({
            "type": "http",
            "url": "https://example.com/mcp",
            "oauth": { "clientId": "abc", "xaa": true }
        }));
        match parsed.config {
            Some(McpServerConfig::HTTP(c)) => {
                assert_eq!(c.oauth.as_ref().and_then(|o| o.client_id.as_deref()), Some("abc"));
                assert_eq!(c.oauth.as_ref().and_then(|o| o.xaa), Some(true));
            }
            _ => panic!("expected http"),
        }
    }

    #[test]
    fn parses_mcp_json_config() {
        let parsed = parse_mcp_json_config(&json!({
            "mcpServers": { "s1": { "command": "x" } }
        }));
        assert!(parsed.is_ok());
        assert!(parsed.unwrap().mcp_servers.contains_key("s1"));
    }
}
