//! headers_helper.rs — MCP 服务器动态 header 获取
//! （重写自 `headersHelper.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `get_mcp_headers_from_helper`：执行 config.headersHelper 脚本（shell、
//!   10s 超时），注入 `CLAUDE_CODE_MCP_SERVER_NAME/URL` 环境变量；stdout
//!   必须是 JSON 对象且值全为字符串，否则返回 None（不抛错）。
//! - 安全检查：project/local scope 且非交互会话时，必须先通过工作区
//!   信任检查（`checkHasTrustDialogAccepted`）才允许执行脚本。
//! - `get_mcp_server_headers`：静态 headers 与动态 headers 合并，动态覆盖
//!   静态。
//!
//! 已知偏差：
//! - `utils/execFileNoThrow.ts` 未转换：用 `tokio::process::Command`
//!   直接执行（shell=true 语义：经 sh -c）。成功 stdout/退出码语义一致。
//! - `getIsNonInteractiveSession`（bootstrap/state，已转换）、
//!   `checkHasTrustDialogAccepted`（utils/auth，已转换）直接复用。

use std::collections::HashMap;

use tokio::process::Command;

use crate::bootstrap::state::get_is_non_interactive_session;
use crate::services::mcp::types::ScopedMcpServerConfig;
use crate::utils::auth::check_has_trust_dialog_accepted;
use crate::utils::debug::{log_ant_error, log_for_debugging, DebugLogLevel};
use crate::utils::log::{log_error, log_mcp_debug, log_mcp_error};

/// 命令输出（对齐 execFileNoThrow 的结果形状）。
struct ExecResult {
    code: i32,
    stdout: String,
}

/// project/local scope 判定（安全检查用）。
fn is_mcp_server_from_project_or_local_settings(config: &ScopedMcpServerConfig) -> bool {
    matches!(
        config.scope,
        crate::services::mcp::types::ConfigScope::Project
            | crate::services::mcp::types::ConfigScope::Local
    )
}

/// 执行 headersHelper 并返回解析出的 header map；未配置或失败返回 None。
///
/// `config_json` 为序列化后的服务器配置 Value（由调用方从
/// `McpServerConfig` 转换），用于读取 `headersHelper`/`url`/`headers`。
pub async fn get_mcp_headers_from_helper(
    server_name: &str,
    config_json: &serde_json::Value,
    scoped: Option<&ScopedMcpServerConfig>,
) -> Option<HashMap<String, String>> {
    let headers_helper = config_json
        .get("headersHelper")
        .and_then(serde_json::Value::as_str)?;
    let url = config_json
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    // 安全检查：project/local 配置在交互会话中必须先建立工作区信任
    if let Some(scoped) = scoped {
        if is_mcp_server_from_project_or_local_settings(scoped) && !get_is_non_interactive_session()
        {
            if !check_has_trust_dialog_accepted() {
                let error = format!(
                    "Security: headersHelper for MCP server '{server_name}' executed before workspace trust is confirmed."
                );
                let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, &*error);
                log_ant_error("MCP headersHelper invoked before trust check", &err);
                log_for_debugging(
                    "MCP headersHelper invoked before trust check",
                    DebugLogLevel::Error,
                );
                return None;
            }
        }
    }

    log_mcp_debug(server_name, "Executing headersHelper to get dynamic headers");

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(headers_helper)
        .env("CLAUDE_CODE_MCP_SERVER_NAME", server_name)
        .env("CLAUDE_CODE_MCP_SERVER_URL", url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        command.output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            let msg = format!("Error getting headers from headersHelper: {e}");
            log_mcp_error(server_name, msg.clone().into());
            log_error(format!(
                "Error getting MCP headers from headersHelper for server '{server_name}': {msg}"
            ));
            return None;
        }
        Err(_) => {
            let msg = "Error getting headers from headersHelper: timed out after 10s";
            log_mcp_error(server_name, msg.into());
            return None;
        }
    };

    let exec = ExecResult {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    };

    if exec.code != 0 || exec.stdout.is_empty() {
        let msg = format!(
            "headersHelper for MCP server '{server_name}' did not return a valid value"
        );
        log_mcp_error(
            server_name,
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, msg.clone())),
        );
        // 对齐 TS：不阻塞连接
        return None;
    }

    let result = exec.stdout.trim();
    let parsed: serde_json::Value = match crate::utils::slow_operations::json_parse(result) {
        Ok(v) => v,
        Err(_) => {
            let msg = format!(
                "headersHelper for MCP server '{server_name}' must return a JSON object with string key-value pairs"
            );
            log_mcp_error(server_name, msg.into());
            return None;
        }
    };

    let obj = match parsed.as_object() {
        Some(obj) => obj,
        None => {
            let msg = format!(
                "headersHelper for MCP server '{server_name}' must return a JSON object with string key-value pairs"
            );
            log_mcp_error(server_name, msg.into());
            return None;
        }
    };

    let mut headers = HashMap::new();
    for (key, value) in obj {
        match value.as_str() {
            Some(s) => {
                headers.insert(key.clone(), s.to_string());
            }
            None => {
                let msg = format!(
                    "headersHelper for MCP server '{server_name}' returned non-string value for key \"{key}\""
                );
                log_mcp_error(server_name, msg.into());
                return None;
            }
        }
    }

    log_mcp_debug(
        server_name,
        &format!("Successfully retrieved {} headers from headersHelper", headers.len()),
    );
    Some(headers)
}

/// 合并静态 + 动态 headers（动态覆盖同名静态键）。
pub async fn get_mcp_server_headers(
    server_name: &str,
    config_json: &serde_json::Value,
    scoped: Option<&ScopedMcpServerConfig>,
) -> HashMap<String, String> {
    let mut headers: HashMap<String, String> = config_json
        .get("headers")
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    if let Some(dynamic) = get_mcp_headers_from_helper(server_name, config_json, scoped).await {
        for (k, v) in dynamic {
            headers.insert(k, v);
        }
    }
    headers
}
