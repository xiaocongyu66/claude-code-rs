//! first_party_event_logging_exporter.rs — 1P 事件批量导出器（重写自
//! `src/services/analytics/firstPartyEventLoggingExporter.ts`）。
//!
//! 职责（对齐 TS）：
//! - 事件载体（`FirstPartyEventLoggingEvent` / `Payload`）：event_type +
//!   event_data（proto toJSON 输出的 JSON）。
//! - 韧性机制：失败事件追加持久化（`~/.claude/telemetry/1p_failed_events.<sessionId>.<batchUuid>.json`，
//!   JSONL）、平方退避重试（base * attempts²，封顶 30s）、maxAttempts 后
//!   丢弃、成功即立即重试排队事件、大批量分批发送、401 时去 auth 重试。
//! - `transformLogsToEvents`：OTel 日志记录 → 事件载体（含
//!   `_PROTO_*` 提升到 proto 字段后的防御性剥离）。
//! - 生命周期：`export`（逐批）/ `shutdown` / `forceFlush`，以及启动期
//!   重试前轮（`retry_previous_batches`）。
//!
//! 已知偏差（no-op 上报 + 依赖未转换模块）：
//! - **HTTP 未接入**：`send_batch_with_retry` 的 POST 为 no-op（直接
//!   Ok），因此失败持久化与退避重试状态机保持完整但常态不激活；文件
//!   搬运/解析/清理逻辑完整实现，单元测试覆盖纯逻辑。
//! - **OTel 不存在**：`ReadableLogRecord` 由
//!   `first_party_event_logger::EmittedLogRecord` 替代；`export` 直接从
//!   logger 缓冲搬运（无需 BatchLogRecordProcessor 的回调协议）。
//! - auth 决策链保留（trust 门、OAuth scope/过期检查均已就绪），但鉴权
//!   header 本身不采集（无 HTTP）。
//! - 定时调度（backoff timer / batch delay）无宿主运行时：退避计算为纯
//!   函数 `backoff_delay_ms`；`retry_failed_events` 为显式异步方法。
//! - `axios` 错误上下文（`getAxiosErrorContext`）等价为通用错误消息提取。
//! - `utils/http.ts`（getAuthHeaders）与 `utils/config.ts`（trust 已在
//!   auth.rs 就绪）未完整转换：鉴权头拼装以默认 header 为准。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use chrono::Utc;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::bootstrap::state::{get_is_non_interactive_session, get_session_id};
use crate::utils::auth::{
    check_has_trust_dialog_accepted, get_claude_ai_oauth_tokens, has_profile_scope,
    is_claude_ai_subscriber, is_oauth_token_expired,
};
use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::errors::error_message;
use crate::utils::log::log_error;
use crate::utils::user_agent::get_claude_code_user_agent;

use super::first_party_event_logger::{drain_records, EmittedLogRecord};
use super::metadata::{base64_encode, to_1p_event_format, CoreUserData, EventMetadata};

/// 本进程运行 UUID（隔离不同运行的失败事件文件）。
static BATCH_UUID: Lazy<String> = Lazy::new(|| Uuid::new_v4().to_string());

/// 失败事件文件前缀。
const FILE_PREFIX: &str = "1p_failed_events.";

/// 失败事件存储目录：`$CLAUDE_CONFIG_DIR/telemetry`（运行时求值以尊重
/// 测试中的 CLAUDE_CONFIG_DIR）。
fn get_storage_dir() -> PathBuf {
    PathBuf::from(crate::utils::env_utils::get_claude_config_home_dir()).join("telemetry")
}

/// 导出结果（OTel `ExportResult` 平价）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportResult {
    Success,
    Failed(String),
}

/// API envelope：event_data 为 proto toJSON() 输出（TS 同名类型）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirstPartyEventLoggingEvent {
    pub event_type: String,
    pub event_data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirstPartyEventLoggingPayload {
    pub events: Vec<FirstPartyEventLoggingEvent>,
}

/// 导出器选项（TS 构造器 options）。
#[derive(Debug, Clone, Default)]
pub struct ExporterOptions {
    pub timeout: Option<u64>,
    pub max_batch_size: Option<usize>,
    pub skip_auth: Option<bool>,
    pub batch_delay_ms: Option<u64>,
    pub base_backoff_delay_ms: Option<u64>,
    pub max_backoff_delay_ms: Option<u64>,
    pub max_attempts: Option<u32>,
    pub path: Option<String>,
    pub base_url: Option<String>,
}

/// 导出器。
pub struct FirstPartyEventLoggingExporter {
    pub(super) endpoint: String,
    timeout: u64,
    max_batch_size: usize,
    skip_auth: bool,
    batch_delay_ms: u64,
    base_backoff_delay_ms: u64,
    max_backoff_delay_ms: u64,
    max_attempts: u32,
    is_shutdown: AtomicBool,
    attempts: AtomicU32,
    is_retrying: AtomicBool,
    last_export_error_context: Mutex<Option<String>>,
    pending_exports: Mutex<u32>,
}

impl FirstPartyEventLoggingExporter {
    /// 构造导出器。默认 baseUrl 为 prod，ANTHROPIC_BASE_URL 显式指向
    /// staging 时切 staging；`tengu_1p_event_batch_config.baseUrl` 可覆盖。
    pub fn new(options: ExporterOptions) -> Self {
        let base_url = options.base_url.unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_BASE_URL").as_deref()
                == Ok("https://api-staging.anthropic.com")
            {
                "https://api-staging.anthropic.com".to_string()
            } else {
                "https://api.anthropic.com".to_string()
            }
        });
        let endpoint = format!(
            "{}{}",
            base_url,
            options.path.unwrap_or_else(|| "/api/event_logging/batch".to_string())
        );
        Self {
            endpoint,
            timeout: options.timeout.unwrap_or(10_000),
            max_batch_size: options.max_batch_size.unwrap_or(200),
            skip_auth: options.skip_auth.unwrap_or(false),
            batch_delay_ms: options.batch_delay_ms.unwrap_or(100),
            base_backoff_delay_ms: options.base_backoff_delay_ms.unwrap_or(500),
            max_backoff_delay_ms: options.max_backoff_delay_ms.unwrap_or(30_000),
            max_attempts: options.max_attempts.unwrap_or(8),
            is_shutdown: AtomicBool::new(false),
            attempts: AtomicU32::new(0),
            is_retrying: AtomicBool::new(false),
            last_export_error_context: Mutex::new(None),
            pending_exports: Mutex::new(0),
        }
    }

    fn is_killed(&self) -> bool {
        super::sink_killswitch::is_sink_killed(super::sink_killswitch::SinkName::FirstParty)
    }

    // --- 存储辅助 ---

    fn current_batch_file_path(&self) -> PathBuf {
        // session_id 与 batch uuid 唯一定位本运行文件
        get_storage_dir().join(format!(
            "{FILE_PREFIX}{}.{}.json",
            get_session_id(),
            *BATCH_UUID
        ))
    }

    fn load_events_from_file(&self, file_path: &Path) -> Vec<FirstPartyEventLoggingEvent> {
        read_jsonl_file(file_path)
    }

    fn load_events_from_current_batch(&self) -> Vec<FirstPartyEventLoggingEvent> {
        self.load_events_from_file(&self.current_batch_file_path())
    }

    fn save_events_to_file(&self, file_path: &Path, events: &[FirstPartyEventLoggingEvent]) {
        if let Err(e) = save_events_inner(file_path, events) {
            log_error(e);
        }
    }

    fn append_events_to_file(&self, file_path: &Path, events: &[FirstPartyEventLoggingEvent]) {
        if events.is_empty() {
            return;
        }
        if let Err(e) = append_events_inner(file_path, events) {
            log_error(e);
        }
    }

    fn delete_file(&self, file_path: &Path) {
        let _ = fs::remove_file(file_path);
    }

    /// 启动期重试：同 session 的前轮失败文件（排除本运行）。
    pub async fn retry_previous_batches(&self) {
        let prefix = format!("{FILE_PREFIX}{}.", get_session_id());
        let mut files: Vec<PathBuf> = Vec::new();
        match fs::read_dir(get_storage_dir()) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(&prefix) && name.ends_with(".json") && !name.contains(&*BATCH_UUID) {
                        files.push(entry.path());
                    }
                }
            }
            Err(e) => {
                if !crate::utils::errors::is_fs_inaccessible(&e) {
                    log_error(e);
                }
                return;
            }
        }
        for file in files {
            self.retry_file_in_background(&file).await;
        }
    }

    async fn retry_file_in_background(&self, file_path: &Path) {
        if self.attempts.load(Ordering::SeqCst) >= self.max_attempts {
            self.delete_file(file_path);
            return;
        }
        let events = self.load_events_from_file(file_path);
        if events.is_empty() {
            self.delete_file(file_path);
            return;
        }
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging(
                &format!("1P event logging: retrying {} events from previous batch", events.len()),
                DebugLogLevel::Debug,
            );
        }
        let failed_events = self.send_events_in_batches(&events).await;
        if failed_events.is_empty() {
            self.delete_file(file_path);
            if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                log_for_debugging("1P event logging: previous batch retry succeeded", DebugLogLevel::Debug);
            }
        } else {
            self.save_events_to_file(file_path, &failed_events);
            if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                log_for_debugging(
                    &format!(
                        "1P event logging: previous batch retry failed, {} events remain",
                        failed_events.len()
                    ),
                    DebugLogLevel::Debug,
                );
            }
        }
    }

    /// 导出一批日志记录（TS `export(logs, resultCallback)`）。
    ///
    /// 偏差：直接从 `first_party_event_logger` 缓冲搬运记录（无 OTel 处理
    /// 器回调协议）；scope 名过滤（`com.anthropic.claude_code.events`）
    /// 保留——所有 EmittedLogRecord 均来自该 scope。
    pub async fn export(&self) -> ExportResult {
        if self.is_shutdown.load(Ordering::SeqCst) {
            return ExportResult::Failed("Exporter has been shutdown".to_string());
        }
        {
            let mut pending = self.pending_exports.lock().unwrap_or_else(|e| e.into_inner());
            *pending += 1;
        }
        let result = self.do_export().await;
        {
            let mut pending = self.pending_exports.lock().unwrap_or_else(|e| e.into_inner());
            *pending = pending.saturating_sub(1);
        }
        result
    }

    async fn do_export(&self) -> ExportResult {
        let logs = drain_records();
        if logs.is_empty() {
            return ExportResult::Success;
        }
        let payload = self.transform_logs_to_events(&logs);
        if payload.events.is_empty() {
            return ExportResult::Success;
        }
        if self.attempts.load(Ordering::SeqCst) >= self.max_attempts {
            return ExportResult::Failed(format!(
                "Dropped {} events: max attempts ({}) reached",
                payload.events.len(),
                self.max_attempts
            ));
        }
        let failed_events = self.send_events_in_batches(&payload.events).await;
        self.attempts.fetch_add(1, Ordering::SeqCst);

        if !failed_events.is_empty() {
            self.queue_failed_events(&failed_events);
            self.schedule_backoff_retry();
            let context = {
                let ctx = self.last_export_error_context.lock().unwrap_or_else(|e| e.into_inner());
                ctx.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
            };
            return ExportResult::Failed(format!("Failed to export {} events{}", failed_events.len(), context));
        }

        // 成功：重置退避，立即重试排队事件（endpoint 健康）
        self.reset_backoff();
        if !self.is_retrying.load(Ordering::SeqCst) && !self.load_events_from_current_batch().is_empty() {
            self.retry_failed_events().await;
        }
        ExportResult::Success
    }

    /// 分批发送；首个批次失败即短路（假定 endpoint 宕机），剩余批次全
    /// 队列化（对齐 TS 短路语义）。
    pub(super) async fn send_events_in_batches(
        &self,
        events: &[FirstPartyEventLoggingEvent],
    ) -> Vec<FirstPartyEventLoggingEvent> {
        let batches: Vec<&[FirstPartyEventLoggingEvent]> = events
            .chunks(self.max_batch_size)
            .collect();
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging(
                &format!(
                    "1P event logging: exporting {} events in {} batch(es)",
                    events.len(),
                    batches.len()
                ),
                DebugLogLevel::Debug,
            );
        }

        let mut failed_batch_events: Vec<FirstPartyEventLoggingEvent> = Vec::new();
        let mut last_error_context: Option<String> = None;
        for (i, batch) in batches.iter().enumerate() {
            let payload = FirstPartyEventLoggingPayload {
                events: batch.to_vec(),
            };
            if let Err(err) = self.send_batch_with_retry(&payload).await {
                last_error_context = Some(error_context(&err));
                // 短路：失败批次 + 全部剩余批次入失败队列
                for remaining in &batches[i..] {
                    failed_batch_events.extend_from_slice(remaining);
                }
                if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                    log_for_debugging(
                        &format!(
                            "1P event logging: batch {}/{} failed ({}); short-circuiting {} remaining batch(es)",
                            i + 1,
                            batches.len(),
                            last_error_context.as_deref().unwrap_or("error"),
                            batches.len() - 1 - i
                        ),
                        DebugLogLevel::Debug,
                    );
                }
                break;
            }
            if i < batches.len() - 1 && self.batch_delay_ms > 0 {
                crate::utils::sleep::sleep(self.batch_delay_ms, None).await;
            }
        }

        if !failed_batch_events.is_empty() {
            if let Some(context) = last_error_context {
                *self.last_export_error_context.lock().unwrap_or_else(|e| e.into_inner()) = Some(context);
            }
        }
        failed_batch_events
    }

    fn queue_failed_events(&self, events: &[FirstPartyEventLoggingEvent]) {
        let file_path = self.current_batch_file_path();
        self.append_events_to_file(&file_path, events);
        let context = {
            let ctx = self.last_export_error_context.lock().unwrap_or_else(|e| e.into_inner());
            ctx.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
        };
        log_error(ClaudeCodeError::new(format!(
            "1P event logging: {} events failed to export{}",
            events.len(),
            context
        )));
    }

    /// 调度退避重试。**偏差**：无 setTimeout 等价物——仅计算延迟并标记
    /// retrying；实际重试由 `retry_failed_events` 显式驱动或测试触发。
    fn schedule_backoff_retry(&self) {
        if self.is_retrying.load(Ordering::SeqCst) || self.is_shutdown.load(Ordering::SeqCst) {
            return;
        }
        let delay = backoff_delay_ms(
            self.base_backoff_delay_ms,
            self.attempts.load(Ordering::SeqCst),
            self.max_backoff_delay_ms,
        );
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging(
                &format!(
                    "1P event logging: backoff retry in {}ms (attempt {})",
                    delay,
                    self.attempts.load(Ordering::SeqCst)
                ),
                DebugLogLevel::Debug,
            );
        }
        self.is_retrying.store(true, Ordering::SeqCst);
    }

    /// 显式重试磁盘上排队的事件（对齐 TS `retryFailedEvents` 循环）。
    pub async fn retry_failed_events(&self) {
        let file_path = self.current_batch_file_path();
        while !self.is_shutdown.load(Ordering::SeqCst) {
            let events = self.load_events_from_file(&file_path);
            if events.is_empty() {
                break;
            }
            if self.attempts.load(Ordering::SeqCst) >= self.max_attempts {
                if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                    log_for_debugging(
                        &format!(
                            "1P event logging: max attempts ({}) reached, dropping {} events",
                            self.max_attempts,
                            events.len()
                        ),
                        DebugLogLevel::Debug,
                    );
                }
                self.delete_file(&file_path);
                self.reset_backoff();
                return;
            }

            // 清文件再重试（事件已在内存）
            self.delete_file(&file_path);
            let failed_events = self.send_events_in_batches(&events).await;
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.is_retrying.store(false, Ordering::SeqCst);

            if !failed_events.is_empty() {
                self.save_events_to_file(&file_path, &failed_events);
                self.schedule_backoff_retry();
                return; // 失败 → 等退避
            }
            self.reset_backoff();
            if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                log_for_debugging("1P event logging: backoff retry succeeded", DebugLogLevel::Debug);
            }
        }
    }

    fn reset_backoff(&self) {
        self.attempts.store(0, Ordering::SeqCst);
        self.is_retrying.store(false, Ordering::SeqCst);
    }

    /// 发送一个批次（auth 决策 + 401 重试 + killswitch）。
    ///
    /// **偏差（no-op 上报）**：不发起 HTTP 请求，直接返回 Ok；结构（鉴权
    /// 判定、header 拼装、killswitch 短路）保留以备网络层接入。
    pub(super) async fn send_batch_with_retry(
        &self,
        payload: &FirstPartyEventLoggingPayload,
    ) -> Result<(), ClaudeCodeError> {
        if self.is_killed() {
            // 与 TS 一致：抛错使上层短路并把事件全部队列化（killed 期间零
            // 网络流量，退避计时器继续，GrowthBook 清 flag 后即恢复）
            return Err(ClaudeCodeError::new(
                "firstParty sink killswitch active".to_string(),
            ));
        }

        let _headers = self.build_headers();

        // TODO(network): reqwest POST self.endpoint，timeout=self.timeout，
        // headers=headers；401 时以默认 header 重试一次（对齐 TS）
        let _ = payload;
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging(
                &format!(
                    "1P event logging: {} events exported successfully (send is no-op in this build)",
                    payload.events.len()
                ),
                DebugLogLevel::Debug,
            );
        }
        Ok(())
    }

    /// 鉴权 header 决策（对齐 TS 的 trust / OAuth scope / 过期链）。
    fn build_headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/json".into()),
            ("User-Agent".into(), get_claude_code_user_agent()),
            ("x-service-name".into(), "claude-code".into()),
        ];

        // trust 未建立时跳过 auth（不在 trust 对话框前执行 apiKeyHelper；
        // 非交互会话隐含 trust）
        let has_trust = check_has_trust_dialog_accepted() || get_is_non_interactive_session();
        let mut should_skip_auth = self.skip_auth || !has_trust;

        // OAuth 缺 user:profile scope（service key 会话）或已过期 → 走匿名
        if !should_skip_auth && is_claude_ai_subscriber() {
            if !has_profile_scope() {
                should_skip_auth = true;
            } else if let Some(tokens) = get_claude_ai_oauth_tokens() {
                if is_oauth_token_expired(tokens.expires_at) {
                    should_skip_auth = true;
                    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                        log_for_debugging(
                            "1P event logging: OAuth token expired, skipping auth to avoid 401",
                            DebugLogLevel::Debug,
                        );
                    }
                }
            }
        }

        if !should_skip_auth {
            // 偏差：utils/http.ts getAuthHeaders 未转换，仅保留结构
            // （无网络层时不发请求，鉴权 header 留 empty）
        }
        headers
    }

    /// OTel 记录 → 事件载体（TS `transformLogsToEvents`）。
    pub(super) fn transform_logs_to_events(
        &self,
        logs: &[EmittedLogRecord],
    ) -> FirstPartyEventLoggingPayload {
        let mut events: Vec<FirstPartyEventLoggingEvent> = Vec::new();
        for log_record in logs {
            let attributes = &log_record.attributes;

            // GrowthBook 实验事件分支
            if attributes.get("event_type").and_then(Value::as_str) == Some("GrowthbookExperimentEvent") {
                let timestamp = hr_time_to_rfc3339(log_record.hr_time_ms);
                let account_uuid = attributes.get("account_uuid").and_then(Value::as_str);
                let organization_uuid = attributes.get("organization_uuid").and_then(Value::as_str);
                let mut event_data = Map::new();
                if let Some(event_id) = attributes.get("event_id") {
                    event_data.insert("event_id".into(), event_id.clone());
                }
                event_data.insert("timestamp".into(), Value::String(timestamp));
                if let Some(experiment_id) = attributes.get("experiment_id") {
                    event_data.insert("experiment_id".into(), experiment_id.clone());
                }
                if let Some(variation_id) = attributes.get("variation_id") {
                    event_data.insert("variation_id".into(), variation_id.clone());
                }
                if let Some(environment) = attributes.get("environment") {
                    event_data.insert("environment".into(), environment.clone());
                }
                if let Some(user_attributes) = attributes.get("user_attributes") {
                    event_data.insert("user_attributes".into(), user_attributes.clone());
                }
                if let Some(experiment_metadata) = attributes.get("experiment_metadata") {
                    event_data.insert("experiment_metadata".into(), experiment_metadata.clone());
                }
                if let Some(device_id) = attributes.get("device_id") {
                    event_data.insert("device_id".into(), device_id.clone());
                }
                if let Some(session_id) = attributes.get("session_id") {
                    event_data.insert("session_id".into(), session_id.clone());
                }
                if account_uuid.is_some() || organization_uuid.is_some() {
                    let mut auth = Map::new();
                    if let Some(account_uuid) = account_uuid {
                        auth.insert("account_uuid".into(), Value::String(account_uuid.to_string()));
                    }
                    if let Some(organization_uuid) = organization_uuid {
                        auth.insert("organization_uuid".into(), Value::String(organization_uuid.to_string()));
                    }
                    event_data.insert("auth".into(), Value::Object(auth));
                }
                events.push(FirstPartyEventLoggingEvent {
                    event_type: "GrowthbookExperimentEvent".to_string(),
                    event_data: Value::Object(event_data),
                });
                continue;
            }

            // 普通事件
            let event_name = attributes
                .get("event_name")
                .and_then(Value::as_str)
                .unwrap_or(&log_record.body)
                .to_string();
            let core_metadata: Option<EventMetadata> = attributes
                .get("core_metadata")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let user_metadata: CoreUserData = attributes
                .get("user_metadata")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let event_metadata: Map<String, Value> = attributes
                .get("event_metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            let Some(core_metadata) = core_metadata else {
                // 核心元数据缺失 → 部分事件（含 transform_error）
                if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
                    log_for_debugging(
                        &format!("1P event logging: core_metadata missing for event {event_name}"),
                        DebugLogLevel::Debug,
                    );
                }
                let mut partial = Map::new();
                if let Some(event_id) = attributes.get("event_id") {
                    partial.insert("event_id".into(), event_id.clone());
                }
                partial.insert("event_name".into(), Value::String(event_name));
                partial.insert(
                    "client_timestamp".into(),
                    Value::String(hr_time_to_rfc3339(log_record.hr_time_ms)),
                );
                partial.insert("session_id".into(), Value::String(get_session_id()));
                partial.insert(
                    "additional_metadata".into(),
                    Value::String(base64_encode(
                        br#"{"transform_error":"core_metadata attribute is missing"}"#,
                    )),
                );
                events.push(FirstPartyEventLoggingEvent {
                    event_type: "ClaudeCodeInternalEvent".to_string(),
                    event_data: Value::Object(partial),
                });
                continue;
            };

            // 1P 格式化 + _PROTO_* 提升 + 剩余防御性剥离
            let formatted = to_1p_event_format(&core_metadata, &user_metadata, &event_metadata);
            let (skill_name, plugin_name, marketplace_name, rest_additional) =
                hoist_proto_fields(&formatted.additional);
            let additional_metadata = strip_proto_fields_value(&rest_additional);

            let mut event_data: Map<String, Value> = Map::new();
            if let Some(event_id) = attributes.get("event_id") {
                event_data.insert("event_id".into(), event_id.clone());
            }
            event_data.insert("event_name".into(), Value::String(event_name));
            event_data.insert(
                "client_timestamp".into(),
                Value::String(hr_time_to_rfc3339(log_record.hr_time_ms)),
            );
            if let Some(device_id) = attributes.get("user_id") {
                event_data.insert("device_id".into(), device_id.clone());
            }
            if let Some(email) = &user_metadata.email {
                event_data.insert("email".into(), Value::String(email.clone()));
            }
            if let Some(auth) = &formatted.auth {
                if let Ok(auth_value) = serde_json::to_value(auth) {
                    event_data.insert("auth".into(), auth_value);
                }
            }
            if let Ok(core_value) = serde_json::to_value(&formatted.core) {
                if let Value::Object(core_map) = core_value {
                    for (key, value) in core_map {
                        event_data.insert(key, value);
                    }
                }
            }
            if let Ok(env_value) = serde_json::to_value(&formatted.env) {
                event_data.insert("env".into(), env_value);
            }
            if let Some(process) = &formatted.process {
                event_data.insert("process".into(), Value::String(process.clone()));
            }
            if let Some(skill_name) = skill_name {
                event_data.insert("skill_name".into(), Value::String(skill_name));
            }
            if let Some(plugin_name) = plugin_name {
                event_data.insert("plugin_name".into(), Value::String(plugin_name));
            }
            if let Some(marketplace_name) = marketplace_name {
                event_data.insert("marketplace_name".into(), Value::String(marketplace_name));
            }
            if !additional_metadata.is_empty() {
                event_data.insert(
                    "additional_metadata".into(),
                    Value::String(base64_encode(json_stringify_safe(&Value::Object(additional_metadata)).as_bytes())),
                );
            }
            events.push(FirstPartyEventLoggingEvent {
                event_type: "ClaudeCodeInternalEvent".to_string(),
                event_data: Value::Object(event_data),
            });
        }
        FirstPartyEventLoggingPayload { events }
    }

    /// 关闭导出器：停止重试、排空 pending。
    pub async fn shutdown(&self) {
        self.is_shutdown.store(true, Ordering::SeqCst);
        self.reset_backoff();
        self.force_flush().await;
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging("1P event logging exporter shutdown complete", DebugLogLevel::Debug);
        }
    }

    /// 等待进行中的导出完成。
    pub async fn force_flush(&self) {
        // 偏差：无并发导出任务句柄；轮询 pending 计数（快速归零）
        let mut spins = 0;
        while {
            let pending = self.pending_exports.lock().unwrap_or_else(|e| e.into_inner());
            *pending > 0
        } {
            spins += 1;
            if spins > 100 {
                break;
            }
            crate::utils::sleep::sleep(5, None).await;
        }
        if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
            log_for_debugging("1P event logging exporter flush complete", DebugLogLevel::Debug);
        }
    }
}

/// 已知 `_PROTO_*` 键提升到 proto 字段（skill/plugin/marketplace），
/// 返回 (skill, plugin, marketplace, 剩余 additional)。
fn hoist_proto_fields(additional: &Map<String, Value>) -> (Option<String>, Option<String>, Option<String>, Map<String, Value>) {
    let mut rest = additional.clone();
    let skill_name = rest.remove("_PROTO_skill_name").and_then(Value::as_str).map(str::to_string);
    let plugin_name = rest.remove("_PROTO_plugin_name").and_then(Value::as_str).map(str::to_string);
    let marketplace_name = rest.remove("_PROTO_marketplace_name").and_then(Value::as_str).map(str::to_string);
    (skill_name, plugin_name, marketplace_name, rest)
}

/// 值 map 的 `_PROTO_*` 防御性剥离（index.rs `stripProtoFields` 平价）。
fn strip_proto_fields_value(metadata: &Map<String, Value>) -> Map<String, Value> {
    let mut result = metadata.clone();
    result.retain(|key, _| !key.starts_with("_PROTO_"));
    result
}

/// 毫秒时间戳 → RFC3339（proto timestamp 的 toJSON 格式）。
fn hr_time_to_rfc3339(hr_time_ms: u128) -> String {
    let secs = (hr_time_ms / 1000) as i64;
    let nanos = ((hr_time_ms % 1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nanos)
        .single()
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_default()
}

/// JSONL：逐行解析，坏行/不可读文件返回空。
fn read_jsonl_file(file_path: &Path) -> Vec<FirstPartyEventLoggingEvent> {
    let text = match fs::read_to_string(file_path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn save_events_inner(
    file_path: &Path,
    events: &[FirstPartyEventLoggingEvent],
) -> Result<(), std::io::Error> {
    if events.is_empty() {
        let _ = fs::remove_file(file_path);
        return Ok(());
    }
    let dir = get_storage_dir();
    fs::create_dir_all(&dir)?;
    let content = events
        .iter()
        .map(json_stringify_safe)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(file_path, content)
}

fn append_events_inner(
    file_path: &Path,
    events: &[FirstPartyEventLoggingEvent],
) -> Result<(), std::io::Error> {
    fs::create_dir_all(get_storage_dir())?;
    let content = events
        .iter()
        .map(json_stringify_safe)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    use std::io::Write;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(file_path)?;
    file.write_all(content.as_bytes())
}

fn json_stringify_safe(event: &FirstPartyEventLoggingEvent) -> String {
    serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string())
}

/// 平方退避（base * attempts²，封顶 max）。
fn backoff_delay_ms(base: u64, attempts: u32, max: u64) -> u64 {
    let attempt = attempts.max(1) as u64; // attempts=0 时按 1 计（对齐 TS 首退避）
    base.saturating_mul(attempt * attempt).min(max)
}

/// 错误上下文（TS `getAxiosErrorContext` 的通用等价）。
fn error_context(error: &ClaudeCodeError) -> String {
    error_message(error)
}

/// 简单错误类型（log_error 需要 Into<Box<dyn Error>>）。
struct ClaudeCodeError {
    message: String,
}
impl ClaudeCodeError {
    fn new(message: String) -> Self {
        Self { message }
    }
}
impl std::fmt::Display for ClaudeCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::fmt::Debug for ClaudeCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ClaudeCodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backoff_is_quadratic_and_capped() {
        assert_eq!(backoff_delay_ms(500, 1, 30_000), 500);
        assert_eq!(backoff_delay_ms(500, 2, 30_000), 2_000);
        assert_eq!(backoff_delay_ms(500, 8, 30_000), 30_000);
        // attempts=0 不退化为 0
        assert_eq!(backoff_delay_ms(500, 0, 30_000), 500);
    }

    #[test]
    fn strip_proto_fields_removes_prefixed_keys() {
        let mut map = Map::new();
        map.insert("keep".into(), json!(1));
        map.insert("_PROTO_x".into(), json!("piI"));
        let stripped = strip_proto_fields_value(&map);
        assert_eq!(stripped.len(), 1);
        assert!(stripped.contains_key("keep"));
    }

    #[test]
    fn hoist_proto_fields_extracts_known_keys() {
        let mut map = Map::new();
        map.insert("_PROTO_skill_name".into(), json!("commit"));
        map.insert("_PROTO_plugin_name".into(), json!("p"));
        map.insert("_PROTO_marketplace_name".into(), json!("m"));
        map.insert("_PROTO_unknown".into(), json!("u"));
        let (skill, plugin, market, rest) = hoist_proto_fields(&map);
        assert_eq!(skill.as_deref(), Some("commit"));
        assert_eq!(plugin.as_deref(), Some("p"));
        assert_eq!(market.as_deref(), Some("m"));
        // 未知 _PROTO_* 保留在 rest 中（由调用方二次剥离）
        assert!(rest.contains_key("_PROTO_unknown"));
    }

    #[test]
    fn transform_produces_internal_event_payload() {
        let record = EmittedLogRecord {
            body: "tengu_init".to_string(),
            attributes: {
                let mut map = Map::new();
                map.insert("event_name".into(), json!("tengu_init"));
                map.insert("event_id".into(), json!("id-1"));
                map.insert(
                    "core_metadata",
                    json!({
                        "model": "claude-sonnet",
                        "sessionId": "sid",
                        "userType": "external",
                        "isInteractive": "true",
                        "clientType": "cli",
                        "envContext": {},
                        "sweBenchRunId": "",
                        "sweBenchInstanceId": "",
                        "sweBenchTaskId": ""
                    }),
                );
                map
            },
            hr_time_ms: 1_000,
        };
        let exporter = FirstPartyEventLoggingExporter::new(ExporterOptions::default());
        let payload = exporter.transform_logs_to_events(&[record]);
        assert_eq!(payload.events.len(), 1);
        assert_eq!(payload.events[0].event_type, "ClaudeCodeInternalEvent");
        assert_eq!(payload.events[0].event_data["event_name"], json!("tengu_init"));
    }
}
