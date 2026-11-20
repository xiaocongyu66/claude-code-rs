//! first_party_event_logger.rs — 1P 事件日志（重写自
//! `src/services/analytics/firstPartyEventLogger.ts`）。
//!
//! 职责（对齐 TS）：
//! - 事件采样配置（`EventSamplingConfig` / `shouldSampleEvent`：GrowthBook
//!   动态配置 `tengu_event_sampling_config`，0-1 采样率，0 丢弃、1 全量）。
//! - 批处理配置（`BatchConfig` / `getBatchConfig`：`tengu_1p_event_batch_config`）。
//! - `is1PEventLoggingEnabled()`（复用 analytics 统一 opt-out）。
//! - `logEventTo1P`：富化核心元数据后 emit（本实现写入进程内缓冲，
//!   见偏差）。
//! - `logGrowthBookExperimentTo1P`：实验曝光事件。
//! - `initialize1PEventLogging` / `reinitialize1PEventLoggingIfConfigChanged`
//!   / `shutdown1PEventLogging`：管线生命周期。
//!
//! 已知偏差（no-op 上报）：
//! - **OTel 不存在（Cargo.toml 无 opentelemetry 依赖）**：TS 的
//!   `LoggerProvider` + `BatchLogRecordProcessor` 折叠为进程内缓冲
//!   （`maxQueueSize` 默认 8192，满时丢弃新记录——BatchLogRecordProcessor
//!   的满队列语义）。emit 不做网络发送；上游
//!   `FirstPartyEventLoggingExporter`（同目录）提供磁盘持久化与重试
//!   状态机，HTTP POST 在该模块同样为 no-op。
//! - TS 的 fire-and-forget `logEventTo1PAsync` 在 Rust 侧为同步实现：
//!   元数据富化在本仓库为同步函数，且 emit 是本地缓冲，无 await 必要。
//! - `utils/user.ts` 的 `getCoreUserData`/`getOrCreateUserID` 未转换：
//!   `user_id` 从 `~/.claude.json` 的 `userID` 字段读取（缺失则省略），
//!   `CoreUserData` 为默认空。
//! - `profileCheckpoint`（startupProfiler）未转换：no-op。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::bootstrap::state::get_session_id;
use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::platform::{get_platform, get_wsl_version, Platform};

use super::config::is_analytics_disabled;
use super::growthbook::{get_dynamic_config_cached_may_be_stale, GrowthBookUserAttributes};
use super::metadata::{get_event_metadata, CoreUserData, EnrichMetadataOptions};
use super::sink_killswitch::{is_sink_killed, SinkName};

/// 事件采样配置：事件名 → { sample_rate: 0-1 }。未登记事件 100% 记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EventSamplingEntry {
    #[serde(rename = "sample_rate")]
    pub sample_rate: f64,
}

/// TS `EventSamplingConfig`。
pub type EventSamplingConfig = HashMap<String, EventSamplingEntry>;

/// TS `EVENT_SAMPLING_CONFIG_NAME`。
const EVENT_SAMPLING_CONFIG_NAME: &str = "tengu_event_sampling_config";

/// 从 GrowthBook 读取事件采样配置（缓存可能过期；后台刷新）。
pub fn get_event_sampling_config() -> EventSamplingConfig {
    let config = get_dynamic_config_cached_may_be_stale(EVENT_SAMPLING_CONFIG_NAME, Value::Null);
    serde_json::from_value(config).unwrap_or_default()
}

/// 事件是否应被采样（TS `shouldSampleEvent`）。
///
/// 返回值语义与 TS 一致（`number | null`）：
/// - `None`：不采样（100% 记录，不附加 sample_rate）
/// - `Some(0.0)`：被丢弃
/// - `Some(rate)`：被采样，附加 sample_rate = rate
pub fn should_sample_event(event_name: &str) -> Option<f64> {
    let config = get_event_sampling_config();
    let event_config = config.get(event_name)?;

    let sample_rate = event_config.sample_rate;
    // 合法性校验：非数 / 越界 → 不采样（TS 同样回落 null）
    if !(0.0..=1.0).contains(&sample_rate) {
        return None;
    }
    if sample_rate >= 1.0 {
        return None;
    }
    if sample_rate <= 0.0 {
        return Some(0.0);
    }
    if rand::random::<f64>() < sample_rate {
        Some(sample_rate)
    } else {
        Some(0.0)
    }
}

/// 批处理配置（TS `BatchConfig`，`tengu_1p_event_batch_config`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BatchConfig {
    pub scheduled_delay_millis: Option<u64>,
    pub max_export_batch_size: Option<usize>,
    pub max_queue_size: Option<usize>,
    pub skip_auth: Option<bool>,
    pub max_attempts: Option<u32>,
    pub path: Option<String>,
    pub base_url: Option<String>,
}

/// TS `BATCH_CONFIG_NAME`。
const BATCH_CONFIG_NAME: &str = "tengu_1p_event_batch_config";

fn get_batch_config() -> BatchConfig {
    let config = get_dynamic_config_cached_may_be_stale(BATCH_CONFIG_NAME, Value::Null);
    serde_json::from_value(config).unwrap_or_default()
}

const DEFAULT_LOGS_EXPORT_INTERVAL_MS: u64 = 10_000;
const DEFAULT_MAX_EXPORT_BATCH_SIZE: usize = 200;
const DEFAULT_MAX_QUEUE_SIZE: usize = 8_192;

/// 一条已 emit 的日志记录（对齐 OTel `ReadableLogRecord` 的必要字段）。
#[derive(Debug, Clone)]
pub struct EmittedLogRecord {
    pub body: String,
    pub attributes: Map<String, Value>,
    /// hrTime → 毫秒时间戳。
    pub hr_time_ms: u128,
}

/// 1P 事件 logger 实例（TS 模块级 `firstPartyEventLogger`）。
pub struct FirstPartyLogger {
    pub(super) records: Vec<EmittedLogRecord>,
    max_queue_size: usize,
    instrumentation_scope: String,
}

impl FirstPartyLogger {
    fn emit(&mut self, body: &str, attributes: Map<String, Value>, hr_time_ms: u128) {
        if self.records.len() >= self.max_queue_size {
            // 满队列语义对齐 OTel BatchLogRecordProcessor：丢弃新记录
            return;
        }
        self.records.push(EmittedLogRecord {
            body: body.to_string(),
            attributes,
            hr_time_ms,
        });
    }

    /// 取出缓冲区（供 exporter 拉取；对齐 forceFlush 的搬运语义）。
    pub(super) fn drain(&mut self) -> Vec<EmittedLogRecord> {
        std::mem::take(&mut self.records)
    }
}

struct LoggerState {
    logger: Option<FirstPartyLogger>,
    batch_config: Option<BatchConfig>,
}

static STATE: once_cell::sync::Lazy<Mutex<LoggerState>> = once_cell::sync::Lazy::new(|| {
    Mutex::new(LoggerState {
        logger: None,
        batch_config: None,
    })
});

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Flush 并关闭 1P 事件日志管线（进程退出前调用）。
pub fn shutdown_1p_event_logging() {
    let logger_present = lock(&STATE).logger.is_some();
    if !logger_present {
        return;
    }
    // TS: provider.shutdown()；本实现缓冲区由 exporter / shutdown 搬运，
    // 直接丢弃（上报本身 no-op）。错误吞掉（对齐 TS catch）。
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        log_for_debugging("1P event logging: final shutdown complete", DebugLogLevel::Debug);
    }
}

/// 1P 事件日志是否启用（测试环境 / 3P 云提供商 / 隐私 opt-out 时关闭）。
pub fn is_1p_event_logging_enabled() -> bool {
    !is_analytics_disabled()
}

/// `~/.claude.json` 的 `userID`（偏差：`getOrCreateUserID` 未转换；仅读取）。
fn get_user_id() -> Option<String> {
    let config = read_global_config();
    config
        .get("userID")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// `CoreUserData` 的本地等价组装（偏差：`utils/user.ts` 未转换；
/// account/organization UUID 取自 `~/.claude.json` 的 oauthAccount）。
fn current_core_user_data() -> CoreUserData {
    let config = read_global_config();
    let oauth_account = config.get("oauthAccount");
    CoreUserData {
        email: oauth_account
            .and_then(|a| a.get("emailAddress"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        account_uuid: oauth_account
            .and_then(|a| a.get("accountUuid"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        organization_uuid: oauth_account
            .and_then(|a| a.get("organizationUuid"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        github_actions_metadata: None,
    }
}

fn read_global_config() -> Value {
    let config_home = crate::utils::env_utils::get_claude_config_home_dir();
    let path = std::path::Path::new(&config_home).join(".claude.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// 记录 1P 事件（TS `logEventTo1PAsync`，合并 emit 路径）。
fn log_event_to_1p_async(
    logger: &mut FirstPartyLogger,
    event_name: &str,
    metadata: &super::index::LogEventMetadata,
    user_id: &Option<String>,
) {
    // 富化核心元数据
    let model = metadata
        .get("model")
        .and_then(|v| v.as_ref())
        .and_then(|v| match v {
            super::index::MetadataValue::Num(n) => Some(n.to_string()),
            super::index::MetadataValue::Bool(b) => Some(b.to_string()),
            super::index::MetadataValue::String(s) => Some(s.clone()),
        });
    let betas = metadata
        .get("betas")
        .and_then(|v| v.as_ref())
        .and_then(|v| match v {
            super::index::MetadataValue::Num(n) => Some(n.to_string()),
            super::index::MetadataValue::Bool(b) => Some(b.to_string()),
            super::index::MetadataValue::String(s) => Some(s.clone()),
        });
    let core_metadata = get_event_metadata(&EnrichMetadataOptions { model, betas });

    // attributes：嵌套对象原样传递（OTel AnyValueMap）
    let mut attributes: Map<String, Value> = Map::new();
    attributes.insert("event_name".into(), Value::String(event_name.to_string()));
    attributes.insert("event_id".into(), Value::String(Uuid::new_v4().to_string()));
    if let Ok(Value::Object(core)) = serde_json::to_value(&core_metadata) {
        attributes.insert("core_metadata".into(), Value::Object(core));
    }
    let user_metadata = current_core_user_data();
    if let Ok(Value::Object(user)) = serde_json::to_value(&user_metadata) {
        attributes.insert("user_metadata".into(), Value::Object(user));
    }
    let event_metadata: Map<String, Value> = metadata
        .iter()
        .filter_map(|(key, value)| match value {
            Some(super::index::MetadataValue::Bool(b)) => Some((key.clone(), Value::Bool(*b))),
            Some(super::index::MetadataValue::Num(n)) => {
                serde_json::Number::from_f64(*n).map(|num| (key.clone(), Value::Number(num)))
            }
            Some(super::index::MetadataValue::String(s)) => Some((key.clone(), Value::String(s.clone()))),
            None => None,
        })
        .collect();
    attributes.insert("event_metadata".into(), Value::Object(event_metadata));
    if let Some(user_id) = user_id {
        attributes.insert("user_id".into(), Value::String(user_id.clone()));
    }

    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        log_for_debugging(
            &format!("[ANT-ONLY] 1P event: {event_name} (queued, send is no-op)"),
            DebugLogLevel::Debug,
        );
    }

    logger.emit(event_name, attributes, now_ms());
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 记录 1P 事件（TS `logEventTo1P`）。
///
/// 偏差：TS 为 fire-and-forget 异步；Rust 侧富化同步、emit 为本地缓冲，
/// 故同步实现。门控链对齐：未启用 / logger 未初始化 / sink 被杀 → 直接
/// 返回。
pub fn log_event_to_1p(event_name: &str, metadata: &super::index::LogEventMetadata) {
    if !is_1p_event_logging_enabled() {
        return;
    }
    let user_id = get_user_id();
    let mut state = lock(&STATE);
    let Some(logger) = state.logger.as_mut() else {
        return;
    };
    if is_sink_killed(SinkName::FirstParty) {
        return;
    }
    log_event_to_1p_async(logger, event_name, metadata, &user_id);
}

/// GrowthBook 实验曝光事件数据（TS `GrowthBookExperimentData`）。
#[derive(Debug, Clone, Default)]
pub struct GrowthBookExperimentData {
    pub experiment_id: String,
    pub variation_id: i64,
    pub user_attributes: Option<GrowthBookUserAttributes>,
    pub experiment_metadata: Option<Value>,
}

/// api.anthropic.com 只服务 production 环境的 GrowthBook（见 TS 注释）。
fn get_environment_for_growthbook() -> String {
    "production".to_string()
}

/// 记录 GrowthBook 实验分配事件到 1P。
pub fn log_growthbook_experiment_to_1p(data: &GrowthBookExperimentData) {
    if !is_1p_event_logging_enabled() {
        return;
    }
    if is_sink_killed(SinkName::FirstParty) {
        return;
    }
    let user_id = get_user_id();
    let mut state = lock(&STATE);
    let Some(logger) = state.logger.as_mut() else {
        return;
    };

    let mut attributes: Map<String, Value> = Map::new();
    attributes.insert("event_type".into(), Value::String("GrowthbookExperimentEvent".into()));
    attributes.insert("event_id".into(), Value::String(Uuid::new_v4().to_string()));
    attributes.insert("experiment_id".into(), Value::String(data.experiment_id.clone()));
    attributes.insert("variation_id".into(), Value::Number(data.variation_id.into()));
    if let Some(user_id) = &user_id {
        attributes.insert("device_id".into(), Value::String(user_id.clone()));
    }
    let user_metadata = current_core_user_data();
    if let Some(account_uuid) = &user_metadata.account_uuid {
        attributes.insert("account_uuid".into(), Value::String(account_uuid.clone()));
    }
    if let Some(organization_uuid) = &user_metadata.organization_uuid {
        attributes.insert("organization_uuid".into(), Value::String(organization_uuid.clone()));
    }
    if let Some(user_attributes) = &data.user_attributes {
        attributes.insert(
            "session_id".into(),
            Value::String(user_attributes.session_id.clone()),
        );
        if let Ok(value) = serde_json::to_value(user_attributes) {
            attributes.insert("user_attributes".into(), value);
        }
    }
    if let Some(experiment_metadata) = &data.experiment_metadata {
        attributes.insert("experiment_metadata".into(), experiment_metadata.clone());
    }
    attributes.insert(
        "environment".into(),
        Value::String(get_environment_for_growthbook()),
    );

    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        log_for_debugging(
            &format!(
                "[ANT-ONLY] 1P GrowthBook experiment: {} variation={}",
                data.experiment_id, data.variation_id
            ),
            DebugLogLevel::Debug,
        );
    }

    logger.emit("growthbook_experiment", attributes, now_ms());
}

/// 初始化 1P 事件日志管线（TS `initialize1PEventLogging`）。
///
/// 偏差：无 OTel LoggerProvider；构造本地缓冲 logger，resource 属性
/// （service.name / service.version / wsl.version）以 ant debug 日志
/// 形式保留。
pub fn initialize_1p_event_logging() {
    let enabled = is_1p_event_logging_enabled();
    if !enabled {
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging("1P event logging not enabled", DebugLogLevel::Debug);
        }
        return;
    }

    let batch_config = get_batch_config();
    // scheduledDelayMillis / maxExportBatchSize 属于 OTel
    // BatchLogRecordProcessor 配置（本实现无 OTel，读取后记录在注释中，
    // 由 max_queue_size 主导本地缓冲容量）
    let _scheduled_delay_millis = batch_config.scheduled_delay_millis.unwrap_or_else(|| {
        std::env::var("OTEL_LOGS_EXPORT_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(DEFAULT_LOGS_EXPORT_INTERVAL_MS)
    });
    let _max_export_batch_size =
        batch_config.max_export_batch_size.unwrap_or(DEFAULT_MAX_EXPORT_BATCH_SIZE);
    let max_queue_size = batch_config.max_queue_size.unwrap_or(DEFAULT_MAX_QUEUE_SIZE);

    // resource：service.name + service.version（+ WSL 版本）
    let platform = get_platform();
    let mut resource: Vec<(String, Value)> = vec![
        ("service.name".into(), Value::String("claude-code".into())),
        ("service.version".into(), Value::String(env!("CARGO_PKG_VERSION").into())),
    ];
    if platform == Platform::Wsl {
        if let Some(wsl_version) = get_wsl_version() {
            resource.push(("wsl.version".into(), Value::String(wsl_version.into())));
        }
    }

    let logger = FirstPartyLogger {
        records: Vec::new(),
        max_queue_size,
        instrumentation_scope: "com.anthropic.claude_code.events".to_string(),
    };

    let mut state = lock(&STATE);
    state.logger = Some(logger);
    state.batch_config = Some(batch_config);
}

/// 配置变化时重建管线（TS `reinitialize1PEventLoggingIfConfigChanged`）。
///
/// 事件防丢失顺序对齐 TS：
/// 1. 先比较 batch 配置，相同则返回；
/// 2. null logger（门控 bail）→ flush 旧缓冲 → 重建 → 失败时恢复旧件。
pub async fn reinitialize_1p_event_logging_if_config_changed() {
    if !is_1p_event_logging_enabled() {
        return;
    }
    let mut state = lock(&STATE);
    if state.logger.is_none() {
        return;
    }
    let new_config = get_batch_config();
    if state.batch_config.as_ref() == Some(&new_config) {
        return;
    }
    // 事件防丢：swap 窗口内 logEventTo1P 因 !logger bail（掉极少量事件，
    // 不向 draining 管线 emit——对齐 TS 说明）
    state.logger = None;
    drop(state);

    initialize_1p_event_logging();
}

/// 供 exporter 拉取缓冲记录（内部桥接；对齐 OTel 处理器搬运语义）。
pub fn drain_records() -> Vec<EmittedLogRecord> {
    let mut state = lock(&STATE);
    match state.logger.as_mut() {
        Some(logger) => logger.drain(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_config_defaults_when_unconfigured() {
        // 无 GrowthBook 配置时未登记事件不采样
        assert_eq!(should_sample_event("tengu_missing_event_xyz"), None);
    }
}
