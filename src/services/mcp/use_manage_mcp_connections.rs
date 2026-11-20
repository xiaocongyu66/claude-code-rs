//! use_manage_mcp_connections.rs — MCP 连接生命周期管理
//! （重写自 `useManageMCPConnections.ts`）。
//!
//! TS 原文是 React hook，缠绕在 AppState 之上（批处理更新、重连退避、
//! channel 通知注册、健康检查启停、list_changed 增量刷新）。Rust 无
//! React/AppState：这里以 `UseManageMcpConnections` 管理器结构体承载
//! 同样的核心语义，UI 回调以泛型/闭包注入。
//!
//! 语义对齐（对 TS 原文）：
//! - MAX_RECONNECT_ATTEMPTS=5 / INITIAL_BACKOFF_MS=1000 / MAX_BACKOFF_MS=30000
//!   指数退避重连。
//! - 服务器启停（disabled/disabledMcpServers）。
//! - channel 通知：gateChannelServer 门控后注册
//!   `notifications/claude/channel`/`.../permission` 处理器。
//! - 健康检查：isMcpHealthCheckEnabled 时 start/stop。
//! - list_changed 通知：tools/resources/prompts 缓存清除 + 增量刷新。
//! - 批量 flush（TS 的 16ms 合并）→ Rust 侧直接按服务器即时提交
//!   （DeferredUpdates 在无 React scheduler 时无意义）。
//!
//! 已知偏差：
//! - AppState 未转换：连接结果以 `McpConnectionSnapshot`（纯数据）回调。
//! - analytics 未转换 → debug 日志占位。
//! - skills（MCP_SKILLS）未实现。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::services::mcp::channel_notification::{
    gate_channel_server, ChannelGateResult,
};
use crate::services::mcp::client::{
    clear_fetch_caches, clear_server_cache, connect_to_server, fetch_commands_for_client,
    fetch_resources_for_client, fetch_tools_for_client, reconnect_mcp_server_impl, McpPromptCommand,
    McpToolEntry, ServerStats,
};
use crate::services::mcp::config::{
    get_claude_code_mcp_configs, is_mcp_server_disabled, set_mcp_server_enabled,
};
use crate::services::mcp::health_check::{
    is_mcp_health_check_enabled, start_health_check, stop_health_check, HealthCheckOptions,
};
use crate::services::mcp::types::{MCPServerConnection, ScopedMcpServerConfig, ServerResource};
use crate::utils::log::{log_mcp_debug, log_mcp_error};

/// 重连退避上限（5 次）。
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// 初始退避（1s）。
const INITIAL_BACKOFF_MS: u64 = 1000;
/// 最大退避（30s）。
const MAX_BACKOFF_MS: u64 = 30_000;

/// 连接结果快照（UI 层据此更新展示）。
#[derive(Clone)]
pub struct ConnectionSnapshot {
    pub client: MCPServerConnection,
    pub tools: Vec<McpToolEntry>,
    pub commands: Vec<McpPromptCommand>,
    pub resources: Option<Vec<ServerResource>>,
}

/// MCP 连接管理器（useManageMCPConnections 的无界面等价物）。
pub struct UseManageMcpConnections {
    dynamic_mcp_config: HashMap<String, ScopedMcpServerConfig>,
    is_strict_mcp_config: bool,
    /// 已连接的服务器（名称 → 连接与状态）。由调用方持有并驱动更新。
    pub connections: HashMap<String, ConnectionSnapshot>,
    reconnect_attempts: HashMap<String, u32>,
}

/// 外部句柄（mcp_connection_manager.rs 用）。
pub struct McpConnectionHandle {
    pub connections: Arc<Mutex<UseManageMcpConnections>>,
}

impl UseManageMcpConnections {
    pub fn new(
        dynamic_mcp_config: HashMap<String, ScopedMcpServerConfig>,
        is_strict_mcp_config: bool,
    ) -> Self {
        Self {
            dynamic_mcp_config,
            is_strict_mcp_config,
            connections: HashMap::new(),
            reconnect_attempts: HashMap::new(),
        }
    }

    /// 连接全部配置的服务器（启动与配置变更时调用）。
    pub async fn connect_all<F>(&mut self, mut on_snapshot: F)
    where
        F: FnMut(ConnectionSnapshot) + Send,
    {
        let configs = if self.is_strict_mcp_config {
            self.dynamic_mcp_config.clone()
        } else {
            let mut configs = get_claude_code_mcp_configs(
                self.dynamic_mcp_config.clone(),
                HashMap::new(),
            )
            .await
            .servers;
            for (name, config) in &self.dynamic_mcp_config {
                configs.insert(name.clone(), config.clone());
            }
            configs
        };

        for (name, config) in configs {
            if is_mcp_server_disabled(&name) {
                on_snapshot(ConnectionSnapshot {
                    client: MCPServerConnection::Disabled(
                        crate::services::mcp::types::DisabledMCPServer { name, config },
                    ),
                    tools: Vec::new(),
                    commands: Vec::new(),
                    resources: None,
                });
                continue;
            }
            let stats = ServerStats::default();
            let connection = connect_to_server(&name, config, Some(&stats)).await;
            let tools = fetch_tools_for_client(&connection).await;
            let commands = fetch_commands_for_client(&connection).await;
            let resources = fetch_resources_for_client(&connection).await;

            // 健康检查（opt-in）
            if is_mcp_health_check_enabled() {
                if let MCPServerConnection::Connected(connected) = &connection {
                    start_health_check(
                        &name,
                        connected.client.clone(),
                        HealthCheckOptions::default(),
                    )
                    .await;
                }
            }

            let snapshot = ConnectionSnapshot {
                resources: if resources.is_empty() {
                    None
                } else {
                    Some(resources.clone())
                },
                tools: tools.clone(),
                commands: commands.clone(),
                client: connection.clone(),
            };
            self.connections.insert(name, snapshot.clone());
            on_snapshot(ConnectionSnapshot {
                resources: if resources.is_empty() { None } else { Some(resources) },
                tools,
                commands,
                client: connection,
            });
        }
    }

    /// 重连指定服务器（reconnectMcpServer；先 toggle disabled 则直接失败）。
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> crate::services::mcp::mcp_connection_manager::ReconnectResult {
        let (client, tools, commands, resources) =
            reconnect_mcp_server_impl(server_name, self.server_config(server_name)).await;
        crate::services::mcp::mcp_connection_manager::ReconnectResult {
            client,
            tools,
            commands,
            resources,
        }
    }

    /// 启停指定服务器（toggleMcpServer）。
    pub async fn toggle_mcp_server(&self, server_name: &str) {
        let currently_disabled = is_mcp_server_disabled(server_name);
        let new_enabled = currently_disabled;
        set_mcp_server_enabled(server_name, new_enabled);
        if new_enabled {
            let _ = self.reconnect_mcp_server(server_name).await;
        } else {
            // 禁用：停健康检查 + 清连接与 fetch 缓存
            stop_health_check(server_name).await;
            if let Some(config) = self.server_config_optional(server_name) {
                clear_server_cache(server_name, &config).await;
            } else {
                clear_fetch_caches(server_name).await;
            }
        }
    }

    /// channel 门控（gateChannelServer）—— UI 注册前调用决定是否注册
    /// `notifications/claude/channel`。
    pub fn gate_channel(
        &self,
        server_name: &str,
        capabilities: &crate::services::mcp::types::ServerCapabilities,
    ) -> ChannelGateResult {
        let config = self.server_config_optional(server_name);
        gate_channel_server(
            server_name,
            Some(capabilities),
            config.as_ref().and_then(|c| c.plugin_source.as_deref()),
        )
    }

    fn server_config(&self, server_name: &str) -> ScopedMcpServerConfig {
        self.server_config_optional(server_name)
            .or_else(|| {
                crate::services::mcp::config::get_mcp_config_by_name(server_name)
            })
            .unwrap_or_else(|| ScopedMcpServerConfig::new(
                crate::services::mcp::types::McpServerConfig::Stdio(
                    crate::services::mcp::types::McpStdioServerConfig {
                        type_field: Some("stdio".to_string()),
                        command: String::new(),
                        args: Vec::new(),
                        env: None,
                        max_buffer_size_mb: None,
                    },
                ),
                crate::services::mcp::types::ConfigScope::Dynamic,
            ))
    }

    fn server_config_optional(&self, server_name: &str) -> Option<ScopedMcpServerConfig> {
        self.connections.get(server_name).map(|s| s.client.config().clone())
            .or_else(|| self.dynamic_mcp_config.get(server_name).cloned())
    }

    /// 指数退避重连（对齐 TS 的 reconnectWithBackoff）。
    pub async fn reconnect_with_backoff(&mut self, server_name: &str) {
        let attempt = self
            .reconnect_attempts
            .entry(server_name.to_string())
            .or_insert(0);
        if *attempt >= MAX_RECONNECT_ATTEMPTS {
            log_mcp_error(
                server_name,
                format!("Max reconnection attempts ({MAX_RECONNECT_ATTEMPTS}) reached").into(),
            );
            return;
        }
        *attempt += 1;
        let backoff_ms = std::cmp::min(
            INITIAL_BACKOFF_MS * 2u64.pow(*attempt - 1),
            MAX_BACKOFF_MS,
        );
        let attempt_log = *attempt;
        drop(attempt);
        log_mcp_debug(
            server_name,
            &format!("Reconnecting in {backoff_ms}ms (attempt {attempt_log}/{MAX_RECONNECT_ATTEMPTS})"),
        );
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;

        let config = self.server_config(server_name);
        let connection = connect_to_server(server_name, config.clone(), None).await;
        if matches!(
            connection,
            MCPServerConnection::Connected(_)
        ) {
            self.reconnect_attempts.remove(server_name);
            let tools = fetch_tools_for_client(&connection).await;
            let commands = fetch_commands_for_client(&connection).await;
            let resources = fetch_resources_for_client(&connection).await;
            self.connections.insert(server_name.to_string(), ConnectionSnapshot {
                client: connection,
                tools,
                commands,
                resources: if resources.is_empty() { None } else { Some(resources) },
            });
        }
    }

    /// 处理工具/资源/提示列表变更通知（list_changed）。
    pub async fn handle_list_changed(&mut self, server_name: &str, kind: ListChangedKind) {
        clear_fetch_caches(server_name).await;
        let Some(snapshot) = self.connections.get_mut(server_name) else {
            return;
        };
        if !matches!(snapshot.client, MCPServerConnection::Connected(_)) {
            return;
        }
        match kind {
            ListChangedKind::Tools => {
                snapshot.tools = fetch_tools_for_client(&snapshot.client).await;
            }
            ListChangedKind::Resources => {
                let resources = fetch_resources_for_client(&snapshot.client).await;
                snapshot.resources = if resources.is_empty() {
                    None
                } else {
                    Some(resources)
                };
            }
            ListChangedKind::Prompts => {
                snapshot.commands = fetch_commands_for_client(&snapshot.client).await;
            }
        }
    }
}

/// list_changed 通知类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChangedKind {
    Tools,
    Resources,
    Prompts,
}

/// 指定服务器是否需要重连（对齐 TS 的 isReconnectNeeded 判定）。
pub fn is_reconnect_needed(
    old: &ScopedMcpServerConfig,
    new: &ScopedMcpServerConfig,
) -> bool {
    !crate::services::mcp::client::are_mcp_configs_equal(old, new)
}

#[allow(dead_code)]
fn unused_link() {
    let _ = crate::services::mcp::claudeai::has_claude_ai_mcp_ever_connected;
}
