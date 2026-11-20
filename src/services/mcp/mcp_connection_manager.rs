//! mcp_connection_manager.rs — MCP 连接管理器（重写自
//! `MCPConnectionManager.tsx`）。
//!
//! TS 原文是 React Context Provider（useMcpReconnect / useMcpToggleEnabled
//! 两个 hook + MCPConnectionManager 组件）。Rust 无 React/Ink 层：
//! 这里保留等价的**动作接口**（reconnect / toggle），把 Context 换成
//! 进程内管理器结构体；UI 层（screens）后续以普通函数调用接入。
//!
//! 语义对齐：
//! - 两个动作均来自 `use_manage_mcp_connections`（本模块的连接管理实现）。
//! - 动态配置（dynamicMcpConfig）与严格模式（isStrictMcpConfig）透传。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::services::mcp::types::{MCPServerConnection, ScopedMcpServerConfig};
use crate::services::mcp::use_manage_mcp_connections::{
    McpConnectionHandle, UseManageMcpConnections,
};

/// MCP 连接管理器（React Context Provider 的无界面等价物）。
pub struct MCPConnectionManager {
    connections: Arc<Mutex<UseManageMcpConnections>>,
}

/// 重连结果（对齐 TS `reconnectMcpServer` 的返回值）。
pub struct ReconnectResult {
    pub client: MCPServerConnection,
    pub tools: Vec<crate::services::mcp::client::McpToolEntry>,
    pub commands: Vec<crate::services::mcp::client::McpPromptCommand>,
    pub resources: Option<Vec<crate::services::mcp::types::ServerResource>>,
}

impl MCPConnectionManager {
    pub fn new(
        dynamic_mcp_config: Option<HashMap<String, ScopedMcpServerConfig>>,
        is_strict_mcp_config: bool,
    ) -> Self {
        Self {
            connections: Arc::new(Mutex::new(UseManageMcpConnections::new(
                dynamic_mcp_config.unwrap_or_default(),
                is_strict_mcp_config,
            ))),
        }
    }

    /// useMcpReconnect：重连指定服务器。
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> ReconnectResult {
        let connections = self.connections.lock().await;
        connections.reconnect_mcp_server(server_name).await
    }

    /// useMcpToggleEnabled：启停指定服务器。
    pub async fn toggle_mcp_server(&self, server_name: &str) {
        let connections = self.connections.lock().await;
        connections.toggle_mcp_server(server_name).await;
    }

    /// 内部句柄（其它平台层可能需要直接访问连接表）。
    pub fn handle(&self) -> McpConnectionHandle {
        McpConnectionHandle {
            connections: self.connections.clone(),
        }
    }
}
