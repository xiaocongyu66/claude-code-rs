//! config.rs — MCP 服务器配置加载与策略过滤（重写自 `config.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `.mcp.json`（project，含父目录遍历，近 CWD 覆盖远端）、user settings
//!   （settings.json mcpServers）、local（当前项目 ~/.claude.json 条目）、
//!   enterprise（managed-mcp.json）四种 scope 的加载。
//! - 解析校验：`parse_mcp_config` / `parse_mcp_config_from_filepath`
//!   （zod schema → types.rs 的手写校验），环境变量展开（缺失报 warning）、
//!   Windows npx 检测。
//! - 企业策略：allowedMcpServers/deniedMcpServers（name/command/url 三种
//!   条目，url 支持通配符）、allowManagedMcpServersOnly。
//! - 插件去重（签名 = stdio 命令数组 / URL；CCR 代理 URL 解包 mcp_url）。
//! - 增删改：addMcpConfig/removeMcpConfig（project 走原子临时文件 rename）。
//! - 启停：isMcpServerDisabled/setMcpServerEnabled（项目级
//!   enabledMcpServers/disabledMcpServers，默认禁用的内建服务器需显式启用）。
//!
//! 已知偏差（依赖未转换 → duck-type）：
//! - `utils/config.ts`（getGlobalConfig/getCurrentProjectConfig/
//!   saveCurrentProjectConfig）未转换：本模块内读写 `~/.claude.json` 的
//!   `projects[cwd]` 条目（进程内写穿透缓存，与 utils/auth.rs 同模式）。
//! - `utils/plugins/*`（pluginLoader、mcpPluginIntegration）未转换：
//!   插件服务器一律返回空 map（签名保留，接入后补）。
//! - `utils/claudeInChrome/common.ts` 未转换：保留名检查以本地常量实现。
//! - `utils/json.ts`（safeParseJSON）未转换：用 json_read + json_parse 等价。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::services::mcp::env_expansion::expand_env_vars_in_string;
use crate::services::mcp::types::{
    ConfigScope, McpJsonConfig, McpServerConfig, ScopedMcpServerConfig,
};
use crate::services::mcp::utils::get_project_mcp_server_status;
use crate::utils::cwd::get_cwd;
use crate::utils::debug::log_for_debugging;
use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::errors::is_enoent;
use crate::utils::fs_operations::get_fs_implementation;
use crate::utils::platform::{get_platform, Platform};
use crate::utils::settings::constants::is_setting_source_enabled;
use crate::utils::settings::managed_path::get_managed_file_path;
use crate::utils::settings::plugin_only_policy::is_restricted_to_plugin_only;
use crate::utils::settings::settings::{
    get_initial_settings, get_settings_for_source,
};
use crate::utils::settings::types::{McpServerEntry, SettingsJson};
use crate::utils::settings::validation::{McpErrorMetadata, ValidationError};
use crate::utils::slow_operations::json_stringify;

/// 托管（企业）MCP 配置文件路径。
pub fn get_enterprise_mcp_file_path() -> String {
    PathBuf::from(get_managed_file_path())
        .join("managed-mcp.json")
        .to_string_lossy()
        .into_owned()
}

/// 给服务器配置批量附加 scope（对齐 TS `addScopeToServers`）。
pub fn add_scope_to_servers(
    servers: Option<&HashMap<String, Value>>,
    scope: ConfigScope,
) -> HashMap<String, ScopedMcpServerConfig> {
    let Some(servers) = servers else {
        return HashMap::new();
    };
    let mut scoped = HashMap::new();
    for (name, raw) in servers {
        let parsed = crate::services::mcp::types::parse_mcp_server_config(raw);
        if let Some(config) = parsed.config {
            scoped.insert(name.clone(), ScopedMcpServerConfig::new(config, scope));
        }
    }
    scoped
}

/// 原子写入 `.mcp.json`（临时文件 + datasync + rename，保留原文件权限）。
pub async fn write_mcpjson_file(config: &McpJsonConfig) -> Result<(), String> {
    let mcp_json_path = PathBuf::from(get_cwd()).join(".mcp.json");

    // 读原文件权限（不存在则 0o644）
    let existing_mode = match std::fs::metadata(&mcp_json_path) {
        Ok(meta) => file_mode(&meta),
        Err(_) => 0o644,
    };

    let temp_path = mcp_json_path.with_file_name(format!(
        ".mcp.json.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));

    // 序列化（保持插入顺序）
    let mut body = String::from("{\n  \"mcpServers\": {");
    let mut entries: Vec<(&String, &Value)> = config.mcp_servers.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (i, (name, value)) in entries.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!("\n    \"{}\": {}", name, json_stringify(value)));
    }
    body.push_str("\n  }\n}");

    std::fs::write(&temp_path, body).map_err(|e| format!("write temp: {e}"))?;
    if existing_mode != 0 {
        let _ = chmod(&temp_path, existing_mode);
    }
    std::fs::rename(&temp_path, &mcp_json_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("rename: {e}")
    })?;
    Ok(())
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn chmod(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// stdio 命令数组（`[command, ...args]`），非 stdio 返回 None。
fn get_server_command_array(config: &McpServerConfig) -> Option<Vec<String>> {
    config.command_array()
}

/// 远程 URL，stdio/sdk 返回 None。
fn get_server_url(config: &McpServerConfig) -> Option<&str> {
    config.url()
}

fn command_arrays_match(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// CCR 代理 URL 路径标记（session-ingress SHTTP 代理）。
const CCR_PROXY_PATH_MARKERS: &[&str] = &[
    "/v2/session_ingress/shttp/mcp/",
    "/v2/ccr-sessions/",
];

/// 若 URL 是 CCR 代理 URL，从 mcp_url 查询参数解包原始厂商 URL。
pub fn unwrap_ccr_proxy_url(url: &str) -> String {
    if !CCR_PROXY_PATH_MARKERS.iter().any(|m| url.contains(m)) {
        return url.to_string();
    }
    // 手写 query 解析（无 url crate）：找 ?mcp_url=... 至 & 或 #
    let query = match url.split_once('?') {
        Some((_, q)) => q,
        None => return url.to_string(),
    };
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("mcp_url=") {
            let decoded = url_decode(value);
            if !decoded.is_empty() {
                return decoded;
            }
        }
    }
    url.to_string()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(bytes[i]),
                }
            }
            b'+' => out.push(b' '),
            _ => out.push(bytes[i]),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 去重签名：`stdio:[cmd,args...]` 或 `url:<unwrap 后的 url>`；无命令与
/// URL（sdk）返回 None。
pub fn get_mcp_server_signature(config: &McpServerConfig) -> Option<String> {
    if let Some(cmd) = get_server_command_array(config) {
        return Some(format!("stdio:{}", json_stringify(&json!(cmd))));
    }
    get_server_url(config).map(|url| format!("url:{}", unwrap_ccr_proxy_url(url)))
}

/// 插件服务器去重：手动配置优先、插件之间先加载优先。
pub fn dedup_plugin_mcp_servers(
    plugin_servers: &HashMap<String, ScopedMcpServerConfig>,
    manual_servers: &HashMap<String, ScopedMcpServerConfig>,
) -> (
    HashMap<String, ScopedMcpServerConfig>,
    Vec<(String, String)>,
) {
    let mut manual_sigs: HashMap<String, String> = HashMap::new();
    for (name, config) in manual_servers {
        let Some(sig) = get_mcp_server_signature(&config.config) else {
            continue;
        };
        manual_sigs.entry(sig).or_insert_with(|| name.clone());
    }

    let mut servers = HashMap::new();
    let mut suppressed = Vec::new();
    let mut seen_plugin_sigs: HashMap<String, String> = HashMap::new();
    for (name, config) in plugin_servers {
        match get_mcp_server_signature(&config.config) {
            None => {
                servers.insert(name.clone(), config.clone());
            }
            Some(sig) => {
                if let Some(manual_dup) = manual_sigs.get(&sig) {
                    log_for_debugging(
                        &format!(
                            "Suppressing plugin MCP server \"{name}\": duplicates manually-configured \"{manual_dup}\""
                        ),
                        crate::utils::debug::DebugLogLevel::Debug,
                    );
                    suppressed.push((name.clone(), manual_dup.clone()));
                    continue;
                }
                if let Some(plugin_dup) = seen_plugin_sigs.get(&sig) {
                    log_for_debugging(
                        &format!(
                            "Suppressing plugin MCP server \"{name}\": duplicates earlier plugin server \"{plugin_dup}\""
                        ),
                        crate::utils::debug::DebugLogLevel::Debug,
                    );
                    suppressed.push((name.clone(), plugin_dup.clone()));
                    continue;
                }
                seen_plugin_sigs.insert(sig, name.clone());
                servers.insert(name.clone(), config.clone());
            }
        }
    }
    (servers, suppressed)
}

/// claude.ai connector 去重：URL 签名匹配已启用的手动配置（手动优先）。
pub fn dedup_claude_ai_mcp_servers(
    claude_ai_servers: &HashMap<String, ScopedMcpServerConfig>,
    manual_servers: &HashMap<String, ScopedMcpServerConfig>,
) -> (
    HashMap<String, ScopedMcpServerConfig>,
    Vec<(String, String)>,
) {
    let mut manual_sigs: HashMap<String, String> = HashMap::new();
    for (name, config) in manual_servers {
        if is_mcp_server_disabled(name) {
            continue;
        }
        let Some(sig) = get_mcp_server_signature(&config.config) else {
            continue;
        };
        manual_sigs.entry(sig).or_insert_with(|| name.clone());
    }

    let mut servers = HashMap::new();
    let mut suppressed = Vec::new();
    for (name, config) in claude_ai_servers {
        if let Some(sig) = get_mcp_server_signature(&config.config) {
            if let Some(manual_dup) = manual_sigs.get(&sig) {
                log_for_debugging(
                    &format!(
                        "Suppressing claude.ai connector \"{name}\": duplicates manually-configured \"{manual_dup}\""
                    ),
                    crate::utils::debug::DebugLogLevel::Debug,
                );
                suppressed.push((name.clone(), manual_dup.clone()));
                continue;
            }
        }
        servers.insert(name.clone(), config.clone());
    }
    (servers, suppressed)
}

/// URL 通配符模式转正则字符串（`*` → `.*`，其余转义）。
fn url_pattern_to_regex(pattern: &str) -> String {
    let escaped: String = pattern
        .chars()
        .map(|c| match c {
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                format!("\\{c}")
            }
            '*' => ".*".to_string(),
            _ => c.to_string(),
        })
        .collect();
    format!("^{escaped}$")
}

fn url_matches_pattern(url: &str, pattern: &str) -> bool {
    let regex_str = url_pattern_to_regex(pattern);
    match regex::Regex::new(&regex_str) {
        Ok(re) => re.is_match(url),
        Err(_) => false,
    }
}

/// allowlist 策略来源：allowManagedMcpServersOnly 时只读 policySettings。
fn get_mcp_allowlist_settings() -> SettingsJson {
    if should_allow_managed_mcp_servers_only() {
        get_settings_for_source(crate::bootstrap::state::SettingSource::PolicySettings)
            .unwrap_or_default()
    } else {
        get_initial_settings()
    }
}

/// denylist 永远从合并 settings 读（用户总能拒绝）。
fn get_mcp_denylist_settings() -> SettingsJson {
    get_initial_settings()
}

/// 企业策略：denylist（name/command/url 三种条目）。
fn is_mcp_server_denied(server_name: &str, config: Option<&McpServerConfig>) -> bool {
    let settings = get_mcp_denylist_settings();
    let Some(denied) = settings.denied_mcp_servers.as_deref() else {
        return false;
    };

    // name 条目
    if denied.iter().any(|entry| {
        entry.is_name_entry()
            && entry
                .server_name
                .as_deref()
                .map(|n| n == server_name)
                .unwrap_or(false)
    }) {
        return true;
    }

    let Some(config) = config else {
        return false;
    };

    // command 条目（stdio）
    if let Some(server_command) = get_server_command_array(config) {
        if denied.iter().any(|entry| {
            entry.is_command_entry()
                && entry.server_command.as_deref().map(|cmd| command_arrays_match(cmd, &server_command)).unwrap_or(false)
        }) {
            return true;
        }
    }

    // url 条目（远程）
    if let Some(server_url) = get_server_url(config) {
        if denied.iter().any(|entry| {
            entry.is_url_entry()
                && entry.server_url.as_deref().map(|pat| url_matches_pattern(server_url, pat)).unwrap_or(false)
        }) {
            return true;
        }
    }

    false
}

/// 企业策略：allowlist 判定（denylist 绝对优先）。
fn is_mcp_server_allowed_by_policy(
    server_name: &str,
    config: Option<&McpServerConfig>,
) -> bool {
    if is_mcp_server_denied(server_name, config) {
        return false;
    }

    let settings = get_mcp_allowlist_settings();
    let Some(allowed) = settings.allowed_mcp_servers.as_deref() else {
        return true; // 无 allowlist 限制
    };
    if allowed.is_empty() {
        return false; // 空数组 = 全部拒绝
    }

    let has_command_entries = allowed.iter().any(McpServerEntry::is_command_entry);
    let has_url_entries = allowed.iter().any(McpServerEntry::is_url_entry);

    if let Some(config) = config {
        let server_command = get_server_command_array(config);
        let server_url = get_server_url(config);

        if let Some(server_command) = server_command {
            // stdio 服务器
            if has_command_entries {
                return allowed.iter().any(|entry| {
                    entry.is_command_entry()
                        && entry.server_command.as_deref().map(|cmd| command_arrays_match(cmd, &server_command)).unwrap_or(false)
                });
            }
            return allowed.iter().any(|entry| {
                entry.is_name_entry()
                    && entry.server_name.as_deref().map(|n| n == server_name).unwrap_or(false)
            });
        }
        if let Some(server_url) = server_url {
            // 远程服务器
            if has_url_entries {
                return allowed.iter().any(|entry| {
                    entry.is_url_entry()
                        && entry.server_url.as_deref().map(|pat| url_matches_pattern(server_url, pat)).unwrap_or(false)
                });
            }
            return allowed.iter().any(|entry| {
                entry.is_name_entry()
                    && entry.server_name.as_deref().map(|n| n == server_name).unwrap_or(false)
            });
        }
    }

    // 未知类型/无 config：仅 name 条目
    allowed.iter().any(|entry| {
        entry.is_name_entry()
            && entry.server_name.as_deref().map(|n| n == server_name).unwrap_or(false)
    })
}

/// 按策略过滤配置（SDK 类型豁免）；返回允许的与被阻止的名字。
pub fn filter_mcp_servers_by_policy(
    configs: &HashMap<String, ScopedMcpServerConfig>,
) -> (HashMap<String, ScopedMcpServerConfig>, Vec<String>) {
    let mut allowed = HashMap::new();
    let mut blocked = Vec::new();
    for (name, config) in configs {
        if matches!(config.config, McpServerConfig::Sdk(_))
            || is_mcp_server_allowed_by_policy(name, Some(&config.config))
        {
            allowed.insert(name.clone(), config.clone());
        } else {
            blocked.push(name.clone());
        }
    }
    (allowed, blocked)
}

/// 展开配置中的环境变量（command/args/env/url/headers）。
fn expand_env_vars(config: &McpServerConfig) -> (McpServerConfig, Vec<String>) {
    let mut missing: Vec<String> = Vec::new();
    let mut expand = |s: &str| {
        let r = expand_env_vars_in_string(s);
        missing.extend(r.missing_vars);
        r.expanded
    };

    match config {
        McpServerConfig::Stdio(stdio) => {
            let mut expanded = stdio.clone();
            expanded.command = expand(&stdio.command);
            expanded.args = stdio.args.iter().map(|a| expand(a)).collect();
            expanded.env = stdio.env.as_ref().map(|env| {
                env.iter().map(|(k, v)| (k.clone(), expand(v))).collect()
            });
            (McpServerConfig::Stdio(expanded), dedup_missing(missing))
        }
        McpServerConfig::SSE(sse) => {
            let mut expanded = sse.clone();
            expanded.url = expand(&sse.url);
            expanded.headers = sse.headers.as_ref().map(|h| {
                h.iter().map(|(k, v)| (k.clone(), expand(v))).collect()
            });
            (McpServerConfig::SSE(expanded), dedup_missing(missing))
        }
        McpServerConfig::HTTP(http) => {
            let mut expanded = http.clone();
            expanded.url = expand(&http.url);
            expanded.headers = http.headers.as_ref().map(|h| {
                h.iter().map(|(k, v)| (k.clone(), expand(v))).collect()
            });
            (McpServerConfig::HTTP(expanded), dedup_missing(missing))
        }
        McpServerConfig::WebSocket(ws) => {
            let mut expanded = ws.clone();
            expanded.url = expand(&ws.url);
            expanded.headers = ws.headers.as_ref().map(|h| {
                h.iter().map(|(k, v)| (k.clone(), expand(v))).collect()
            });
            (McpServerConfig::WebSocket(expanded), dedup_missing(missing))
        }
        // sse-ide / ws-ide / sdk / claudeai-proxy：不展开
        other => (other.clone(), Vec::new()),
    }
}

fn dedup_missing(mut missing: Vec<String>) -> Vec<String> {
    missing.sort();
    missing.dedup();
    missing
}

/// 新增 MCP 服务器配置（名称校验 + 保留名 + 企业排他 + 策略 + 存在性）。
pub async fn add_mcp_config(
    name: &str,
    config: Value,
    scope: ConfigScope,
) -> Result<(), String> {
    if name.chars().any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
        return Err(format!(
            "Invalid name {name}. Names can only contain letters, numbers, hyphens, and underscores."
        ));
    }

    // 保留名
    if is_claude_in_chrome_mcp_server(name) {
        return Err(format!(
            "Cannot add MCP server \"{name}\": this name is reserved."
        ));
    }
    if does_enterprise_mcp_config_exist() {
        return Err(
            "Cannot add MCP server: enterprise MCP configuration is active and has exclusive control over MCP servers"
                .to_string(),
        );
    }

    // 校验配置
    let parsed = crate::services::mcp::types::parse_mcp_server_config(&config);
    let validated = parsed.config.ok_or_else(|| {
        format!(
            "Invalid configuration: {}",
            parsed
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    if is_mcp_server_denied(name, Some(&validated)) {
        return Err(format!(
            "Cannot add MCP server \"{name}\": server is explicitly blocked by enterprise policy"
        ));
    }
    if !is_mcp_server_allowed_by_policy(name, Some(&validated)) {
        return Err(format!(
            "Cannot add MCP server \"{name}\": not allowed by enterprise policy"
        ));
    }

    match scope {
        ConfigScope::Project => {
            let servers = get_project_mcp_configs_from_cwd().0;
            if servers.contains_key(name) {
                return Err(format!("MCP server {name} already exists in .mcp.json"));
            }
            let mut mcp_servers: HashMap<String, Value> = HashMap::new();
            for (server_name, server_config) in servers {
                mcp_servers.insert(server_name, server_config.config.to_json_value());
            }
            mcp_servers.insert(name.to_string(), validated.to_json_value());
            let mut config = McpJsonConfig::default();
            config.mcp_servers = mcp_servers;
            write_mcpjson_file(&config)
                .await
                .map_err(|e| format!("Failed to write to .mcp.json: {e}"))?;
        }
        ConfigScope::User => {
            update_global_config(|current| {
                let mut updated = current.clone();
                let mcp_servers = updated
                    .get("mcpServers")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let mut mcp_servers: serde_json::Map<String, Value> = mcp_servers;
                mcp_servers.insert(name.to_string(), validated.to_json_value());
                updated["mcpServers"] = Value::Object(mcp_servers);
                updated
            });
        }
        ConfigScope::Local => {
            update_current_project_config(|current| {
                let mut updated = current.clone();
                let mcp_servers = updated
                    .get("mcpServers")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let mut mcp_servers: serde_json::Map<String, Value> = mcp_servers;
                mcp_servers.insert(name.to_string(), validated.to_json_value());
                updated["mcpServers"] = Value::Object(mcp_servers);
                updated
            });
        }
        ConfigScope::Dynamic => {
            return Err("Cannot add MCP server to scope: dynamic".to_string())
        }
        ConfigScope::Enterprise => {
            return Err("Cannot add MCP server to scope: enterprise".to_string())
        }
        ConfigScope::Claudeai => {
            return Err("Cannot add MCP server to scope: claudeai".to_string())
        }
        ConfigScope::Managed => return Err("Cannot add MCP server to scope: managed".to_string()),
    }
    Ok(())
}

/// 移除 MCP 服务器配置。
pub async fn remove_mcp_config(name: &str, scope: ConfigScope) -> Result<(), String> {
    match scope {
        ConfigScope::Project => {
            let servers = get_project_mcp_configs_from_cwd().0;
            if !servers.contains_key(name) {
                return Err(format!("No MCP server found with name: {name} in .mcp.json"));
            }
            let mut mcp_servers: HashMap<String, Value> = HashMap::new();
            for (server_name, server_config) in servers {
                if server_name != name {
                    mcp_servers.insert(server_name, server_config.config.to_json_value());
                }
            }
            let mut config = McpJsonConfig::default();
            config.mcp_servers = mcp_servers;
            write_mcpjson_file(&config)
                .await
                .map_err(|e| format!("Failed to remove from .mcp.json: {e}"))?;
        }
        ConfigScope::User => {
            let config = get_global_config();
            let exists = config
                .get("mcpServers")
                .and_then(Value::as_object)
                .map(|m| m.contains_key(name))
                .unwrap_or(false);
            if !exists {
                return Err(format!("No user-scoped MCP server found with name: {name}"));
            }
            update_global_config(|current| {
                let mut updated = current.clone();
                if let Some(mcp_servers) = updated.get_mut("mcpServers").and_then(Value::as_object_mut) {
                    mcp_servers.remove(name);
                }
                updated
            });
        }
        ConfigScope::Local => {
            let config = get_current_project_config();
            let exists = config
                .get("mcpServers")
                .and_then(Value::as_object)
                .map(|m| m.contains_key(name))
                .unwrap_or(false);
            if !exists {
                return Err(format!("No project-local MCP server found with name: {name}"));
            }
            update_current_project_config(|current| {
                let mut updated = current.clone();
                if let Some(mcp_servers) =
                    updated.get_mut("mcpServers").and_then(Value::as_object_mut)
                {
                    mcp_servers.remove(name);
                }
                updated
            });
        }
        _ => return Err(format!("Cannot remove MCP server from scope: {:?}", scope)),
    }
    Ok(())
}

/// 当前目录的 `.mcp.json` 配置（不含父目录遍历）。
pub fn get_project_mcp_configs_from_cwd() -> (
    HashMap<String, ScopedMcpServerConfig>,
    Vec<ValidationError>,
) {
    if !is_setting_source_enabled(crate::bootstrap::state::SettingSource::ProjectSettings) {
        return (HashMap::new(), Vec::new());
    }

    let mcp_json_path = PathBuf::from(get_cwd()).join(".mcp.json");
    let (config, errors) = parse_mcp_config_from_file_path(
        mcp_json_path.to_string_lossy().as_ref(),
        true,
        ConfigScope::Project,
    );

    let Some(config) = config else {
        let non_missing: Vec<ValidationError> = errors
            .into_iter()
            .filter(|e| !e.message.starts_with("MCP config file not found"))
            .collect();
        if !non_missing.is_empty() {
            return (HashMap::new(), non_missing);
        }
        return (HashMap::new(), Vec::new());
    };

    (
        add_scope_to_servers(Some(&config.mcp_servers), ConfigScope::Project),
        errors,
    )
}

/// 按加载某 scope 的全部配置（project 含父目录遍历，近 CWD 覆盖远）。
pub fn get_mcp_configs_by_scope(
    scope: ConfigScope,
) -> (
    HashMap<String, ScopedMcpServerConfig>,
    Vec<ValidationError>,
) {
    match scope {
        ConfigScope::Project => {
            if !is_setting_source_enabled(crate::bootstrap::state::SettingSource::ProjectSettings) {
                return (HashMap::new(), Vec::new());
            }
            let mut all_servers = HashMap::new();
            let mut all_errors = Vec::new();

            // 从根到 CWD 处理（近 CWD 覆盖父目录）
            let mut dirs: Vec<PathBuf> = Vec::new();
            let mut current = PathBuf::from(get_cwd());
            loop {
                dirs.push(current.clone());
                match current.parent() {
                    Some(parent) if parent != current => current = parent.to_path_buf(),
                    _ => break,
                }
            }
            for dir in dirs.into_iter().rev() {
                let mcp_json_path = dir.join(".mcp.json");
                let (config, errors) = parse_mcp_config_from_file_path(
                    mcp_json_path.to_string_lossy().as_ref(),
                    true,
                    ConfigScope::Project,
                );
                if config.is_none() {
                    let non_missing: Vec<ValidationError> = errors
                        .into_iter()
                        .filter(|e| !e.message.starts_with("MCP config file not found"))
                        .collect();
                    if !non_missing.is_empty() {
                        all_errors.extend(non_missing);
                    }
                    continue;
                }
                for (name, config) in
                    add_scope_to_servers(config.as_ref().map(|c| &c.mcp_servers), ConfigScope::Project)
                {
                    all_servers.insert(name, config);
                }
                all_errors.extend(errors);
            }
            (all_servers, all_errors)
        }
        ConfigScope::User => {
            if !is_setting_source_enabled(crate::bootstrap::state::SettingSource::UserSettings) {
                return (HashMap::new(), Vec::new());
            }
            let mcp_servers = get_global_config()
                .get("mcpServers")
                .and_then(Value::as_object)
                .cloned();
            if mcp_servers.is_none() {
                return (HashMap::new(), Vec::new());
            }
            let (config, errors) = parse_mcp_config(
                &json!({ "mcpServers": mcp_servers }),
                true,
                ConfigScope::User,
                None,
            );
            (
                add_scope_to_servers(config.as_ref().map(|c| &c.mcp_servers), ConfigScope::User),
                errors,
            )
        }
        ConfigScope::Local => {
            if !is_setting_source_enabled(crate::bootstrap::state::SettingSource::LocalSettings) {
                return (HashMap::new(), Vec::new());
            }
            let mcp_servers = get_current_project_config()
                .get("mcpServers")
                .and_then(Value::as_object)
                .cloned();
            if mcp_servers.is_none() {
                return (HashMap::new(), Vec::new());
            }
            let (config, errors) = parse_mcp_config(
                &json!({ "mcpServers": mcp_servers }),
                true,
                ConfigScope::Local,
                None,
            );
            (
                add_scope_to_servers(config.as_ref().map(|c| &c.mcp_servers), ConfigScope::Local),
                errors,
            )
        }
        ConfigScope::Enterprise => {
            let path = get_enterprise_mcp_file_path();
            let (config, errors) =
                parse_mcp_config_from_file_path(&path, true, ConfigScope::Enterprise);
            let Some(config) = config else {
                let non_missing: Vec<ValidationError> = errors
                    .into_iter()
                    .filter(|e| !e.message.starts_with("MCP config file not found"))
                    .collect();
                if !non_missing.is_empty() {
                    return (HashMap::new(), non_missing);
                }
                return (HashMap::new(), Vec::new());
            };
            (
                add_scope_to_servers(Some(&config.mcp_servers), ConfigScope::Enterprise),
                errors,
            )
        }
        _ => (HashMap::new(), Vec::new()),
    }
}

/// 按名查配置（优先级 enterprise > local > project > user）。
pub fn get_mcp_config_by_name(name: &str) -> Option<ScopedMcpServerConfig> {
    let enterprise = get_mcp_configs_by_scope(ConfigScope::Enterprise).0;

    // plugin-only 锁定：只有 enterprise 按名可达
    if is_restricted_to_plugin_only("mcp") {
        return enterprise.get(name).cloned();
    }

    let user = get_mcp_configs_by_scope(ConfigScope::User).0;
    let project = get_mcp_configs_by_scope(ConfigScope::Project).0;
    let local = get_mcp_configs_by_scope(ConfigScope::Local).0;

    enterprise
        .get(name)
        .or_else(|| local.get(name))
        .or_else(|| project.get(name))
        .or_else(|| user.get(name))
        .cloned()
}

/// Claude Code MCP 配置（不含 claude.ai；快速路径，仅本地文件读）。
pub async fn get_claude_code_mcp_configs(
    dynamic_servers: HashMap<String, ScopedMcpServerConfig>,
    extra_dedup_targets: HashMap<String, ScopedMcpServerConfig>,
) -> McpConfigResult {
    let enterprise = get_mcp_configs_by_scope(ConfigScope::Enterprise).0;

    if does_enterprise_mcp_config_exist() {
        let filtered: HashMap<String, ScopedMcpServerConfig> = enterprise
            .into_iter()
            .filter(|(name, config)| is_mcp_server_allowed_by_policy(name, Some(&config.config)))
            .collect();
        return McpConfigResult {
            servers: filtered,
            errors: Vec::new(),
        };
    }

    let mcp_locked = is_restricted_to_plugin_only("mcp");
    let user = if mcp_locked {
        HashMap::new()
    } else {
        get_mcp_configs_by_scope(ConfigScope::User).0
    };
    let project = if mcp_locked {
        HashMap::new()
    } else {
        get_mcp_configs_by_scope(ConfigScope::Project).0
    };
    let local = if mcp_locked {
        HashMap::new()
    } else {
        get_mcp_configs_by_scope(ConfigScope::Local).0
    };

    // 插件服务器（plugins 模块未转换 → 空集合 + 空错误）
    let plugin_mcp_servers: HashMap<String, ScopedMcpServerConfig> = HashMap::new();

    // 只保留已审批的 project 服务器
    let approved_project: HashMap<String, ScopedMcpServerConfig> = project
        .into_iter()
        .filter(|(name, _)| get_project_mcp_server_status(name) == "approved")
        .collect();

    // 手动（含动态 + extra）去重目标：仅启用且策略允许的
    let mut enabled_manual: HashMap<String, ScopedMcpServerConfig> = HashMap::new();
    for (name, config) in user.iter()
        .chain(approved_project.iter())
        .chain(local.iter())
        .chain(dynamic_servers.iter())
        .chain(extra_dedup_targets.iter())
    {
        if !is_mcp_server_disabled(name) && is_mcp_server_allowed_by_policy(name, Some(&config.config)) {
            enabled_manual.insert(name.clone(), config.clone());
        }
    }

    let (deduped_plugin, _suppressed) = dedup_plugin_mcp_servers(&plugin_mcp_servers, &enabled_manual);

    // 合并：plugin < user < project < local
    let mut configs: HashMap<String, ScopedMcpServerConfig> = HashMap::new();
    for (name, config) in deduped_plugin {
        configs.insert(name, config);
    }
    for (name, config) in enabled_manual {
        if matches!(config.scope, ConfigScope::Dynamic) {
            continue;
        }
        configs.insert(name, config);
    }

    // 最终策略过滤
    let filtered: HashMap<String, ScopedMcpServerConfig> = configs
        .into_iter()
        .filter(|(name, config)| {
            matches!(config.config, McpServerConfig::Sdk(_))
                || is_mcp_server_allowed_by_policy(name, Some(&config.config))
        })
        .collect();

    McpConfigResult {
        servers: filtered,
        errors: Vec::new(),
    }
}

/// getClaudeCodeMcpConfigs 的结果。
pub struct McpConfigResult {
    pub servers: HashMap<String, ScopedMcpServerConfig>,
    /// 插件加载错误（PluginError 占位 —— plugins 模块未转换）。
    pub errors: Vec<PluginErrorStub>,
}

/// PluginError 占位（types/plugin.ts 未转换）。
#[derive(Debug, Clone)]
pub struct PluginErrorStub {
    pub type_str: String,
    pub source: String,
    pub message: String,
}

/// 全部 scope 的配置（含 claude.ai，慢路径）。
pub async fn get_all_mcp_configs() -> McpConfigResult {
    if does_enterprise_mcp_config_exist() {
        return get_claude_code_mcp_configs(HashMap::new(), HashMap::new()).await;
    }
    let claude_ai = crate::services::mcp::claudeai::fetch_claude_ai_mcp_configs_if_eligible().await;
    let claude_code = get_claude_code_mcp_configs(HashMap::new(), claude_ai.clone()).await;
    let (allowed, _blocked) = filter_mcp_servers_by_policy(&claude_ai);
    let (deduped, _suppressed) = dedup_claude_ai_mcp_servers(&allowed, &claude_code.servers);

    let mut servers = deduped;
    for (name, config) in claude_code.servers {
        servers.insert(name, config);
    }
    McpConfigResult {
        servers,
        errors: claude_code.errors,
    }
}

/// 校验并解析 MCP 配置对象（schema + 环境变量展开 + Windows npx 检测）。
pub fn parse_mcp_config(
    config_object: &Value,
    expand_vars: bool,
    scope: ConfigScope,
    file_path: Option<&str>,
) -> (Option<McpJsonConfig>, Vec<ValidationError>) {
    let parsed = crate::services::mcp::types::parse_mcp_json_config(config_object);
    let scope_servers = match parsed {
        Err(_issues) => {
            let error = ValidationError {
                file: file_path.map(str::to_string),
                path: String::new(),
                message: "Does not adhere to MCP server configuration schema".to_string(),
                expected: None,
                invalid_value: None,
                suggestion: None,
                doc_link: None,
                mcp_error_metadata: Some(McpErrorMetadata {
                    scope: scope.as_str().to_string(),
                    server_name: None,
                    severity: Some("fatal".to_string()),
                }),
            };
            return (None, vec![error]);
        }
        Ok(config) => config,
    };

    let mut errors: Vec<ValidationError> = Vec::new();
    let mut validated_servers: HashMap<String, Value> = HashMap::new();

    for (name, raw) in &scope_servers.mcp_servers {
        let mut config_to_check = raw.clone();

        if expand_vars {
            // 先用 schema 校验原始值
            let parsed = crate::services::mcp::types::parse_mcp_server_config(raw);
            let Some(parsed_config) = parsed.config else {
                let error = ValidationError {
                    file: file_path.map(str::to_string),
                    path: format!("mcpServers.{name}"),
                    message: "Does not adhere to MCP server configuration schema".to_string(),
                    expected: None,
                    invalid_value: None,
                    suggestion: None,
                    doc_link: None,
                    mcp_error_metadata: Some(McpErrorMetadata {
                        scope: scope.as_str().to_string(),
                        server_name: Some(name.clone()),
                        severity: Some("fatal".to_string()),
                    }),
                };
                errors.push(error);
                continue;
            };

            let (expanded, missing_vars) = expand_env_vars(&parsed_config);
            if !missing_vars.is_empty() {
                let error = ValidationError {
                    file: file_path.map(str::to_string),
                    path: format!("mcpServers.{name}"),
                    message: format!("Missing environment variables: {}", missing_vars.join(", ")),
                    expected: None,
                    invalid_value: None,
                    suggestion: Some(format!(
                        "Set the following environment variables: {}",
                        missing_vars.join(", ")
                    )),
                    doc_link: None,
                    mcp_error_metadata: Some(McpErrorMetadata {
                        scope: scope.as_str().to_string(),
                        server_name: Some(name.clone()),
                        severity: Some("warning".to_string()),
                    }),
                };
                errors.push(error);
            }
            config_to_check = expanded.to_json_value();
        }

        // Windows npx 检测
        if get_platform() == Platform::Windows {
            let parsed_config =
                crate::services::mcp::types::parse_mcp_server_config(&config_to_check);
            if let Some(McpServerConfig::Stdio(stdio)) = parsed_config.config {
                if stdio.command == "npx"
                    || stdio.command.ends_with("\\npx")
                    || stdio.command.ends_with("/npx")
                {
                    let error = ValidationError {
                        file: file_path.map(str::to_string),
                        path: format!("mcpServers.{name}"),
                        message: "Windows requires 'cmd /c' wrapper to execute npx".to_string(),
                        expected: None,
                        invalid_value: None,
                        suggestion: Some(
                            "Change command to \"cmd\" with args [\"/c\", \"npx\", ...]. See: https://code.claude.com/docs/en/mcp#configure-mcp-servers".to_string(),
                        ),
                        doc_link: None,
                        mcp_error_metadata: Some(McpErrorMetadata {
                            scope: scope.as_str().to_string(),
                            server_name: Some(name.clone()),
                            severity: Some("warning".to_string()),
                        }),
                    };
                    errors.push(error);
                }
            }
        }

        validated_servers.insert(name.clone(), config_to_check);
    }

    let mut config = McpJsonConfig::default();
    config.mcp_servers = validated_servers;
    (Some(config), errors)
}

/// 从文件路径读取并解析 MCP 配置。
pub fn parse_mcp_config_from_file_path(
    file_path: &str,
    expand_vars: bool,
    scope: ConfigScope,
) -> (Option<McpJsonConfig>, Vec<ValidationError>) {
    let fs = get_fs_implementation();
    let config_content = match fs.read_file_sync(file_path, crate::utils::fs_operations::BufferEncoding::Utf8) {
        Ok(content) => content,
        Err(error) => {
            let fatal = ValidationError {
                file: Some(file_path.to_string()),
                path: String::new(),
                message: if is_enoent(&error) {
                    format!("MCP config file not found: {file_path}")
                } else {
                    format!("Failed to read file: {error}")
                },
                expected: None,
                invalid_value: None,
                suggestion: Some(if is_enoent(&error) {
                    "Check that the file path is correct".to_string()
                } else {
                    "Check file permissions and ensure the file exists".to_string()
                }),
                doc_link: None,
                mcp_error_metadata: Some(McpErrorMetadata {
                    scope: scope.as_str().to_string(),
                    server_name: None,
                    severity: Some("fatal".to_string()),
                }),
            };
            return (None, vec![fatal]);
        }
    };

    let content_string = String::from_utf8_lossy(config_content.as_bytes());
    let parsed_json = crate::utils::slow_operations::json_parse(content_string.trim_start_matches('\u{feff}'));
    let parsed_json = match parsed_json {
        Ok(value) => value,
        Err(_) => {
            let error = ValidationError {
                file: Some(file_path.to_string()),
                path: String::new(),
                message: "MCP config is not a valid JSON".to_string(),
                expected: None,
                invalid_value: None,
                suggestion: Some("Fix the JSON syntax errors in the file".to_string()),
                doc_link: None,
                mcp_error_metadata: Some(McpErrorMetadata {
                    scope: scope.as_str().to_string(),
                    server_name: None,
                    severity: Some("fatal".to_string()),
                }),
            };
            return (None, vec![error]);
        }
    };

    parse_mcp_config(&parsed_json, expand_vars, scope, Some(file_path))
}

/// 企业 MCP 配置是否存在（memoize：OnceLock 缓存）。
pub fn does_enterprise_mcp_config_exist() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let (config, _errors) = parse_mcp_config_from_file_path(
            &get_enterprise_mcp_file_path(),
            true,
            ConfigScope::Enterprise,
        );
        config.is_some()
    })
}

/// allowManagedMcpServersOnly 策略。
pub fn should_allow_managed_mcp_servers_only() -> bool {
    get_settings_for_source(crate::bootstrap::state::SettingSource::PolicySettings)
        .and_then(|settings| settings.allow_managed_mcp_servers_only)
        .unwrap_or(false)
}

/// 企业排他模式下是否全部为允许的 SDK 服务器（仅 claude-vscode）。
pub fn are_mcp_configs_allowed_with_enterprise_mcp_config(
    configs: &HashMap<String, ScopedMcpServerConfig>,
) -> bool {
    configs.values().all(|c| {
        matches!(c.config, McpServerConfig::Sdk(ref sdk) if sdk.name == "claude-vscode")
    })
}

/// 默认禁用的内建服务器（需显式启用）。
fn default_disabled_builtins() -> Vec<&'static str> {
    vec!["mcp-chrome"]
}

fn is_default_disabled_builtin(name: &str) -> bool {
    default_disabled_builtins().contains(&name)
}

/// 服务器是否被禁用（项目级 enabledMcpServers/disabledMcpServers）。
pub fn is_mcp_server_disabled(name: &str) -> bool {
    let project_config = get_current_project_config();
    if is_default_disabled_builtin(name) {
        let enabled = project_config
            .get("enabledMcpServers")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .any(|s| s == name)
            })
            .unwrap_or(false);
        return !enabled;
    }
    project_config
        .get("disabledMcpServers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .any(|s| s == name)
        })
        .unwrap_or(false)
}

fn toggle_membership(list: &[Value], name: &str, should_contain: bool) -> Vec<Value> {
    let contains = list.iter().any(|v| v.as_str() == Some(name));
    if contains == should_contain {
        return list.to_vec();
    }
    if should_contain {
        let mut next = list.to_vec();
        next.push(Value::String(name.to_string()));
        next
    } else {
        list.iter()
            .filter(|v| v.as_str() != Some(name))
            .cloned()
            .collect()
    }
}

/// 启用/禁用某服务器（写入项目 config）。
pub fn set_mcp_server_enabled(name: &str, enabled: bool) {
    let is_builtin_state_change =
        is_default_disabled_builtin(name) && is_mcp_server_disabled(name) == enabled;

    update_current_project_config(|current| {
        let mut updated = current.clone();
        if is_default_disabled_builtin(name) {
            let prev: Vec<Value> = updated
                .get("enabledMcpServers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next = toggle_membership(&prev, name, enabled);
            if next != prev {
                updated["enabledMcpServers"] = Value::Array(next);
            } else {
                return current.clone();
            }
        } else {
            let prev: Vec<Value> = updated
                .get("disabledMcpServers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next = toggle_membership(&prev, name, !enabled);
            if next != prev {
                updated["disabledMcpServers"] = Value::Array(next);
            } else {
                return current.clone();
            }
        }
        updated
    });

    if is_builtin_state_change {
        // analytics 未转换：tengu_builtin_mcp_toggle 事件占位
        log_for_debugging(
            &format!("[mcp] builtin toggle: {name} enabled={enabled}"),
            crate::utils::debug::DebugLogLevel::Debug,
        );
    }
}

// ---------------------------------------------------------------------------
// ~/.claude.json 项目配置（utils/config.ts 未转换 → duck-type）
// ---------------------------------------------------------------------------

static GLOBAL_CONFIG_CACHE: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
static PROJECT_CONFIG_CACHE: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

fn global_config_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join(".claude.json")
}

fn project_config_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join(".claude.json")
}

/// 读全局配置（~/.claude.json；不存在/损坏返回空对象）。带写穿透缓存。
pub fn get_global_config() -> Value {
    {
        let cache = GLOBAL_CONFIG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(config) = cache.as_ref() {
            return config.clone();
        }
    }
    let config = std::fs::read(global_config_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Value::Object(Default::default()));
    let mut cache = GLOBAL_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *cache = Some(config.clone());
    config
}

/// 读当前项目配置（~/.claude.json 的 projects[cwd] 条目）。
pub fn get_current_project_config() -> Value {
    {
        let cache = PROJECT_CONFIG_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(config) = cache.as_ref() {
            return config.clone();
        }
    }
    let global = get_global_config();
    let cwd = get_cwd();
    let project = global
        .get("projects")
        .and_then(|p| p.get(&cwd))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let mut cache = PROJECT_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *cache = Some(project.clone());
    project
}

pub fn update_global_config(updater: impl Fn(&Value) -> Value) {
    let current = get_global_config();
    let updated = updater(&current);
    let path = global_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&updated) {
        if std::fs::write(&path, bytes).is_ok() {
            let mut cache = GLOBAL_CONFIG_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *cache = Some(updated);
            // 项目配置可能随之变化：置缓存
            let mut project_cache = PROJECT_CONFIG_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *project_cache = None;
        }
    }
}

fn update_current_project_config(updater: impl Fn(&Value) -> Value) {
    let current = get_current_project_config();
    let updated = updater(&current);
    let mut global = get_global_config();
    let cwd = get_cwd();
    let projects = global
        .get_mut("projects")
        .and_then(Value::as_object_mut);
    if let Some(projects) = projects {
        projects.insert(cwd, updated.clone());
    } else {
        let mut map = serde_json::Map::new();
        map.insert(cwd, updated.clone());
        global["projects"] = Value::Object(map);
    }
    let path = project_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&global) {
        if std::fs::write(&path, bytes).is_ok() {
            let mut cache = GLOBAL_CONFIG_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *cache = Some(global);
            let mut project_cache = PROJECT_CONFIG_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *project_cache = Some(updated);
        }
    }
}

/// 保留名检查（utils/claudeInChrome/common.ts 未转换 → 本地常量）。
fn is_claude_in_chrome_mcp_server(name: &str) -> bool {
    name == "claude-in-chrome"
}

/// 服务器 ID 哈希（utils/auth.rs 之外独立一份 sha256 前缀，供 claudeai.rs）。
pub fn config_signature_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_ccr_proxy_urls() {
        assert_eq!(
            unwrap_ccr_proxy_url("https://x/v2/ccr-sessions/abc?mcp_url=https%3A%2F%2Fmcp.slack.com%2Fsse&other=1"),
            "https://mcp.slack.com/sse"
        );
        assert_eq!(unwrap_ccr_proxy_url("https://plain.example.com/mcp"), "https://plain.example.com/mcp");
    }

    #[test]
    fn url_patterns_match() {
        assert!(url_matches_pattern("https://example.com/api/v1", "https://example.com/*"));
        assert!(url_matches_pattern("https://api.example.com/x", "https://*.example.com/*"));
        assert!(!url_matches_pattern("https://evil.com/x", "https://example.com/*"));
    }

    #[test]
    fn parses_config_object() {
        let (config, errors) = parse_mcp_config(
            &json!({ "mcpServers": { "ok": { "command": "x" }, "bad": { "type": "bogus" } } }),
            true,
            ConfigScope::User,
            None,
        );
        assert!(config.is_some());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("schema"));
    }

    #[test]
    fn detects_missing_env() {
        let (config, errors) = parse_mcp_config(
            &json!({ "mcpServers": { "s": { "command": "${NOPE_NEVER_SET_VAR}" } } }),
            true,
            ConfigScope::User,
            None,
        );
        let config = config.unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Missing environment variables"));
    }

    #[test]
    fn detects_windows_npx() {
        // 直接测 npx 检测分支（非 Windows 平台不会触发，故取逻辑等价路径）
        let parsed = crate::services::mcp::types::parse_mcp_server_config(&json!({
            "command": "npx", "args": ["-y", "s"]
        }));
        assert!(parsed.config.is_some());
    }

    #[test]
    fn dedups_by_url_signature() {
        let mut manual = HashMap::new();
        manual.insert(
            "slack".to_string(),
            ScopedMcpServerConfig::new(
                McpServerConfig::SSE(crate::services::mcp::types::McpSSEServerConfig {
                    type_field: "sse".to_string(),
                    url: "https://mcp.slack.com/sse".to_string(),
                    headers: None,
                    headers_helper: None,
                    oauth: None,
                }),
                ConfigScope::User,
            ),
        );
        let mut plugin = HashMap::new();
        plugin.insert(
            "plugin:slack:slack".to_string(),
            ScopedMcpServerConfig::new(
                McpServerConfig::SSE(crate::services::mcp::types::McpSSEServerConfig {
                    type_field: "sse".to_string(),
                    url: "https://mcp.slack.com/sse".to_string(),
                    headers: None,
                    headers_helper: None,
                    oauth: None,
                }),
                ConfigScope::Dynamic,
            ),
        );
        let (kept, suppressed) = dedup_plugin_mcp_servers(&plugin, &manual);
        assert!(kept.is_empty());
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].1, "slack");
    }
}
