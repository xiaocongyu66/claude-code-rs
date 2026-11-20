//! datadog.rs — Datadog 日志 sink（重写自 `src/services/analytics/datadog.ts`）。
//!
//! 职责（对齐 TS）：
//! - 事件 allowlist（`DATADOG_ALLOWED_EVENTS`）：未登记的事件名一律不发。
//! - tag 字段（`TAG_FIELDS`）与 `camelToSnakeCase` 归一化。
//! - 批量缓冲（`MAX_BATCH_SIZE = 100`）与 flush 定时（默认 15s）。
//! - `initializeDatadog`（memoized）/ `shutdownDatadog` / `trackDatadogEvent`
//!   的门控链：prod 环境 + firstParty provider + 已初始化 + allowlist。
//! - `getUserBucket`：sha256(userID) 前 8 位 % 30 的分桶（隐私 + 基数控制）。
//!
//! 已知偏差（no-op 上报 + 依赖未转换模块）：
//! - **HTTP 未接入**：`flush_logs` 批量上报为 no-op（仅 ant debug 日志），
//!   与 analytics 层整体「数据结构落地、上报 no-op」一致；待 reqwest
//!   网络层接入后补 POST。
//! - TS 的 `setTimeout` flush 定时器无 Rust 等价（无宿主运行时）：
//!   `schedule_flush` 为 no-op；批量满 100 时立即触发一次 flush
//!   （flush 本身 no-op，保证状态机完整）。
//! - `utils/config.ts` 的 `getOrCreateUserID` 未转换：`getUserBucket` 读
//!   `~/.claude.json` 的 `userID` 字段（本地 duck-type 读取），缺失时
//!   以空串计算（hash 仍确定，仅分桶熵下降）。
//! - `getEventMetadata` 的 model 规范化（非 ant 用户）依赖
//!   `getCanonicalName` + `MODEL_COSTS`（已就绪）。
//! - `lodash.memoize` 等价物：`initialize_datadog` 用 Once 保证只跑一次。

use std::collections::HashSet;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::model::model::get_canonical_name;
use crate::utils::model::provider_classification::ApiProvider;
use crate::utils::model::providers::get_api_provider;
use crate::utils::model_cost::MODEL_COSTS;

use super::config::is_analytics_disabled;
use super::index::{LogEventMetadata, MetadataValue};
use super::metadata::{get_event_metadata, EnrichMetadataOptions};

/// Datadog endpoint 与 token 经环境变量配置；两者皆未设置时整个
/// Datadog 日志关闭。
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 15_000;
const MAX_BATCH_SIZE: usize = 100;
const NETWORK_TIMEOUT_MS: u64 = 5_000;

/// 允许上报到 Datadog 的事件名集合（对齐 TS `DATADOG_ALLOWED_EVENTS`，
/// 含 W8 扩展块）。
pub static DATADOG_ALLOWED_EVENTS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "chrome_bridge_connection_succeeded",
        "chrome_bridge_connection_failed",
        "chrome_bridge_disconnected",
        "chrome_bridge_tool_call_completed",
        "chrome_bridge_tool_call_error",
        "chrome_bridge_tool_call_started",
        "chrome_bridge_tool_call_timeout",
        "tengu_api_error",
        "tengu_api_success",
        "tengu_brief_mode_enabled",
        "tengu_brief_mode_toggled",
        "tengu_brief_send",
        "tengu_cancel",
        "tengu_compact_failed",
        "tengu_exit",
        "tengu_flicker",
        "tengu_init",
        "tengu_model_fallback_triggered",
        "tengu_oauth_error",
        "tengu_oauth_success",
        "tengu_oauth_token_refresh_failure",
        "tengu_oauth_token_refresh_success",
        "tengu_oauth_token_refresh_lock_acquiring",
        "tengu_oauth_token_refresh_lock_acquired",
        "tengu_oauth_token_refresh_starting",
        "tengu_oauth_token_refresh_completed",
        "tengu_oauth_token_refresh_lock_releasing",
        "tengu_oauth_token_refresh_lock_released",
        "tengu_query_error",
        "tengu_session_file_read",
        "tengu_started",
        "tengu_tool_use_error",
        "tengu_tool_use_granted_in_prompt_permanent",
        "tengu_tool_use_granted_in_prompt_temporary",
        "tengu_tool_use_rejected_in_prompt",
        "tengu_tool_use_success",
        "tengu_uncaught_exception",
        "tengu_unhandled_rejection",
        "tengu_voice_recording_started",
        "tengu_voice_toggled",
        "tengu_team_mem_sync_pull",
        "tengu_team_mem_sync_push",
        "tengu_team_mem_sync_started",
        "tengu_team_mem_entries_capped",
        // W8 (#59) 扩展：本仓库 logEvent 调用点白名单
        "tengu_api_retry",
        "tengu_api_persistent_retry_wait",
        "tengu_streaming_fallback_to_non_streaming",
        "tengu_api_model_not_found_fallback_triggered",
        "tengu_api_opus_fallback_triggered",
        "tengu_nonstreaming_fallback_started",
        "tengu_nonstreaming_fallback_error",
        "tengu_oauth_token_refresh_lock_retry",
        "tengu_oauth_token_refresh_lock_retry_limit_reached",
        "tengu_oauth_token_refresh_lock_error",
        "tengu_issue_fallback",
        "tengu_issue_failed",
        "tengu_streaming_error",
        "tengu_streaming_idle_timeout",
        "tengu_config_parse_error",
        "tengu_version_check_failure",
        "tengu_bridge_heartbeat_error",
        "tengu_bridge_fatal_error",
        "tengu_bridge_registration_failed",
        "tengu_bridge_session_timeout",
        "tengu_mcp_ide_server_connection_failed",
        "tengu_mcp_tool_call_auth_error",
        "tengu_file_upload_failed",
        "tengu_schedule_failed",
        "tengu_session_resumed",
        "tengu_bridge_spawn_mode_chosen",
        "tengu_bridge_spawn_mode_toggled",
        "tengu_notification_method_used",
        "tengu_refusal_api_response",
        "tengu_sandbox_network_dialog_result",
        "tengu_mcp_list_changed",
        "tengu_mcp_server_connection_failed",
        "tengu_plan_exit",
        "tengu_config_changed",
        "tengu_continue",
        "tengu_copy",
        "tengu_lifecycle_hook_verdict_discarded",
    ])
});

/// 高基数过滤字段（对齐 TS `TAG_FIELDS`）。
const TAG_FIELDS: &[&str] = &[
    "arch",
    "clientType",
    "errorType",
    "http_status_range",
    "http_status",
    "kairosActive",
    "model",
    "platform",
    "provider",
    "skillMode",
    "subscriptionType",
    "toolName",
    "userBucket",
    "userType",
    "version",
    "versionBase",
];

/// camelCase → snake_case（对齐 TS `camelToSnakeCase`）。
pub fn camel_to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Datadog 日志条目（必填字段 + 任意附加字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatadogLog {
    pub ddsource: String,
    pub ddtags: String,
    pub message: String,
    pub service: String,
    pub hostname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// 全部归一化字段（camel→snake 后键）。
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Default)]
struct DatadogState {
    log_batch: Vec<DatadogLog>,
    initialized: Option<bool>,
}

static STATE: Lazy<Mutex<DatadogState>> = Lazy::new(|| Mutex::new(DatadogState::default()));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 批量上报。**偏差（no-op）**：不发送网络请求，仅清空批量并记录
/// ant debug 日志。
pub async fn flush_logs() {
    let logs_to_send: Vec<DatadogLog> = {
        let mut state = lock(&STATE);
        std::mem::take(&mut state.log_batch)
    };
    if logs_to_send.is_empty() {
        return;
    }
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        log_for_debugging(
            &format!("Datadog flush: {} log(s) (send is no-op in this build)", logs_to_send.len()),
            DebugLogLevel::Debug,
        );
    }
    // TODO(network): reqwest POST DATADOG_LOGS_ENDPOINT，headers
    // Content-Type: application/json + DD-API-KEY，timeout NETWORK_TIMEOUT_MS
    let _ = NETWORK_TIMEOUT_MS;
}

/// 定时 flush。**偏差**：无 setTimeout 等价物，此处 no-op（批量满时
/// 由 trackDatadogEvent 立即 flush）。
fn schedule_flush() {
    get_flush_interval_ms(); // 保持环境变量读取（供未来定时器接入）
}

/// `initializeDatadog`（memoized）：analytics 禁用或 endpoint/token 缺失
/// 时返回 false 且不初始化。
pub async fn initialize_datadog() -> bool {
    let mut state = lock(&STATE);
    if let Some(initialized) = state.initialized {
        return initialized;
    }
    if is_analytics_disabled() {
        state.initialized = Some(false);
        return false;
    }
    let endpoint = std::env::var("DATADOG_LOGS_ENDPOINT").unwrap_or_default();
    let token = std::env::var("DATADOG_API_KEY").unwrap_or_default();
    if endpoint.is_empty() || token.is_empty() {
        state.initialized = Some(false);
        return false;
    }
    state.initialized = Some(true);
    true
}

/// 关闭 Datadog：排空剩余批量（no-op 发送）。
pub async fn shutdown_datadog() {
    flush_logs().await;
}

/// 记录一个 Datadog 事件（TS `trackDatadogEvent`）。
///
/// 门控链：NODE_ENV=production → provider=firstParty → 已初始化 →
/// allowlist 命中。之后富化元数据、归一化基数字段（MCP 工具名、模型
/// 短名、dev 版本截断、status→http_status），拼装 ddtags，入批量；满
/// MAX_BATCH_SIZE 立即 flush，否则 schedule_flush。
pub async fn track_datadog_event(event_name: &str, properties: &LogEventMetadata) {
    if std::env::var("NODE_ENV").as_deref() != Ok("production") {
        return;
    }
    if get_api_provider() != ApiProvider::FirstParty {
        return;
    }

    // 快速路径：缓存命中时避免 await
    let initialized = {
        let state = lock(&STATE);
        state.initialized
    };
    let initialized = match initialized {
        Some(value) => value,
        None => initialize_datadog().await,
    };
    if !initialized || !DATADOG_ALLOWED_EVENTS.contains(event_name) {
        return;
    }

    // LogEventMetadata（bool/number/string/undefined）→ JSON map：undefined 落键即跳过
    let event_properties: Map<String, Value> = properties
        .iter()
        .filter_map(|(key, value)| match value {
            Some(MetadataValue::Bool(b)) => Some((key.clone(), Value::Bool(*b))),
            Some(MetadataValue::Num(n)) => {
                serde_json::Number::from_f64(*n).map(|num| (key.clone(), Value::Number(num)))
            }
            Some(MetadataValue::String(s)) => Some((key.clone(), Value::String(s.clone()))),
            None => None,
        })
        .collect();

    let model = event_properties.get("model").and_then(Value::as_str);
    let betas = event_properties.get("betas").and_then(Value::as_str);
    let metadata = get_event_metadata(&EnrichMetadataOptions {
        model: model.map(|s| s.to_string()),
        betas: betas.map(|s| s.to_string()),
    });

    // envContext 展平：一次嵌套、一次扁平
    let mut all_data: Map<String, Value> = match serde_json::to_value(&metadata.env_context) {
        Ok(Value::Object(env)) => env,
        _ => Map::new(),
    };
    all_data.insert("model".into(), Value::String(metadata.model.clone()));
    all_data.insert("sessionId".into(), Value::String(metadata.session_id.clone()));
    all_data.insert("userType".into(), Value::String(metadata.user_type.clone()));
    all_data.insert("isInteractive".into(), Value::String(metadata.is_interactive.clone()));
    all_data.insert("clientType".into(), Value::String(metadata.client_type.clone()));
    for (key, value) in event_properties {
        all_data.insert(key, value);
    }
    all_data.insert("userBucket".into(), Value::Number(get_user_bucket().into()));

    // MCP 工具名归一化为 mcp（基数控制）
    if let Some(Value::String(tool)) = all_data.get_mut("toolName") {
        if tool.starts_with("mcp__") {
            *tool = "mcp".to_string();
        }
    }

    // 模型名归一化（仅非 ant：外部用户基数控制）；去除尾部 [1m]/[1M] 后缀
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        if let Some(Value::String(model)) = all_data.get_mut("model") {
            let trimmed = if model.to_lowercase().ends_with("[1m]") {
                &model[..model.len() - "[1m]".len()]
            } else {
                model.as_str()
            };
            let short_name = get_canonical_name(trimmed);
            let mapped = if MODEL_COSTS.iter().any(|(key, _)| *key == short_name) {
                short_name
            } else {
                "other".to_string()
            };
            *model = mapped;
        }
    }

    // dev 版本截断为 base + 日期：2.0.53-dev.20251124.t173302.sha526cc6a
    // → 2.0.53-dev.20251124
    if let Some(Value::String(version)) = all_data.get_mut("version") {
        if let Ok(re) = regex::Regex::new(r"^(\d+\.\d+\.\d+-dev\.\d{8})\.t\d+\.sha[a-f0-9]+$") {
            if let Some(captured) = re.captures(version) {
                if let Some(base) = captured.get(1) {
                    *version = base.as_str().to_string();
                }
            }
        }
    }

    // status → http_status / http_status_range（Datadog 保留字段冲突）
    if let Some(status) = all_data.remove("status") {
        if !status.is_null() {
            let status_code = match status {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s,
                other => other.to_string(),
            };
            all_data.insert("http_status".into(), Value::String(status_code.clone()));
            let first_digit = status_code.chars().next();
            if let Some(d) = first_digit {
                if ('1'..='5').contains(&d) {
                    all_data.insert(
                        "http_status_range".into(),
                        Value::String(format!("{d}xx")),
                    );
                }
            }
        }
    }

    // ddtags：event:<name> 前缀（message 不可查询）+ 高基数字段
    let mut tags: Vec<String> = vec![format!("event:{event_name}")];
    for field in TAG_FIELDS {
        let Some(value) = all_data.get(*field) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        tags.push(format!("{}:{}", camel_to_snake_case(field), value));
    }

    let log = DatadogLog {
        ddsource: "nodejs".to_string(),
        ddtags: tags.join(","),
        message: event_name.to_string(),
        service: "claude-code".to_string(),
        hostname: "claude-code".to_string(),
        env: std::env::var("USER_TYPE").ok(),
        fields: all_data,
    };

    let batch_full = {
        let mut state = lock(&STATE);
        state.log_batch.push(log);
        state.log_batch.len() >= MAX_BATCH_SIZE
    };
    if batch_full {
        flush_logs().await;
    } else {
        schedule_flush();
    }
}

const NUM_USER_BUCKETS: u32 = 30;

/// 用户分桶：sha256(userID) 前 8 位 hex % NUM_USER_BUCKETS。
///
/// 用于按「受影响用户数」而非「事件数」告警，同时避免直接计数用户 ID
/// 的基数与隐私问题。
fn get_user_bucket() -> u32 {
    let user_id = get_or_create_user_id();
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(4).map(|b| format!("{:02x}", b)).collect();
    let parsed = u32::from_str_radix(&hex, 16).unwrap_or(0);
    parsed % NUM_USER_BUCKETS
}

/// 读 `~/.claude.json` 的 `userID`（偏差：`utils/config.ts` 的
/// `getOrCreateUserID` 未转换；此处仅读取，缺失返回空串——确定性分桶
/// 语义保留，熵略降）。
fn get_or_create_user_id() -> String {
    let config_home = crate::utils::env_utils::get_claude_config_home_dir();
    let path = std::path::Path::new(&config_home).join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    value
        .get("userID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn get_flush_interval_ms() -> u64 {
    // 测试可覆盖默认 flush 间隔（对齐 TS）
    std::env::var("CLAUDE_CODE_DATADOG_FLUSH_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_FLUSH_INTERVAL_MS)
}
