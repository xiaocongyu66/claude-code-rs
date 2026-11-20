//! utils.rs — MCP 纯工具函数（重写自 `utils.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 过滤/排除某服务器的工具、命令、资源（命令区分 prompt 命名
//!   `mcp__<server>__` 与 skill 命名 `<server>:` 两种形态）。
//! - `hash_mcp_config`：对配置做 sha256（键排序、排除 scope），前 16 位。
//! - `exclude_stale_plugin_clients`：dynamic scope 且名称消失、或配置哈希
//!   变化的客户端视为 stale。
//! - scope/transport 校验、header 解析、项目服务器审批状态。
//!
//! 已知偏差（依赖未转换）：
//! - `../../commands.js` 的 `Command` 类型未转换：命令过滤改为名称访问器
//!   泛型 + `CommandLike` 纯数据视图。
//! - `AgentDefinition` / `AgentMcpServerInfo`（components/mcp/types）未转换：
//!   以本模块内的轻量结构体承接。
//! - `getCwd` 用 `crate::utils::cwd::get_cwd`（已转换）。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bootstrap::state::get_is_non_interactive_session;
use crate::services::mcp::mcp_string_utils::mcp_info_from_string;
use crate::services::mcp::normalization::normalize_name_for_mcp;
use crate::services::mcp::types::{ConfigScope, McpServerConfig, ScopedMcpServerConfig, CONFIG_SCOPE_OPTIONS};
use crate::utils::cwd::get_cwd;
use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::settings::constants::is_setting_source_enabled;
use crate::utils::settings::settings::{
    get_initial_settings, has_skip_dangerous_mode_permission_prompt,
};

/// 命令的最小视图（TS `Command` 的字段子集；`Command` 类型未转换）。
#[derive(Debug, Clone)]
pub struct CommandLike {
    pub name: Option<String>,
    /// prompt / skill 等；MCP skill 的 `loadedFrom === 'mcp'`。
    pub kind: Option<String>,
    pub loaded_from: Option<String>,
}

/// 由 name 访问器过滤的工具/命令泛型辅助。
pub fn filter_items_by_server<'a, T, F>(items: &'a [T], name_of: F, server_name: &str) -> Vec<&'a T>
where
    F: Fn(&T) -> Option<&str>,
{
    let prefix = format!("mcp__{}__", normalize_name_for_mcp(server_name));
    items
        .iter()
        .filter(|item| {
            name_of(item)
                .map(|name| name.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect()
}

/// 命令是否属于某 MCP 服务器（prompt `mcp__<server>__x` 或 skill
/// `<server>:x`）。
pub fn command_belongs_to_server(command: &CommandLike, server_name: &str) -> bool {
    let normalized = normalize_name_for_mcp(server_name);
    let Some(name) = command.name.as_deref() else {
        return false;
    };
    name.starts_with(&format!("mcp__{normalized}__"))
        || name.starts_with(&format!("{normalized}:"))
}

/// 按服务器过滤命令（prompt + skill 两种形态）。
pub fn filter_commands_by_server<'a>(
    commands: &'a [CommandLike],
    server_name: &str,
) -> Vec<&'a CommandLike> {
    commands
        .iter()
        .filter(|c| command_belongs_to_server(c, server_name))
        .collect()
}

/// 仅过滤 MCP **prompt**（skill 以 `<server>:` 命名且 `loadedFrom === 'mcp'`，
/// 展示在 `/skills` 而非 capabilities 徽标）。
pub fn filter_mcp_prompts_by_server<'a>(
    commands: &'a [CommandLike],
    server_name: &str,
) -> Vec<&'a CommandLike> {
    commands
        .iter()
        .filter(|c| {
            command_belongs_to_server(c, server_name)
                && !(c.kind.as_deref() == Some("prompt") && c.loaded_from.as_deref() == Some("mcp"))
        })
        .collect()
}

/// 按服务器过滤资源（`resource.server === serverName`）。
pub fn filter_resources_by_server<'a>(
    resources: &'a [crate::services::mcp::types::ServerResource],
    server_name: &str,
) -> Vec<&'a crate::services::mcp::types::ServerResource> {
    resources
        .iter()
        .filter(|r| r.server == server_name)
        .collect()
}

/// 排除属于某服务器的工具（名称前缀匹配，返回新 Vec）。
pub fn exclude_tools_by_server<'a, T, F>(tools: &'a [T], name_of: F, server_name: &str) -> Vec<&'a T>
where
    F: Fn(&T) -> Option<&str>,
{
    let prefix = format!("mcp__{}__", normalize_name_for_mcp(server_name));
    tools
        .iter()
        .filter(|t| !name_of(t).map(|n| n.starts_with(&prefix)).unwrap_or(false))
        .collect()
}

/// 排除属于某服务器的命令。
pub fn exclude_commands_by_server<'a>(
    commands: &'a [CommandLike],
    server_name: &str,
) -> Vec<&'a CommandLike> {
    commands
        .iter()
        .filter(|c| !command_belongs_to_server(c, server_name))
        .collect()
}

/// 排除某服务器的资源 map 条目。
pub fn exclude_resources_by_server(
    resources: &HashMap<String, Vec<crate::services::mcp::types::ServerResource>>,
    server_name: &str,
) -> HashMap<String, Vec<crate::services::mcp::types::ServerResource>> {
    let mut result = resources.clone();
    result.remove(server_name);
    result
}

/// 递归按键排序的稳定 JSON 序列化（对齐 TS `jsonStringify` 的 replacer）。
fn stable_json_stringify(value: &Value) -> String {
    let mut sorted = value.clone();
    sort_object_keys(&mut sorted);
    serde_json::to_string(&sorted).unwrap_or_else(|_| String::from("null"))
}

fn sort_object_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            map.clear();
            for (k, mut v) in entries {
                sort_object_keys(&mut v);
                map.insert(k, v);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sort_object_keys(item);
            }
        }
        _ => {}
    }
}

/// MCP 服务器配置的稳定哈希（排除 scope，键排序，sha256 前 16 位）。
pub fn hash_mcp_config(config: &ScopedMcpServerConfig) -> String {
    let mut json = config.config.to_json_value();
    if let Value::Object(ref mut map) = json {
        map.remove("scope");
    }
    let stable = stable_json_stringify(&json);
    let mut hasher = Sha256::new();
    hasher.update(stable.as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex[..16].to_string()
}

/// stale 客户端过滤结果（TS `excludeStalePluginClients`）。
pub struct StaleFilterResult<T> {
    pub kept: Vec<T>,
    pub stale: Vec<T>,
}

/// 移除陈旧 MCP 客户端：
/// - dynamic scope 且名称不在 configs 中（插件被禁用），或
/// - 配置哈希变化（任意 scope）。
///
/// `is_stale` 由调用方提供对 `MCPServerConnection` 的判定；本函数只做
/// 分区，避免在纯工具层耦合连接类型。
pub fn partition_stale<T, F>(items: Vec<T>, mut is_stale: F) -> StaleFilterResult<T>
where
    F: FnMut(&T) -> bool,
{
    let mut kept = Vec::new();
    let mut stale = Vec::new();
    for item in items {
        if is_stale(&item) {
            stale.push(item);
        } else {
            kept.push(item);
        }
    }
    StaleFilterResult { kept, stale }
}

/// 工具名是否属于某 MCP 服务器（`mcp__server__tool`）。
pub fn is_tool_from_mcp_server(tool_name: &str, server_name: &str) -> bool {
    mcp_info_from_string(tool_name)
        .map(|info| info.server_name == server_name)
        .unwrap_or(false)
}

/// 工具是否来自任意 MCP 服务器（`mcp__` 前缀或 `isMcp` 标志）。
pub fn is_mcp_tool(name: &str, is_mcp_flag: bool) -> bool {
    name.starts_with("mcp__") || is_mcp_flag
}

/// `getGlobalClaudeFile()`：`$CLAUDE_CONFIG_DIR/.claude.json`（回退
/// `$HOME/.claude.json`）。
fn get_global_claude_file() -> PathBuf {
    let config_home = get_claude_config_home_dir();
    PathBuf::from(config_home).join(".claude.json")
}

/// 描述某 scope 的配置文件位置（TS `describeMcpConfigFilePath`）。
pub fn describe_mcp_config_file_path(scope: ConfigScope) -> String {
    match scope {
        ConfigScope::User => get_global_claude_file().to_string_lossy().into_owned(),
        ConfigScope::Project => {
            format!("{}/.mcp.json", get_cwd())
        }
        ConfigScope::Local => format!(
            "{} [project: {}]",
            get_global_claude_file().to_string_lossy(),
            get_cwd()
        ),
        ConfigScope::Dynamic => "Dynamically configured".to_string(),
        ConfigScope::Enterprise => get_enterprise_mcp_file_path(),
        ConfigScope::Claudeai => "claude.ai".to_string(),
        ConfigScope::Managed => "managed".to_string(),
    }
}

/// 企业 MCP 配置路径（config.rs 定义；此处仅用于展示，提前声明）。
fn get_enterprise_mcp_file_path() -> String {
    crate::services::mcp::config::get_enterprise_mcp_file_path()
}

/// scope 的可读标签（TS `getScopeLabel`）。
pub fn get_scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Local => "Local config (private to you in this project)",
        ConfigScope::Project => "Project config (shared via .mcp.json)",
        ConfigScope::User => "User config (available in all your projects)",
        ConfigScope::Dynamic => "Dynamic config (from command line)",
        ConfigScope::Enterprise => "Enterprise config (managed by your organization)",
        ConfigScope::Claudeai => "claude.ai config",
        ConfigScope::Managed => "managed config",
    }
}

/// 校验并返回 scope（非法抛错，对齐 TS `ensureConfigScope`）。
pub fn ensure_config_scope(scope: Option<&str>) -> Result<ConfigScope, String> {
    let Some(scope) = scope else {
        return Ok(ConfigScope::Local);
    };
    if scope.is_empty() {
        return Ok(ConfigScope::Local);
    }
    ConfigScope::from_str_lossy(scope).ok_or_else(|| {
        format!(
            "Invalid scope: {scope}. Must be one of: {}",
            CONFIG_SCOPE_OPTIONS
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// 校验 CLI 可配置的传输类型（stdio/sse/http）。
pub fn ensure_transport(type_str: Option<&str>) -> Result<&'static str, String> {
    let Some(t) = type_str else {
        return Ok("stdio");
    };
    match t {
        "stdio" => Ok("stdio"),
        "sse" => Ok("sse"),
        "http" => Ok("http"),
        other => Err(format!(
            "Invalid transport type: {other}. Must be one of: stdio, sse, http"
        )),
    }
}

/// 解析 `["Header-Name: value", ...]` 为 header map。
pub fn parse_headers(header_array: &[String]) -> Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    for header in header_array {
        let colon_index = header.find(':');
        let Some(colon_index) = colon_index else {
            return Err(format!(
                "Invalid header format: \"{header}\". Expected format: \"Header-Name: value\""
            ));
        };
        let key = header[..colon_index].trim();
        let value = header[colon_index + 1..].trim();
        if key.is_empty() {
            return Err(format!(
                "Invalid header: \"{header}\". Header name cannot be empty."
            ));
        }
        headers.insert(key.to_string(), value.to_string());
    }
    Ok(headers)
}

/// 项目 `.mcp.json` 服务器的审批状态（TS `getProjectMcpServerStatus`）。
pub fn get_project_mcp_server_status(server_name: &str) -> &'static str {
    let settings = get_initial_settings();
    let normalized = normalize_name_for_mcp(server_name);

    let disabled = settings.disabled_mcpjson_servers.as_deref().unwrap_or(&[]);
    if disabled
        .iter()
        .any(|name| normalize_name_for_mcp(name) == normalized)
    {
        return "rejected";
    }

    let enabled = settings.enabled_mcpjson_servers.as_deref().unwrap_or(&[]);
    let enable_all = settings.enable_all_project_mcp_servers.unwrap_or(false);
    if enabled
        .iter()
        .any(|name| normalize_name_for_mcp(name) == normalized)
        || enable_all
    {
        return "approved";
    }

    // 绕过权限模式（--dangerously-skip-permissions）：仅信任 user/local/
    // flag/policy settings（projectSettings 不能代表用户接受对话框）。
    if has_skip_dangerous_mode_permission_prompt()
        && is_setting_source_enabled(crate::bootstrap::state::SettingSource::ProjectSettings)
    {
        return "approved";
    }

    // 非交互模式（SDK / -p / 管道）无弹窗，projectSettings 启用时自动批准。
    if get_is_non_interactive_session()
        && is_setting_source_enabled(crate::bootstrap::state::SettingSource::ProjectSettings)
    {
        return "approved";
    }

    "pending"
}

/// 由工具名反查服务器 scope（TS `getMcpServerScopeFromToolName`）。
pub fn get_mcp_server_scope_from_tool_name(tool_name: &str) -> Option<ConfigScope> {
    if !is_mcp_tool(tool_name, false) {
        return None;
    }
    let mcp_info = mcp_info_from_string(tool_name)?;
    let server_config = crate::services::mcp::config::get_mcp_config_by_name(&mcp_info.server_name);
    match server_config {
        Some(cfg) => Some(cfg.scope),
        None if mcp_info.server_name.starts_with("claude_ai_") => Some(ConfigScope::Claudeai),
        None => None,
    }
}

/// agent frontmatter 中的 MCP 服务器信息（components/mcp/types 未转换，
/// 本模块内定义轻量等价结构）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMcpServerInfo {
    pub name: String,
    pub source_agents: Vec<String>,
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub needs_auth: bool,
}

/// 从 agent 定义（`{ agentType, mcpServers }` 视图）提取内联 MCP 服务器，
/// 按服务器名分组（多个 agent 引用同一服务器时合并 source agents）。
///
/// 输入为 `serde_json::Value` 数组，元素形如
/// `{ "agentType": "...", "mcpServers": [ "name-ref" | { "name": {...config} } ] }`。
pub fn extract_agent_mcp_servers(agents: &[Value]) -> Vec<AgentMcpServerInfo> {
    // server name -> (config raw, source agents)
    let mut server_map: Vec<(String, Value, Vec<String>)> = Vec::new();

    for agent in agents {
        let Some(agent_type) = agent.get("agentType").and_then(Value::as_str) else {
            continue;
        };
        let Some(specs) = agent.get("mcpServers").and_then(Value::as_array) else {
            continue;
        };
        for spec in specs {
            // 字符串引用指向全局配置中的服务器，跳过
            let Some(obj) = spec.as_object() else {
                continue;
            };
            if obj.len() != 1 {
                continue;
            }
            let Some((server_name, server_config)) = obj.iter().next() else {
                continue;
            };
            if server_name.is_empty() {
                continue;
            }
            if let Some(entry) = server_map.iter_mut().find(|(n, _, _)| n == server_name) {
                if !entry.2.iter().any(|a| a == agent_type) {
                    entry.2.push(agent_type.to_string());
                }
            } else {
                server_map.push((
                    server_name.clone(),
                    server_config.clone(),
                    vec![agent_type.to_string()],
                ));
            }
        }
    }

    let mut result = Vec::new();
    for (name, config, source_agents) in server_map {
        let parsed = crate::services::mcp::types::parse_mcp_server_config(&config);
        let Some(parsed) = parsed.config else {
            continue;
        };
        match parsed {
            McpServerConfig::Stdio(c) => result.push(AgentMcpServerInfo {
                name,
                source_agents,
                transport: "stdio".to_string(),
                command: Some(c.command),
                needs_auth: false,
                url: None,
            }),
            McpServerConfig::SSE(c) => result.push(AgentMcpServerInfo {
                name,
                source_agents,
                transport: "sse".to_string(),
                command: None,
                needs_auth: true,
                url: Some(c.url),
            }),
            McpServerConfig::HTTP(c) => result.push(AgentMcpServerInfo {
                name,
                source_agents,
                transport: "http".to_string(),
                command: None,
                needs_auth: true,
                url: Some(c.url),
            }),
            McpServerConfig::WebSocket(c) => result.push(AgentMcpServerInfo {
                name,
                source_agents,
                transport: "ws".to_string(),
                command: None,
                needs_auth: false,
                url: Some(c.url),
            }),
            // sdk / claudeai-proxy / sse-ide / ws-ide 为内部类型，不展示
            _ => {}
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// 日志安全的 MCP base URL：去除 query string（可能含 access token）与
/// 尾斜杠；stdio/sdk 或解析失败返回 None。
pub fn get_logging_safe_mcp_base_url(config: &McpServerConfig) -> Option<String> {
    let url = config.url()?;
    Some(strip_url_query_and_slash(url))
}

/// 去掉 `?` 后的 query 与 `#` 后的 fragment，再去除尾斜杠
/// （无 url crate 依赖的手写最小实现）。
pub(crate) fn strip_url_query_and_slash(url: &str) -> String {
    // 只处理 scheme://... 形态；非法 URL 原样返回裁剪结果
    let scheme_end = url.find("://").unwrap_or(0);
    let rest_start = if scheme_end > 0 { scheme_end + 3 } else { 0 };
    let rest = &url[rest_start..];
    if rest.is_empty() {
        return url.trim_end_matches('/').to_string();
    }
    let path_end = rest
        .find(['?', '#'])
        .unwrap_or(rest.len());
    let host_and_path = &rest[..path_end];
    let trimmed = host_and_path.trim_end_matches('/');
    if scheme_end > 0 {
        format!("{}://{trimmed}", &url[..scheme_end])
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_headers() {
        let headers = parse_headers(&[
            "Authorization: Bearer x".to_string(),
            "X-Custom: a: b".to_string(),
        ])
        .unwrap();
        assert_eq!(headers.get("Authorization").map(|s| s.as_str()), Some("Bearer x"));
        assert_eq!(headers.get("X-Custom").map(|s| s.as_str()), Some("a: b"));
    }

    #[test]
    fn rejects_bad_header() {
        assert!(parse_headers(&["no-colon".to_string()]).is_err());
        assert!(parse_headers(&[": value".to_string()]).is_err());
    }

    #[test]
    fn ensures_scope_and_transport() {
        assert_eq!(ensure_config_scope(None).unwrap(), ConfigScope::Local);
        assert_eq!(ensure_config_scope(Some("user")).unwrap(), ConfigScope::User);
        assert!(ensure_config_scope(Some("bogus")).is_err());
        assert_eq!(ensure_transport(None).unwrap(), "stdio");
        assert_eq!(ensure_transport(Some("sse")).unwrap(), "sse");
        assert!(ensure_transport(Some("ws")).is_err());
    }

    #[test]
    fn strips_url_query() {
        assert_eq!(
            strip_url_query_and_slash("https://example.com/mcp?token=x"),
            "https://example.com/mcp"
        );
        assert_eq!(
            strip_url_query_and_slash("https://example.com/"),
            "https://example.com"
        );
    }

    #[test]
    fn hashes_configs_stably() {
        let cfg = ScopedMcpServerConfig::new(
            crate::services::mcp::types::McpServerConfig::Stdio(
                crate::services::mcp::types::McpStdioServerConfig {
                    type_field: None,
                    command: "x".to_string(),
                    args: vec![],
                    env: None,
                    max_buffer_size_mb: None,
                },
            ),
            ConfigScope::User,
        );
        let h1 = hash_mcp_config(&cfg);
        let h2 = hash_mcp_config(&cfg);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn extracts_agent_mcp_servers() {
        let agents = vec![json!({
            "agentType": "researcher",
            "mcpServers": [
                "global-ref",
                { "inline": { "type": "stdio", "command": "run" } }
            ]
        })];
        let info = extract_agent_mcp_servers(&agents);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].name, "inline");
        assert_eq!(info[0].transport, "stdio");
        assert_eq!(info[0].source_agents, vec!["researcher".to_string()]);
    }
}
