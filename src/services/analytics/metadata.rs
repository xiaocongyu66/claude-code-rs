//! metadata.rs — 共享事件元数据富化（重写自 `src/services/analytics/metadata.ts`）。
//!
//! 职责（对齐 TS）：
//! - 工具名/扩展名/工具输入的脱敏与截断（`sanitizeToolNameForAnalytics`、
//!   `extractMcpToolDetails`、`extractSkillName`、`truncateToolInputValue`、
//!   `extractToolInputForTelemetry`、`getFileExtensionForAnalytics`、
//!   `getFileExtensionsFromBashCommand`）。
//! - 核心事件元数据结构（`EnvContext` / `ProcessMetrics` / `EventMetadata`）。
//! - 1P snake_case 格式转换（`to1PEventFormat`）。
//! - `getEventMetadata()` 富化入口。
//!
//! 已知偏差（依赖未转换模块）：
//! - `utils/env.ts`（runtimes/packageManagers/conductor 检测）、
//!   `utils/git.ts`（repo remote hash）、`utils/teammate.ts` 与
//!   `utils/agentContext.ts`（swarm/agent 标识）、
//!   `services/mcp/officialRegistry.ts`（`isOfficialMcpUrl`）、
//!   `utils/betas.ts`（`getModelBetas`）均未转换：对应字段在 `EnvContext`
//!   /`EventMetadata` 中要么留空（字符串字段）要么缺省（Option 字段）；
//!   `isAnalyticsToolDetailsLoggingEnabled` 的 URL 分支按保守值 false
//!   （自定义 MCP 保持脱敏）。结构完整，运行期数据回归待依赖落地。
//! - proto 生成的 `EnvironmentMetadata` / `PublicApiAuth` /
//!   `ClaudeCodeInternalEvent` 未转换：Rust 侧以本模块的结构体表达同一
//!   schema（snake_case 字段名对齐 proto.toJSON 输出），新增字段需同步
//!   proto 定义与这里的结构体。
//! - 统计字段（ProcessMetrics 的 rss/heap 等）读取 Node 运行时指标；
//!   Rust 侧以 `buildProcessMetrics()` 的最小实现填充（uptime/cpu%），
//!   其余字段为 0（HTTP 上报本身未接入，见 sink.rs 偏差）。


use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::bootstrap::state::{get_client_type, get_is_interactive, get_kairos_active, get_session_id};
use crate::utils::auth::{get_subscription_type, is_claude_ai_subscriber};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::model::model::get_main_loop_model;
use crate::utils::platform::{detect_vcs, get_linux_distro_info, get_platform, get_wsl_version, Platform};
use crate::utils::slow_operations::json_stringify;

// ---------------------------------------------------------------------------
// 标记类型（TS 的 `AnalyticsMetadata_I_VERIFIED_*`，类型为 never 的意图
// 标记；Rust 无对应物，以注释保留语义）
// ---------------------------------------------------------------------------

// `AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS`：调用方声明
// 该字符串已人工核对不含代码片段/文件路径。
// `AnalyticsMetadata_I_VERIFIED_THIS_IS_PII_TAGGED`：调用方声明该值走
// `_PROTO_*` 键进入 BQ 特权列，非特权 sink 必须先行剥离（见 index.rs
// `stripProtoFields`）。

// ---------------------------------------------------------------------------
// 工具名脱敏
// ---------------------------------------------------------------------------

/// MCP 工具名脱敏：`mcp__*` → `mcp_tool`（PII-medium），其余原样返回。
pub fn sanitize_tool_name_for_analytics(tool_name: &str) -> String {
    if tool_name.starts_with("mcp__") {
        "mcp_tool".to_string()
    } else {
        tool_name.to_string()
    }
}

/// OTEL 工具详情记录是否开启（`OTEL_LOG_TOOL_DETAILS=1`）。
pub fn is_tool_details_logging_enabled() -> bool {
    is_env_truthy(std::env::var("OTEL_LOG_TOOL_DETAILS").ok().as_deref())
}

/// analytics 事件中的 MCP server/tool 名是否可记录。
///
/// 可记录：Cowork（entrypoint=local-agent）、claude.ai 代理连接器、
/// 命中官方 MCP registry 的 URL。自定义/用户配置的 MCP 保持脱敏。
/// **偏差**：`isOfficialMcpUrl`（services/mcp/officialRegistry.ts）未转换，
/// URL 分支保守返回 false。
pub fn is_analytics_tool_details_logging_enabled(
    mcp_server_type: Option<&str>,
    mcp_server_base_url: Option<&str>,
) -> bool {
    if std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref() == Ok("local-agent") {
        return true;
    }
    if mcp_server_type == Some("claudeai-proxy") {
        return true;
    }
    // isOfficialMcpUrl 未转换：保守不记录（对齐 TS 的 false 分支语义）
    let _ = mcp_server_base_url;
    false
}

/// 内置 first-party MCP server 名集合（computerUse MCP；feature
/// `CHICAGO_MCP` 关闭时为空集）。
fn get_builtin_mcp_server_names() -> Vec<&'static str> {
    if is_env_truthy(std::env::var("FEATURE_CHICAGO_MCP").ok().as_deref()) {
        vec!["computer-use"]
    } else {
        Vec::new()
    }
}

/// MCP 工具名 → server/tool 明细，若门控通过则返回可记录的字段对。
pub fn mcp_tool_details_for_analytics(
    tool_name: &str,
    mcp_server_type: Option<&str>,
    mcp_server_base_url: Option<&str>,
) -> Option<(String, String)> {
    let details = extract_mcp_tool_details(tool_name)?;
    if !get_builtin_mcp_server_names().contains(&details.0.as_str())
        && !is_analytics_tool_details_logging_enabled(mcp_server_type, mcp_server_base_url)
    {
        return None;
    }
    Some(details)
}

/// 从完整 MCP 工具名（`mcp__<server>__<tool>`）解析 server 与 tool 名。
/// tool 名本身可含 `__`，故剩余部分重新连接。
pub fn extract_mcp_tool_details(tool_name: &str) -> Option<(String, String)> {
    if !tool_name.starts_with("mcp__") {
        return None;
    }
    let parts: Vec<&str> = tool_name.split("__").collect();
    if parts.len() < 3 {
        return None;
    }
    let server_name = parts[1];
    let mcp_tool_name = parts[2..].join("__");
    if server_name.is_empty() || mcp_tool_name.is_empty() {
        return None;
    }
    Some((server_name.to_string(), mcp_tool_name))
}

/// 从 Skill 工具输入中提取 skill 名（仅 toolName == "Skill" 时）。
pub fn extract_skill_name(tool_name: &str, input: &Value) -> Option<String> {
    if tool_name != "Skill" {
        return None;
    }
    input
        .get("skill")
        .and_then(|skill| skill.as_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// 工具输入截断
// ---------------------------------------------------------------------------

const TOOL_INPUT_STRING_TRUNCATE_AT: usize = 512;
const TOOL_INPUT_STRING_TRUNCATE_TO: usize = 128;
const TOOL_INPUT_MAX_JSON_CHARS: usize = 4 * 1024;
const TOOL_INPUT_MAX_COLLECTION_ITEMS: usize = 20;
const TOOL_INPUT_MAX_DEPTH: usize = 2;

/// 对齐 TS `truncateToolInputValue`：长字符串截断、集合限项、深层嵌套
/// 折叠为 `<nested>`，并跳过 `_` 前缀内部标记键。
fn truncate_tool_input_value(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(s) => {
            if s.chars().count() > TOOL_INPUT_STRING_TRUNCATE_AT {
                let head: String = s.chars().take(TOOL_INPUT_STRING_TRUNCATE_TO).collect();
                Value::String(format!("{}…[{} chars]", head, s.chars().count()))
            } else {
                value.clone()
            }
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
        _ => {
            if depth >= TOOL_INPUT_MAX_DEPTH {
                return Value::String("<nested>".to_string());
            }
            match value {
                Value::Array(arr) => {
                    let mut mapped: Vec<Value> = arr
                        .iter()
                        .take(TOOL_INPUT_MAX_COLLECTION_ITEMS)
                        .map(|v| truncate_tool_input_value(v, depth + 1))
                        .collect();
                    if arr.len() > TOOL_INPUT_MAX_COLLECTION_ITEMS {
                        mapped.push(Value::String(format!("…[{} items]", arr.len())));
                    }
                    Value::Array(mapped)
                }
                Value::Object(obj) => {
                    let entries: Vec<(&String, &Value)> = obj
                        .iter()
                        .filter(|(k, _)| !k.starts_with('_'))
                        .collect();
                    let mut map = Map::new();
                    for (k, v) in entries.iter().take(TOOL_INPUT_MAX_COLLECTION_ITEMS) {
                        map.insert((*k).to_string(), truncate_tool_input_value(v, depth + 1));
                    }
                    if entries.len() > TOOL_INPUT_MAX_COLLECTION_ITEMS {
                        map.insert(
                            "…".to_string(),
                            Value::String(format!("{} keys", entries.len())),
                        );
                    }
                    Value::Object(map)
                }
                _ => Value::String(value.to_string()),
            }
        }
    }
}

/// 序列化工具入参用于 OTel tool_result 事件；截断长串与深嵌套保持有界。
/// `OTEL_LOG_TOOL_DETAILS` 未开启时返回 None（对齐 TS）。
pub fn extract_tool_input_for_telemetry(input: &Value) -> Option<String> {
    if !is_tool_details_logging_enabled() {
        return None;
    }
    let truncated = truncate_tool_input_value(input, 0);
    let mut json = json_stringify(&truncated);
    if json.len() > TOOL_INPUT_MAX_JSON_CHARS {
        json.truncate(TOOL_INPUT_MAX_JSON_CHARS);
        json.push_str("…[truncated]");
    }
    Some(json)
}

// ---------------------------------------------------------------------------
// 文件扩展名
// ---------------------------------------------------------------------------

/// 扩展名最大长度；超出视为潜在敏感（hash 文件名），记为 `other`。
const MAX_FILE_EXTENSION_LENGTH: usize = 10;

/// 提取并脱敏文件扩展名（小写、去前导点；无扩展名或超长返回 None / other）。
pub fn get_file_extension_for_analytics(file_path: &str) -> Option<String> {
    // 等价 path.extname：最后一个路径分隔符之后的最后一个 '.' 部分
    let base = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_path);
    let dot = match base.rfind('.') {
        Some(0) => {
            return None; // "." 隐藏文件：无扩展名（extname 返回 ""）
        }
        Some(dot) => dot,
        None => {
            // 无扩展名时：hash 风格文件名（连字符分段且超长，如
            // "hash-abcd-12345678901234567890"）同样视为潜在敏感，记为 `other`
            // （对齐 TS 注释中 hash-based filename 一律记为 other 的意图）。
            if base.contains('-') && base.len() > MAX_FILE_EXTENSION_LENGTH {
                return Some("other".to_string());
            }
            return None;
        }
    };
    let ext = &base[dot + 1..];
    if ext.is_empty() {
        return None;
    }
    let ext = ext.to_lowercase();
    if ext.len() > MAX_FILE_EXTENSION_LENGTH {
        return Some("other".to_string());
    }
    Some(ext)
}

/// 允许提取扩展名的命令集合（TS `FILE_COMMANDS`）。
const FILE_COMMANDS: &[&str] = &[
    "rm", "mv", "cp", "touch", "mkdir", "chmod", "chown", "cat", "head", "tail", "sort", "stat",
    "diff", "wc", "grep", "rg", "sed",
];

/// 从 bash 命令中提取文件扩展名（尽力而为：按复合操作符与空白切分，
/// 对允许命令的非 flag 参数提取扩展名，逗号连接唯一值顺序）。
pub fn get_file_extensions_from_bash_command(
    command: &str,
    simulated_sed_edit_file_path: Option<&str>,
) -> Option<String> {
    if !command.contains('.') && simulated_sed_edit_file_path.is_none() {
        return None;
    }

    let mut result: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();

    if let Some(path) = simulated_sed_edit_file_path {
        if let Some(ext) = get_file_extension_for_analytics(path) {
            seen.push(ext.clone());
            result = Some(ext);
        }
    }

    for subcmd in split_compound_operators(command) {
        if subcmd.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = subcmd.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let first_token = tokens[0];
        let base_cmd = match first_token.rfind('/') {
            Some(slash_idx) => &first_token[slash_idx + 1..],
            None => first_token,
        };
        if !FILE_COMMANDS.contains(&base_cmd) {
            continue;
        }
        for arg in &tokens[1..] {
            if arg.starts_with('-') {
                continue;
            }
            if let Some(ext) = get_file_extension_for_analytics(arg) {
                if !seen.contains(&ext) {
                    seen.push(ext.clone());
                    result = Some(match result {
                        Some(prev) => format!("{prev},{ext}"),
                        None => ext,
                    });
                }
            }
        }
    }

    result
}

/// 按复合操作符（`&&`、`||`、`;`、`|`）切分命令。
fn split_compound_operators(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '&' && i + 1 < chars.len() && chars[i + 1] == '&' {
            push_if_nonempty(&mut parts, &current);
            current.clear();
            i += 2;
            continue;
        }
        if c == '|' && i + 1 < chars.len() && chars[i + 1] == '|' {
            push_if_nonempty(&mut parts, &current);
            current.clear();
            i += 2;
            continue;
        }
        if c == ';' || c == '|' {
            push_if_nonempty(&mut parts, &current);
            current.clear();
            i += 1;
            continue;
        }
        current.push(c);
        i += 1;
    }
    push_if_nonempty(&mut parts, &current);
    parts
}

fn push_if_nonempty(parts: &mut Vec<String>, s: &str) {
    if !s.is_empty() {
        parts.push(s.to_string());
    }
}

// ---------------------------------------------------------------------------
// 元数据结构
// ---------------------------------------------------------------------------

/// 环境上下文（TS `EnvContext`）。Option 字段对齐 TS 的条件展开。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnvContext {
    pub platform: String,
    pub platform_raw: String,
    pub arch: String,
    pub node_version: Option<String>,
    pub terminal: Option<String>,
    pub package_managers: String,
    pub runtimes: String,
    pub is_running_with_bun: bool,
    pub is_ci: bool,
    pub is_claubbit: bool,
    pub is_claude_code_remote: bool,
    pub is_local_agent_mode: bool,
    pub is_conductor: bool,
    pub remote_environment_type: Option<String>,
    pub coworker_type: Option<String>,
    pub claude_code_container_id: Option<String>,
    pub claude_code_remote_session_id: Option<String>,
    pub tags: Option<String>,
    pub is_github_action: bool,
    pub is_claude_code_action: bool,
    pub is_claude_ai_auth: bool,
    pub version: String,
    pub version_base: Option<String>,
    pub build_time: String,
    pub deployment_environment: String,
    pub github_event_name: Option<String>,
    pub github_actions_runner_environment: Option<String>,
    pub github_actions_runner_os: Option<String>,
    pub github_action_ref: Option<String>,
    pub wsl_version: Option<String>,
    pub linux_distro_id: Option<String>,
    pub linux_distro_version: Option<String>,
    pub linux_kernel: Option<String>,
    pub vcs: Option<String>,
}

/// 进程指标（TS `ProcessMetrics`）。Node 运行时字段在 Rust 侧为 0
/// （见模块偏差）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessMetrics {
    pub uptime: f64,
    pub rss: u64,
    pub heap_total: u64,
    pub heap_used: u64,
    pub external: u64,
    pub array_buffers: u64,
    pub constrained_memory: Option<u64>,
    pub cpu_user_us: u64,
    pub cpu_system_us: u64,
    pub cpu_percent: Option<f64>,
}

/// 核心事件元数据（TS `EventMetadata`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventMetadata {
    pub model: String,
    pub session_id: String,
    pub user_type: String,
    pub betas: Option<String>,
    pub env_context: EnvContext,
    pub entrypoint: Option<String>,
    pub agent_sdk_version: Option<String>,
    pub is_interactive: String,
    pub client_type: String,
    pub process_metrics: Option<ProcessMetrics>,
    pub swe_bench_run_id: String,
    pub swe_bench_instance_id: String,
    pub swe_bench_task_id: String,
    /// CLAUDE_CODE_AGENT_ID（agentName@teamName）或子代理 UUID。
    pub agent_id: Option<String>,
    /// CLAUDE_CODE_PARENT_SESSION_ID（team lead 的会话）。
    pub parent_session_id: Option<String>,
    /// 区分 swarm teammate / Agent 子代理 / standalone agent。
    #[serde(rename = "agentType")]
    pub agent_type: Option<String>,
    /// swarm agent 的 team 名。
    pub team_name: Option<String>,
    /// OAuth 订阅层级（max/pro/enterprise/team）。
    pub subscription_type: Option<String>,
    /// repo remote URL hash（SHA256 前 16 字符）。
    pub rh: Option<String>,
    /// KAIROS 助手模式激活（ant-only）。
    pub kairos_active: Option<bool>,
    pub skill_mode: Option<String>,
    pub observer_mode: Option<String>,
}

/// 富化选项（TS `EnrichMetadataOptions`）。
#[derive(Debug, Clone, Default)]
pub struct EnrichMetadataOptions {
    /// 使用的模型；未提供时回落 `getMainLoopModel()`。
    pub model: Option<String>,
    /// 显式 betas 字符串（未提供时不写入 betas 字段；偏差：TS 侧会经
    /// `getModelBetas(model)` 计算，该模块未转换）。
    pub betas: Option<String>,
}

/// 从完整版本号提取 base 版本：`2.0.36-dev.20251107.t174150.sha2709699`
/// → `2.0.36-dev`。
fn get_version_base(version: &str) -> Option<String> {
    let re = regex::Regex::new(r"^\d+\.\d+\.\d+(?:-[a-z]+)?").ok()?;
    re.find(version).map(|m| m.as_str().to_string())
}

/// 构建环境上下文（偏差：env.ts 检测能力未转换，保守填充）。
fn build_env_context() -> EnvContext {
    let platform = match get_platform() {
        Platform::Macos => "darwin",
        Platform::Windows => "win32",
        _ => "linux",
    };
    let platform_raw = std::env::var("CLAUDE_CODE_HOST_PLATFORM")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    EnvContext {
        platform: platform.to_string(),
        platform_raw,
        arch: std::env::consts::ARCH.to_string(),
        node_version: None, // 无 Node 运行时（偏差）
        terminal: std::env::var("TERM_PROGRAM").ok().filter(|s| !s.is_empty()),
        package_managers: String::new(), // env.ts 未转换（偏差）
        runtimes: String::new(),
        is_running_with_bun: false,
        is_ci: is_env_truthy(std::env::var("CI").ok().as_deref()),
        is_claubbit: is_env_truthy(std::env::var("CLAUBBIT").ok().as_deref()),
        is_claude_code_remote: is_env_truthy(std::env::var("CLAUDE_CODE_REMOTE").ok().as_deref()),
        is_local_agent_mode: std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref() == Ok("local-agent"),
        is_conductor: false, // env.isConductor() 未转换（偏差）
        remote_environment_type: std::env::var("CLAUDE_CODE_REMOTE_ENVIRONMENT_TYPE").ok(),
        coworker_type: std::env::var("CLAUDE_CODE_COWORKER_TYPE").ok(),
        claude_code_container_id: std::env::var("CLAUDE_CODE_CONTAINER_ID").ok(),
        claude_code_remote_session_id: std::env::var("CLAUDE_CODE_REMOTE_SESSION_ID").ok(),
        tags: std::env::var("CLAUDE_CODE_TAGS").ok(),
        is_github_action: is_env_truthy(std::env::var("GITHUB_ACTIONS").ok().as_deref()),
        is_claude_code_action: is_env_truthy(std::env::var("CLAUDE_CODE_ACTION").ok().as_deref()),
        is_claude_ai_auth: is_claude_ai_subscriber(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        version_base: get_version_base(env!("CARGO_PKG_VERSION")),
        build_time: String::new(), // MACRO.BUILD_TIME 无 Rust 等价（偏差）
        deployment_environment: "production".to_string(),
        github_event_name: std::env::var("GITHUB_EVENT_NAME").ok(),
        github_actions_runner_environment: std::env::var("RUNNER_ENVIRONMENT").ok(),
        github_actions_runner_os: std::env::var("RUNNER_OS").ok(),
        github_action_ref: std::env::var("GITHUB_ACTION_PATH").ok(),
        wsl_version: get_wsl_version().map(|s| s.to_string()),
        linux_distro_id: get_linux_distro_info().and_then(|info| info.linux_distro_id),
        linux_distro_version: get_linux_distro_info().and_then(|info| info.linux_distro_version),
        linux_kernel: get_linux_distro_info().map(|info| info.linux_kernel),
        vcs: {
            let vcs = detect_vcs(None);
            if vcs.is_empty() {
                None
            } else {
                Some(vcs.join(","))
            }
        },
    }
}

/// 进程指标（CPU% 差值跟踪为进程全局，对齐 TS 的 prev* 静态变量）。
fn build_process_metrics() -> Option<ProcessMetrics> {
    static PREV: std::sync::Mutex<Option<(u64, u64, u128)>> =
        std::sync::Mutex::new(None);
    let (user_us, system_us) = current_cpu_time_us();
    let now_ms = now_ms();
    let cpu_percent = {
        let mut prev = PREV.lock().unwrap_or_else(|e| e.into_inner());
        let pct = match *prev {
            Some((p_user, p_system, p_wall)) => {
                let wall_delta_ms = now_ms.saturating_sub(p_wall);
                if wall_delta_ms > 0 {
                    let user_delta = user_us.saturating_sub(p_user);
                    let system_delta = system_us.saturating_sub(p_system);
                    Some(((user_delta + system_delta) as f64 / (wall_delta_ms * 1000) as f64) * 100.0)
                } else {
                    None
                }
            }
            None => None,
        };
        *prev = Some((user_us, system_us, now_ms));
        pct
    };
    Some(ProcessMetrics {
        uptime: 0.0,           // process.uptime() 未移植（偏差）
        rss: 0,
        heap_total: 0,
        heap_used: 0,
        external: 0,
        array_buffers: 0,
        constrained_memory: None,
        cpu_user_us: user_us,
        cpu_system_us: system_us,
        cpu_percent,
    })
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 当前进程 CPU 时间（用户+系统，微秒）。非 Linux/macOS 返回 0。
fn current_cpu_time_us() -> (u64, u64) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // libc 未入 Cargo.toml：读取 /proc/self/stat（Linux）；其他平台返回 0。
        #[cfg(target_os = "linux")]
        {
            if let Ok(text) = std::fs::read_to_string("/proc/self/stat") {
                // `pid (comm) state ppid ... utime stime ...`：')' 之后第
                // 11/12 个字段为 utime/stime（stat(5) 字段 14/15），clock tick 计。
                let rest = text.rsplit(')').next().unwrap_or("");
                let fields: Vec<&str> = rest.split_whitespace().collect();
                let utime: u64 = fields.get(11).and_then(|f| f.parse().ok()).unwrap_or(0);
                let stime: u64 = fields.get(12).and_then(|f| f.parse().ok()).unwrap_or(0);
                let hz: u64 = 100;
                return (utime * 1_000_000 / hz, stime * 1_000_000 / hz);
            }
        }
        (0, 0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (0, 0)
    }
}

/// 核心事件元数据富化入口（TS `getEventMetadata`）。
///
/// 偏差：runtimes/packageManagers、repo remote hash、agent 标识、betas
/// 计算依赖未转换模块，对应字段缺省（见模块头注释）。
pub fn get_event_metadata(options: &EnrichMetadataOptions) -> EventMetadata {
    let model = options
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(get_main_loop_model);
    let env_context = build_env_context();
    let process_metrics = build_process_metrics();
    let subscription_type = get_subscription_type().map(|t| t.as_str().to_string());
    let kairos_active = if is_env_truthy(std::env::var("FEATURE_KAIROS").ok().as_deref())
        && get_kairos_active()
    {
        Some(true)
    } else {
        None
    };

    EventMetadata {
        model,
        session_id: get_session_id().to_string(),
        user_type: std::env::var("USER_TYPE").unwrap_or_default(),
        betas: options.betas.clone(),
        env_context,
        entrypoint: std::env::var("CLAUDE_CODE_ENTRYPOINT").ok(),
        agent_sdk_version: std::env::var("CLAUDE_AGENT_SDK_VERSION").ok(),
        is_interactive: get_is_interactive().to_string(),
        client_type: get_client_type(),
        process_metrics,
        swe_bench_run_id: std::env::var("SWE_BENCH_RUN_ID").unwrap_or_default(),
        swe_bench_instance_id: std::env::var("SWE_BENCH_INSTANCE_ID").unwrap_or_default(),
        swe_bench_task_id: std::env::var("SWE_BENCH_TASK_ID").unwrap_or_default(),
        agent_id: None,        // teammate/agentContext 未转换（偏差）
        parent_session_id: None,
        agent_type: None,
        team_name: None,
        subscription_type,
        rh: None,              // getRepoRemoteHash 未转换（偏差）
        kairos_active,
        skill_mode: None,      // 由调用方经 additional metadata 补充
        observer_mode: None,
    }
}

// ---------------------------------------------------------------------------
// 1P 事件格式（snake_case）
// ---------------------------------------------------------------------------

/// GitHub Actions 元数据（TS `GitHubActionsMetadata`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GitHubActionsMetadata {
    pub actor_id: Option<String>,
    pub repository_id: Option<String>,
    pub repository_owner_id: Option<String>,
}

/// 1P 事件的用户元数据子集（TS `CoreUserData` 中被 1P 格式消费的字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CoreUserData {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub github_actions_metadata: Option<GitHubActionsMetadata>,
}

/// OTEL 风格的鉴权信息（proto `PublicApiAuth`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PublicApiAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
}

/// 1P 核心元数据（snake_case；`FirstPartyEventLoggingCoreMetadata`）。
/// Option 字段对齐 TS 的条件赋值：未设置时不发射（proto toJSON 语义）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct FirstPartyEventLoggingCoreMetadata {
    pub session_id: String,
    pub model: String,
    pub user_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub betas: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_sdk_version: Option<String>,
    pub is_interactive: bool,
    pub client_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swe_bench_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swe_bench_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swe_bench_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
}

/// 1P 事件完整格式（`FirstPartyEventLoggingMetadata`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FirstPartyEventLoggingMetadata {
    /// 环境（proto `EnvironmentMetadata` 的 toJSON 输出）。
    pub env: Map<String, Value>,
    /// 进程指标（base64 编码的 JSON）。
    pub process: Option<String>,
    /// 顶层鉴权字段（有意不含 account_id：仅 UUID 字段客户端填充）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<PublicApiAuth>,
    /// 核心字段，直接导出到 BigQuery 顶层列。
    pub core: FirstPartyEventLoggingCoreMetadata,
    /// additional_metadata proto 字段（按事件类型变化）。
    pub additional: Map<String, Value>,
}

/// 元数据转 1P 事件格式（snake_case 字段；`to1PEventFormat`）。
///
/// 纯函数：把核心元数据、用户元数据与附加元数据重组为
/// `/api/event_logging/batch` 期望的 schema。环境字段必须与 monorepo
/// proto 定义一致（新增字段需先更新 proto，见模块头注释）。
pub fn to_1p_event_format(
    metadata: &EventMetadata,
    user_metadata: &CoreUserData,
    additional_metadata: &Map<String, Value>,
) -> FirstPartyEventLoggingMetadata {
    let env_context = &metadata.env_context;
    let mut env = Map::new();
    env.insert("platform".into(), Value::String(env_context.platform.clone()));
    env.insert("platform_raw".into(), Value::String(env_context.platform_raw.clone()));
    env.insert("arch".into(), Value::String(env_context.arch.clone()));
    if let Some(node_version) = &env_context.node_version {
        env.insert("node_version".into(), Value::String(node_version.clone()));
    }
    env.insert(
        "terminal".into(),
        Value::String(env_context.terminal.clone().unwrap_or_else(|| "unknown".to_string())),
    );
    env.insert("package_managers".into(), Value::String(env_context.package_managers.clone()));
    env.insert("runtimes".into(), Value::String(env_context.runtimes.clone()));
    env.insert("is_running_with_bun".into(), Value::Bool(env_context.is_running_with_bun));
    env.insert("is_ci".into(), Value::Bool(env_context.is_ci));
    env.insert("is_claubbit".into(), Value::Bool(env_context.is_claubbit));
    env.insert("is_claude_code_remote".into(), Value::Bool(env_context.is_claude_code_remote));
    env.insert("is_local_agent_mode".into(), Value::Bool(env_context.is_local_agent_mode));
    env.insert("is_conductor".into(), Value::Bool(env_context.is_conductor));
    env.insert("is_github_action".into(), Value::Bool(env_context.is_github_action));
    env.insert("is_claude_code_action".into(), Value::Bool(env_context.is_claude_code_action));
    env.insert("is_claude_ai_auth".into(), Value::Bool(env_context.is_claude_ai_auth));
    env.insert("version".into(), Value::String(env_context.version.clone()));
    env.insert("build_time".into(), Value::String(env_context.build_time.clone()));
    env.insert(
        "deployment_environment".into(),
        Value::String(env_context.deployment_environment.clone()),
    );

    // 可选环境字段（对齐 TS 的条件展开序列）
    macro_rules! env_opt_str {
        ($key:expr, $field:expr) => {
            if let Some(value) = &$field {
                env.insert($key.into(), Value::String(value.clone()));
            }
        };
    }
    env_opt_str!("remote_environment_type", env_context.remote_environment_type);
    if is_env_truthy(std::env::var("FEATURE_COWORKER_TYPE_TELEMETRY").ok().as_deref()) {
        env_opt_str!("coworker_type", env_context.coworker_type);
    }
    env_opt_str!("claude_code_container_id", env_context.claude_code_container_id);
    env_opt_str!("claude_code_remote_session_id", env_context.claude_code_remote_session_id);
    if let Some(tags) = &env_context.tags {
        let list: Vec<Value> = tags
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|t| Value::String(t.to_string()))
            .collect();
        env.insert("tags".into(), Value::Array(list));
    }
    env_opt_str!("github_event_name", env_context.github_event_name);
    env_opt_str!("github_actions_runner_environment", env_context.github_actions_runner_environment);
    env_opt_str!("github_actions_runner_os", env_context.github_actions_runner_os);
    env_opt_str!("github_action_ref", env_context.github_action_ref);
    env_opt_str!("wsl_version", env_context.wsl_version);
    env_opt_str!("linux_distro_id", env_context.linux_distro_id);
    env_opt_str!("linux_distro_version", env_context.linux_distro_version);
    env_opt_str!("linux_kernel", env_context.linux_kernel);
    env_opt_str!("vcs", env_context.vcs);
    env_opt_str!("version_base", env_context.version_base);
    // snake_case 环境字段：is_interactive 布尔化（与 core 同值）
    env.insert(
        "is_interactive".into(),
        Value::Bool(metadata.is_interactive == "true"),
    );

    // 核心字段（snake_case）
    let core = FirstPartyEventLoggingCoreMetadata {
        session_id: metadata.session_id.clone(),
        model: metadata.model.clone(),
        user_type: metadata.user_type.clone(),
        betas: metadata.betas.clone(),
        entrypoint: metadata.entrypoint.clone(),
        agent_sdk_version: metadata.agent_sdk_version.clone(),
        is_interactive: metadata.is_interactive == "true",
        client_type: metadata.client_type.clone(),
        swe_bench_run_id: Some(metadata.swe_bench_run_id.clone()).filter(|s| !s.is_empty()),
        swe_bench_instance_id: Some(metadata.swe_bench_instance_id.clone()).filter(|s| !s.is_empty()),
        swe_bench_task_id: Some(metadata.swe_bench_task_id.clone()).filter(|s| !s.is_empty()),
        agent_id: metadata.agent_id.clone(),
        parent_session_id: metadata.parent_session_id.clone(),
        agent_type: metadata.agent_type.clone(),
        team_name: metadata.team_name.clone(),
    };

    // GitHub Actions 元数据（放入 env）
    if let Some(gh_meta) = &user_metadata.github_actions_metadata {
        let mut gh = Map::new();
        gh.insert("actor_id".into(), gh_meta.actor_id.clone().into());
        gh.insert("repository_id".into(), gh_meta.repository_id.clone().into());
        gh.insert("repository_owner_id".into(), gh_meta.repository_owner_id.clone().into());
        env.insert("github_actions_metadata".into(), Value::Object(gh));
    }

    let auth = if user_metadata.account_uuid.is_some() || user_metadata.organization_uuid.is_some() {
        Some(PublicApiAuth {
            account_uuid: user_metadata.account_uuid.clone(),
            organization_uuid: user_metadata.organization_uuid.clone(),
        })
    } else {
        None
    };

    // 进程指标 base64
    let process = metadata.process_metrics.as_ref().map(|metrics| {
        let value = serde_json::to_value(metrics).unwrap_or_else(|_| Value::Null);
        base64_encode(json_stringify(&value).as_bytes())
    });

    // additional（_PROTO_* 由 exporter 提升到 proto 字段后仍在此剥离）
    let mut additional = Map::new();
    if let Some(rh) = &metadata.rh {
        additional.insert("rh".into(), Value::String(rh.clone()));
    }
    if metadata.kairos_active == Some(true) {
        additional.insert("is_assistant_mode".into(), Value::Bool(true));
    }
    if let Some(skill_mode) = &metadata.skill_mode {
        additional.insert("skill_mode".into(), Value::String(skill_mode.clone()));
    }
    if let Some(observer_mode) = &metadata.observer_mode {
        additional.insert("observer_mode".into(), Value::String(observer_mode.clone()));
    }
    for (key, value) in additional_metadata {
        additional.insert(key.clone(), value.clone());
    }

    FirstPartyEventLoggingMetadata {
        env,
        process,
        auth,
        core,
        additional,
    }
}

/// 标准 base64 编码（含 padding；等价 TS `Buffer.toString('base64')`）。
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(n >> 12 & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_mcp_tool_names() {
        assert_eq!(sanitize_tool_name_for_analytics("mcp__x__y"), "mcp_tool");
        assert_eq!(sanitize_tool_name_for_analytics("Bash"), "Bash");
    }

    #[test]
    fn extract_mcp_details_handles_double_underscores() {
        let (server, tool) = extract_mcp_tool_details("mcp__slack__read__channel").unwrap();
        assert_eq!(server, "slack");
        assert_eq!(tool, "read__channel");
        assert!(extract_mcp_tool_details("Bash").is_none());
        assert!(extract_mcp_tool_details("mcp__only").is_none());
    }

    #[test]
    fn extract_skill_name_from_skill_tool() {
        assert_eq!(
            extract_skill_name("Skill", &json!({"skill": "commit"})),
            Some("commit".to_string())
        );
        assert!(extract_skill_name("Bash", &json!({"skill": "x"})).is_none());
    }

    #[test]
    fn truncate_long_strings() {
        let long = Value::String("x".repeat(600));
        let truncated = truncate_tool_input_value(&long, 0);
        let s = truncated.as_str().unwrap_or("");
        assert!(s.starts_with(&"x".repeat(128)));
        assert!(s.contains("600 chars"));
    }

    #[test]
    fn collapse_deep_nesting() {
        let nested = json!({"a": {"b": {"c": {"d": 1}}}});
        let truncated = truncate_tool_input_value(&nested, 0);
        assert_eq!(truncated["a"]["b"], json!("<nested>"));
    }

    #[test]
    fn file_extension_extraction() {
        assert_eq!(get_file_extension_for_analytics("a/b/c.rs"), Some("rs".into()));
        assert_eq!(get_file_extension_for_analytics("hash-abcd-12345678901234567890"), Some("other".into()));
        assert_eq!(get_file_extension_for_analytics("Makefile"), None);
    }

    #[test]
    fn bash_command_extensions() {
        let exts = get_file_extensions_from_bash_command("rg foo bar.rs && cat notes.md", None);
        assert_eq!(exts.as_deref(), Some("rs,md"));
        // flag 参数不算
        let none = get_file_extensions_from_bash_command("grep -r pattern .", None);
        assert_eq!(none, None);
    }

    #[test]
    fn to_1p_event_format_snake_cases_env() {
        let metadata = EventMetadata {
            model: "claude-sonnet".into(),
            session_id: "sid".into(),
            is_interactive: "true".into(),
            client_type: "cli".into(),
            ..Default::default()
        };
        let user = CoreUserData::default();
        let empty = Map::new();
        let formatted = to_1p_event_format(&metadata, &user, &empty);
        assert_eq!(formatted.env["is_interactive"], Value::Bool(true));
        assert_eq!(formatted.core.model, "claude-sonnet");
        assert!(formatted.auth.is_none());
    }

    #[test]
    fn base64_roundtrip_basics() {
        // "Man" → "TWFu"
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }
}
