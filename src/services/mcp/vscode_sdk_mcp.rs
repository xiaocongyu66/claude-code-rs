//! vscode_sdk_mcp.rs — VSCode MCP 内部桥（重写自 `vscodeSdkMcp.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `notify_vscode_file_updated`：USER_TYPE=ant 且 vscodeMcpClient 已注册时
//!   发 `file_updated` 通知（filePath/oldContent/newContent）；失败仅 debug。
//! - `setup_vscode_sdk_mcp`：从 SDK 客户端列表找 `claude-vscode` 的已连接
//!   客户端，注册 `log_event` 通知处理器（转发 analytics —— 未转换 → debug
//!   日志占位），并立即下发 experiment gates（GrowthBook 门控未转换 →
//!   静态默认值 + autoMode 配置读取）。

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::services::mcp::client::McpClient;
use crate::services::mcp::types::MCPServerConnection;
use crate::utils::log::log_mcp_debug;

/// log_event 通知参数（TS `LogEventNotificationSchema` 的形状）。
#[derive(Debug, serde::Deserialize)]
struct LogEventNotificationParams {
    event_name: String,
    #[allow(dead_code)]
    event_data: serde_json::Value,
}

/// 已注册的 VSCode MCP 客户端引用。
static VSCODE_MCP_CLIENT: once_cell::sync::Lazy<Mutex<Option<Arc<McpClient>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

/// 通知 VSCode 文件已更新（USER_TYPE=ant 且客户端已注册时）。
pub async fn notify_vscode_file_updated(
    file_path: &str,
    old_content: Option<&str>,
    new_content: Option<&str>,
) {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        return;
    }
    let client = {
        let registered = VSCODE_MCP_CLIENT.lock().await;
        registered.clone()
    };
    let Some(client) = client else {
        return;
    };

    let params = serde_json::json!({
        "filePath": file_path,
        "oldContent": old_content,
        "newContent": new_content,
    });
    if let Err(e) = client
        .notification("file_updated", Some(params))
        .await
    {
        // 通知失败不抛（对齐 TS 的 .catch）
        log_mcp_debug(
            "vscode",
            &format!("Failed to send file_updated notification: {e}"),
        );
    }
}

/// 设置 VSCode SDK MCP 双向通信：注册 log_event 处理器 + 下发实验门控。
pub async fn setup_vscode_sdk_mcp(sdk_clients: &[MCPServerConnection]) {
    for client in sdk_clients {
        if client.name() != "claude-vscode" {
            continue;
        }
        let MCPServerConnection::Connected(connected) = client else {
            continue;
        };

        // 保存客户端引用
        {
            let mut registered = VSCODE_MCP_CLIENT.lock().await;
            *registered = Some(connected.client.clone());
        }

        // log_event 通知 → analytics（未转换 → debug 日志占位）
        let handler = Arc::new(|params: &serde_json::Value| {
            let parsed: Result<LogEventNotificationParams, _> =
                serde_json::from_value(params.clone());
            match parsed {
                Ok(notification) => {
                    log_mcp_debug(
                        "vscode",
                        &format!("tengu_vscode_{} event", notification.event_name),
                    );
                }
                Err(_) => {
                    log_mcp_debug("vscode", "Invalid log_event notification params");
                }
            }
        }) as crate::services::mcp::client::ServerNotificationHandler;
        connected
            .client
            .set_notification_handler("log_event", handler)
            .await;

        // 立即下发实验门控（GrowthBook 未转换 → 静态默认值）
        let mut gates = serde_json::json!({
            "tengu_vscode_review_upsell": false,
            "tengu_vscode_onboarding": false,
            // 浏览器支持
            "tengu_quiet_fern": false,
            // 带内 OAuth（claude_authenticate）
            "tengu_vscode_cc_auth": false,
        });
        // autoMode 三态（enabled/disabled/opt-in）；未知时省略让 VSCode fail-closed
        let auto_mode_state = read_auto_mode_enabled_state();
        if let Some(state) = auto_mode_state {
            gates["tengu_auto_mode_state"] = serde_json::json!(state);
        }

        let _ = connected
            .client
            .notification("experiment_gates", Some(serde_json::json!({ "gates": gates })))
            .await;
    }
}

/// 读 autoMode 启用状态（TS 读 GrowthBook `tengu_auto_mode_config` 的
/// `enabled` 字段；analytics/GrowthBook 未转换 → 恒返回 None，让 VSCode
/// fail-closed 视作 disabled）。
fn read_auto_mode_enabled_state() -> Option<&'static str> {
    None
}
