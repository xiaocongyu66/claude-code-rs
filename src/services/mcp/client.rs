//! client.rs — MCP 客户端核心（重写自 `client.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 传输层：stdio（子进程 + ndjson 帧 + stderr 捕获，8MB 上限）、
//!   streamable HTTP（POST + SSE/JSON 响应 + session id）、legacy SSE
//!   （GET event stream + endpoint POST）。WebSocket/ws-ide 未实现
//!   （仓库无 WebSocket 依赖）。
//! - **initialize 双步握手**：`initialize` 请求（协议版本 + 客户端信息 +
//!   客户端能力 roots/elicitation）→ 存储服务器能力/信息 → 发送
//!   `notifications/initialized` 通知。
//! - 工具发现：`tools/list` → `McpToolEntry`（含全限定名、annotations、
//!   `_meta` 扩展）；`prompts/list` → `McpPromptCommand`；`resources/list`
//!   → `ServerResource`；`resources/templates/list` → 见 completions.rs。
//!   均按服务器名做上限 20 的缓存，连接关闭时清除。
//! - 工具调用：`tools/call`（超时 + 取消传播 + progress 占位），401 →
//!   `McpAuthError`，会话过期（404/-32001）清缓存抛
//!   `McpSessionExpiredError`，`isError: true` → `McpToolCallError`，
//!   -32042（URL elicitation）循环重试（≤3 次）。
//! - 连接管理：`connect_to_server` 按配置缓存（key = name + 配置 JSON），
//!   超时竞争（MCP_TIMEOUT，默认 30s），失败 → failed/needs-auth 连接态，
//!   stdio 清理走 SIGINT→SIGTERM→SIGKILL 升级。
//!
//! 已知偏差（声明）：
//! - `@modelcontextprotocol/sdk` 不存在：JSON-RPC 层按 MCP 规范手写。
//! - 无 `futures`/`tokio-stream` 依赖：SSE 响应体以 `bytes()` 整块读取
//!   （POST 的 SSE 响应在服务器发完响应后立即结束，实际可用）；空闲期
//!   的长连 GET 流只在流结束时投递累积消息 —— 服务器主动推送的实时性
//!   低于 TS 实现。
//! - `utils/http.ts`（getMCPUserAgent）、`utils/proxy.ts`、`utils/mtls.ts`、
//!   `utils/mcpWebSocketTransport.ts`、`utils/mcpValidation.ts`、
//!   `utils/mcpOutputStorage.ts`、`utils/imageResizer.ts`、
//!   `utils/toolResultStorage.ts`、analytics、AppState 均未转换：占位
//!   最小实现（见各处 TODO 注释）。
//! - fetchToolsForClient 产出纯数据 `McpToolEntry`；映射到 `Tool` trait
//!   由编排层负责（tool.rs 的 McpServerConnection 目前是 Value 占位）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex, Semaphore};

use crate::bootstrap::state::{get_original_cwd, get_session_id};
use crate::services::mcp::completions::{self, CompletionConnection};
use crate::services::mcp::mcp_string_utils::build_mcp_tool_name;
use crate::services::mcp::normalization::normalize_name_for_mcp;
use crate::services::mcp::types::{
    error_codes, CallToolResult, ContentBlock, JSONRPCError, JSONRPCMessage,
    JSONRPCNotification, JSONRPCRequest, JSONRPCResponse, ListPromptsResult, ListResourcesResult,
    ListToolsResult, McpError, ServerCapabilities, ServerInfo, ServerResource,
    ScopedMcpServerConfig,
};
use crate::utils::abort_controller::CancellationToken;
use crate::utils::env_utils::{get_claude_config_home_dir, is_env_truthy};
use crate::utils::errors::is_abort_error;
use crate::utils::log::{log_mcp_debug, log_mcp_error};
use crate::utils::slow_operations::json_stringify;

/// 客户端版本（对齐 TS `MACRO.VERSION`，与 Cargo.toml 对齐）。
pub const CLIENT_VERSION: &str = "2.8.4";
/// 客户端展示名（initialize clientInfo.title）。
pub const CLIENT_TITLE: &str = "Claude Code";
/// MCP 协议版本（对齐 SDK 默认）。
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06.18";

/// 握手协商超时（MCP_TIMEOUT，默认 30s）。
pub fn get_connection_timeout_ms() -> u64 {
    std::env::var("MCP_TIMEOUT")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(30_000)
}

/// 单个 MCP 请求（auth、工具调用等）默认超时。
pub const MCP_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// MCP Streamable HTTP 规范要求每次 POST 声明同时接受 JSON 与 SSE。
pub const MCP_STREAMABLE_HTTP_ACCEPT: &str = "application/json, text/event-stream";

/// MCP 工具描述/服务器指令发往模型的上限（对齐包常量）。
pub const MAX_MCP_DESCRIPTION_LENGTH: usize = 4096;

/// 本地服务器（stdio/sdk）连接并发（MCP_SERVER_CONNECTION_BATCH_SIZE，默认 3）。
pub fn get_mcp_server_connection_batch_size() -> usize {
    std::env::var("MCP_SERVER_CONNECTION_BATCH_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(3)
}

/// 远程服务器连接并发（默认 20）。
pub fn get_remote_mcp_server_connection_batch_size() -> usize {
    std::env::var("MCP_REMOTE_SERVER_CONNECTION_BATCH_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(20)
}

/// 工具调用超时（MCP_TOOL_TIMEOUT；默认 ~27.8 小时，事实上的无限）。
pub fn get_mcp_tool_timeout_ms() -> u64 {
    std::env::var("MCP_TOOL_TIMEOUT")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(100_000_000)
}

/// needs-auth 缓存 TTL（15 分钟）。
const MCP_AUTH_CACHE_TTL_MS: i64 = 15 * 60 * 1000;

/// IDE MCP 服务器中只保留特定工具（对齐 TS `ALLOWED_IDE_TOOLS`）。
const ALLOWED_IDE_TOOLS: &[&str] = &["mcp__ide__executeCode", "mcp__ide__getDiagnostics"];

fn is_included_mcp_tool(tool_name: &str) -> bool {
    !tool_name.starts_with("mcp__ide__") || ALLOWED_IDE_TOOLS.contains(&tool_name)
}

fn is_local_mcp_server(config: &ScopedMcpServerConfig) -> bool {
    matches!(
        config.config,
        crate::services::mcp::types::McpServerConfig::Stdio(_)
            | crate::services::mcp::types::McpServerConfig::Sdk(_)
    )
}

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// 传输层错误（Unauthorized 单列：连接时映射为 needs-auth）。
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("{0}")]
    Message(String),
    /// HTTP 401：需要 OAuth 授权。
    #[error("unauthorized (HTTP 401)")]
    Unauthorized,
}

/// MCP 客户端错误（请求级别）。
#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("connection closed")]
    Closed,
    #[error("request aborted")]
    Aborted,
    #[error(transparent)]
    Protocol(#[from] McpError),
    #[error("invalid response: {0}")]
    Decode(String),
    #[error("unauthorized (HTTP 401)")]
    Unauthorized,
}

/// MCP 工具调用因认证失败（401 等）抛出（TS `McpAuthError`）。
#[derive(Debug, Error)]
#[error("MCP server \"{server_name}\" requires re-authorization (token expired)")]
pub struct McpAuthError {
    pub server_name: String,
}

/// 会话过期：连接缓存已清，调用方应取新客户端重试（TS `McpSessionExpiredError`）。
#[derive(Debug, Error)]
#[error("MCP server \"{0}\" session expired")]
pub struct McpSessionExpiredError(pub String);

/// MCP 工具返回 `isError: true`（携带 `_meta` 供 SDK 消费者）。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct McpToolCallError {
    pub message: String,
    /// 进遥测的安全消息（TS `TelemetrySafeError` 语义）。
    pub telemetry_message: String,
    pub mcp_meta: Option<HashMap<String, Value>>,
}

/// 判定是否 MCP "Session not found"（HTTP 404 + JSON-RPC -32001）。
pub fn is_mcp_session_expired_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(mcp_error) = error.downcast_ref::<McpError>() {
        return mcp_error.code == error_codes::REQUEST_TIMEOUT
            || mcp_error.message.to_lowercase().contains("session not found");
    }
    if let Some(client_error) = error.downcast_ref::<McpClientError>() {
        return match client_error {
            McpClientError::Protocol(mcp_error) => {
                mcp_error.code == error_codes::REQUEST_TIMEOUT
                    || mcp_error.message.to_lowercase().contains("session not found")
            }
            _ => false,
        };
    }
    false
}

// ---------------------------------------------------------------------------
// SSE 解析（无 futures 依赖：整块 bytes 解析）
// ---------------------------------------------------------------------------

/// 从 SSE 事件流字节中解析出 (event, data) 对。
///
/// 解析 RFC 8985 SSE：以空行分隔事件，`event:` 行取类型，`data:` 行
///（可多行）拼接。忽略注释行（`:` 开头）。`retry`/`id` 行忽略。
pub fn parse_sse_events(body: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(body);
    let mut events = Vec::new();
    let mut current_event = String::new();
    let mut current_data: Vec<String> = Vec::new();

    let flush = |event: &mut String, data: &mut Vec<String>, events: &mut Vec<(String, String)>| {
        if !data.is_empty() {
            events.push((event.clone(), data.join("\n")));
        }
        event.clear();
        data.clear();
    };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut current_event, &mut current_data, &mut events);
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => current_event = value.to_string(),
            "data" => current_data.push(value.to_string()),
            _ => {}
        }
    }
    flush(&mut current_event, &mut current_data, &mut events);
    events
}

// ---------------------------------------------------------------------------
// 传输层
// ---------------------------------------------------------------------------

/// MCP 传输契约（对 TS SDK Transport 的 Rust 表达）。
///
/// 生命周期：`start()` 取走消息接收端 → 驱动循环把 `send()` 的消息写出、
/// 把接收端收到的消息交给客户端分发 → `close()` 关闭传输。
pub trait McpTransport: Send + 'static {
    /// 启动并返回服务器→客户端消息接收端（仅一次）。
    fn start(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<tokio::sync::mpsc::UnboundedReceiver<JSONRPCMessage>, TransportError>> + Send + '_>>;

    /// 发出一条消息。
    fn send<'a>(
        &'a self,
        message: &'a JSONRPCMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + 'a>>;

    /// 关闭传输（幂等）。
    fn close(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>>;
}

/// stdio 传输：spawn 子进程，stdin/stdout 以换行分隔 JSON，stderr 捕获
/// （8MB 上限，对齐 `captureStderr`）。
pub struct StdioTransport {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    /// 读缓冲上限（字节），默认 32MB（对齐 TS `maxBufferSize` 兜底）。
    max_buffer_size: usize,
    inner: Mutex<StdioTransportInner>,
}

struct StdioTransportInner {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// stderr 捕获槽（连接失败诊断用；8MB 上限）。
    stderr_slot: Option<Arc<Mutex<String>>>,
}

impl StdioTransport {
    const STDERR_CAPTURE_CAP: usize = 8 * 1024 * 1024;

    /// 构造 stdio 传输。env 应为 `subprocess_env()` 与服务器 env 的合并。
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
        max_buffer_size_mb: Option<u32>,
    ) -> Self {
        Self {
            command: command.into(),
            args,
            env,
            max_buffer_size: max_buffer_size_mb.unwrap_or(32) as usize * 1024 * 1024,
            inner: Mutex::new(StdioTransportInner {
                child: None,
                stdin: None,
                stderr_slot: None,
            }),
        }
    }

    /// 读取当前累积的 stderr 输出（诊断用，读后清空）。
    pub async fn take_stderr_output(&self) -> String {
        let inner = self.inner.lock().await;
        let Some(slot) = inner.stderr_slot.as_ref() else {
            return String::new();
        };
        let mut capture = slot.lock().await;
        let out = capture.clone();
        capture.clear();
        out
    }
}

impl McpTransport for StdioTransport {
    fn start(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<tokio::sync::mpsc::UnboundedReceiver<JSONRPCMessage>, TransportError>> + Send + '_>> {
        let command = self.command.clone();
        let args = self.args.clone();
        let env = self.env.clone();
        let max_buffer_size = self.max_buffer_size;
        let inner = &self.inner;
        Box::pin(async move {
            use tokio::process::Command;
            let mut cmd = Command::new(&command);
            cmd.args(&args);
            for (key, value) in env {
                cmd.env(key, value);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| TransportError::Message(format!("Failed to spawn MCP server: {e}")))?;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            {
                let mut inner = inner.lock().await;
                inner.child = Some(child);
                inner.stdin = stdin;
            }

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<JSONRPCMessage>();

            // stdout 读循环：ndjson 帧解析，缓冲上限 max_buffer_size
            if let Some(stdout) = stdout {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    let mut consumed = 0usize;
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // EOF
                            Ok(_) => {
                                consumed = consumed.saturating_add(line.len());
                                if consumed > max_buffer_size {
                                    log_mcp_error(
                                        "stdio",
                                        "stdio message exceeded max buffer size, closing"
                                            .to_string()
                                            .into(),
                                    );
                                    break;
                                }
                                let trimmed = line.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<JSONRPCMessage>(trimmed) {
                                    Ok(message) => {
                                        if tx.send(message).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        log_mcp_debug("stdio", &format!("Invalid JSON line from MCP server: {e}"));
                                    }
                                }
                            }
                            Err(e) => {
                                log_mcp_debug("stdio", &format!("stdout read error: {e}"));
                                break;
                            }
                        }
                    }
                });
            }

            // stderr 捕获循环（8MB 上限，对齐 captureStderr）
            if let Some(stderr) = stderr {
                let capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
                let cap_for_task = capture.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stderr);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let mut cap = cap_for_task.lock().await;
                                if cap.len() + line.len() <= Self::STDERR_CAPTURE_CAP {
                                    cap.push_str(&line);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                let mut inner = inner.lock().await;
                inner.stderr_slot = Some(capture);
            }

            Ok(rx)
        })
    }

    fn send(
        &self,
        message: &JSONRPCMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        let message = message.clone();
        let inner = &self.inner;
        Box::pin(async move {
            let mut inner = inner.lock().await;
            let closed = inner.child.is_none();
            if closed {
                return Err(TransportError::Message("Transport is closed".to_string()));
            }
            let Some(stdin) = inner.stdin.as_mut() else {
                return Err(TransportError::Message("Transport has no stdin".to_string()));
            };
            let mut bytes = match serde_json::to_string(&message) {
                Ok(s) => s,
                Err(e) => {
                    return Err(TransportError::Message(format!(
                        "Failed to serialize message: {e}"
                    )))
                }
            };
            bytes.push('\n');
            stdin
                .write_all(bytes.as_bytes())
                .await
                .map_err(|e| TransportError::Message(format!("stdin write failed: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| TransportError::Message(format!("stdin flush failed: {e}")))?;
            Ok(())
        })
    }

    fn close(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        let inner = &self.inner;
        Box::pin(async move {
            let mut inner = inner.lock().await;
            if let Some(mut child) = inner.child.take() {
                // 对齐 TS 清理：SIGINT → SIGTERM → SIGKILL 升级（简化：
                // 直接 kill，等待退出；子进程的 graceful 依赖 stdin 关闭）
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            inner.stdin = None;
            Ok(())
        })
    }
}

/// streamable HTTP 传输（MCP 2025-03-26）。
///
/// 每次 POST 声明 `Accept: application/json, text/event-stream`；响应
/// 为 JSON 时解析单条消息，为 SSE 时解析全部事件。session id 从
/// `mcp-session-id` 响应头提取并回带。`auth_header_provider` 在每次
/// 请求时求值（对齐 SDK authProvider.tokens() 的 per-request 注入）。
pub struct StreamableHttpTransport {
    url: String,
    http: reqwest::Client,
    static_headers: HashMap<String, String>,
    auth_header_provider: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    incoming_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<JSONRPCMessage>>>,
    session_id: Mutex<Option<String>>,
    closed: std::sync::atomic::AtomicBool,
}

impl StreamableHttpTransport {
    pub fn new(
        url: impl Into<String>,
        static_headers: HashMap<String, String>,
        auth_header_provider: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
        Ok(Self {
            url: url.into(),
            http,
            static_headers,
            auth_header_provider,
            incoming_tx: Mutex::new(None),
            session_id: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    async fn deliver(&self, message: JSONRPCMessage) {
        let tx = self.incoming_tx.lock().await;
        if let Some(tx) = tx.as_ref() {
            let _ = tx.send(message);
        }
    }
}

impl McpTransport for StreamableHttpTransport {
    fn start(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<tokio::sync::mpsc::UnboundedReceiver<JSONRPCMessage>, TransportError>> + Send + '_>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<JSONRPCMessage>();
        let tx_for_get = tx.clone();
        Box::pin(async move {
            {
                let mut incoming = self.incoming_tx.lock().await;
                *incoming = Some(tx);
            }
            let url = self.url.clone();
            let http = self.http.clone();
            let static_headers = self.static_headers.clone();
            let auth_provider = self.auth_header_provider.clone();
            // 可选的 SSE GET 流：无 futures 依赖，只能等流结束后整块投递
            tokio::spawn(async move {
                let mut req = http
                    .get(&url)
                    .header("Accept", "text/event-stream")
                    .header("User-Agent", mcp_user_agent());
                for (k, v) in &static_headers {
                    req = req.header(k, v);
                }
                if let Some(provider) = auth_provider {
                    if let Some(token) = provider() {
                        req = req.header("Authorization", format!("Bearer {token}"));
                    }
                }
                match req.send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            if let Ok(body) = response.bytes().await {
                                for (_event, data) in parse_sse_events(&body) {
                                    if let Ok(message) =
                                        serde_json::from_str::<JSONRPCMessage>(&data)
                                    {
                                        let _ = tx_for_get.send(message);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            });
            Ok(rx)
        })
    }

    fn send(
        &self,
        message: &JSONRPCMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        let url = self.url.clone();
        let http = self.http.clone();
        let message = message.clone();
        let static_headers = self.static_headers.clone();
        let auth_provider = self.auth_header_provider.clone();
        Box::pin(async move {
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(TransportError::Message("Transport is closed".to_string()));
            }
            let body = serde_json::to_vec(&message)
                .map_err(|e| TransportError::Message(format!("serialize failed: {e}")))?;

            let mut req = http
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", MCP_STREAMABLE_HTTP_ACCEPT)
                .header("User-Agent", mcp_user_agent())
                .body(body);
            for (k, v) in &static_headers {
                req = req.header(k, v);
            }
            if let Some(token) = auth_provider.as_ref().and_then(|p| p()) {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            let session_id = self.session_id.lock().await.clone();
            if let Some(session_id) = &session_id {
                req = req.header("mcp-session-id", session_id);
            }

            let response = req
                .send()
                .await
                .map_err(|e| TransportError::Message(format!("HTTP request failed: {e}")))?;
            let status = response.status();
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            // 记录服务器下发的 session id
            if let Some(session) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                *self.session_id.lock().await = Some(session);
            }

            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(TransportError::Unauthorized);
            }
            if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                return Err(TransportError::Message(
                    "Server does not support streamable HTTP transport (405)".to_string(),
                ));
            }
            if !status.is_success() {
                let body_text = response.text().await.unwrap_or_default();
                return Err(TransportError::Message(format!(
                    "HTTP {status}: {}",
                    body_text.chars().take(200).collect::<String>()
                )));
            }

            if content_type.contains("text/event-stream") {
                let body = response
                    .bytes()
                    .await
                    .map_err(|e| TransportError::Message(format!("read SSE body failed: {e}")))?;
                for (_event, data) in parse_sse_events(&body) {
                    if let Ok(message) = serde_json::from_str::<JSONRPCMessage>(&data) {
                        self.deliver(message).await;
                    }
                }
                Ok(())
            } else {
                // application/json：单条消息
                let value: Value = response
                    .json()
                    .await
                    .map_err(|e| TransportError::Message(format!("invalid JSON body: {e}")))?;
                let message: JSONRPCMessage = serde_json::from_value(value)
                    .map_err(|e| TransportError::Message(format!("invalid JSONRPC message: {e}")))?;
                self.deliver(message).await;
                Ok(())
            }
        })
    }

    fn close(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        let url = self.url.clone();
        let http = self.http.clone();
        Box::pin(async move {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            // 关闭会话（DELETE），best-effort
            let session_id = self.session_id.lock().await.clone();
            if let Some(session_id) = session_id {
                let req = http
                    .delete(&url)
                    .header("mcp-session-id", session_id)
                    .header("User-Agent", mcp_user_agent());
                let _ = req.send().await;
            }
            Ok(())
        })
    }
}

/// legacy SSE 传输（HTTP+SSE）：GET 打开事件流，首事件 `endpoint` 给出
/// POST 目标；后续 `message` 事件投递消息。
pub struct SseTransport {
    url: String,
    http: reqwest::Client,
    static_headers: HashMap<String, String>,
    auth_header_provider: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    endpoint: Arc<Mutex<Option<String>>>,
    incoming_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<JSONRPCMessage>>>,
    closed: std::sync::atomic::AtomicBool,
}

impl SseTransport {
    pub fn new(
        url: impl Into<String>,
        static_headers: HashMap<String, String>,
        auth_header_provider: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
        Ok(Self {
            url: url.into(),
            http,
            static_headers,
            auth_header_provider,
            endpoint: Arc::new(Mutex::new(None)),
            incoming_tx: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl McpTransport for SseTransport {
    fn start(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<tokio::sync::mpsc::UnboundedReceiver<JSONRPCMessage>, TransportError>> + Send + '_>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<JSONRPCMessage>();
        let tx_for_task = tx.clone();
        Box::pin(async move {
            {
                let mut incoming = self.incoming_tx.lock().await;
                *incoming = Some(tx);
            }
            let url = self.url.clone();
            let http = self.http.clone();
            let static_headers = self.static_headers.clone();
            let auth_provider = self.auth_header_provider.clone();
            let endpoint_for_task = self.endpoint.clone();
            tokio::spawn(async move {
                let mut req = http
                    .get(&url)
                    .header("Accept", "text/event-stream")
                    .header("User-Agent", mcp_user_agent());
                for (k, v) in &static_headers {
                    req = req.header(k, v);
                }
                if let Some(provider) = auth_provider {
                    if let Some(token) = provider() {
                        req = req.header("Authorization", format!("Bearer {token}"));
                    }
                }
                match req.send().await {
                    Ok(response) if response.status().is_success() => {
                        if let Ok(body) = response.bytes().await {
                            for (event, data) in parse_sse_events(&body) {
                                if event == "endpoint" {
                                    let mut endpoint = endpoint_for_task.lock().await;
                                    *endpoint = Some(data);
                                } else if let Ok(message) = serde_json::from_str::<JSONRPCMessage>(&data) {
                                    let _ = tx_for_task.send(message);
                                }
                            }
                        }
                    }
                    Ok(response) => {
                        log_mcp_debug(
                            "sse",
                            &format!("SSE stream closed with status {}", response.status()),
                        );
                    }
                    Err(e) => {
                        log_mcp_debug("sse", &format!("SSE stream error: {e}"));
                    }
                }
            });
            Ok(rx)
        })
    }

    fn send(
        &self,
        message: &JSONRPCMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        let message = message.clone();
        let http = self.http.clone();
        let static_headers = self.static_headers.clone();
        let url = self.url.clone();
        Box::pin(async move {
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(TransportError::Message("Transport is closed".to_string()));
            }
            // endpoint 未就绪时回退到原 URL（TS SDK 等待首事件；此处以
            // 回退保底并在日志中记录）
            let endpoint = self.endpoint.lock().await.clone();
            let target = endpoint.as_deref().unwrap_or(&url);
            let body = serde_json::to_vec(&message)
                .map_err(|e| TransportError::Message(format!("serialize failed: {e}")))?;
            let mut req = http
                .post(target)
                .header("Content-Type", "application/json")
                .header("Accept", MCP_STREAMABLE_HTTP_ACCEPT)
                .header("User-Agent", mcp_user_agent())
                .body(body);
            for (k, v) in &static_headers {
                req = req.header(k, v);
            }
            let response = req
                .send()
                .await
                .map_err(|e| TransportError::Message(format!("HTTP POST failed: {e}")))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(TransportError::Unauthorized);
            }
            if !response.status().is_success() {
                return Err(TransportError::Message(format!(
                    "HTTP {}",
                    response.status()
                )));
            }
            Ok(())
        })
    }

    fn close(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        Box::pin(async move {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }
}

/// MCP User-Agent（utils/http.ts 未转换：本地复刻基础形态）。
fn mcp_user_agent() -> String {
    format!("claude-code/{CLIENT_VERSION}")
}

// ---------------------------------------------------------------------------
// McpClient：JSON-RPC 客户端（initialize 双步握手 + 请求/通知/响应分发）
// ---------------------------------------------------------------------------

/// 服务器→客户端请求处理器（roots/list、elicitation/create 等）。
pub type ServerRequestHandler =
    Arc<dyn Fn(Value) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> + Send + Sync>;

/// 服务器通知处理器（notifications/*）。
pub type ServerNotificationHandler = Arc<dyn Fn(&Value) + Send + Sync>;

struct ClientCore {
    tx: tokio::sync::mpsc::UnboundedSender<JSONRPCMessage>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpError>>>>,
    request_handlers: Mutex<HashMap<String, Vec<ServerRequestHandler>>>,
    notification_handlers: Mutex<HashMap<String, Vec<ServerNotificationHandler>>>,
    next_id: AtomicI64,
    cancel: CancellationToken,
    capabilities: Mutex<Option<ServerCapabilities>>,
    server_info: Mutex<Option<ServerInfo>>,
    protocol_version: Mutex<Option<String>>,
    instructions: Mutex<Option<String>>,
    closed: Mutex<bool>,
}

impl ClientCore {
    fn new(tx: tokio::sync::mpsc::UnboundedSender<JSONRPCMessage>, cancel: CancellationToken) -> Self {
        Self {
            tx,
            pending: Mutex::new(HashMap::new()),
            request_handlers: Mutex::new(HashMap::new()),
            notification_handlers: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            cancel,
            capabilities: Mutex::new(None),
            server_info: Mutex::new(None),
            protocol_version: Mutex::new(None),
            instructions: Mutex::new(None),
            closed: Mutex::new(false),
        }
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    async fn send_raw(&self, message: JSONRPCMessage) -> Result<(), McpClientError> {
        self.tx
            .send(message)
            .map_err(|_| McpClientError::Closed)
    }

    async fn request_internal(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, McpClientError> {
        let id = self.next_id();
        let (os_tx, os_rx) = oneshot::channel::<Result<Value, McpError>>();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, os_tx);
        }

        let message = JSONRPCMessage::Request(JSONRPCRequest {
            jsonrpc: "2.0".to_string(),
            id: Value::from(id),
            method: method.to_string(),
            params,
        });
        if let Err(e) = self.send_raw(message).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let result = tokio::select! {
            received = os_rx => {
                match received {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(mcp_error)) => Err(McpClientError::Protocol(mcp_error)),
                    Err(_) => Err(McpClientError::Closed),
                }
            }
            _ = tokio::time::sleep(timeout) => {
                // 对齐 SDK：超时发送 notifications/cancelled
                let _ = self.send_raw(JSONRPCMessage::Notification(JSONRPCNotification {
                    jsonrpc: "2.0".to_string(),
                    method: "notifications/cancelled".to_string(),
                    params: Some(json!({ "requestId": id })),
                })).await;
                self.pending.lock().await.remove(&id);
                Err(McpClientError::Timeout(timeout))
            }
            _ = self.cancel_cancelled(cancel) => {
                let _ = self.send_raw(JSONRPCMessage::Notification(JSONRPCNotification {
                    jsonrpc: "2.0".to_string(),
                    method: "notifications/cancelled".to_string(),
                    params: Some(json!({ "requestId": id })),
                })).await;
                self.pending.lock().await.remove(&id);
                Err(McpClientError::Aborted)
            }
        };
        result
    }

    async fn cancel_cancelled(&self, cancel: Option<&CancellationToken>) {
        match cancel {
            Some(token) => {
                // 轮询取消令牌（无 Future 适配器；间隔 50ms）
                while !token.aborted() && !self.closed_flag().await {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            None => {
                // 无外部 token：永远挂起（select 会等其它分支先就绪）
                std::future::pending::<()>().await;
            }
        }
    }

    async fn closed_flag(&self) -> bool {
        *self.closed.lock().await
    }

    async fn dispatch(&self, message: JSONRPCMessage, out_tx: &tokio::sync::mpsc::UnboundedSender<JSONRPCMessage>) {
        match message {
            JSONRPCMessage::Response(response) => {
                let id = match &response.id {
                    Value::Number(n) => n.as_i64(),
                    _ => None,
                };
                let Some(id) = id else { return };
                let entry = self.pending.lock().await.remove(&id);
                let Some(resolver) = entry else { return };
                match response.error {
                    Some(err) => {
                        let _ = resolver.send(Err(McpError {
                            code: err.code,
                            message: err.message,
                            data: err.data,
                        }));
                    }
                    None => {
                        let _ = resolver.send(Ok(response.result.unwrap_or(Value::Null)));
                    }
                }
            }
            JSONRPCMessage::Request(request) => {
                let handlers = {
                    let handlers = self.request_handlers.lock().await;
                    handlers.get(&request.method).cloned()
                };
                if let Some(handlers) = handlers {
                    for handler in handlers {
                        let params = request.params.clone().unwrap_or(Value::Null);
                        let id = request.id.clone();
                        match handler(params).await {
                            Ok(result) => {
                                let _ = out_tx.send(JSONRPCMessage::Response(JSONRPCResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id,
                                    result: Some(result),
                                    error: None,
                                }));
                            }
                            Err(message) => {
                                let _ = out_tx.send(JSONRPCMessage::Response(JSONRPCResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id,
                                    result: None,
                                    error: Some(JSONRPCError {
                                        code: error_codes::INTERNAL_ERROR,
                                        message,
                                        data: None,
                                    }),
                                }));
                            }
                        }
                    }
                }
            }
            JSONRPCMessage::Notification(notification) => {
                let handlers = {
                    let handlers = self.notification_handlers.lock().await;
                    handlers.get(&notification.method).cloned()
                };
                if let Some(handlers) = handlers {
                    let params = notification.params.unwrap_or(Value::Null);
                    for handler in handlers {
                        handler(&params);
                    }
                }
            }
        }
    }
}

/// MCP 客户端：一个已初始化的服务器连接。
pub struct McpClient {
    core: Arc<ClientCore>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").finish_non_exhaustive()
    }
}

impl McpClient {
    /// 执行 initialize 双步握手并返回客户端。
    ///
    /// 1. `initialize`（协议版本 + clientInfo + 客户端能力 roots/elicitation）
    /// 2. 存储 capabilities / serverInfo / instructions
    /// 3. 发送 `notifications/initialized`
    pub async fn connect<T: McpTransport>(
        transport: T,
    ) -> Result<Arc<Self>, McpClientError> {
        let incoming = transport
            .start()
            .await
            .map_err(|e| McpClientError::Transport(e.to_string()))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JSONRPCMessage>();
        let cancel = CancellationToken::new();
        let core = Arc::new(ClientCore::new(tx.clone(), cancel.clone()));

        // 驱动循环：本侧外发消息 → transport.send；transport 收到的消息 →
        // 分发到 pending 请求 / 通知处理器；取消或传输断开时退出并清理。
        {
            let core = core.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let transport = transport;
                let mut incoming = incoming;
                loop {
                    tokio::select! {
                        maybe = rx.recv() => {
                            match maybe {
                                Some(message) => {
                                    if transport.send(&message).await.is_err() {
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        maybe = incoming.recv() => {
                            match maybe {
                                Some(message) => core.dispatch(message, &core.tx.clone()).await,
                                None => break,
                            }
                        }
                        _ = token_cancelled(&cancel) => break,
                    }
                }
                let _ = transport.close().await;
                // 驱动退出：标记关闭并拒绝全部 pending
                {
                    let mut closed = core.closed.lock().await;
                    *closed = true;
                }
                let mut pending = core.pending.lock().await;
                pending.clear();
            });
        }

        // 注册 roots/list 默认处理器（对齐 TS）
        {
            let roots = vec![json!({ "uri": format!("file://{}", get_original_cwd()) })];
            let handler: ServerRequestHandler = Arc::new(move |_params| {
                let roots = roots.clone();
                Box::pin(async move { Ok(json!({ "roots": roots })) })
            });
            core.request_handlers.lock().await.insert("roots/list".to_string(), vec![handler]);
        }
        // 默认 elicitation 处理器：连接建立前的窗口内一律 cancel（对齐 TS）
        {
            let handler: ServerRequestHandler = Arc::new(|_params| {
                Box::pin(async move { Ok(json!({ "action": "cancel" })) })
            });
            core.request_handlers.lock().await.insert("elicitation/create".to_string(), vec![handler]);
        }

        let client = Arc::new(Self { core: core.clone() });

        // 1) initialize
        let init_params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {
                "roots": {},
                "elicitation": {}
            },
            "clientInfo": {
                "name": "claude-code",
                "title": CLIENT_TITLE,
                "version": CLIENT_VERSION,
            }
        });
        let result = core
            .request_internal(
                "initialize",
                Some(init_params),
                Duration::from_millis(get_connection_timeout_ms()),
                None,
            )
            .await?;

        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_string);
        let capabilities: ServerCapabilities = result
            .get("capabilities")
            .map(|v| serde_json::from_value(v.clone()).unwrap_or_default())
            .unwrap_or_default();
        let server_info: ServerInfo = result
            .get("serverInfo")
            .map(|v| serde_json::from_value(v.clone()).unwrap_or_default())
            .unwrap_or_default();
        let instructions = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string);

        *core.protocol_version.lock().await = protocol_version;
        *core.capabilities.lock().await = Some(capabilities);
        *core.server_info.lock().await = Some(server_info);
        let mut final_instructions = instructions;
        if let Some(ref raw) = final_instructions {
            if raw.len() > MAX_MCP_DESCRIPTION_LENGTH {
                let truncated: String = raw.chars().take(MAX_MCP_DESCRIPTION_LENGTH).collect();
                final_instructions = Some(format!("{truncated}… [truncated]"));
            }
        }
        *core.instructions.lock().await = final_instructions;

        // 2) initialized 通知
        client.notification("notifications/initialized", None).await;

        Ok(client)
    }

    /// 发送请求（默认 60s 超时）。
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, McpClientError> {
        self.core
            .request_internal(method, params, Duration::from_millis(MCP_REQUEST_TIMEOUT_MS), None)
            .await
    }

    /// 发送请求（自定义超时与可选取消令牌）。
    pub async fn request_with(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, McpClientError> {
        self.core.request_internal(method, params, timeout, cancel).await
    }

    /// 发送通知。
    pub async fn notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), McpClientError> {
        self.core
            .send_raw(JSONRPCMessage::Notification(JSONRPCNotification {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params,
            }))
            .await
    }

    /// ping（默认 2s 超时；可注入探测超时）。
    pub async fn ping(&self, timeout: Option<Duration>) -> Result<(), McpClientError> {
        let timeout = timeout.unwrap_or(Duration::from_secs(2));
        self.core.request_internal("ping", Some(json!({})), timeout, None).await?;
        Ok(())
    }

    pub async fn capabilities(&self) -> ServerCapabilities {
        self.core
            .capabilities
            .lock()
            .await
            .clone()
            .unwrap_or_default()
    }

    pub async fn server_info(&self) -> Option<ServerInfo> {
        self.core.server_info.lock().await.clone()
    }

    pub async fn protocol_version(&self) -> Option<String> {
        self.core.protocol_version.lock().await.clone()
    }

    pub async fn instructions(&self) -> Option<String> {
        self.core.instructions.lock().await.clone()
    }

    /// 注册服务器→客户端请求处理器。
    pub async fn set_request_handler(&self, method: &str, handler: ServerRequestHandler) {
        self.core
            .request_handlers
            .lock()
            .await
            .insert(method.to_string(), vec![handler]);
    }

    /// 注册服务器通知处理器（可多个，按注册顺序调用）。
    pub async fn set_notification_handler(&self, method: &str, handler: ServerNotificationHandler) {
        self.core
            .notification_handlers
            .lock()
            .await
            .entry(method.to_string())
            .or_default()
            .push(handler);
    }

    /// 关闭（幂等）：取消驱动循环、关闭传输。
    pub async fn close(&self) -> Result<(), McpClientError> {
        self.core.cancel.abort(None);
        Ok(())
    }

    /// tools/call。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        meta: Option<&Value>,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<CallToolResult, McpClientError> {
        let mut params = json!({
            "name": name,
            "arguments": arguments,
        });
        if let Some(meta) = meta {
            params["_meta"] = meta.clone();
        }
        let value = self.core.request_internal("tools/call", Some(params), timeout, cancel).await?;
        let result: CallToolResult = serde_json::from_value(value)
            .map_err(|e| McpClientError::Decode(format!("invalid tools/call result: {e}")))?;
        Ok(result)
    }
}

fn token_cancelled(token: &CancellationToken) -> TokenCancelled<'_> {
    TokenCancelled { token }
}

struct TokenCancelled<'a> {
    token: &'a CancellationToken,
}

impl<'a> std::future::Future for TokenCancelled<'a> {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.token.aborted() {
            std::task::Poll::Ready(())
        } else {
            // 借助定时器唤醒，避免空转
            let waker = cx.waker().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                waker.wake();
            });
            std::task::Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// needs-auth 缓存（mcp-needs-auth-cache.json，15 分钟 TTL）
// ---------------------------------------------------------------------------

fn get_mcp_auth_cache_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join("mcp-needs-auth-cache.json")
}

static AUTH_CACHE: std::sync::Mutex<Option<HashMap<String, i64>>> = std::sync::Mutex::new(None);

fn read_mcp_auth_cache() -> HashMap<String, i64> {
    {
        let cache = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref() {
            return cached.clone();
        }
    }
    let read = std::fs::read(get_mcp_auth_cache_path());
    let parsed: HashMap<String, i64> = match read {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    };
    let mut cache = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some(parsed.clone());
    parsed
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 该服务器是否在 15 分钟 TTL 内刚刚 needs-auth（连接批量时跳过探测）。
pub async fn is_mcp_auth_cached(server_id: &str) -> bool {
    let cache = read_mcp_auth_cache();
    match cache.get(server_id) {
        Some(ts) => now_unix_ms() - ts < MCP_AUTH_CACHE_TTL_MS,
        None => false,
    }
}

fn set_mcp_auth_cache_entry(server_id: &str) {
    let mut cache = read_mcp_auth_cache();
    cache.insert(server_id.to_string(), now_unix_ms());
    let path = get_mcp_auth_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(path, bytes);
    }
    let mut guard = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(cache);
}

/// 清除 needs-auth 缓存（登录后或重连前）。
pub fn clear_mcp_auth_cache() {
    {
        let mut guard = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
    let _ = std::fs::remove_file(get_mcp_auth_cache_path());
}

// ---------------------------------------------------------------------------
// 连接缓存与 connectToServer
// ---------------------------------------------------------------------------

/// 连接缓存 key：`name-<config JSON>`（对齐 TS `getServerCacheKey`）。
pub fn get_server_cache_key(name: &str, server_ref: &ScopedMcpServerConfig) -> String {
    format!("{name}-{}", json_stringify(&server_ref.config.to_json_value()))
}

/// 连接表项：已连接客户端 + 清理句柄。
struct ConnectionEntry {
    client: Arc<McpClient>,
    transport_kind: &'static str,
    cleanup: Mutex<Option<CleanupTicket>>,
}

/// 清理票据：drop cleanup_registration（注销 + 执行一次）。
struct CleanupTicket {
    _registration: crate::utils::cleanup_registry::CleanupRegistration,
}

static CONNECTION_CACHE: once_cell::sync::Lazy<
    Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<ConnectionEntry>>>>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

struct ConnectionFactory;

impl ConnectionFactory {
    /// 按配置类型构造传输。remote 类型注入静态 headers + auth provider。
    async fn build(
        name: &str,
        config: &ScopedMcpServerConfig,
    ) -> Result<(TransportKind, TransportHandle), TransportError> {
        use crate::services::mcp::types::McpServerConfig;
        let headers = crate::services::mcp::headers_helper::get_mcp_server_headers(
            name,
            &config.config.to_json_value(),
            Some(config),
        )
        .await;

        let auth_provider = crate::services::mcp::auth::auth_header_provider(name, config);

        match &config.config {
            McpServerConfig::Stdio(stdio) => {
                let shell_prefix = std::env::var("CLAUDE_CODE_SHELL_PREFIX").ok();
                let (command, args) = match shell_prefix {
                    Some(prefix) => (
                        prefix,
                        vec![[stdio.command.clone()]
                            .into_iter()
                            .chain(stdio.args.iter().cloned())
                            .collect::<Vec<_>>()
                            .join(" ")],
                    ),
                    None => (stdio.command.clone(), stdio.args.clone()),
                };
                let mut env = crate::utils::subprocess_env::subprocess_env();
                if let Some(config_env) = &stdio.env {
                    for (k, v) in config_env {
                        env.insert(k.clone(), v.clone());
                    }
                }
                let transport = StdioTransport::new(command, args, env, stdio.max_buffer_size_mb);
                Ok((TransportKind::Stdio, TransportHandle::Stdio(Arc::new(transport))))
            }
            McpServerConfig::HTTP(http) => {
                let transport = StreamableHttpTransport::new(
                    http.url.clone(),
                    headers,
                    auth_provider,
                )
                .map_err(|e| TransportError::Message(e))?;
                Ok((TransportKind::Http, TransportHandle::Http(Arc::new(transport))))
            }
            McpServerConfig::SSE(sse) => {
                let transport =
                    SseTransport::new(sse.url.clone(), headers, auth_provider)
                        .map_err(|e| TransportError::Message(e))?;
                Ok((TransportKind::Sse, TransportHandle::Sse(Arc::new(transport))))
            }
            McpServerConfig::SSEIde(sse_ide) => {
                let transport =
                    SseTransport::new(sse_ide.url.clone(), headers, None)
                        .map_err(|e| TransportError::Message(e))?;
                Ok((TransportKind::SseIde, TransportHandle::Sse(Arc::new(transport))))
            }
            McpServerConfig::WebSocket(_) | McpServerConfig::WSIde(_) => Err(
                TransportError::Message(
                    "WebSocket transports are not implemented in this build".to_string(),
                ),
            ),
            McpServerConfig::Sdk(_) => Err(TransportError::Message(
                "SDK servers should be handled in print.ts".to_string(),
            )),
            McpServerConfig::ClaudeAiProxy(proxy) => {
                // 代理 URL：oauthConfig.MCP_PROXY_URL + path（{server_id} 替换）
                let oauth_config = crate::constants::oauth::get_oauth_config()
                    .map_err(|e| TransportError::Message(format!("oauth config: {e}")))?;
                let proxy_url = format!(
                    "{}{}",
                    oauth_config.mcp_proxy_url,
                    oauth_config
                        .mcp_proxy_path
                        .replace("{server_id}", &proxy.id)
                );
                let provider = crate::services::mcp::claudeai::claudeai_proxy_header_provider();
                let mut static_headers = headers;
                static_headers.insert(
                    "X-Mcp-Client-Session-Id".to_string(),
                    get_session_id().0,
                );
                let transport = StreamableHttpTransport::new(proxy_url, static_headers, Some(provider))
                    .map_err(|e| TransportError::Message(e))?;
                Ok((
                    TransportKind::ClaudeAiProxy,
                    TransportHandle::Http(Arc::new(transport)),
                ))
            }
        }
    }
}

/// 传输种类（日志用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Sse,
    SseIde,
    Http,
    Sdk,
    ClaudeAiProxy,
}

impl TransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Sse => "sse",
            TransportKind::SseIde => "sse-ide",
            TransportKind::Http => "http",
            TransportKind::Sdk => "sdk",
            TransportKind::ClaudeAiProxy => "claudeai-proxy",
        }
    }
}

/// 已构造的传输句柄（包装具体传输；enum 分发，trait object-free）。
#[derive(Clone)]
pub enum TransportHandle {
    Stdio(Arc<StdioTransport>),
    Http(Arc<StreamableHttpTransport>),
    Sse(Arc<SseTransport>),
    InProcess(Arc<crate::services::mcp::in_process_transport::InProcessTransport>),
    SdkControl(Arc<crate::services::mcp::sdk_control_transport::SdkControlClientTransport>),
}

impl McpTransport for TransportHandle {
    fn start(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<tokio::sync::mpsc::UnboundedReceiver<JSONRPCMessage>, TransportError>> + Send + '_>> {
        match self {
            TransportHandle::Stdio(t) => t.start(),
            TransportHandle::Http(t) => t.start(),
            TransportHandle::Sse(t) => t.start(),
            TransportHandle::InProcess(t) => Box::pin(async move {
                t.start().await.map_err(TransportError::Message)
            }),
            TransportHandle::SdkControl(t) => Box::pin(async move {
                t.start().await.map_err(TransportError::Message)
            }),
        }
    }

    fn send<'a>(
        &'a self,
        message: &'a JSONRPCMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + 'a>> {
        let message = message.clone();
        match self {
            TransportHandle::Stdio(t) => Box::pin(async move { t.send(&message).await }),
            TransportHandle::Http(t) => Box::pin(async move { t.send(&message).await }),
            TransportHandle::Sse(t) => Box::pin(async move { t.send(&message).await }),
            TransportHandle::InProcess(t) => Box::pin(async move {
                t.send(message.clone()).await.map_err(TransportError::Message)
            }),
            TransportHandle::SdkControl(t) => Box::pin(async move {
                t.send(message.clone()).await.map_err(TransportError::Message)
            }),
        }
    }

    fn close(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send + '_>> {
        match self {
            TransportHandle::Stdio(t) => t.close(),
            TransportHandle::Http(t) => t.close(),
            TransportHandle::Sse(t) => t.close(),
            TransportHandle::InProcess(t) => Box::pin(async move {
                t.close().await.map_err(TransportError::Message)
            }),
            TransportHandle::SdkControl(t) => Box::pin(async move {
                t.close().await.map_err(TransportError::Message)
            }),
        }
    }
}

/// Mertric 快照（连接事件用；analytics 未转换 → 结构体暂存）。
#[derive(Debug, Clone, Default)]
pub struct ServerStats {
    pub total_servers: usize,
    pub stdio_count: usize,
    pub sse_count: usize,
    pub http_count: usize,
    pub sse_ide_count: usize,
    pub ws_ide_count: usize,
}

/// 尝试连接单个 MCP 服务器（缓存 per `getServerCacheKey`）。
///
/// 返回连接状态联合：connected / failed / needs-auth（远程 401）。
pub async fn connect_to_server(
    name: &str,
    server_ref: ScopedMcpServerConfig,
    stats: Option<&ServerStats>,
) -> crate::services::mcp::types::MCPServerConnection {
    let connect_start = Instant::now();
    let key = get_server_cache_key(name, &server_ref);

    // 缓存命中直接返回（保持缓存项不变，只克隆客户端引用）。
    // 注意：std Mutex guard 不跨 await —— 先克隆 Arc 再锁 tokio Mutex。
    {
        let cache = CONNECTION_CACHE.lock().await;
        let entry = cache.get(&key).cloned();
        drop(cache);
        if let Some(entry) = entry {
            let guard = entry.lock().await;
            if let Some(entry) = guard.as_ref() {
                let client = entry.client.clone();
                let kind = entry.transport_kind;
                drop(guard);
                return crate::services::mcp::types::MCPServerConnection::Connected(
                    build_connected(name, server_ref, client, kind).await,
                );
            }
        }
    }

    // 构造传输
    let (kind, transport) = match ConnectionFactory::build(name, &server_ref).await {
        Ok(built) => built,
        Err(e) => {
            return failed_connection(name, server_ref, &e.to_string());
        }
    };

    // 连接 + 超时竞争（TransportHandle 内部为 Arc，克隆廉价）
    let timeout = Duration::from_millis(get_connection_timeout_ms());
    let result = match tokio::time::timeout(timeout, McpClient::connect(transport.clone())).await {
        Ok(inner) => inner,
        Err(_) => {
            let _ = transport.close().await;
            return failed_connection(
                name,
                server_ref,
                &format!(
                    "MCP server \"{name}\" connection timed out after {}ms",
                    get_connection_timeout_ms()
                ),
            );
        }
    };

    let client = match result {
        Ok(client) => client,
        Err(McpClientError::Unauthorized) => {
            return handle_remote_auth_failure(name, server_ref, kind.as_str())
        }
        Err(e) => {
            let _ = transport.close().await;
            let mut message = error_message_dyn(&e);
            if matches!(kind, TransportKind::Stdio) {
                if let TransportHandle::Stdio(stdio) = &transport {
                    let stderr = stdio.take_stderr_output().await;
                    if !stderr.is_empty() {
                        message.push_str(&format!("\nServer stderr: {stderr}"));
                    }
                }
            }
            return failed_connection(name, server_ref, &message);
        }
    };
    let elapsed = connect_start.elapsed().as_millis();
    log_mcp_debug(
        name,
        &format!(
            "Successfully connected (transport: {}) in {elapsed}ms",
            kind.as_str()
        ),
    );

    // 注册清理（进程退出时关闭传输；对齐 TS registerCleanup）
    let transport_for_cleanup: Arc<dyn Fn() + Send + Sync> = match &transport {
        TransportHandle::Stdio(t) => {
            let t = t.clone();
            Arc::new(move || {
                let t = t.clone();
                tokio::spawn(async move {
                    let _ = t.close().await;
                });
            })
        }
        TransportHandle::Http(t) => {
            let t = t.clone();
            Arc::new(move || {
                let t = t.clone();
                tokio::spawn(async move {
                    let _ = t.close().await;
                });
            })
        }
        TransportHandle::Sse(t) => {
            let t = t.clone();
            Arc::new(move || {
                let t = t.clone();
                tokio::spawn(async move {
                    let _ = t.close().await;
                });
            })
        }
        TransportHandle::InProcess(t) => {
            let t = t.clone();
            Arc::new(move || {
                let t = t.clone();
                tokio::spawn(async move {
                    let _ = t.close().await;
                });
            })
        }
        TransportHandle::SdkControl(t) => {
            let t = t.clone();
            Arc::new(move || {
                let t = t.clone();
                tokio::spawn(async move {
                    let _ = t.close().await;
                });
            })
        }
    };
    let registration = crate::utils::cleanup_registry::register_cleanup(move || {
        (transport_for_cleanup)();
    });

    let transport_kind = match kind {
        TransportKind::Stdio => "stdio",
        TransportKind::Sse => "sse",
        TransportKind::SseIde => "sse-ide",
        TransportKind::Http => "http",
        TransportKind::Sdk => "sdk",
        TransportKind::ClaudeAiProxy => "claudeai-proxy",
    };
    let entry = ConnectionEntry {
        client: client.clone(),
        transport_kind,
        cleanup: Mutex::new(Some(CleanupTicket {
            _registration: registration,
        })),
    };
    let entry = Arc::new(tokio::sync::Mutex::new(Some(entry)));
    {
        let mut cache = CONNECTION_CACHE.lock().await;
        cache.insert(key.clone(), entry.clone());
    }

    crate::services::mcp::types::MCPServerConnection::Connected(build_connected(
        name,
        server_ref,
        client,
        transport_kind,
    )
    .await)
}

async fn build_connected(
    name: &str,
    server_ref: ScopedMcpServerConfig,
    client: Arc<McpClient>,
    transport_kind: &'static str,
) -> crate::services::mcp::types::ConnectedMCPServer {
    let _ = transport_kind;
    let capabilities = client.capabilities().await;
    crate::services::mcp::types::ConnectedMCPServer {
        client,
        name: name.to_string(),
        capabilities,
        server_info: None,
        instructions: None,
        health: None,
        config: server_ref,
    }
}

fn failed_connection(
    name: &str,
    config: ScopedMcpServerConfig,
    message: &str,
) -> crate::services::mcp::types::MCPServerConnection {
    log_mcp_error(name, format!("Connection failed: {message}").into());
    crate::services::mcp::types::MCPServerConnection::Failed(
        crate::services::mcp::types::FailedMCPServer {
            name: name.to_string(),
            config,
            error: Some(message.to_string()),
        },
    )
}

/// 远程认证失败：记录 needs-auth 缓存并返回 needs-auth 连接态。
pub fn handle_remote_auth_failure(
    name: &str,
    server_ref: ScopedMcpServerConfig,
    transport_type: &str,
) -> crate::services::mcp::types::MCPServerConnection {
    let label = match transport_type {
        "sse" => "SSE",
        "http" => "HTTP",
        "claudeai-proxy" => "claude.ai proxy",
        other => other,
    };
    log_mcp_debug(name, &format!("Authentication required for {label} server"));
    set_mcp_auth_cache_entry(name);
    crate::services::mcp::types::MCPServerConnection::NeedsAuth(
        crate::services::mcp::types::NeedsAuthMCPServer {
            name: name.to_string(),
            config: server_ref,
        },
    )
}

fn error_message_dyn(error: &McpClientError) -> String {
    match error {
        McpClientError::Transport(msg) => msg.clone(),
        McpClientError::Timeout(d) => format!("request timed out after {:?}", d),
        McpClientError::Closed => "connection closed".to_string(),
        McpClientError::Aborted => "request aborted".to_string(),
        McpClientError::Protocol(mcp_error) => mcp_error.message.clone(),
        McpClientError::Decode(msg) => msg.clone(),
        McpClientError::Unauthorized => "unauthorized".to_string(),
    }
}

/// 清除某服务器的连接与 fetch 缓存（重连前调用）。
pub async fn clear_server_cache(name: &str, server_ref: &ScopedMcpServerConfig) {
    let key = get_server_cache_key(name, server_ref);
    let entry = {
        let mut cache = CONNECTION_CACHE.lock().await;
        cache.remove(&key)
    };
    if let Some(entry) = entry {
        let mut guard = entry.lock().await;
        if let Some(entry) = guard.take() {
            // 清理票据 drop：注销进程级清理 + 关闭传输
            drop(entry);
        }
    }
    clear_fetch_caches(name).await;
}

/// 清除指定服务器的 tools/resources/prompts/templates 缓存。
pub async fn clear_fetch_caches(server_name: &str) {
    {
        let mut cache = TOOLS_CACHE.lock().await;
        cache.remove(server_name);
    }
    {
        let mut cache = RESOURCES_CACHE.lock().await;
        cache.remove(server_name);
    }
    {
        let mut cache = COMMANDS_CACHE.lock().await;
        cache.remove(server_name);
    }
    completions::clear_resource_templates_cache(server_name).await;
}

/// 确保客户端仍连接（缓存被清则重连）。SDK 服务器原样返回。
pub async fn ensure_connected_client(
    client: &crate::services::mcp::types::ConnectedMCPServer,
) -> Result<
    (
        crate::services::mcp::types::ConnectedMCPServer,
        Arc<McpClient>,
    ),
    McpClientError,
> {
    if matches!(
        client.config.config,
        crate::services::mcp::types::McpServerConfig::Sdk(_)
    ) {
        return Ok((client.clone(), client.client.clone()));
    }
    let connected = connect_to_server(&client.name, client.config.clone(), None).await;
    match connected {
        crate::services::mcp::types::MCPServerConnection::Connected(c) => Ok((c.clone(), c.client.clone())),
        _ => Err(McpClientError::Closed),
    }
}

/// 比较两个服务器配置是否等价（排除 scope 元数据）。
pub fn are_mcp_configs_equal(
    a: &ScopedMcpServerConfig,
    b: &ScopedMcpServerConfig,
) -> bool {
    if a.config.type_str() != b.config.type_str() {
        return false;
    }
    json_stringify(&a.config.to_json_value()) == json_stringify(&b.config.to_json_value())
}

/// MCP 工具输入编码为 auto-mode 分类器输入。
pub fn mcp_tool_input_to_auto_classifier_input(input: &Value, tool_name: &str) -> String {
    match input.as_object() {
        Some(obj) if !obj.is_empty() => obj
            .iter()
            .map(|(k, v)| {
                // TS `String(input[k])`：字符串取原值，其它类型按字面序列化
                let value = match v {
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                };
                // 含空白的值加 shell 式双引号，避免键值边界歧义
                if value.chars().any(char::is_whitespace) {
                    format!("{k}=\"{value}\"")
                } else {
                    format!("{k}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => tool_name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// fetch*ForClient：工具 / 命令 / 资源发现（LRU 近似缓存，按服务器名）
// ---------------------------------------------------------------------------

/// MCP 工具条目（fetchToolsForClient 产出；映射到 `Tool` trait 由编排层负责）。
#[derive(Debug, Clone)]
pub struct McpToolEntry {
    /// 全限定名 `mcp__<server>__<tool>`。
    pub name: String,
    /// 原始（未规范化）工具名。
    pub original_name: String,
    pub server_name: String,
    pub description: Option<String>,
    pub prompt: String,
    pub input_json_schema: Value,
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub open_world_hint: bool,
    pub search_hint: Option<String>,
    pub always_load: bool,
    pub user_facing_name: String,
}

/// MCP prompt 命令（fetchCommandsForClient 产出）。
#[derive(Debug, Clone)]
pub struct McpPromptCommand {
    /// `mcp__<server>__<prompt>`。
    pub name: String,
    pub description: String,
    pub has_user_specified_description: bool,
    /// 参数名（有序）。
    pub arg_names: Vec<String>,
    pub server_name: String,
}

fn truncate_description(description: &Option<String>) -> (Option<String>, String) {
    match description {
        Some(desc) => {
            let prompt = if desc.len() > MAX_MCP_DESCRIPTION_LENGTH {
                let truncated: String = desc.chars().take(MAX_MCP_DESCRIPTION_LENGTH).collect();
                format!("{truncated}… [truncated]")
            } else {
                desc.clone()
            };
            (Some(desc.clone()), prompt)
        }
        None => (None, String::new()),
    }
}

static TOOLS_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, Vec<McpToolEntry>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static RESOURCES_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, Vec<ServerResource>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static COMMANDS_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, Vec<McpPromptCommand>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

const MCP_FETCH_CACHE_SIZE: usize = 20;

fn evict_if_full<V>(cache: &mut HashMap<String, V>) {
    while cache.len() >= MCP_FETCH_CACHE_SIZE {
        let key = cache.keys().next().cloned();
        match key {
            Some(key) => {
                cache.remove(&key);
            }
            None => break,
        }
    }
}

/// tools/list 取该服务器的工具（按服务器名缓存；失败返回空数组）。
pub async fn fetch_tools_for_client(
    connection: &crate::services::mcp::types::MCPServerConnection,
) -> Vec<McpToolEntry> {
    let name = connection.name().to_string();
    {
        let cache = TOOLS_CACHE.lock().await;
        if let Some(cached) = cache.get(&name) {
            return cached.clone();
        }
    }

    let crate::services::mcp::types::MCPServerConnection::Connected(connected) = connection else {
        return Vec::new();
    };
    if connected.capabilities.tools.is_none() {
        return Vec::new();
    }

    let value = match connected.client.request("tools/list", None).await {
        Ok(value) => value,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to fetch tools: {e}").into());
            return Vec::new();
        }
    };
    let result: ListToolsResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to parse tools: {e}").into());
            return Vec::new();
        }
    };

    let skip_prefix = matches!(
        connected.config.config,
        crate::services::mcp::types::McpServerConfig::Sdk(_)
    ) && is_env_truthy(std::env::var("CLAUDE_AGENT_SDK_MCP_NO_PREFIX").ok().as_deref());

    let mut entries = Vec::new();
    for tool in result.tools {
        let fully_qualified = build_mcp_tool_name(&name, &tool.name);
        if !is_included_mcp_tool(if skip_prefix { &tool.name } else { &fully_qualified }) {
            continue;
        }
        let search_hint = tool
            .meta
            .as_ref()
            .and_then(|m| m.get("anthropic/searchHint"))
            .and_then(Value::as_str)
            .map(|s| {
                let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
                if collapsed.is_empty() {
                    None
                } else {
                    Some(collapsed)
                }
            })
            .unwrap_or(None);
        let always_load = tool
            .meta
            .as_ref()
            .and_then(|m| m.get("anthropic/alwaysLoad"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (description, prompt) = truncate_description(&tool.description);
        let annotations = tool.annotations.unwrap_or_default();
        let display_name = annotations.title.clone().unwrap_or_else(|| tool.name.clone());
        entries.push(McpToolEntry {
            name: if skip_prefix {
                tool.name.clone()
            } else {
                fully_qualified.clone()
            },
            original_name: tool.name.clone(),
            server_name: name.clone(),
            description,
            prompt,
            input_json_schema: tool.input_schema,
            read_only_hint: annotations.read_only_hint.unwrap_or(false),
            destructive_hint: annotations.destructive_hint.unwrap_or(false),
            open_world_hint: annotations.open_world_hint.unwrap_or(false),
            search_hint,
            always_load,
            user_facing_name: format!("{name} - {display_name} (MCP)"),
        });
    }

    let mut cache = TOOLS_CACHE.lock().await;
    evict_if_full(&mut cache);
    cache.insert(name, entries.clone());
    entries
}

/// resources/list 取该服务器的资源（失败返回空数组）。
pub async fn fetch_resources_for_client(
    connection: &crate::services::mcp::types::MCPServerConnection,
) -> Vec<ServerResource> {
    let name = connection.name().to_string();
    {
        let cache = RESOURCES_CACHE.lock().await;
        if let Some(cached) = cache.get(&name) {
            return cached.clone();
        }
    }

    let crate::services::mcp::types::MCPServerConnection::Connected(connected) = connection else {
        return Vec::new();
    };
    if connected.capabilities.resources.is_none() {
        return Vec::new();
    }

    let value = match connected.client.request("resources/list", None).await {
        Ok(value) => value,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to fetch resources: {e}").into());
            return Vec::new();
        }
    };
    let result: ListResourcesResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to parse resources: {e}").into());
            return Vec::new();
        }
    };

    let resources = result
        .resources
        .into_iter()
        .map(|resource| ServerResource {
            resource,
            server: name.clone(),
        })
        .collect::<Vec<_>>();
    let mut cache = RESOURCES_CACHE.lock().await;
    evict_if_full(&mut cache);
    cache.insert(name, resources.clone());
    resources
}

/// prompts/list 取该服务器的 prompt 命令（失败返回空数组）。
pub async fn fetch_commands_for_client(
    connection: &crate::services::mcp::types::MCPServerConnection,
) -> Vec<McpPromptCommand> {
    let name = connection.name().to_string();
    {
        let cache = COMMANDS_CACHE.lock().await;
        if let Some(cached) = cache.get(&name) {
            return cached.clone();
        }
    }

    let crate::services::mcp::types::MCPServerConnection::Connected(connected) = connection else {
        return Vec::new();
    };
    if connected.capabilities.prompts.is_none() {
        return Vec::new();
    }

    let value = match connected.client.request("prompts/list", None).await {
        Ok(value) => value,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to fetch commands: {e}").into());
            return Vec::new();
        }
    };
    let result: ListPromptsResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(e) => {
            log_mcp_error(&name, format!("Failed to parse prompts: {e}").into());
            return Vec::new();
        }
    };

    let mut commands = Vec::new();
    for prompt in result.prompts {
        let arg_names = prompt
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|arg| arg.name)
            .collect::<Vec<_>>();
        let has_description = prompt.description.is_some();
        commands.push(McpPromptCommand {
            name: format!(
                "mcp__{}__{}",
                normalize_name_for_mcp(&name),
                prompt.name
            ),
            description: prompt.description.unwrap_or_default(),
            has_user_specified_description: has_description,
            arg_names,
            server_name: name.clone(),
        });
    }

    let mut cache = COMMANDS_CACHE.lock().await;
    evict_if_full(&mut cache);
    cache.insert(name, commands.clone());
    commands
}

// ---------------------------------------------------------------------------
// 工具调用
// ---------------------------------------------------------------------------

/// MCP 工具调用结果（content + _meta + structuredContent）。
#[derive(Debug, Clone)]
pub struct McpToolCallResult {
    /// 处理后的内容（字符串或 ContentBlock 数组的 JSON）。
    pub content: Value,
    pub meta: Option<HashMap<String, Value>>,
    pub structured_content: Option<Value>,
}

/// URL elicitation 重试包装（-32042，最多 3 次重试）。
///
/// `handle_elicitation` 为空时（无 UI/SDK 桥）拒绝并返回错误内容。
pub async fn call_mcp_tool_with_url_elicitation_retry(
    client: &crate::services::mcp::types::ConnectedMCPServer,
    tool: &str,
    args: &Value,
    meta: Option<&Value>,
    cancel: Option<&CancellationToken>,
    handle_elicitation: Option<
        Arc<
            dyn Fn(
                    &str,
                    &crate::services::mcp::types::ElicitParams,
                    &CancellationToken,
                ) -> Pin<Box<dyn std::future::Future<Output = crate::services::mcp::types::ElicitResult> + Send>>
                + Send
                + Sync,
        >,
    >,
) -> Result<McpToolCallResult, McpClientError> {
    let max_retries = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match call_mcp_tool(client, tool, args, meta, cancel).await {
            Ok(result) => return Ok(result),
            Err(McpClientError::Protocol(mcp_error))
                if mcp_error.code == error_codes::URL_ELICITATION_REQUIRED =>
            {
                if attempt > max_retries {
                    return Err(McpClientError::Protocol(mcp_error));
                }
                // 校验 error.data.elicitations 形态
                let elicitations: Vec<crate::services::mcp::types::ElicitParams> = mcp_error
                    .data
                    .as_ref()
                    .and_then(|d| d.get("elicitations"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| serde_json::from_value(e.clone()).ok())
                            .filter(|e: &crate::services::mcp::types::ElicitParams| {
                                e.mode.as_deref() == Some("url")
                                    && e.url.is_some()
                                    && e.elicitation_id.is_some()
                                    && !e.message.is_empty()
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let server_name = &client.name;
                if elicitations.is_empty() {
                    log_mcp_debug(
                        server_name,
                        &format!("Tool '{tool}' returned -32042 but no valid elicitations in error data"),
                    );
                    return Err(McpClientError::Protocol(mcp_error));
                }

                for elicitation in &elicitations {
                    let elicitation_id = elicitation.elicitation_id.clone().unwrap_or_default();

                    // elicitation hooks（utils/hooks.ts 未转换 → 占位：无 hook 拦截）
                    let hook_response = crate::services::mcp::elicitation_handler::run_elicitation_hooks(
                        server_name,
                        elicitation,
                        cancel.unwrap_or(&client_cancel_placeholder()),
                    )
                    .await;
                    if let Some(hook_result) = hook_response {
                        if hook_result.action != crate::services::mcp::types::ElicitAction::Accept {
                            return Ok(McpToolCallResult {
                                content: Value::String(format!(
                                    "URL elicitation was {} by a hook. The tool \"{tool}\" could not complete because it requires the user to open a URL.",
                                    action_name(hook_result.action)
                                )),
                                meta: None,
                                structured_content: None,
                            });
                        }
                        continue;
                    }

                    let user_result = match &handle_elicitation {
                        Some(handler) => handler(server_name, elicitation, cancel.unwrap_or(&client_cancel_placeholder())).await,
                        None => crate::services::mcp::types::ElicitResult {
                            action: crate::services::mcp::types::ElicitAction::Cancel,
                            content: None,
                        },
                    };

                    let final_result = crate::services::mcp::elicitation_handler::run_elicitation_result_hooks(
                        server_name,
                        &user_result,
                        cancel.unwrap_or(&client_cancel_placeholder()),
                        "url",
                        &elicitation_id,
                    )
                    .await
                    .unwrap_or(user_result);

                    if final_result.action != crate::services::mcp::types::ElicitAction::Accept {
                        log_mcp_debug(
                            server_name,
                            &format!("User {} URL elicitation {elicitation_id}", action_name(final_result.action)),
                        );
                        return Ok(McpToolCallResult {
                            content: Value::String(format!(
                                "URL elicitation was {} by the user. The tool \"{tool}\" could not complete because it requires the user to open a URL.",
                                action_name(final_result.action)
                            )),
                            meta: None,
                            structured_content: None,
                        });
                    }
                    log_mcp_debug(
                        server_name,
                        &format!("Elicitation {elicitation_id} completed, retrying tool call"),
                    );
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn client_cancel_placeholder() -> CancellationToken {
    CancellationToken::new()
}

fn action_name(action: crate::services::mcp::types::ElicitAction) -> &'static str {
    match action {
        crate::services::mcp::types::ElicitAction::Accept => "accepted",
        crate::services::mcp::types::ElicitAction::Decline => "declined",
        crate::services::mcp::types::ElicitAction::Cancel => "canceled",
    }
}

/// 调用 MCP 工具（超时、401→McpAuthError、会话过期→清缓存重抛、
/// isError:true→McpToolCallError）。
pub async fn call_mcp_tool(
    client: &crate::services::mcp::types::ConnectedMCPServer,
    tool: &str,
    args: &Value,
    meta: Option<&Value>,
    cancel: Option<&CancellationToken>,
) -> Result<McpToolCallResult, McpClientError> {
    let name = client.name.clone();
    let config = client.config.clone();
    log_mcp_debug(&name, &format!("Calling MCP tool: {tool}"));

    let timeout_ms = get_mcp_tool_timeout_ms();
    let result = client
        .client
        .call_tool(tool, args, meta, Duration::from_millis(timeout_ms), cancel)
        .await;

    let result = match result {
        Ok(result) => result,
        Err(McpClientError::Unauthorized) => {
            log_mcp_debug(&name, "Tool call returned 401 Unauthorized - token may have expired");
            return Err(McpClientError::Unauthorized);
        }
        Err(e) => {
            // 会话过期：清连接缓存，下次调用重连
            if is_mcp_session_expired_error(&e) {
                log_mcp_debug(
                    &name,
                    "MCP session expired during tool call, clearing connection cache",
                );
                clear_server_cache(&name, &config).await;
                return Err(e);
            }
            if !is_abort_error(&e) {
                log_mcp_debug(&name, &format!("Tool '{tool}' failed: {e}"));
            }
            return Err(e);
        }
    };

    // isError → McpToolCallError
    if result.is_error.unwrap_or(false) {
        let mut error_details = "Unknown error".to_string();
        if let Some(first) = result.content.first() {
            if let ContentBlock::Text { text, .. } = first {
                error_details = text.clone();
            }
        }
        log_mcp_error(&name, error_details.clone().into());
        return Err(McpClientError::Protocol(McpError {
            code: 0,
            message: error_details,
            data: result
                .meta
                .as_ref()
                .map(|m| Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
        }));
    }

    log_mcp_debug(&name, &format!("Tool '{tool}' completed successfully"));
    let content = process_mcp_result(&result, tool, &name);
    Ok(McpToolCallResult {
        content,
        meta: result.meta,
        structured_content: result.structured_content,
    })
}

// ---------------------------------------------------------------------------
// 结果转换（transform/process）
// ---------------------------------------------------------------------------

/// 由值生成紧凑的类型签名（对齐 TS `inferCompactSchema`）。
pub fn infer_compact_schema(value: &Value, depth: i32) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            format!("[{}]", infer_compact_schema(&arr[0], depth - 1))
        }
        Value::Object(obj) => {
            if depth <= 0 {
                return "{...}".to_string();
            }
            let entries: Vec<String> = obj
                .iter()
                .take(10)
                .map(|(k, v)| format!("{k}: {}", infer_compact_schema(v, depth - 1)))
                .collect();
            let suffix = if obj.len() > 10 { ", ..." } else { "" };
            format!("{{{}}}{}", entries.join(", "), suffix)
        }
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
    }
}

/// 转换 MCP 结果内容为可序列化形式（对齐 TS `transformMCPResult`）。
pub fn transform_content(content: &[ContentBlock], _server_name: &str) -> Value {
    // 逐块转 JSON 值；image/audio 保持 base64 原样（imageResizer 未转换）
    let mapped: Vec<Value> = content
        .iter()
        .map(|block| serde_json::to_value(block).unwrap_or(Value::Null))
        .collect();
    Value::Array(mapped)
}

/// 处理 MCP 工具结果（对齐 TS `processMCPResult`）。
///
/// 已知偏差：mcpValidation/mcpOutputStorage 未转换 —— 大结果落盘与截断
/// 逻辑未实现，内容原样返回（结构化分支：toolResult / structuredContent /
/// contentArray 仍按 TS 形状区分）。
pub fn process_mcp_result(result: &CallToolResult, tool: &str, name: &str) -> Value {
    if let Some(structured) = &result.structured_content {
        let _ = (tool, name);
        return Value::String(json_stringify(structured));
    }
    transform_content(&result.content, name)
}

/// 兼容点: TS 的 transformResultContent 单独导出；此处等价 transform_content。
pub async fn transform_result_content(
    content: &[ContentBlock],
    server_name: &str,
) -> Value {
    transform_content(content, server_name)
}

// ---------------------------------------------------------------------------
// 批量连接：getMcpToolsCommandsAndResources / prefetchAllMcpResources
// ---------------------------------------------------------------------------

/// 单个服务器的连接结果（回调参数，对齐 TS onConnectionAttempt）。
pub struct ConnectionAttempt {
    pub client: crate::services::mcp::types::MCPServerConnection,
    pub tools: Vec<McpToolEntry>,
    pub commands: Vec<McpPromptCommand>,
    pub resources: Option<Vec<ServerResource>>,
}

/// 并发连接全部配置的服务器并逐个回调。
///
/// 本地（stdio/sdk）与远程服务器分别走不同并发上限（3 / 20）。
/// 禁用的服务器直接以 disabled 状态回调，不发起连接。
pub async fn get_mcp_tools_commands_and_resources<F>(
    mut on_connection_attempt: F,
    mcp_configs: Option<HashMap<String, ScopedMcpServerConfig>>,
) where
    F: FnMut(ConnectionAttempt) + Send,
{
    let configs = match mcp_configs {
        Some(configs) => configs,
        None => {
            // getAllMcpConfigs 涉及 claude.ai 网络拉取；此处降级为
            // getClaudeCodeMcpConfigs（本地文件）
            let result = crate::services::mcp::config::get_claude_code_mcp_configs(
                HashMap::new(),
                HashMap::new(),
            )
            .await;
            result.servers
        }
    };

    // 拆分 disabled / active
    let mut active: Vec<(String, ScopedMcpServerConfig)> = Vec::new();
    for (name, config) in configs.into_iter() {
        if crate::services::mcp::config::is_mcp_server_disabled(&name) {
            on_connection_attempt(ConnectionAttempt {
                client: crate::services::mcp::types::MCPServerConnection::Disabled(
                    crate::services::mcp::types::DisabledMCPServer {
                        name: name.clone(),
                        config,
                    },
                ),
                tools: Vec::new(),
                commands: Vec::new(),
                resources: None,
            });
        } else {
            active.push((name, config));
        }
    }

    let stats = ServerStats {
        total_servers: active.len(),
        stdio_count: count_transport(&active, "stdio"),
        sse_count: count_transport(&active, "sse"),
        http_count: count_transport(&active, "http"),
        sse_ide_count: count_transport(&active, "sse-ide"),
        ws_ide_count: count_transport(&active, "ws-ide"),
    };

    let local: Vec<(String, ScopedMcpServerConfig)> = active
        .iter()
        .filter(|(_, config)| is_local_mcp_server(config))
        .cloned()
        .collect();
    let remote: Vec<(String, ScopedMcpServerConfig)> = active
        .iter()
        .filter(|(_, config)| !is_local_mcp_server(config))
        .cloned()
        .collect();

    let mut handles = Vec::new();
    for (group, concurrency) in [
        (local, get_mcp_server_connection_batch_size()),
        (remote, get_remote_mcp_server_connection_batch_size()),
    ] {
        let semaphore = Arc::new(Semaphore::new(concurrency));
        for (name, config) in group {
            let permit_sem = semaphore.clone();
            let handle = tokio::spawn(async move {
                let _permit = permit_sem.acquire_owned().await.ok()?;
                    Some((name.clone(), config.clone()))
            });
            handles.push(handle);
        }
    }

    // 串行收集 spawned 结果并处理（保持回调在调用线程，对齐 TS 顺序语义）
    for handle in handles {
        let Some((name, config)) = handle.await.ok().flatten() else {
            continue;
        };
        if crate::services::mcp::config::is_mcp_server_disabled(&name) {
            continue;
        }
        // 跳过最近 401 的远程服务器（15 分钟 TTL）
        let is_remote = !is_local_mcp_server(&config);
        if is_remote && is_mcp_auth_cached(&name).await {
            on_connection_attempt(ConnectionAttempt {
                client: crate::services::mcp::types::MCPServerConnection::NeedsAuth(
                    crate::services::mcp::types::NeedsAuthMCPServer {
                        name: name.clone(),
                        config: config.clone(),
                    },
                ),
                tools: Vec::new(),
                commands: Vec::new(),
                resources: None,
            });
            continue;
        }

        let connection = connect_to_server(&name, config.clone(), Some(&stats)).await;
        let tools: Vec<McpToolEntry>;
        let commands: Vec<McpPromptCommand>;
        let resources: Option<Vec<ServerResource>>;

        match &connection {
            crate::services::mcp::types::MCPServerConnection::Connected(_) => {
                tools = fetch_tools_for_client(&connection).await;
                commands = fetch_commands_for_client(&connection).await;
                let resources_list = fetch_resources_for_client(&connection).await;
                resources = if resources_list.is_empty() {
                    None
                } else {
                    Some(resources_list)
                };
            }
            _ => {
                tools = Vec::new();
                commands = Vec::new();
                resources = None;
            }
        }
        on_connection_attempt(ConnectionAttempt {
            client: connection,
            tools,
            commands,
            resources,
        });
    }
}

fn count_transport(configs: &[(String, ScopedMcpServerConfig)], kind: &str) -> usize {
    configs
        .iter()
        .filter(|(_, config)| config.config.type_str() == kind)
        .count()
}

/// 预取全部 MCP 资源（启动/重配置时调 2-3 次）。
pub async fn prefetchAll_mcp_resources(
    mcp_configs: HashMap<String, ScopedMcpServerConfig>,
) -> (
    Vec<crate::services::mcp::types::MCPServerConnection>,
    Vec<McpToolEntry>,
    Vec<McpPromptCommand>,
) {
    let mut clients = Vec::new();
    let mut tools = Vec::new();
    let mut commands = Vec::new();
    get_mcp_tools_commands_and_resources(
        |attempt| {
            clients.push(attempt.client);
            tools.extend(attempt.tools);
            commands.extend(attempt.commands);
        },
        Some(mcp_configs),
    )
    .await;
    (clients, tools, commands)
}

/// 重连单个服务器：清缓存 → 连接 → 拉取 tools/commands/resources。
pub async fn reconnect_mcp_server_impl(
    name: &str,
    config: ScopedMcpServerConfig,
) -> (
    crate::services::mcp::types::MCPServerConnection,
    Vec<McpToolEntry>,
    Vec<McpPromptCommand>,
    Option<Vec<ServerResource>>,
) {
    clear_server_cache(name, &config).await;
    let connection = connect_to_server(name, config.clone(), None).await;

    if !matches!(
        connection,
        crate::services::mcp::types::MCPServerConnection::Connected(_)
    ) {
        return (connection, Vec::new(), Vec::new(), None);
    }

    if matches!(
        config.config,
        crate::services::mcp::types::McpServerConfig::ClaudeAiProxy(_)
    ) {
        crate::services::mcp::claudeai::mark_claude_ai_mcp_connected(name);
    }

    let tools = fetch_tools_for_client(&connection).await;
    let commands = fetch_commands_for_client(&connection).await;
    let resources_list = fetch_resources_for_client(&connection).await;
    let resources = if resources_list.is_empty() {
        None
    } else {
        Some(resources_list)
    };
    (connection, tools, commands, resources)
}

/// completions/complete 服务级入口（先查能力，再走 completions 模块）。
pub async fn complete_mcp_resource_template(
    client: &crate::services::mcp::types::ConnectedMCPServer,
    uri: &str,
    argument_name: &str,
    argument_value: &str,
    context_arguments: Option<&HashMap<String, String>>,
) -> Vec<String> {
    if !completions::supports_completions(Some(&client.capabilities)) {
        return Vec::new();
    }
    match ensure_connected_client(client).await {
        Ok((connected, client_ref)) => {
            let connection = CompletionConnection {
                name: connected.name.clone(),
                client: client_ref,
                capabilities: Some(connected.capabilities.clone()),
                config: Some(connected.config.clone()),
            };
            completions::complete_resource_template(
                &connection,
                uri,
                argument_name,
                argument_value,
                context_arguments,
            )
            .await
        }
        Err(e) => {
            log_mcp_error(&client.name, format!("complete failed: {e}").into());
            Vec::new()
        }
    }
}

/// 设置 SDK MCP 客户端（SDK 进程内服务器经控制通道通信）。
pub async fn setup_sdk_mcp_clients(
    sdk_mcp_configs: HashMap<String, crate::services::mcp::types::McpSdkServerConfig>,
    send_mcp_message: crate::services::mcp::sdk_control_transport::SendMcpMessageCallback,
) -> (
    Vec<crate::services::mcp::types::MCPServerConnection>,
    Vec<McpToolEntry>,
) {
    let mut clients = Vec::new();
    let mut tools = Vec::new();

    for (name, config) in sdk_mcp_configs {
        let transport = Arc::new(
            crate::services::mcp::sdk_control_transport::SdkControlClientTransport::new(
                name.clone(),
                send_mcp_message.clone(),
            ),
        );
        let transport_ref = &transport;
        match McpClient::connect(TransportHandle::SdkControl(transport_ref.clone())).await {
            Ok(client) => {
                let capabilities = client.capabilities().await;
                let scoped = ScopedMcpServerConfig::new(
                    crate::services::mcp::types::McpServerConfig::Sdk(config),
                    crate::services::mcp::types::ConfigScope::Dynamic,
                );
                let connection =
                    crate::services::mcp::types::MCPServerConnection::Connected(
                        crate::services::mcp::types::ConnectedMCPServer {
                            client: client.clone(),
                            name: name.clone(),
                            capabilities,
                            server_info: None,
                            instructions: None,
                            health: None,
                            config: scoped,
                        },
                    );
                let server_tools = fetch_tools_for_client(&connection).await;
                tools.extend(server_tools);
                clients.push(connection);
            }
            Err(e) => {
                log_mcp_error(&name, format!("Failed to connect SDK MCP server: {e}").into());
                clients.push(crate::services::mcp::types::MCPServerConnection::Failed(
                    crate::services::mcp::types::FailedMCPServer {
                        name,
                        config: ScopedMcpServerConfig::new(
                            crate::services::mcp::types::McpServerConfig::Sdk(config),
                            crate::services::mcp::types::ConfigScope::User,
                        ),
                        error: Some(e.to_string()),
                    },
                ));
            }
        }
    }

    (clients, tools)
}

/// IDE 工具直连 RPC（对齐 TS `callIdeRpc`）。
pub async fn call_ide_rpc(
    tool_name: &str,
    args: &Value,
    client: &crate::services::mcp::types::ConnectedMCPServer,
) -> Result<McpToolCallResult, McpClientError> {
    call_mcp_tool(client, tool_name, args, None, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sse_events() {
        let body = b"event: endpoint\ndata: /message?x=1\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "endpoint");
        assert_eq!(events[0].1, "/message?x=1");
        assert!(events[1].1.contains("\"id\":1"));
    }

    #[test]
    fn infers_compact_schemas() {
        assert_eq!(infer_compact_schema(&json!({"a": 1, "b": "x"}), 2), "{a: number, b: string}");
        assert_eq!(infer_compact_schema(&json!([1, 2]), 2), "[number]");
        assert_eq!(infer_compact_schema(&json!([]), 2), "[]");
        assert_eq!(infer_compact_schema(&json!(null), 2), "null");
    }

    #[test]
    fn classifier_input_encoding() {
        assert_eq!(
            mcp_tool_input_to_auto_classifier_input(&json!({"a": 1, "b": "x"}), "tool"),
            "a=1 b=x"
        );
        assert_eq!(mcp_tool_input_to_auto_classifier_input(&json!({}), "tool"), "tool");
    }

    #[test]
    fn cache_keys_differ_by_config() {
        let stdio_a = ScopedMcpServerConfig::new(
            crate::services::mcp::types::McpServerConfig::Stdio(
                crate::services::mcp::types::McpStdioServerConfig {
                    type_field: None,
                    command: "a".to_string(),
                    args: vec![],
                    env: None,
                    max_buffer_size_mb: None,
                },
            ),
            crate::services::mcp::types::ConfigScope::User,
        );
        let stdio_b = ScopedMcpServerConfig::new(
            crate::services::mcp::types::McpServerConfig::Stdio(
                crate::services::mcp::types::McpStdioServerConfig {
                    type_field: None,
                    command: "b".to_string(),
                    args: vec![],
                    env: None,
                    max_buffer_size_mb: None,
                },
            ),
            crate::services::mcp::types::ConfigScope::User,
        );
        assert_ne!(
            get_server_cache_key("s", &stdio_a),
            get_server_cache_key("s", &stdio_b)
        );
        assert_eq!(
            get_server_cache_key("s", &stdio_a),
            get_server_cache_key("s", &stdio_a)
        );
    }
}
