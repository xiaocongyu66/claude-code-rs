//! src/services/mcp — MCP 客户端主体。
//!
//! 重写自 TS 同名目录：服务器配置加载（config.rs）、连接生命周期
//! （client.rs + use_manage_mcp_connections.rs）、stdio/SSE/HTTP 传输、
//! initialize 双步握手、工具列表与转发、OAuth（auth.rs + xaa*）。
//!
//! 未转换的依赖（留占位/最小实现，见各文件头）：
//! - `services/analytics`（logEvent / GrowthBook 门控）
//! - `state/AppState`（React 状态）
//! - `utils/secureStorage`（macOS keychain —— 本目录以文件存储 duck-type）
//! - `utils/config.ts`（getGlobalConfig/getCurrentProjectConfig —— 本目录内
//!   duck-type 读取 ~/.claude.json）
//! - `utils/plugins/*`、`utils/browser.ts`、`utils/http.ts`、`utils/proxy.ts`、
//!   `utils/mtls.ts`、`utils/mcpWebSocketTransport.ts`（WebSocket 传输未实现）
//! - `utils/hooks.ts`（elicitation hooks）
//! - `@modelcontextprotocol/sdk`（JSON-RPC 层按 MCP 规范手写实现，见
//!   types.rs / client.rs）
pub mod auth;
pub mod channel_allowlist;
pub mod channel_notification;
pub mod channel_permissions;
pub mod claudeai;
pub mod client;
pub mod completions;
pub mod config;
pub mod elicitation_handler;
pub mod env_expansion;
pub mod headers_helper;
pub mod health_check;
pub mod in_process_transport;
pub mod mcp_connection_manager;
pub mod mcp_string_utils;
pub mod normalization;
pub mod oauth_port;
pub mod official_registry;
pub mod sdk_control_transport;
pub mod tunnels;
pub mod types;
pub mod use_manage_mcp_connections;
pub mod utils;
pub mod vscode_sdk_mcp;
pub mod xaa;
pub mod xaa_idp_login;
