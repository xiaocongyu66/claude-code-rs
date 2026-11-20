//! mcp_string_utils.rs — MCP 工具/服务器名解析的纯字符串工具
//! （重写自 `mcpStringUtils.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `mcp_info_from_string`：解析 `mcp__serverName__toolName`，保留工具名
//!   中的双下划线（已知限制：服务器名含 `__` 时解析不正确）。
//! - `get_mcp_prefix` / `build_mcp_tool_name`：服务器名与工具名均经
//!   `normalize_name_for_mcp` 规范化后拼接。
//! - `get_tool_name_for_permission_check`：MCP 工具用全限定名做权限匹配。
//! - `get_mcp_display_name`：去除 `mcp__<server>__` 前缀。
//! - `extract_mcp_tool_display_name`：从 `"<server> - <tool> (MCP)"`
//!   用户可见名中提取展示名。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::services::mcp::normalization::normalize_name_for_mcp;

/// MCP 工具信息（serverName + 可选 toolName）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInfo {
    pub server_name: String,
    pub tool_name: Option<String>,
}

/// 从工具名解析 MCP 服务器/工具名。
///
/// 已知限制：服务器名含 `__` 时解析不正确（例如
/// `mcp__my__server__tool` 解析为 server="my"、tool="server__tool"）。
pub fn mcp_info_from_string(tool_string: &str) -> Option<McpInfo> {
    let parts: Vec<&str> = tool_string.split("__").collect();
    if parts.len() < 2 || parts[0] != "mcp" || parts[1].is_empty() {
        return None;
    }
    let server_name = parts[1].to_string();
    let tool_name = if parts.len() > 2 {
        Some(parts[2..].join("__"))
    } else {
        None
    };
    Some(McpInfo {
        server_name,
        tool_name,
    })
}

/// 生成 MCP 工具/命令名前缀：`mcp__<normalized>__`。
pub fn get_mcp_prefix(server_name: &str) -> String {
    format!("mcp__{}__", normalize_name_for_mcp(server_name))
}

/// 由服务器名 + 工具名构造全限定 MCP 工具名（两侧都做规范化）。
pub fn build_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        normalize_name_for_mcp(server_name),
        normalize_name_for_mcp(tool_name)
    )
}

/// 权限规则匹配用的工具名：MCP 工具用全限定名，其余用 `tool.name`。
///
/// 语义对齐 TS `getToolNameForPermissionCheck(tool)`，参数为扁平结构，
/// 由调用方从 `Tool` 上取 `name` / `mcpInfo`。
pub fn get_tool_name_for_permission_check(
    name: &str,
    mcp_info: Option<&crate::tool::McpInfo>,
) -> String {
    match mcp_info {
        Some(info) => build_mcp_tool_name(&info.server_name, &info.tool_name),
        None => name.to_string(),
    }
}

static MCP_SUFFIX_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"\s*\(MCP\)\s*$").ok());

/// 从全限定 MCP 工具/命令名中移除 `mcp__<server>__` 前缀，返回展示名。
pub fn get_mcp_display_name(full_name: &str, server_name: &str) -> String {
    let prefix = get_mcp_prefix(server_name);
    full_name
        .strip_prefix(&prefix)
        .map(|rest| rest.to_string())
        .unwrap_or_else(|| full_name.to_string())
}

/// 从用户可见名（如 `"github - Add comment to issue (MCP)"`）提取工具
/// 展示名：先去 `(MCP)` 后缀，再截掉 `" - "` 之前的服务器前缀。
pub fn extract_mcp_tool_display_name(user_facing_name: &str) -> String {
    let without_suffix = match MCP_SUFFIX_RE.as_ref() {
        Some(rx) => rx.replace_all(user_facing_name, "").trim().to_string(),
        None => user_facing_name.trim().to_string(),
    };
    match without_suffix.find(" - ") {
        Some(dash_index) => without_suffix[dash_index + 3..].trim().to_string(),
        None => without_suffix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_tool_names() {
        assert_eq!(
            mcp_info_from_string("mcp__github__create_issue"),
            Some(McpInfo {
                server_name: "github".to_string(),
                tool_name: Some("create_issue".to_string()),
            })
        );
        assert_eq!(mcp_info_from_string("mcp__github"), Some(McpInfo {
            server_name: "github".to_string(),
            tool_name: None,
        }));
        assert_eq!(mcp_info_from_string("Bash"), None);
        assert_eq!(mcp_info_from_string("mcp____tool"), None);
        // 双下划线保留在工具名一侧
        assert_eq!(
            mcp_info_from_string("mcp__srv__a__b"),
            Some(McpInfo {
                server_name: "srv".to_string(),
                tool_name: Some("a__b".to_string()),
            })
        );
    }

    #[test]
    fn builds_and_displays_names() {
        assert_eq!(build_mcp_tool_name("my srv", "do it"), "mcp__my_srv__do_it");
        assert_eq!(get_mcp_display_name("mcp__my_srv__do_it", "my srv"), "do_it");
        assert_eq!(
            extract_mcp_tool_display_name("github - Add comment (MCP)"),
            "Add comment"
        );
        assert_eq!(extract_mcp_tool_display_name("no suffix"), "no suffix");
    }

    #[test]
    fn permission_check_name() {
        assert_eq!(
            get_tool_name_for_permission_check(
                "ignored",
                Some(&crate::tool::McpInfo {
                    server_name: "s".to_string(),
                    tool_name: "t".to_string(),
                })
            ),
            "mcp__s__t"
        );
        assert_eq!(get_tool_name_for_permission_check("Bash", None), "Bash");
    }
}
