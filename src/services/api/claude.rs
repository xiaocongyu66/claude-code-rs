//! claude.rs — 核心 API 客户端（重写自 `src/services/api/claude.ts`）。
//!
//! 职责（对齐 TS）：
//! - system prompt 组装（`buildSystemPromptBlocks` / `splitSysPromptPrefix`）
//! - betas header 计算（`getMergedBetas` / `getModelBetas` / `getExtraBodyParams`）
//! - 消息转换（`userMessageToMessageParam` / `assistantMessageToMessageParam` /
//!   `addCacheBreakpoints` / `stripExcessMediaItems`）
//! - 请求参数组装（`paramsFromContext` → `build_message_stream_params`）
//! - 流式事件（`BetaRawMessageStreamEvent`）解析与累计（`StreamAccumulator`）
//! - usage 累计（`updateUsage` / `accumulateUsage`）
//!
//! 已知偏差（依赖未转换模块）：
//! - **HTTP 层未接入**（Cargo.toml 暂无 reqwest）：`query_model` 系列、
//!   `execute_non_streaming_request`、`verify_api_key`、`query_haiku`、
//!   `query_with_model` 保留完整签名，统一返回
//!   `ApiError::HttpClientUnavailable`，待 reqwest 落地后填充。
//! - growthbook/statsig 未转换：`tengu_*` 实验门控（1h cache TTL allowlist、
//!   max-tokens cap、strict tools、token-efficient tools 等）一律按 3P
//!   默认值（false）处理；Bedrock 环境变量快速路径与 bootstrap state 的
//!   latch 逻辑完整保留。
//! - `utils/api.ts`、`utils/messages.ts`、`constants/system.ts`、
//!   `commands/break-cache` 未转换：`split_sys_prompt_prefix`、CLI prefix
//!   判定、tool_use input 规范化（JSON 解析失败回退 `{}`）等在本模块
//!   内置最小等价实现。
//! - `utils/model/model.ts` 未转换：默认模型字符串（sonnet/opus/haiku）
//!   使用 `ALL_MODEL_CONFIGS` 内置实现，不含 settings modelOverrides 与
//!   Bedrock inference profile 解析。
//! - VCR / langfuse / telemetry / headlessProfiler / sessionActivity /
//!   quota 侧效：HTTP 未接入，均不产生。
//! - `feature()` 与 TS 约定一致：读取 `FEATURE_<NAME>=1` 环境变量。

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::bootstrap::state::{
    get_afk_mode_header_latched, get_cache_editing_header_latched, get_fast_mode_header_latched,
    get_is_non_interactive_session, get_prompt_cache_1h_allowlist,
    get_prompt_cache_1h_eligible, get_sdk_betas, get_session_id, set_afk_mode_header_latched,
    set_cache_editing_header_latched, set_fast_mode_header_latched, set_prompt_cache_1h_allowlist,
    set_prompt_cache_1h_eligible, ModelUsage,
};
use crate::constants::api_limits::API_MAX_MEDIA_PER_REQUEST;
use crate::constants::betas::{
    ADVISOR_BETA_HEADER, CACHE_EDITING_BETA_HEADER, CLAUDE_CODE_20250219_BETA_HEADER,
    CONTEXT_1M_BETA_HEADER, CONTEXT_MANAGEMENT_BETA_HEADER, EFFORT_BETA_HEADER,
    FAST_MODE_BETA_HEADER, INTERLEAVED_THINKING_BETA_HEADER, PROMPT_CACHING_SCOPE_BETA_HEADER,
    REDACT_THINKING_BETA_HEADER, SEARCH_EXTRA_TOOLS_BETA_HEADER_3P,
    STRUCTURED_OUTPUTS_BETA_HEADER, TASK_BUDGETS_BETA_HEADER, TOKEN_EFFICIENT_TOOLS_BETA_HEADER,
    WEB_SEARCH_BETA_HEADER,
};
use crate::constants::query_source::QuerySource;
use crate::utils::auth::{get_oauth_account_info, is_claude_ai_subscriber};
use crate::utils::context::{
    get_max_thinking_tokens_for_model, get_model_max_output_tokens,
    get_sonnet_1m_exp_treatment_enabled, has_1m_context, CAPPED_DEFAULT_MAX_TOKENS,
};
use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::env_utils::{get_claude_config_home_dir, is_env_truthy};
use crate::utils::env_validation::validate_bounded_int_env_var;
use crate::utils::model::configs::{ALL_MODEL_CONFIGS, ModelKey};
use crate::utils::model::model_support_overrides::{
    get_3p_model_capability_override, ModelCapabilityOverride,
};
use crate::utils::model::provider_classification::ApiProvider;
use crate::utils::model::providers::{get_api_provider, is_first_party_anthropic_base_url};
use crate::utils::model_cost::{calculate_usd_cost, CostUsage};
use crate::utils::settings::settings::get_initial_settings;

// ---------------------------------------------------------------------------
// feature flag（对齐 TS `feature('NAME')`：运行时读 `FEATURE_<NAME>=1`）
// ---------------------------------------------------------------------------

fn feature(name: &str) -> bool {
    env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// `claude.ts` 用到的 SDK 错误子集 + 本地映射。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ApiError {
    /// 需要 HTTP 客户端（reqwest 尚未加入 Cargo.toml）。
    #[error("HTTP client unavailable: {0} requires reqwest (not yet in Cargo.toml)")]
    HttpClientUnavailable(&'static str),
    /// 用户终止（对齐 SDK `APIUserAbortError`）。
    #[error("Request was aborted by the user")]
    UserAbort,
    /// 连接超时（对齐 SDK `APIConnectionTimeoutError`）。
    #[error("Request timed out")]
    ConnectionTimeout,
    /// 流式流未产生任何 message_start / 内容块事件（触发非流式回退）。
    #[error("Stream ended without receiving any events")]
    StreamNoEvents,
    /// 流空闲超时（watchdog）。
    #[error("Stream idle timeout - no chunks received")]
    StreamIdleTimeout,
    /// content_block_delta/stop 引用的 index 不存在（对齐 TS `RangeError`）。
    #[error("Content block not found (index {index})")]
    ContentBlockNotFound { index: u32 },
    /// delta 类型与内容块类型不匹配（对齐 TS 抛出的各类 mismatch）。
    #[error("Content block type mismatch: expected {expected}, got {actual}")]
    ContentBlockTypeMismatch { expected: &'static str, actual: String },
    /// tool_use input 累计值不是字符串。
    #[error("Content block input is not a string")]
    ContentBlockInputNotString,
    /// 未收到 message_start（对齐 TS `'Message not found'`）。
    #[error("Message not found")]
    MessageNotFound,
    /// 非流式请求返回但无助手消息。
    #[error("No assistant message found")]
    NoAssistantMessage,
    /// 流式端点返回 404（需回退非流式）。
    #[error("Streaming endpoint returned 404")]
    StreamCreation404,
    /// 模型回退信号（对齐 `FallbackTriggeredError`，由 query.ts 处理）。
    #[error("Model fallback triggered")]
    FallbackTriggered,
    /// HTTP / JSON 错误。
    #[error("{0}")]
    Http(String),
    #[error("{0}")]
    Json(String),
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::Json(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// SDK wire 类型（@anthropic-ai/sdk 实际使用子集，serde 建模）
// ---------------------------------------------------------------------------

/// `message_start.message.usage` 的完整 usage 形状。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BetaUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// SDK 类型缺失但 API 真实返回的 `cache_creation` 细分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreationBreakdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// 兜底（cache_deleted_input_tokens、iterations 等未建模字段）。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheCreationBreakdown {
    #[serde(default)]
    pub ephemeral_1h_input_tokens: i64,
    #[serde(default)]
    pub ephemeral_5m_input_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUsage {
    #[serde(default)]
    pub web_search_requests: i64,
    #[serde(default)]
    pub web_fetch_requests: i64,
}

/// `message_delta.usage`：累计 usage，字段可空（SDK `BetaMessageDeltaUsage`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BetaMessageDeltaUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// 兜底（`cache_creation`、`cache_deleted_input_tokens` 等顶层字段）。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl BetaMessageDeltaUsage {
    fn field_i64(&self, key: &str) -> Option<i64> {
        self.extra.get(key).and_then(Value::as_i64)
    }

    /// SDK 类型缺失的 `cache_creation.ephemeral_*`（TS 用 as 强读）。
    fn cache_creation(&self) -> Option<&CacheCreationBreakdown> {
        self.extra.get("cache_creation").and_then(|v| {
            if v.is_null() {
                None
            } else {
                serde_json::from_value::<CacheCreationBreakdown>(v.clone()).ok()
            }
        })
    }
}

/// `NonNullableUsage`：序列化的 usage 对象（对齐 @ant/model-provider）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NonNullableUsage {
    pub input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub output_tokens: i64,
    pub server_tool_use: ServerToolUsage,
    pub service_tier: String,
    pub cache_creation: CacheCreationBreakdown,
    pub inference_geo: String,
    pub iterations: Vec<Value>,
    pub speed: String,
    /// cache editing 删除的 KV 条目（CACHED_MICROCOMPACT 门控，见 updateUsage）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_deleted_input_tokens: Option<i64>,
}

/// `EMPTY_USAGE`：零初始化 usage。
pub fn empty_usage() -> NonNullableUsage {
    NonNullableUsage {
        input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        output_tokens: 0,
        server_tool_use: ServerToolUsage::default(),
        service_tier: "standard".to_string(),
        cache_creation: CacheCreationBreakdown::default(),
        inference_geo: String::new(),
        iterations: Vec::new(),
        speed: "standard".to_string(),
        cache_deleted_input_tokens: None,
    }
}

/// `getCacheControl` 的返回：附在内容块上的缓存标记。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub typ: String, // 恒为 "ephemeral"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>, // "1h"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>, // "global"
}

/// 消息内容：字符串或内容块数组（`string | BetaContentBlockParam[]`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<Value>),
}

/// 通用内容块：`type` + 其余字段（对齐 SDK 各 Beta*BlockParam 的并集）。
///
/// 请求侧直接透传；流式累计侧对 `text` / `thinking` / `input` /
/// `connector_text` 做字符串拼接（同 TS 的可变累计）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockParam {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl ContentBlockParam {
    fn get_str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    fn set_str(&mut self, key: &str, value: impl Into<String>) {
        self.fields.insert(key.to_string(), Value::String(value.into()));
    }

    fn append_str(&mut self, key: &str, delta: &str) {
        let prev = self.get_str(key).unwrap_or("").to_string();
        self.set_str(key, format!("{prev}{delta}"));
    }

    /// 是否 `image` / `document`（`isMedia`）。
    pub fn is_media(&self) -> bool {
        self.typ == "image" || self.typ == "document"
    }

    /// 是否 `tool_result`（含 `tool_use_id`）。
    pub fn is_tool_result(&self) -> bool {
        self.typ == "tool_result" && self.fields.contains_key("tool_use_id")
    }
}

/// `MessageParam`：`{ role, content }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageParam {
    pub role: String,
    pub content: MessageContent,
}

/// 系统提示块（`TextBlockParam`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlockParam {
    #[serde(rename = "type")]
    pub typ: String, // "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// `BetaToolChoice`：`auto` / `any` / `tool{ name }` / `none`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
    None,
}

/// `BetaToolUnion`：`custom` 工具（input_schema）与服务端工具（advisor 等）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolParam {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub typ: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// 兜底（`defer_loading` 等字段）。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `BetaOutputConfig`：effort / format / task_budget。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<JsonOutputFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_budget: Option<TaskBudgetParam>,
    /// 兜底。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OutputConfig {
    pub fn is_empty(&self) -> bool {
        self.effort.is_none() && self.format.is_none() && self.task_budget.is_none() && self.extra.is_empty()
    }
}

/// `output_config.task_budget`（Wire 形状，Stainless SDK 尚未建模）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudgetParam {
    #[serde(rename = "type")]
    pub typ: String, // "tokens"
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>,
}

/// 调用侧传入的 task budget（`Options['taskBudget']`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>,
}

/// `BetaJSONOutputFormat`：按实际使用建模（json_schema 等）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonOutputFormat {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// 请求体中的 `thinking`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingParam {
    Adaptive,
    Enabled { budget_tokens: i64 },
}

/// 调用侧的思考配置（`utils/thinking.ts` 的 `ThinkingConfig`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Disabled,
    Enabled { budget_tokens: i64 },
}

/// `metadata`：`getAPIMetadata()` 的返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiMetadata {
    /// `user_id` 为 JSON 字符串（同 TS）。
    pub user_id: String,
}

/// `BetaMessage`：`message_start.message` 的完整形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetaMessage {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "message_type")]
    pub typ: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Vec<ContentBlockParam>,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: BetaUsage,
    /// 兜底（ant 内部 `research` 等字段）。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn message_type() -> String {
    "message".to_string()
}

/// `message_delta.delta`：`{ stop_reason, stop_sequence }`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// `BetaRawMessageStreamEvent`：流式端点事件联合。
///
/// 未知 `type` 落入 `Unknown`（TS 的 switch 对未知名类型不做处理）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BetaRawMessageStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: BetaMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockParam,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: u32,
        delta: ContentBlockDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<BetaMessageDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: Value },
    #[serde(other)]
    Unknown,
}

/// `content_block_delta.delta`：`type` + 增量字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlockDelta {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl ContentBlockDelta {
    fn get_str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    pub fn text(&self) -> Option<&str> {
        self.get_str("text")
    }

    pub fn partial_json(&self) -> Option<&str> {
        self.get_str("partial_json")
    }

    pub fn thinking(&self) -> Option<&str> {
        self.get_str("thinking")
    }

    pub fn signature(&self) -> Option<&str> {
        self.get_str("signature")
    }
}

// ---------------------------------------------------------------------------
// 消息模型（`types/message.ts` 的使用子集，本模块本地定义）
// ---------------------------------------------------------------------------

/// `UserMessage`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub message: UserMessageBody,
    #[serde(rename = "type")]
    pub typ: String, // "user"
    pub uuid: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_meta: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessageBody {
    pub content: MessageContent,
}

/// `AssistantMessage`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub message: AssistantMessageBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(rename = "type")]
    pub typ: String, // "assistant"
    pub uuid: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageBody {
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<NonNullableUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// `Message`：user / assistant 联合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "type")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
}

impl Message {
    /// 取 `requestId`（`getPreviousRequestIdFromMessages` 只读 assistant）。
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Message::Assistant(a) => a.request_id.as_deref(),
            Message::User(_) => None,
        }
    }
}

/// `SystemPrompt`：字符串数组（`utils/systemPromptType.ts`）。
pub type SystemPrompt = Vec<String>;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 非流式请求的 10min 上限对应的 token 数（SDK 21333 上限被 client 超时绕过）。
pub const MAX_NON_STREAMING_TOKENS: i64 = 64_000;

/// `system_prompt` 动态边界标记（`constants/prompts.ts`）。
const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

const ATTRIBUTION_HEADER_PREFIX: &str = "x-anthropic-billing-header";

const CLI_SYSPROMPT_PREFIXES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

const DEFAULT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const AGENT_SDK_CLAUDE_CODE_PRESET_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";
const AGENT_SDK_PREFIX: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// `getCLISyspromptPrefix`（`constants/system.ts`）。
pub fn get_cli_sysprompt_prefix(is_non_interactive: bool, has_append_system_prompt: bool) -> &'static str {
    if get_api_provider() == ApiProvider::Vertex {
        return DEFAULT_PREFIX;
    }
    if is_non_interactive {
        return if has_append_system_prompt {
            AGENT_SDK_CLAUDE_CODE_PRESET_PREFIX
        } else {
            AGENT_SDK_PREFIX
        };
    }
    DEFAULT_PREFIX
}

/// 流式空闲看门狗默认超时（`CLAUDE_STREAM_IDLE_TIMEOUT_MS` 覆盖）。
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 90_000;

/// 流式停滞检测阈值（30s，仅诊断日志）。
pub const STALL_THRESHOLD_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// system prompt 组装
// ---------------------------------------------------------------------------

/// `splitSysPromptPrefix`（`utils/api.ts`）的最小等价实现。
///
/// 输出 `(text, cache_scope)` 块序列：
/// 1. 全局缓存 + 跳过系统提示缓存：剔除边界标记，org 级缓存两段。
/// 2. 全局缓存且存在边界标记：static（global）/ dynamic（null）四段。
/// 3. 默认：attribution（null）+ CLI prefix（org）+ 其余（org）三段。
pub fn split_sys_prompt_prefix(
    system_prompt: &SystemPrompt,
    skip_global_cache_for_system_prompt: bool,
) -> Vec<(String, Option<String>)> {
    let use_global_cache_feature = should_use_global_cache_scope();
    let is_cli_prefix = |block: &str| CLI_SYSPROMPT_PREFIXES.contains(&block);

    // 1. 工具缓存模式：无全局缓存。
    if use_global_cache_feature && skip_global_cache_for_system_prompt {
        let mut attribution_header: Option<String> = None;
        let mut system_prompt_prefix: Option<String> = None;
        let mut rest: Vec<String> = Vec::new();
        for prompt in system_prompt {
            if prompt.is_empty() || prompt == SYSTEM_PROMPT_DYNAMIC_BOUNDARY {
                continue;
            }
            if prompt.starts_with(ATTRIBUTION_HEADER_PREFIX) {
                attribution_header = Some(prompt.clone());
            } else if is_cli_prefix(prompt) {
                system_prompt_prefix = Some(prompt.clone());
            } else {
                rest.push(prompt.clone());
            }
        }
        let mut result = Vec::new();
        if let Some(h) = attribution_header {
            result.push((h, None));
        }
        if let Some(p) = system_prompt_prefix {
            result.push((p, Some("org".to_string())));
        }
        let rest_joined = rest.join("\n\n");
        if !rest_joined.is_empty() {
            result.push((rest_joined, Some("org".to_string())));
        }
        return result;
    }

    // 2. 全局缓存 + 边界标记：static / dynamic 分割。
    if use_global_cache_feature {
        let boundary_index = system_prompt
            .iter()
            .position(|b| b == SYSTEM_PROMPT_DYNAMIC_BOUNDARY);
        if let Some(boundary_index) = boundary_index {
            let mut attribution_header: Option<String> = None;
            let mut system_prompt_prefix: Option<String> = None;
            let mut static_blocks: Vec<String> = Vec::new();
            let mut dynamic_blocks: Vec<String> = Vec::new();
            for (i, block) in system_prompt.iter().enumerate() {
                if block.is_empty() || block == SYSTEM_PROMPT_DYNAMIC_BOUNDARY {
                    continue;
                }
                if block.starts_with(ATTRIBUTION_HEADER_PREFIX) {
                    attribution_header = Some(block.clone());
                } else if is_cli_prefix(block) {
                    system_prompt_prefix = Some(block.clone());
                } else if i < boundary_index {
                    static_blocks.push(block.clone());
                } else {
                    dynamic_blocks.push(block.clone());
                }
            }
            let mut result = Vec::new();
            if let Some(h) = attribution_header {
                result.push((h, None));
            }
            if let Some(p) = system_prompt_prefix {
                result.push((p, None));
            }
            let static_joined = static_blocks.join("\n\n");
            if !static_joined.is_empty() {
                result.push((static_joined, Some("global".to_string())));
            }
            let dynamic_joined = dynamic_blocks.join("\n\n");
            if !dynamic_joined.is_empty() {
                result.push((dynamic_joined, None));
            }
            return result;
        }
    }

    // 3. 默认模式。
    let mut attribution_header: Option<String> = None;
    let mut system_prompt_prefix: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    for block in system_prompt {
        if block.is_empty() {
            continue;
        }
        if block.starts_with(ATTRIBUTION_HEADER_PREFIX) {
            attribution_header = Some(block.clone());
        } else if is_cli_prefix(block) {
            system_prompt_prefix = Some(block.clone());
        } else {
            rest.push(block.clone());
        }
    }
    let mut result = Vec::new();
    if let Some(h) = attribution_header {
        result.push((h, None));
    }
    if let Some(p) = system_prompt_prefix {
        result.push((p, Some("org".to_string())));
    }
    let rest_joined = rest.join("\n\n");
    if !rest_joined.is_empty() {
        result.push((rest_joined, Some("org".to_string())));
    }
    result
}

/// `buildSystemPromptBlocks`：系统提示 → `TextBlockParam[]`（含 cache_control）。
pub fn build_system_prompt_blocks(
    system_prompt: &SystemPrompt,
    enable_prompt_caching: bool,
    options: &BuildSystemPromptOptions,
) -> Vec<TextBlockParam> {
    split_sys_prompt_prefix(system_prompt, options.skip_global_cache_for_system_prompt)
        .into_iter()
        .map(|(text, cache_scope)| TextBlockParam {
            typ: "text".to_string(),
            text,
            cache_control: if enable_prompt_caching && cache_scope.is_some() {
                Some(get_cache_control(CacheControlOpts {
                    scope: cache_scope.as_deref(),
                    query_source: options.query_source.as_deref(),
                }))
            } else {
                None
            },
        })
        .collect()
}

/// `buildSystemPromptBlocks` 的选项。
#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    /// `skipGlobalCacheForSystemPrompt`（存在 MCP 工具时跳过全局缓存）。
    pub skip_global_cache_for_system_prompt: bool,
    pub query_source: Option<String>,
}

// ---------------------------------------------------------------------------
// cache control
// ---------------------------------------------------------------------------

/// `getCacheControl` 的入参。
#[derive(Debug, Clone, Default)]
pub struct CacheControlOpts<'a> {
    pub scope: Option<&'a str>,
    pub query_source: Option<&'a str>,
}

/// `getCacheControl`：`{ type: 'ephemeral', ttl?: '1h', scope?: 'global' }`。
pub fn get_cache_control(opts: CacheControlOpts) -> CacheControl {
    CacheControl {
        typ: "ephemeral".to_string(),
        ttl: if should_1h_cache_ttl(opts.query_source) {
            Some("1h".to_string())
        } else {
            None
        },
        scope: if opts.scope == Some("global") {
            Some("global".to_string())
        } else {
            None
        },
    }
}

/// `should1hCacheTTL`（GrowthBook allowlist 部分按未配置处理）。
pub fn should_1h_cache_ttl(query_source: Option<&str>) -> bool {
    // 3P Bedrock 用户通过环境变量开启 1h TTL（自助计费，无需 GrowthBook）。
    if get_api_provider() == ApiProvider::Bedrock
        && is_env_truthy(env::var("ENABLE_PROMPT_CACHING_1H_BEDROCK").ok().as_deref())
    {
        return true;
    }

    //Latch 资格到 bootstrap state，保证会话内 TTL 稳定。
    let mut user_eligible = get_prompt_cache_1h_eligible();
    if user_eligible.is_none() {
        user_eligible = Some(
            env::var("USER_TYPE").map(|t| t == "ant").unwrap_or(false)
                || (is_claude_ai_subscriber() && !crate::bootstrap::state::current_limits_is_using_overage()),
        );
        set_prompt_cache_1h_eligible(user_eligible);
    }
    if !user_eligible.unwrap_or(false) {
        return false;
    }

    // Latch allowlist；GrowthBook 未接入时缓存一次后恒为空表。
    let mut allowlist = get_prompt_cache_1h_allowlist();
    if allowlist.is_none() {
        allowlist = Some(Vec::new());
        set_prompt_cache_1h_allowlist(allowlist.clone());
    }
    let Some(query_source) = query_source else {
        return false;
    };
    allowlist
        .unwrap_or_default()
        .iter()
        .any(|pattern| {
            pattern
                .strip_suffix('*')
                .map(|prefix| query_source.starts_with(prefix))
                .unwrap_or_else(|| pattern == query_source)
        })
}

/// `getPromptCachingEnabled`：全局 / 小模型 / 默认 sonnet / 默认 opus 禁用开关。
pub fn get_prompt_caching_enabled(model: &str) -> bool {
    if is_env_truthy(env::var("DISABLE_PROMPT_CACHING").ok().as_deref()) {
        return false;
    }
    if is_env_truthy(env::var("DISABLE_PROMPT_CACHING_HAIKU").ok().as_deref())
        && model == get_small_fast_model().as_str()
    {
        return false;
    }
    if is_env_truthy(env::var("DISABLE_PROMPT_CACHING_SONNET").ok().as_deref())
        && model == get_default_sonnet_model().as_str()
    {
        return false;
    }
    if is_env_truthy(env::var("DISABLE_PROMPT_CACHING_OPUS").ok().as_deref())
        && model == get_default_opus_model().as_str()
    {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// extra body params / metadata
// ---------------------------------------------------------------------------

/// `getExtraBodyParams`：解析 `CLAUDE_CODE_EXTRA_BODY`（JSON 对象）并合并
/// beta headers 到 `anthropic_beta`（去重）。
pub fn get_extra_body_params(beta_headers: Option<&[String]>) -> Map<String, Value> {
    let mut result: Map<String, Value> = Map::new();

    if let Ok(extra_body_str) = env::var("CLAUDE_CODE_EXTRA_BODY") {
        if !extra_body_str.is_empty() {
            match serde_json::from_str::<Value>(&extra_body_str) {
                Ok(Value::Object(parsed)) => {
                    // 浅拷贝——不可变，直接吸收 map。
                    result = parsed;
                }
                Ok(_) => {
                    log_for_debugging(
                        &format!(
                            "CLAUDE_CODE_EXTRA_BODY env var must be a JSON object, but was given {extra_body_str}"
                        ),
                        DebugLogLevel::Error,
                    );
                }
                Err(e) => {
                    log_for_debugging(
                        &format!("Error parsing CLAUDE_CODE_EXTRA_BODY: {e}"),
                        DebugLogLevel::Error,
                    );
                }
            }
        }
    }

    if let Some(headers) = beta_headers {
        if !headers.is_empty() {
            match result.get("anthropic_beta") {
                Some(Value::Array(existing)) => {
                    let new_headers: Vec<Value> = headers
                        .iter()
                        .filter(|h| !existing.iter().any(|v| v == h))
                        .map(|h| Value::String(h.clone()))
                        .collect();
                    let mut merged = existing.clone();
                    merged.extend(new_headers);
                    result.insert("anthropic_beta".to_string(), Value::Array(merged));
                }
                _ => {
                    result.insert(
                        "anthropic_beta".to_string(),
                        Value::Array(headers.iter().map(|h| Value::String(h.clone())).collect()),
                    );
                }
            }
        }
    }

    result
}

/// `getAPIMetadata`：`{ user_id: jsonStringify({...}) }`。
///
/// 偏差：`getOrCreateUserID`（config.ts 未转换）为本地实现，读写
/// `${CLAUDE_CONFIG_HOME}/config.json`；`accountUuid` 恒为空串
/// （auth.rs 的 OauthAccountInfo 无该字段）。
pub fn get_api_metadata() -> ApiMetadata {
    let mut extra: Map<String, Value> = Map::new();
    if let Ok(extra_str) = env::var("CLAUDE_CODE_EXTRA_METADATA") {
        if !extra_str.is_empty() {
            match serde_json::from_str::<Value>(&extra_str) {
                Ok(Value::Object(parsed)) => extra = parsed,
                _ => log_for_debugging(
                    &format!(
                        "CLAUDE_CODE_EXTRA_METADATA env var must be a JSON object, but was given {extra_str}"
                    ),
                    DebugLogLevel::Error,
                ),
            }
        }
    }
    extra.insert("device_id".to_string(), Value::String(get_or_create_user_id()));
    extra.insert("account_uuid".to_string(), Value::String(String::new()));
    extra.insert(
        "session_id".to_string(),
        Value::String(get_session_id().0),
    );
    ApiMetadata {
        user_id: serde_json::to_string(&extra).unwrap_or_default(),
    }
}

/// `getOrCreateUserID`（最小实现）：读 `${CLAUDE_CONFIG_HOME}/config.json`
/// 的 `userID`，缺失则生成 64 位 hex 并尽力持久化。
fn get_or_create_user_id() -> String {
    let path = config_file_path();
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&content) {
            if let Some(Value::String(id)) = map.get("userID") {
                return id.clone();
            }
            let id = random_hex_32();
            map.insert("userID".to_string(), Value::String(id.clone()));
            let _ = fs::write(&path, serde_json::to_string(&Value::Object(map)).unwrap_or_default());
            return id;
        }
    }
    let id = random_hex_32();
    let mut map = Map::new();
    map.insert("userID".to_string(), Value::String(id.clone()));
    let _ = fs::write(&path, serde_json::to_string(&Value::Object(map)).unwrap_or_default());
    id
}

fn config_file_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join("config.json")
}

fn random_hex_32() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// 请求参数（paramsFromContext）
// ---------------------------------------------------------------------------

/// `Options`（`claude.ts`）：query 层传入的请求选项。
///
/// 偏差：不含回调类字段（`getToolPermissionContext` / `onStreamingFallback` /
/// `addNotification`）与 `fetchOverride`（HTTP 未接入）、`mcpTools` /
/// `queryTracking` / `agents` / `langfuseTrace`。
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub model: String,
    pub tool_choice: Option<ToolChoice>,
    pub is_non_interactive_session: bool,
    pub extra_tool_schemas: Vec<ToolParam>,
    pub max_output_tokens_override: Option<i64>,
    pub fallback_model: Option<String>,
    pub query_source: QuerySource,
    pub has_append_system_prompt: bool,
    pub enable_prompt_caching: Option<bool>,
    pub skip_cache_write: bool,
    pub temperature_override: Option<f64>,
    pub effort_value: Option<EffortValue>,
    pub output_format: Option<JsonOutputFormat>,
    pub fast_mode: bool,
    pub advisor_model: Option<String>,
    pub task_budget: Option<TaskBudget>,
    pub agent_id: Option<String>,
    pub has_pending_mcp_servers: bool,
}

/// `EffortValue`：`'high' | 'medium' | 'low' | 'max' | number`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffortValue {
    Level(EffortLevel),
    Numeric(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffortLevel {
    High,
    Medium,
    Low,
    Max,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::High => "high",
            EffortLevel::Medium => "medium",
            EffortLevel::Low => "low",
            EffortLevel::Max => "max",
        }
    }
}

/// `getAPIMetadata` 的请求参数：queryModel 一次性算好的上下文。
#[derive(Debug, Clone)]
pub struct RequestSpec<'a> {
    pub options: &'a Options,
    pub thinking_config: &'a ThinkingConfig,
    /// 已过 `addCacheBreakpoints` 的请求消息。
    pub messages: &'a [MessageParam],
    /// 已过 `buildSystemPromptBlocks` 的系统提示块。
    pub system: &'a [TextBlockParam],
    /// toolSchemas + extraToolSchemas（advisor 等服务端工具）。
    pub tools: &'a [ToolParam],
    /// 基础 betas（`getMergedBetas` + advisor beta + 全局缓存 beta 等）。
    pub base_betas: &'a [String],
    /// `retryContext.maxTokensOverride`（上下文超限时的重试覆盖）。
    pub max_tokens_override: Option<i64>,
    /// `retryContext.fastMode`（withRetry 传入的 fastMode）。
    pub retry_fast_mode: bool,
    /// sticky-on latches（queryModel 内 latch 后传入）。
    pub fast_mode_header_latched: bool,
    pub afk_header_latched: bool,
    pub cache_editing_header_latched: bool,
}

/// `BetaMessageStreamParams`：发往 streaming 端点的请求体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStreamParams {
    pub model: String,
    pub messages: Vec<MessageParam>,
    pub system: Vec<TextBlockParam>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub betas: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ApiMetadata>,
    pub max_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// 计算出的 extra body params（env）。序列化时**覆盖**同名字段，
    /// 对齐 TS `{ ...computed, ...extraBodyParams }` 的展开顺序。
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub extra_body: Map<String, Value>,
}

impl MessageStreamParams {
    /// 序列化为请求体 JSON：extra_body 字段覆盖计算字段（对齐 TS 展开）。
    pub fn into_request_body(mut self) -> Value {
        let mut value = serde_json::to_value(&self).unwrap_or(Value::Null);
        let extra = std::mem::take(&mut self.extra_body);
        if let Value::Object(map) = &mut value {
            for (k, v) in extra {
                map.insert(k, v);
            }
        }
        value
    }
}

/// `paramsFromContext`：组装一次 API 请求的全部参数。
///
/// 对齐 TS 语义：betas 追加（1m / effort / task_budget / structured
/// outputs / fast mode / afk / cache editing）、effort / output_config、
/// thinking（adaptive vs budget）、temperature、speed。
pub fn build_message_stream_params(spec: &RequestSpec) -> MessageStreamParams {
    let options = spec.options;
    let mut betas_params: Vec<String> = spec.base_betas.to_vec();

    // Sonnet 1m 实验头。
    if !betas_params.contains(&CONTEXT_1M_BETA_HEADER.to_string())
        && get_sonnet_1m_exp_treatment_enabled(&options.model)
    {
        betas_params.push(CONTEXT_1M_BETA_HEADER.to_string());
    }

    // Bedrock：模型级 betas 走 extraBodyParams（而非 headers）。
    let bedrock_betas: Vec<String> = if get_api_provider() == ApiProvider::Bedrock {
        get_bedrock_extra_body_params_betas(&options.model)
    } else {
        Vec::new()
    };
    let mut extra_body_params = get_extra_body_params(Some(&bedrock_betas));

    let mut output_config: OutputConfig = extra_body_params
        .get("output_config")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut anthropic_internal: Option<Map<String, Value>> = extra_body_params
        .get("anthropic_internal")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let effort = resolve_applied_effort(&options.model, options.effort.as_ref());
    configure_effort_params_internal(
        &options.model,
        effort.as_ref(),
        &mut output_config,
        &mut anthropic_internal,
        &mut betas_params,
    );
    if let Some(internal) = anthropic_internal {
        extra_body_params.insert("anthropic_internal".to_string(), Value::Object(internal));
    }

    configure_task_budget_params(options.task_budget.as_ref(), &mut output_config, &mut betas_params);

    // outputFormat 与 structured outputs beta。
    if let Some(format) = &options.output_format {
        if output_config.format.is_none() {
            output_config.format = Some(format.clone());
            if model_supports_structured_outputs(&options.model)
                && !betas_params.contains(&STRUCTURED_OUTPUTS_BETA_HEADER.to_string())
            {
                betas_params.push(STRUCTURED_OUTPUTS_BETA_HEADER.to_string());
            }
        }
    }

    // max_tokens：retry 上下文 > 调用方覆盖 > 模型默认。
    let max_output_tokens = spec
        .max_tokens_override
        .or(options.max_output_tokens_override)
        .unwrap_or_else(|| get_max_output_tokens_for_model(&options.model));

    // thinking：自适应 vs 固定预算。
    let has_thinking = !matches!(spec.thinking_config, ThinkingConfig::Disabled)
        && !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_THINKING").ok().as_deref());
    let mut thinking: Option<ThinkingParam> = None;
    if has_thinking && model_supports_thinking(&options.model) {
        if !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING").ok().as_deref())
            && model_supports_adaptive_thinking(&options.model)
        {
            thinking = Some(ThinkingParam::Adaptive);
        } else {
            let mut thinking_budget = i64::from(get_max_thinking_tokens_for_model(&options.model));
            if let ThinkingConfig::Enabled { budget_tokens } = spec.thinking_config {
                thinking_budget = budget_tokens;
            }
            thinking_budget = thinking_budget.min(max_output_tokens - 1);
            thinking = Some(ThinkingParam::Enabled {
                budget_tokens: thinking_budget,
            });
        }
    }

    // Fast mode：speed 字段动态、header latched（缓存稳定）。
    let mut speed: Option<String> = None;
    let is_fast_mode_for_retry = is_fast_mode_enabled()
        && is_fast_mode_available()
        && !is_fast_mode_cooldown()
        && is_fast_mode_supported_by_model(&options.model)
        && spec.retry_fast_mode;
    if is_fast_mode_for_retry {
        speed = Some("fast".to_string());
    }
    if spec.fast_mode_header_latched
        && !betas_params.contains(&FAST_MODE_BETA_HEADER.to_string())
    {
        betas_params.push(FAST_MODE_BETA_HEADER.to_string());
    }

    // AFK mode beta：latched + agentic 门控。
    if spec.afk_header_latched
        && should_include_first_party_only_betas()
        && is_agentic_query_source(&options.query_source)
        && !betas_params.contains(&crate::constants::betas::afk_mode_beta_header().to_string())
    {
        betas_params.push(crate::constants::betas::afk_mode_beta_header().to_string());
    }

    // Cache editing beta：header latched，body 行为保持动态。
    if spec.cache_editing_header_latched
        && !CACHE_EDITING_BETA_HEADER.is_empty()
        && get_api_provider() == ApiProvider::FirstParty
        && options.query_source == "repl_main_thread"
        && !betas_params.contains(&CACHE_EDITING_BETA_HEADER.to_string())
    {
        betas_params.push(CACHE_EDITING_BETA_HEADER.to_string());
        log_for_debugging(
            "Cache editing beta header enabled for cached microcompact",
            DebugLogLevel::Debug,
        );
    }

    // temperature：仅思考关闭时发送（API 要求思考时 temperature=1 默认）。
    let temperature = if !has_thinking {
        Some(options.temperature_override.unwrap_or(1.0))
    } else {
        None
    };

    // 过滤空串 beta（未开启功能的常量头为 ''，发出去会 400）。
    let filtered_betas: Vec<String> = betas_params.into_iter().filter(|b| !b.is_empty()).collect();

    let use_betas = !spec.base_betas.is_empty();

    MessageStreamParams {
        model: normalize_model_string_for_api(&options.model),
        messages: spec.messages.to_vec(),
        system: spec.system.to_vec(),
        tools: spec.tools.to_vec(),
        tool_choice: options.tool_choice.clone(),
        betas: if use_betas { Some(filtered_betas) } else { None },
        metadata: Some(get_api_metadata()),
        max_tokens: max_output_tokens,
        thinking,
        temperature,
        // context_management 需要 `services/compact/apiMicrocompact` 未转换，暂不产出。
        context_management: None,
        output_config: if output_config.is_empty() {
            None
        } else {
            Some(output_config)
        },
        speed,
        extra_body_params,
    }
}

/// `configureEffortParams`：effort 参数与 beta 头组装。
fn configure_effort_params_internal(
    model: &str,
    effort: Option<&EffortValue>,
    output_config: &mut OutputConfig,
    anthropic_internal: &mut Option<Map<String, Value>>,
    betas: &mut Vec<String>,
) {
    if !model_supports_effort(model) || output_config.effort.is_some() {
        return;
    }
    match effort {
        None => {
            betas.push(EFFORT_BETA_HEADER.to_string());
        }
        Some(EffortValue::Level(level)) => {
            output_config.effort = Some(level.as_str().to_string());
            betas.push(EFFORT_BETA_HEADER.to_string());
        }
        Some(EffortValue::Numeric(n)) => {
            // 数值 effort 为 ant 专属（anthropic_internal.effort_override）。
            if env::var("USER_TYPE").map(|t| t == "ant").unwrap_or(false) {
                let internal = anthropic_internal.get_or_insert_with(Map::new);
                internal.insert("effort_override".to_string(), json!(*n));
            }
        }
    }
}

/// `configureTaskBudgetParams`：`output_config.task_budget` + beta 头。
pub fn configure_task_budget_params(
    task_budget: Option<&TaskBudget>,
    output_config: &mut OutputConfig,
    betas: &mut Vec<String>,
) {
    if task_budget.is_none()
        || output_config.task_budget.is_some()
        || !should_include_first_party_only_betas()
    {
        return;
    }
    let budget = task_budget.unwrap();
    output_config.task_budget = Some(TaskBudgetParam {
        typ: "tokens".to_string(),
        total: budget.total,
        remaining: budget.remaining,
    });
    if !betas.contains(&TASK_BUDGETS_BETA_HEADER.to_string()) {
        betas.push(TASK_BUDGETS_BETA_HEADER.to_string());
    }
}

/// `resolveAppliedEffort`：env `CLAUDE_CODE_EFFORT_LEVEL`（`unset`/`auto`
/// 表示不发）→ appState → 模型默认（ant 专属，未接入 → None）。
pub fn resolve_applied_effort(_model: &str, app_state_effort: Option<&EffortValue>) -> Option<EffortValue> {
    let env_override = effort_env_override();
    if env_override.is_none() {
        return None;
    }
    env_override.or_else(|| app_state_effort.cloned())
}

fn effort_env_override() -> Option<EffortValue> {
    let raw = env::var("CLAUDE_CODE_EFFORT_LEVEL").ok()?;
    let lower = raw.to_ascii_lowercase();
    if lower == "unset" || lower == "auto" {
        return None(None); // `null`：显式不发送
    }
    parse_effort_value(&raw)
}

// `Option<Option<T>>` 区分「未设置」与「显式 unset/auto」。
#[allow(clippy::allow_attributes, unused)]
fn none_marker() {}

fn parse_effort_value(raw: &str) -> Option<EffortValue> {
    match raw.to_ascii_lowercase().as_str() {
        "high" => Some(EffortValue::Level(EffortLevel::High)),
        "medium" => Some(EffortValue::Level(EffortLevel::Medium)),
        "low" => Some(EffortValue::Level(EffortLevel::Low)),
        "max" => Some(EffortValue::Level(EffortLevel::Max)),
        _ => raw.parse::<u32>().ok().map(EffortValue::Numeric),
    }
}

// ---------------------------------------------------------------------------
// max output tokens / timeout
// ---------------------------------------------------------------------------

/// `getMaxOutputTokensForModel`：模型默认 + env 覆盖（slot cap 实验未接入）。
pub fn get_max_output_tokens_for_model(model: &str) -> i64 {
    let max_output_tokens = get_model_max_output_tokens(model);
    // `isMaxTokensCapEnabled`（tengu_otk_slot_v1）GrowthBook 未接入 → false（3P 默认）。
    let default_tokens = i64::from(max_output_tokens.default);
    validate_bounded_int_env_var(
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        env::var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").ok().as_deref(),
        default_tokens.min(i64::from(CAPPED_DEFAULT_MAX_TOKENS)),
        i64::from(max_output_tokens.upper_limit),
    )
    .effective
}

/// `getNonstreamingFallbackTimeoutMs`：`API_TIMEOUT_MS` 或远程 120s / 本地 300s。
pub fn get_nonstreaming_fallback_timeout_ms() -> u64 {
    if let Ok(raw) = env::var("API_TIMEOUT_MS") {
        if let Ok(ms) = raw.trim().parse::<u64>() {
            if ms > 0 {
                return ms;
            }
        }
    }
    if is_env_truthy(env::var("CLAUDE_CODE_REMOTE").ok().as_deref()) {
        120_000
    } else {
        300_000
    }
}

/// `adjustParamsForNonStreaming`：max_tokens 截断 + thinking 预算同步收缩。
pub fn adjust_params_for_non_streaming(params: &mut MessageStreamParams, max_tokens_cap: i64) {
    let capped_max_tokens = params.max_tokens.min(max_tokens_cap);
    if let Some(ThinkingParam::Enabled { budget_tokens }) = &mut params.thinking {
        *budget_tokens = (*budget_tokens).min(capped_max_tokens - 1);
    }
    params.max_tokens = capped_max_tokens;
}

// ---------------------------------------------------------------------------
// 消息转换
// ---------------------------------------------------------------------------

/// `userMessageToMessageParam`。
///
/// `add_cache=true` 时给最后一个内容块加 `cache_control`（字符串内容
/// 包一层 text 块）。非缓存路径克隆数组内容，避免原地变异污染原消息。
pub fn user_message_to_message_param(
    message: &UserMessage,
    add_cache: bool,
    enable_prompt_caching: bool,
    query_source: Option<&str>,
) -> MessageParam {
    if add_cache {
        match &message.message.content {
            MessageContent::Text(text) => MessageParam {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![json!({
                    "type": "text",
                    "text": text,
                    "cache_control": enable_prompt_caching.then(|| get_cache_control(CacheControlOpts {
                        query_source,
                        ..Default::default()
                    })),
                })]),
            },
            MessageContent::Blocks(blocks) => {
                let last = blocks.len().saturating_sub(1);
                let mapped: Vec<Value> = blocks
                    .iter()
                    .enumerate()
                    .map(|(i, block)| {
                        if i == last && enable_prompt_caching {
                            let mut obj = block.clone();
                            if let Value::Object(map) = &mut obj {
                                map.insert(
                                    "cache_control".to_string(),
                                    serde_json::to_value(get_cache_control(CacheControlOpts {
                                        query_source,
                                        ..Default::default()
                                    }))
                                    .unwrap_or(Value::Null),
                                );
                            }
                            obj
                        } else {
                            block.clone()
                        }
                    })
                    .collect();
                MessageParam {
                    role: "user".to_string(),
                    content: MessageContent::Blocks(mapped),
                }
            }
        }
    } else {
        MessageParam {
            role: "user".to_string(),
            // 克隆，防止 insertCacheEditsBlock 等原地 splice 污染原消息。
            content: message.message.content.clone(),
        }
    }
}

/// `assistantMessageToMessageParam`：思考 / redacted_thinking /
/// connector_text 块不加缓存标记（API 约束）。
pub fn assistant_message_to_message_param(
    message: &AssistantMessage,
    add_cache: bool,
    enable_prompt_caching: bool,
    query_source: Option<&str>,
) -> MessageParam {
    if add_cache {
        match &message.message.content {
            MessageContent::Text(text) => MessageParam {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![json!({
                    "type": "text",
                    "text": text,
                    "cache_control": enable_prompt_caching.then(|| get_cache_control(CacheControlOpts {
                        query_source,
                        ..Default::default()
                    })),
                })]),
            },
            MessageContent::Blocks(blocks) => {
                let last = blocks.len().saturating_sub(1);
                let mapped: Vec<Value> = blocks
                    .iter()
                    .enumerate()
                    .map(|(i, block)| {
                        let stripped = strip_gemini_provider_metadata_value(block);
                        let can_cache = i == last
                            && stripped
                                .get("type")
                                .and_then(Value::as_str)
                                .map(|t| {
                                    t != "thinking"
                                        && t != "redacted_thinking"
                                        && (!feature("CONNECTOR_TEXT")
                                            || !crate::types::connector_text::is_connector_text_block(
                                                block,
                                            ))
                                })
                                .unwrap_or(true);
                        if can_cache && enable_prompt_caching {
                            let mut obj = stripped;
                            if let Value::Object(map) = &mut obj {
                                map.insert(
                                    "cache_control".to_string(),
                                    serde_json::to_value(get_cache_control(CacheControlOpts {
                                        query_source,
                                        ..Default::default()
                                    }))
                                    .unwrap_or(Value::Null),
                                );
                            }
                            obj
                        } else {
                            stripped
                        }
                    })
                    .collect();
                MessageParam {
                    role: "assistant".to_string(),
                    content: MessageContent::Blocks(mapped),
                }
            }
        }
    } else {
        MessageParam {
            role: "assistant".to_string(),
            content: match &message.message.content {
                MessageContent::Text(text) => MessageContent::Text(text.clone()),
                MessageContent::Blocks(blocks) => {
                    MessageContent::Blocks(blocks.iter().map(strip_gemini_provider_metadata_value).collect())
                }
            },
        }
    }
}

/// `stripGeminiProviderMetadata`：剔除 `_geminiThoughtSignature`。
pub fn strip_gemini_provider_metadata(block: &ContentBlockParam) -> ContentBlockParam {
    let mut out = block.clone();
    out.fields.remove("_geminiThoughtSignature");
    out
}

fn strip_gemini_provider_metadata_value(block: &Value) -> Value {
    match block {
        Value::Object(map) => {
            let mut map = map.clone();
            map.remove("_geminiThoughtSignature");
            Value::Object(map)
        }
        other => other.clone(),
    }
}

/// `stripExcessMediaItems`：保证 media（image/document）块数 ≤ limit，
/// 优先剥离最旧的（含 tool_result 内嵌的 media）。
pub fn strip_excess_media_items(
    messages: &[Message],
    limit: u32,
) -> Vec<Message> {
    let mut to_remove: i64 = 0;
    for msg in messages {
        let Message::User(m) = msg else {
            continue;
        };
        let MessageContent::Blocks(blocks) = &m.message.content else {
            continue;
        };
        for block in blocks {
            if is_media_value(block) {
                to_remove += 1;
            }
            if is_tool_result_value(block) {
                if let Some(nested) = block.get("content").and_then(Value::as_array) {
                    to_remove += i64::try_from(nested.iter().filter(|b| is_media_value(b)).count()).unwrap_or(0);
                }
            }
        }
    }
    to_remove -= i64::from(limit);
    if to_remove <= 0 {
        return messages.to_vec();
    }

    messages
        .iter()
        .map(|msg| {
            if to_remove <= 0 {
                return msg.clone();
            }
            let Message::User(m) = msg else {
                return msg.clone();
            };
            let MessageContent::Blocks(content) = &m.message.content else {
                return msg.clone();
            };
            let before = to_remove;

            let stripped: Vec<Value> = content
                .iter()
                .map(|block| {
                    if to_remove <= 0 || !is_tool_result_value(block) {
                        return block.clone();
                    }
                    let Some(nested) = block.get("content").and_then(Value::as_array) else {
                        return block.clone();
                    };
                    let filtered: Vec<Value> = nested
                        .iter()
                        .filter(|n| {
                            if to_remove > 0 && is_media_value(n) {
                                to_remove -= 1;
                                false
                            } else {
                                true
                            }
                        })
                        .collect();
                    if filtered.len() == nested.len() {
                        block.clone()
                    } else {
                        let mut obj = match block {
                            Value::Object(map) => map.clone(),
                            _ => Map::new(),
                        };
                        obj.insert("content".to_string(), Value::Array(filtered));
                        Value::Object(obj)
                    }
                })
                .filter(|block| {
                    if to_remove > 0 && is_media_value(block) {
                        to_remove -= 1;
                        false
                    } else {
                        true
                    }
                })
                .collect();

            if before == to_remove {
                msg.clone().into()
            } else {
                Message::User(UserMessage {
                    message: UserMessageBody {
                        content: MessageContent::Blocks(stripped),
                    },
                    typ: m.typ.clone(),
                    uuid: m.uuid.clone(),
                    timestamp: m.timestamp.clone(),
                    is_meta: m.is_meta,
                })
            }
        })
        .collect()
}

/// `isMedia`（按 JSON 形状判定）。
pub fn is_media(block: &ContentBlockParam) -> bool {
    block.is_media()
}

fn is_media_value(block: &Value) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .map(|t| t == "image" || t == "document")
        .unwrap_or(false)
}

fn is_tool_result_value(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_result")
        && block.get("tool_use_id").is_some()
}

/// `addCacheBreakpoints`：给最后一条（或倒数第二条，skipCacheWrite）消息
/// 加 `cache_control`，并按 cached-microcompact 需求插入 `cache_edits` 与
/// `cache_reference`。
pub fn add_cache_breakpoints(
    messages: &[Message],
    enable_prompt_caching: bool,
    query_source: Option<&str>,
    use_cached_mc: bool,
    new_cache_edits: Option<&CacheEditsBlock>,
    pinned_edits: &[PinnedCacheEdits],
    skip_cache_write: bool,
) -> Vec<MessageParam> {
    // 每请求仅一个消息级 cache_control 标记；skipCacheWrite 时移到倒数第二条。
    let marker_index = if skip_cache_write {
        messages.len().saturating_sub(2)
    } else {
        messages.len().saturating_sub(1)
    };
    let mut result: Vec<MessageParam> = messages
        .iter()
        .enumerate()
        .map(|(index, msg)| {
            let add_cache = index == marker_index;
            match msg {
                Message::User(m) => user_message_to_message_param(
                    m,
                    add_cache,
                    enable_prompt_caching,
                    query_source,
                ),
                Message::Assistant(a) => assistant_message_to_message_param(
                    a,
                    add_cache,
                    enable_prompt_caching,
                    query_source,
                ),
            }
        })
        .collect();

    if !use_cached_mc {
        return result;
    }

    // cache_edits 去重集合（跨块防止重复删除同一 cache_reference）。
    let mut seen_delete_refs: Vec<String> = Vec::new();

    let deduplicate_edits = |block: &CacheEditsBlock, seen: &mut Vec<String>| {
        let unique_edits: Vec<CacheEdit> = block
            .edits
            .iter()
            .filter(|edit| {
                if seen.contains(&edit.cache_reference) {
                    return false;
                }
                seen.push(edit.cache_reference.clone());
                true
            })
            .cloned()
            .collect();
        CacheEditsBlock {
            type_field: block.type_field.clone(),
            edits: unique_edits,
        }
    };

    // 重新插入此前 pinned 的 cache_edits（保持原位置）。
    for pinned in pinned_edits {
        let Some(msg) = result.get_mut(pinned.user_message_index) else {
            continue;
        };
        if msg.role != "user" {
            continue;
        }
        ensure_blocks_content(msg);
        let deduped = deduplicate_edits(&pinned.block, &mut seen_delete_refs);
        if !deduped.edits.is_empty() {
            insert_block_after_tool_results_msg(msg, &deduped);
        }
    }

    // 新 cache_edits 插入最后一条 user 消息。
    if let Some(new_cache_edits) = new_cache_edits {
        let deduped_new = deduplicate_edits(new_cache_edits, &mut seen_delete_refs);
        if !deduped_new.edits.is_empty() {
            for i in (0..result.len()).rev() {
                if result[i].role == "user" {
                    ensure_blocks_content(&mut result[i]);
                    insert_block_after_tool_results_msg(&mut result[i], &deduped_new);
                    break;
                }
            }
        }
    }

    // cached prefix 内的 tool_result 加 cache_reference（在 cache_edits 之后）。
    if enable_prompt_caching {
        let mut last_cc_msg: isize = -1;
        for (i, msg) in result.iter().enumerate() {
            if let MessageContent::Blocks(blocks) = &msg.content {
                if blocks.iter().any(|b| b.get("cache_control").is_some()) {
                    last_cc_msg = i as isize;
                }
            }
        }
        if last_cc_msg >= 0 {
            for i in 0..last_cc_msg as usize {
                if result[i].role != "user" {
                    continue;
                }
                let MessageContent::Blocks(blocks) = &mut result[i].content else {
                    continue;
                };
                let mut cloned = false;
                for j in 0..blocks.len() {
                    if is_tool_result_value(&blocks[j]) {
                        if !cloned {
                            *blocks = blocks.clone();
                            cloned = true;
                        }
                        let mut obj = match &blocks[j] {
                            Value::Object(map) => map.clone(),
                            _ => Map::new(),
                        };
                        if let Some(tool_use_id) = obj.get("tool_use_id").and_then(Value::as_str) {
                            obj.insert(
                                "cache_reference".to_string(),
                                Value::String(tool_use_id.to_string()),
                            );
                        }
                        blocks[j] = Value::Object(obj);
                    }
                }
            }
        }
    }

    result
}

/// `CachedMCEditsBlock`：cache editing 的删除指令块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEditsBlock {
    #[serde(rename = "type")]
    pub type_field: String, // "cache_edits"
    pub edits: Vec<CacheEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEdit {
    #[serde(rename = "type")]
    pub type_field: String, // "delete"
    pub cache_reference: String,
}

/// `CachedMCPinnedEdits`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedCacheEdits {
    pub user_message_index: usize,
    pub block: CacheEditsBlock,
}

fn ensure_blocks_content(msg: &mut MessageParam) {
    if let MessageContent::Text(text) = &msg.content {
        msg.content = MessageContent::Blocks(vec![json!({
            "type": "text",
            "text": text,
        })]);
    }
}

/// `insertBlockAfterToolResults`（`utils/contentArray.ts` 最小等价）：
/// 将 cache_edits 块插入到最后一个 tool_result 之后（无 tool_result 则末尾）。
fn insert_block_after_tool_results_msg(msg: &mut MessageParam, block: &CacheEditsBlock) {
    let MessageContent::Blocks(blocks) = &mut msg.content else {
        return;
    };
    let block_value = serde_json::to_value(block).unwrap_or(Value::Null);
    let insert_at = blocks
        .iter()
        .rposition(|b| is_tool_result_value(b))
        .map(|i| i + 1)
        .unwrap_or_else(|| blocks.len());
    blocks.insert(insert_at, block_value);
}

/// `getPreviousRequestIdFromMessages`：逆序找最后一条带 requestId 的助手消息。
pub fn get_previous_request_id_from_messages(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find_map(|m| m.request_id().map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------
// usage 累计
// ---------------------------------------------------------------------------

/// `updateUsage`：以流式事件 usage 更新累计 usage。
///
/// 输入类 token（input/cache_creation/cache_read）只在事件给出 >0 值时
/// 覆盖（message_delta 可能显式补 0）；output_tokens 使用最新值。
pub fn update_usage(usage: &NonNullableUsage, part_usage: Option<&BetaMessageDeltaUsage>) -> NonNullableUsage {
    let mut out = usage.clone();
    let Some(part) = part_usage else {
        return out;
    };
    if let Some(input_tokens) = part.input_tokens {
        if input_tokens > 0 {
            out.input_tokens = input_tokens;
        }
    }
    if let Some(cache_creation) = part.cache_creation_input_tokens {
        if cache_creation > 0 {
            out.cache_creation_input_tokens = cache_creation;
        }
    }
    if let Some(cache_read) = part.cache_read_input_tokens {
        if cache_read > 0 {
            out.cache_read_input_tokens = cache_read;
        }
    }
    out.output_tokens = part.output_tokens.unwrap_or(out.output_tokens);
    if let Some(server_tool_use) = &part.server_tool_use {
        out.server_tool_use.web_search_requests = server_tool_use.web_search_requests;
        out.server_tool_use.web_fetch_requests = server_tool_use.web_fetch_requests;
    }
    if let Some(cache_creation) = part.cache_creation() {
        out.cache_creation.ephemeral_1h_input_tokens = cache_creation.ephemeral_1h_input_tokens;
        out.cache_creation.ephemeral_5m_input_tokens = cache_creation.ephemeral_5m_input_tokens;
    }
    // cache_deleted_input_tokens：API cache editing 的真实返回字段（>0 才覆盖）。
    if feature("CACHED_MICROCOMPACT") {
        if let Some(deleted) = part.field_i64("cache_deleted_input_tokens") {
            if deleted != 0 && deleted > 0 {
                out.cache_deleted_input_tokens = Some(deleted);
            }
        }
    }
    if let Some(iterations) = &part.iterations {
        out.iterations = iterations.clone();
    }
    if let Some(speed) = &part.speed {
        out.speed = speed.clone();
    }
    out
}

/// `accumulateUsage`：跨多个助手回合累计 usage。
pub fn accumulate_usage(total: &NonNullableUsage, message_usage: &NonNullableUsage) -> NonNullableUsage {
    let mut out = NonNullableUsage {
        input_tokens: total.input_tokens + message_usage.input_tokens,
        cache_creation_input_tokens: total.cache_creation_input_tokens
            + message_usage.cache_creation_input_tokens,
        cache_read_input_tokens: total.cache_read_input_tokens + message_usage.cache_read_input_tokens,
        output_tokens: total.output_tokens + message_usage.output_tokens,
        server_tool_use: ServerToolUsage {
            web_search_requests: total.server_tool_use.web_search_requests
                + message_usage.server_tool_use.web_search_requests,
            web_fetch_requests: total.server_tool_use.web_fetch_requests
                + message_usage.server_tool_use.web_fetch_requests,
        },
        // 最新的 service_tier / inference_geo / iterations / speed 覆盖旧值。
        service_tier: message_usage.service_tier.clone(),
        cache_creation: CacheCreationBreakdown {
            ephemeral_1h_input_tokens: total.cache_creation.ephemeral_1h_input_tokens
                + message_usage.cache_creation.ephemeral_1h_input_tokens,
            ephemeral_5m_input_tokens: total.cache_creation.ephemeral_5m_input_tokens
                + message_usage.cache_creation.ephemeral_5m_input_tokens,
        },
        inference_geo: message_usage.inference_geo.clone(),
        iterations: message_usage.iterations.clone(),
        speed: message_usage.speed.clone(),
        cache_deleted_input_tokens: None,
    };
    if feature("CACHED_MICROCOMPACT") {
        out.cache_deleted_input_tokens = Some(
            total.cache_deleted_input_tokens.unwrap_or(0)
                + message_usage.cache_deleted_input_tokens.unwrap_or(0),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// betas（utils/betas.ts 的最小等价实现）
// ---------------------------------------------------------------------------

/// `shouldIncludeFirstPartyOnlyBetas`。
pub fn should_include_first_party_only_betas() -> bool {
    matches!(get_api_provider(), ApiProvider::FirstParty | ApiProvider::Foundry)
        && !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS").ok().as_deref())
        && is_first_party_anthropic_base_url()
}

/// `shouldUseGlobalCacheScope`（Foundry 除外：GrowthBook 未覆盖）。
pub fn should_use_global_cache_scope() -> bool {
    get_api_provider() == ApiProvider::FirstParty
        && !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS").ok().as_deref())
}

/// `modelSupportsStructuredOutputs`。
pub fn model_supports_structured_outputs(model: &str) -> bool {
    let canonical = canonical_name(model);
    if !matches!(get_api_provider(), ApiProvider::FirstParty | ApiProvider::Foundry) {
        return false;
    }
    canonical.contains("claude-sonnet-4-6")
        || canonical.contains("claude-sonnet-4-5")
        || canonical.contains("claude-opus-4-1")
        || canonical.contains("claude-opus-4-5")
        || canonical.contains("claude-opus-4-6")
        || canonical.contains("claude-opus-4-7")
        || canonical.contains("claude-haiku-4-5")
}

/// `modelSupportsISP`（interleaved thinking）。
pub fn model_supports_isp(model: &str) -> bool {
    if let Some(v) = get_3p_model_capability_override(model, ModelCapabilityOverride::InterleavedThinking) {
        return v;
    }
    let canonical = canonical_name(model);
    match get_api_provider() {
        ApiProvider::Foundry => true,
        ApiProvider::FirstParty => !canonical.contains("claude-3-"),
        _ => canonical.contains("claude-opus-4") || canonical.contains("claude-sonnet-4"),
    }
}

/// `modelSupportsContextManagement`。
pub fn model_supports_context_management(model: &str) -> bool {
    let canonical = canonical_name(model);
    match get_api_provider() {
        ApiProvider::Foundry => true,
        ApiProvider::FirstParty => !canonical.contains("claude-3-"),
        _ => canonical.contains("claude-opus-4")
            || canonical.contains("claude-sonnet-4")
            || canonical.contains("claude-haiku-4"),
    }
}

/// `modelSupportsEffort`（utils/effort.ts）。
pub fn model_supports_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if is_env_truthy(env::var("CLAUDE_CODE_ALWAYS_ENABLE_EFFORT").ok().as_deref()) {
        return true;
    }
    if let Some(v) = get_3p_model_capability_override(model, ModelCapabilityOverride::Effort) {
        return v;
    }
    if m.contains("opus-4-7")
        || m.contains("opus-4-6")
        || m.contains("sonnet-4-6")
        || m.contains("deepseek-v4-pro")
    {
        return true;
    }
    if m.contains("haiku") || m.contains("sonnet") || m.contains("opus") {
        return false;
    }
    // 未知模型：1P 默认支持。
    get_api_provider() == ApiProvider::FirstParty
}

/// `modelSupportsThinking`（utils/thinking.ts）。
pub fn model_supports_thinking(model: &str) -> bool {
    if let Some(v) = get_3p_model_capability_override(model, ModelCapabilityOverride::Thinking) {
        return v;
    }
    let canonical = canonical_name(model);
    match get_api_provider() {
        ApiProvider::Foundry | ApiProvider::FirstParty => !canonical.contains("claude-3-"),
        _ => canonical.contains("sonnet-4") || canonical.contains("opus-4"),
    }
}

/// `modelSupportsAdaptiveThinking`。
pub fn model_supports_adaptive_thinking(model: &str) -> bool {
    if let Some(v) =
        get_3p_model_capability_override(model, ModelCapabilityOverride::AdaptiveThinking)
    {
        return v;
    }
    let canonical = canonical_name(model);
    if canonical.contains("opus-4-7")
        || canonical.contains("opus-4-6")
        || canonical.contains("sonnet-4-6")
    {
        return true;
    }
    if canonical.contains("opus") || canonical.contains("sonnet") || canonical.contains("haiku") {
        return false;
    }
    // 未知模型默认支持（对齐 TS 的 1P 实验默认）。
    get_api_provider() == ApiProvider::FirstParty
}

/// `getAllModelBetas`（growthbook/statsig 门控部分按未启用处理）。
pub fn get_all_model_betas(model: &str) -> Vec<String> {
    let mut beta_headers: Vec<String> = Vec::new();
    let is_haiku = canonical_name(model).contains("haiku");
    let provider = get_api_provider();
    let include_first_party_only_betas = should_include_first_party_only_betas();

    if !is_haiku {
        beta_headers.push(CLAUDE_CODE_20250219_BETA_HEADER.to_string());
        let cli_internal = crate::constants::betas::cli_internal_beta_header();
        if !cli_internal.is_empty()
            && env::var("USER_TYPE").map(|t| t == "ant").unwrap_or(false)
            && env::var("CLAUDE_CODE_ENTRYPOINT").map(|e| e == "cli").unwrap_or(false)
        {
            beta_headers.push(cli_internal.to_string());
        }
    }
    if is_claude_ai_subscriber() {
        beta_headers.push(crate::constants::oauth::OAUTH_BETA_HEADER.to_string());
    }
    if has_1m_context(model) {
        beta_headers.push(CONTEXT_1M_BETA_HEADER.to_string());
    }
    if !is_env_truthy(env::var("DISABLE_INTERLEAVED_THINKING").ok().as_deref())
        && model_supports_isp(model)
    {
        beta_headers.push(INTERLEAVED_THINKING_BETA_HEADER.to_string());
    }
    if include_first_party_only_betas
        && model_supports_isp(model)
        && !get_is_non_interactive_session()
        && get_initial_settings().show_thinking_summaries != Some(true)
    {
        beta_headers.push(REDACT_THINKING_BETA_HEADER.to_string());
    }
    let tool_clearing_opt_in =
        is_env_truthy(env::var("USE_API_CONTEXT_MANAGEMENT").ok().as_deref())
            || model_supports_context_management(model);
    if should_include_first_party_only_betas()
        && (tool_clearing_opt_in || model_supports_context_management(model))
    {
        beta_headers.push(CONTEXT_MANAGEMENT_BETA_HEADER.to_string());
    }
    // strict tools / token-efficient tools 均 GrowthBook 门控 → 跳过（3P 默认 false）。
    if provider == ApiProvider::Vertex && vertex_model_supports_web_search(model) {
        beta_headers.push(WEB_SEARCH_BETA_HEADER.to_string());
    }
    if provider == ApiProvider::Foundry {
        beta_headers.push(WEB_SEARCH_BETA_HEADER.to_string());
    }
    if include_first_party_only_betas {
        beta_headers.push(PROMPT_CACHING_SCOPE_BETA_HEADER.to_string());
    }
    if let Ok(betas_env) = env::var("ANTHROPIC_BETAS") {
        beta_headers.extend(
            betas_env
                .split(',')
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty()),
        );
    }
    beta_headers
}

fn vertex_model_supports_web_search(model: &str) -> bool {
    let canonical = canonical_name(model);
    canonical.contains("claude-opus-4")
        || canonical.contains("claude-sonnet-4")
        || canonical.contains("claude-haiku-4")
}

/// `getModelBetas`：Bedrock 过滤走 extraBodyParams 的 betas。
pub fn get_model_betas(model: &str) -> Vec<String> {
    let model_betas = get_all_model_betas(model);
    if get_api_provider() == ApiProvider::Bedrock {
        model_betas
            .into_iter()
            .filter(|b| !crate::constants::betas::is_bedrock_extra_params_header(b))
            .collect()
    } else {
        model_betas
    }
}

/// `getBedrockExtraBodyParamsBetas`。
pub fn get_bedrock_extra_body_params_betas(model: &str) -> Vec<String> {
    get_all_model_betas(model)
        .into_iter()
        .filter(|b| crate::constants::betas::is_bedrock_extra_params_header(b))
        .collect()
}

/// `getMergedBetas`：模型 betas + agentic 补充 + SDK betas（去重）。
pub fn get_merged_betas(model: &str, is_agentic_query: bool) -> Vec<String> {
    let mut base_betas = get_model_betas(model);

    if is_agentic_query {
        if !base_betas.contains(&CLAUDE_CODE_20250219_BETA_HEADER.to_string()) {
            base_betas.push(CLAUDE_CODE_20250219_BETA_HEADER.to_string());
        }
        let cli_internal = crate::constants::betas::cli_internal_beta_header();
        if !cli_internal.is_empty()
            && !base_betas.contains(&cli_internal.to_string())
            && env::var("USER_TYPE").map(|t| t == "ant").unwrap_or(false)
            && env::var("CLAUDE_CODE_ENTRYPOINT").map(|e| e == "cli").unwrap_or(false)
        {
            base_betas.push(cli_internal.to_string());
        }
    }

    if let Some(sdk_betas) = get_sdk_betas() {
        for beta in sdk_betas {
            if !base_betas.contains(&beta) {
                base_betas.push(beta);
            }
        }
    }
    base_betas
}

// ---------------------------------------------------------------------------
// 模型字符串辅助（utils/model/model.ts 最小等价）
// ---------------------------------------------------------------------------

/// `normalizeModelStringForAPI`：剥离 `[1m]` / `[2m]` 标记。
pub fn normalize_model_string_for_api(model: &str) -> String {
    regex_strip_context_tag
        .replace_all(model, "")
        .to_string()
}

static regex_strip_context_tag: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"\[(1|2)m\]").unwrap_or_else(|_| regex::Regex::new("").unwrap()));

/// `getCanonicalName` 的最小等价：小写化 + 剥离 `-YYYYMMDD` 日期后缀。
fn canonical_name(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    let lower = regex_strip_date
        .replace_all(&lower, "")
        .to_string();
    lower.trim().to_string()
}

static regex_strip_date: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"-\d{8}").unwrap_or_else(|_| regex::Regex::new("").unwrap()));

fn model_string(key: ModelKey) -> &'static str {
    ALL_MODEL_CONFIGS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, cfg)| cfg.for_provider(get_api_provider()))
        .unwrap_or("")
}

/// `getSmallFastModel`（最小等价：env 覆盖 → 默认 haiku）。
pub fn get_small_fast_model() -> String {
    match get_api_provider() {
        ApiProvider::Openai => env::var("OPENAI_SMALL_FAST_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| model_string(ModelKey::Haiku45).to_string()),
        ApiProvider::Gemini => env::var("GEMINI_SMALL_FAST_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| model_string(ModelKey::Haiku45).to_string()),
        _ => env::var("ANTHROPIC_SMALL_FAST_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| model_string(ModelKey::Haiku45).to_string()),
    }
}

/// `getDefaultSonnetModel`（不含 3P primaryModel 回退）。
pub fn get_default_sonnet_model() -> String {
    if let Ok(env_model) = env::var("ANTHROPIC_DEFAULT_SONNET_MODEL") {
        if !env_model.is_empty() {
            return env_model;
        }
    }
    if get_api_provider() != ApiProvider::FirstParty {
        model_string(ModelKey::Sonnet45).to_string()
    } else {
        model_string(ModelKey::Sonnet46).to_string()
    }
}

/// `getDefaultOpusModel`（不含 3P primaryModel 回退）。
pub fn get_default_opus_model() -> String {
    if let Ok(env_model) = env::var("ANTHROPIC_DEFAULT_OPUS_MODEL") {
        if !env_model.is_empty() {
            return env_model;
        }
    }
    model_string(ModelKey::Opus47).to_string()
}

/// `isNonCustomOpusModel`。
pub fn is_non_custom_opus_model(model: &str) -> bool {
    [
        ModelKey::Opus40,
        ModelKey::Opus41,
        ModelKey::Opus45,
        ModelKey::Opus46,
        ModelKey::Opus47,
    ]
    .iter()
    .any(|key| model == model_string(**key))
}

/// 判断 querySource 为 agentic（`repl_main_thread*` / `agent:*` / sdk / hook / verification）。
pub fn is_agentic_query_source(query_source: &str) -> bool {
    query_source.starts_with("repl_main_thread")
        || query_source.starts_with("agent:")
        || query_source == "sdk"
        || query_source == "hook_agent"
        || query_source == "verification_agent"
}

// ---------------------------------------------------------------------------
// fastMode / searchExtraTools 等未转换模块的最小判定
// ---------------------------------------------------------------------------

/// `isFastModeEnabled`（utils/fastMode.ts 未转换 → 环境变量快速路径）。
pub fn is_fast_mode_enabled() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_FAST_MODE").ok().as_deref())
}

/// `isFastModeAvailable`（未转换 → 与 enabled 一致）。
pub fn is_fast_mode_available() -> bool {
    is_fast_mode_enabled()
}

/// `isFastModeCooldown`（未转换 → 无冷却）。
pub fn is_fast_mode_cooldown() -> bool {
    false
}

/// `isFastModeSupportedByModel`（未转换 → 与 effort 支持判定一致）。
pub fn is_fast_mode_supported_by_model(model: &str) -> bool {
    canonical_name(model).contains("opus-4-7") || canonical_name(model).contains("opus-4-6")
}

/// `shouldDeferLspTool`（LSP manager 未转换 → 不 defer）。
pub fn should_defer_lsp_tool() -> bool {
    false
}

// ---------------------------------------------------------------------------
// 流式事件解析
// ---------------------------------------------------------------------------

/// 从 SSE 原始文本提取 `data:` 负载（跳过 `[DONE]`，多行 data 按 SSE 规范
/// 以 `\n` 连接）。
pub fn extract_sse_data(raw: &str) -> Vec<String> {
    raw.split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim))
                .collect();
            if data.is_empty() {
                None
            } else {
                Some(data.join("\n"))
            }
        })
        .filter(|payload| payload != "[DONE]")
        .collect()
}

/// 解析单个流式事件 JSON。
pub fn parse_stream_event(json_str: &str) -> Result<BetaRawMessageStreamEvent, ApiError> {
    Ok(serde_json::from_str(json_str)?)
}

/// `StreamAccumulator`：queryModel 中 `switch (part.type)` 的累计状态机。
///
/// - `message_start`：记录 partialMessage / ttft / 初始 usage
/// - `content_block_start`：初始化块（text/thinking 清零，tool_use input 置空串）
/// - `content_block_delta`：拼接 text/thinking/input/signature/connector_text
/// - `content_block_stop`：产出单块 AssistantMessage
/// - `message_delta`：更新 usage / stop_reason（并回写会话成本）
#[derive(Debug, Clone)]
pub struct StreamAccumulator {
    start_ms: i64,
    partial_message: Option<BetaMessage>,
    content_blocks: Vec<Option<ContentBlockParam>>,
    text_deltas: HashMap<u32, Vec<String>>,
    usage: NonNullableUsage,
    stop_reason: Option<String>,
    pub ttft_ms: i64,
    advisor_in_progress: bool,
    last_usage: Option<NonNullableUsage>,
    last_stop_reason: Option<String>,
}

/// 累计所需的调用方上下文。
#[derive(Debug, Clone)]
pub struct AccumulateContext<'a> {
    pub model: &'a str,
    pub request_id: Option<&'a str>,
    pub advisor_model: Option<&'a str>,
    /// 用于成本累计与 stop_reason 回写。
    pub resolved_model: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEventOutcome {
    Continue,
    /// content_block_stop 产出的单块助手消息。
    AssistantMessage(AssistantMessage),
    /// message_delta 的最终 stop_reason（调用方应回写已产出的消息）。
    StopReason(String),
}

impl StreamAccumulator {
    pub fn new(start_ms: i64) -> Self {
        Self {
            start_ms,
            partial_message: None,
            content_blocks: Vec::new(),
            text_deltas: HashMap::new(),
            usage: empty_usage(),
            stop_reason: None,
            ttft_ms: 0,
            advisor_in_progress: false,
            last_usage: None,
            last_stop_reason: None,
        }
    }

    pub fn usage(&self) -> &NonNullableUsage {
        &self.usage
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// message_delta 后最新的 usage / stop_reason（回写最后一条已产出消息）。
    pub fn last_message_updates(&self) -> Option<(&NonNullableUsage, &str)> {
        match (&self.last_usage, &self.last_stop_reason) {
            (Some(u), Some(r)) => Some((u, r)),
            _ => None,
        }
    }

    /// 处理一个事件，返回需要产出给调用方的结果。
    pub fn handle(
        &mut self,
        event: &BetaRawMessageStreamEvent,
        ctx: &AccumulateContext,
        now_ms: i64,
    ) -> Result<StreamEventOutcome, ApiError> {
        match event {
            BetaRawMessageStreamEvent::MessageStart { message } => {
                self.partial_message = Some(message.clone());
                self.ttft_ms = now_ms - self.start_ms;
                let part_usage = beta_message_delta_usage_from(&message.usage);
                self.usage = update_usage(&self.usage, Some(&part_usage));
                Ok(StreamEventOutcome::Continue)
            }
            BetaRawMessageStreamEvent::ContentBlockStart { index, content_block } => {
                let mut block = content_block.clone();
                match content_block.typ.as_str() {
                    "tool_use" | "server_tool_use" => {
                        block.set_str("input", "");
                    }
                    "text" => {
                        // SDK 偶尔在 start 里带 text，随后 delta 重复发；清零。
                        block.set_str("text", "");
                        self.text_deltas.insert(*index, Vec::new());
                    }
                    "thinking" => {
                        block.set_str("thinking", "");
                        // 保证 signature 字段存在（signature_delta 可能不来）。
                        block.set_str("signature", "");
                    }
                    "advisor_tool_result" => {
                        self.advisor_in_progress = false;
                    }
                    _ => {}
                }
                self.set_block(*index, block);
                Ok(StreamEventOutcome::Continue)
            }
            BetaRawMessageStreamEvent::ContentBlockDelta { index, delta } => {
                let block = self
                    .get_block_mut(*index)
                    .ok_or(ApiError::ContentBlockNotFound { index: *index })?;
                match delta.typ.as_str() {
                    "connector_text_delta" => {
                        if block.typ != "connector_text" {
                            return Err(ApiError::ContentBlockTypeMismatch {
                                expected: "connector_text",
                                actual: block.typ.clone(),
                            });
                        }
                        if let Some(text) = delta.get_str("connector_text") {
                            block.append_str("connector_text", text);
                        }
                    }
                    "text_delta" => {
                        if block.typ != "text" {
                            return Err(ApiError::ContentBlockTypeMismatch {
                                expected: "text",
                                actual: block.typ.clone(),
                            });
                        }
                        if let Some(text) = delta.text() {
                            self.text_deltas.entry(*index).or_default().push(text.to_string());
                        }
                    }
                    "input_json_delta" => {
                        if block.typ != "tool_use" && block.typ != "server_tool_use" {
                            return Err(ApiError::ContentBlockTypeMismatch {
                                expected: "tool_use",
                                actual: block.typ.clone(),
                            });
                        }
                        if block.get_str("input").is_none() {
                            return Err(ApiError::ContentBlockInputNotString);
                        }
                        if let Some(partial) = delta.partial_json() {
                            if let Some(Value::String(input)) = block.fields.get("input") {
                                let mut input = input.clone();
                                input.push_str(partial);
                                block.set_str("input", input);
                            }
                        }
                    }
                    "thinking_delta" => {
                        if block.typ != "thinking" {
                            return Err(ApiError::ContentBlockTypeMismatch {
                                expected: "thinking",
                                actual: block.typ.clone(),
                            });
                        }
                        if let Some(thinking) = delta.thinking() {
                            block.append_str("thinking", thinking);
                        }
                    }
                    "signature_delta" => {
                        if feature("CONNECTOR_TEXT") && block.typ == "connector_text" {
                            if let Some(signature) = delta.signature() {
                                block.set_str("signature", signature);
                            }
                        } else {
                            if block.typ != "thinking" {
                                return Err(ApiError::ContentBlockTypeMismatch {
                                    expected: "thinking",
                                    actual: block.typ.clone(),
                                });
                            }
                            if let Some(signature) = delta.signature() {
                                block.set_str("signature", signature);
                            }
                        }
                    }
                    // citations_delta 等暂不处理（对齐 TS TODO 分支）。
                    _ => {}
                }
                Ok(StreamEventOutcome::Continue)
            }
            BetaRawMessageStreamEvent::ContentBlockStop { index } => {
                let partial_message = self
                    .partial_message
                    .clone()
                    .ok_or(ApiError::MessageNotFound)?;
                let block = self
                    .take_block(*index)
                    .ok_or(ApiError::ContentBlockNotFound { index: *index })?;
                // text 块合并累积 delta（O(n) join 而非 O(n²) +=）。
                let block = if let Some(deltas) = self.text_deltas.remove(index) {
                    let mut block = block;
                    block.set_str("text", deltas.join(""));
                    block
                } else {
                    block
                };
                let message = AssistantMessage {
                    message: AssistantMessageBody {
                        content: MessageContent::Blocks(vec![normalize_block_for_app(&block)]),
                        model: Some(partial_message.model.clone()),
                        usage: Some(if partial_message.usage == BetaUsage::default() {
                            empty_usage()
                        } else {
                            usage_from_beta(&partial_message.usage)
                        }),
                        stop_reason: partial_message.stop_reason.clone(),
                    },
                    request_id: ctx.request_id.map(|s| s.to_string()),
                    typ: "assistant".to_string(),
                    uuid: Uuid::new_v4().to_string(),
                    timestamp: utc_iso_now(),
                    advisor_model: ctx.advisor_model.map(|s| s.to_string()),
                };
                Ok(StreamEventOutcome::AssistantMessage(message))
            }
            BetaRawMessageStreamEvent::MessageDelta { delta, usage } => {
                self.usage = update_usage(&self.usage, usage.as_ref());
                if let Some(stop_reason) = &delta.stop_reason {
                    self.stop_reason = Some(stop_reason.clone());
                }
                // 成本累计（对齐 TS 在 message_delta 的 calculateUSDCost 调用）。
                let cost = calculate_usd_cost(ctx.resolved_model, &cost_usage_from(&self.usage));
                add_to_total_session_cost(cost, &self.usage, ctx.model);
                self.last_usage = Some(self.usage.clone());
                self.last_stop_reason = self.stop_reason.clone();
                match &self.stop_reason {
                    Some(reason) => Ok(StreamEventOutcome::StopReason(reason.clone())),
                    None => Ok(StreamEventOutcome::Continue),
                }
            }
            BetaRawMessageStreamEvent::MessageStop
            | BetaRawMessageStreamEvent::Ping
            | BetaRawMessageStreamEvent::Unknown => Ok(StreamEventOutcome::Continue),
            BetaRawMessageStreamEvent::Error { error } => Err(ApiError::Http(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream error")
                    .to_string(),
            )),
        }
    }

    /// 流结束校验：`message_start` 缺失或无内容块且无 stop_reason →
    /// `StreamNoEvents`（触发非流式回退的条件，对齐 TS 抛错点）。
    pub fn finish(&self, produced_messages: usize) -> Result<(), ApiError> {
        if self.partial_message.is_none() || (produced_messages == 0 && self.stop_reason.is_none()) {
            Err(ApiError::StreamNoEvents)
        } else {
            Ok(())
        }
    }

    fn set_block(&mut self, index: u32, block: ContentBlockParam) {
        let idx = index as usize;
        if self.content_blocks.len() <= idx {
            self.content_blocks.resize_with(idx + 1, || None);
        }
        self.content_blocks[idx] = Some(block);
    }

    fn get_block_mut(&mut self, index: u32) -> Option<&mut ContentBlockParam> {
        self.content_blocks
            .get_mut(index as usize)
            .and_then(|b| b.as_mut())
    }

    fn take_block(&mut self, index: u32) -> Option<ContentBlockParam> {
        self.content_blocks
            .get_mut(index as usize)
            .and_then(|b| b.take())
    }
}

fn add_to_total_session_cost(cost: f64, usage: &NonNullableUsage, model: &str) {
    let model_usage = ModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        web_search_requests: usage.server_tool_use.web_search_requests,
        cost_usd: cost,
        context_window: 0,
        max_output_tokens: 0,
    };
    crate::bootstrap::state::add_to_total_cost_state(cost, model_usage, model);
}

fn cost_usage_from(usage: &NonNullableUsage) -> CostUsage {
    CostUsage {
        input_tokens: usage.input_tokens as f64,
        output_tokens: usage.output_tokens as f64,
        cache_read_input_tokens: Some(usage.cache_read_input_tokens as f64),
        cache_creation_input_tokens: Some(usage.cache_creation_input_tokens as f64),
        web_search_requests: Some(usage.server_tool_use.web_search_requests as f64),
        speed: Some(usage.speed.clone()),
    }
}

fn usage_from_beta(usage: &BetaUsage) -> NonNullableUsage {
    NonNullableUsage {
        input_tokens: usage.input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens,
        server_tool_use: usage.server_tool_use.clone().unwrap_or_default(),
        service_tier: usage.service_tier.clone().unwrap_or_else(|| "standard".to_string()),
        cache_creation: usage.cache_creation.clone().unwrap_or_default(),
        inference_geo: String::new(),
        iterations: Vec::new(),
        speed: usage.speed.clone().unwrap_or_else(|| "standard".to_string()),
        cache_deleted_input_tokens: usage
            .extra
            .get("cache_deleted_input_tokens")
            .and_then(Value::as_i64),
    }
}

fn beta_message_delta_usage_from(usage: &BetaUsage) -> BetaMessageDeltaUsage {
    BetaMessageDeltaUsage {
        input_tokens: Some(usage.input_tokens),
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        output_tokens: Some(usage.output_tokens),
        server_tool_use: usage.server_tool_use.clone(),
        iterations: None,
        speed: usage.speed.clone(),
        extra: usage.extra.clone(),
    }
}

/// `normalizeContentFromAPI` 的 tool_use 分支最小等价：
/// 累计的 input 字符串解析为 JSON，失败回退 `{}`。
fn normalize_block_for_app(block: &ContentBlockParam) -> Value {
    let mut value = serde_json::to_value(block).unwrap_or(Value::Null);
    if block.typ == "tool_use" || block.typ == "server_tool_use" {
        if let Value::Object(map) = &mut value {
            if let Some(Value::String(input)) = map.get("input") {
                let parsed = if input.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(input).unwrap_or_else(|_| Value::Object(Map::new()))
                };
                map.insert("input".to_string(), parsed);
            }
        }
    }
    value
}

fn utc_iso_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// HTTP 请求函数（需要 reqwest；当前为签名占位）
// ---------------------------------------------------------------------------

/// `queryModel`：流式请求 + 重试 + 非流式回退的完整生成器。
///
/// 需要 reqwest：创建 Anthropic 客户端、发送 streaming 请求、
/// 与 withRetry / VCR / langfuse 集成。
pub async fn query_model(
    _messages: &[Message],
    _system_prompt: &SystemPrompt,
    _thinking_config: &ThinkingConfig,
    _tools: &[ToolParam],
    _options: &Options,
) -> Result<(), ApiError> {
    Err(ApiError::HttpClientUnavailable("query_model"))
}

/// `queryModelWithStreaming`：流式生成器入口（yield StreamEvent /
/// AssistantMessage / SystemAPIErrorMessage）。
pub async fn query_model_with_streaming(
    _messages: &[Message],
    _system_prompt: &SystemPrompt,
    _thinking_config: &ThinkingConfig,
    _tools: &[ToolParam],
    _options: &Options,
) -> Result<(), ApiError> {
    Err(ApiError::HttpClientUnavailable("query_model_with_streaming"))
}

/// `queryModelWithoutStreaming`：消费完整流并返回最终 AssistantMessage。
pub async fn query_model_without_streaming(
    _messages: &[Message],
    _system_prompt: &SystemPrompt,
    _thinking_config: &ThinkingConfig,
    _tools: &[ToolParam],
    _options: &Options,
) -> Result<AssistantMessage, ApiError> {
    Err(ApiError::HttpClientUnavailable("query_model_without_streaming"))
}

/// `executeNonStreamingRequest`：流式失败后的非流式重试请求。
pub async fn execute_non_streaming_request(
    _client_options: NonStreamingClientOptions,
    _retry_options: NonStreamingRetryOptions<'_>,
) -> Result<BetaMessage, ApiError> {
    Err(ApiError::HttpClientUnavailable("execute_non_streaming_request"))
}

/// `verifyApiKey`：用 small fast model 发一次最小请求验证 API key。
pub async fn verify_api_key(_api_key: &str, is_non_interactive_session: bool) -> Result<bool, ApiError> {
    if is_non_interactive_session {
        return Ok(true);
    }
    Err(ApiError::HttpClientUnavailable("verify_api_key"))
}

/// `queryHaiku`：用 small fast model 发非流式单轮请求。
pub async fn query_haiku(
    _user_prompt: &str,
    _system_prompt: &SystemPrompt,
    _output_format: Option<&JsonOutputFormat>,
    _options: &Options,
) -> Result<AssistantMessage, ApiError> {
    Err(ApiError::HttpClientUnavailable("query_haiku"))
}

/// `queryWithModel`：经完整 query 管道查询指定模型。
pub async fn query_with_model(
    _user_prompt: &str,
    _system_prompt: &SystemPrompt,
    _output_format: Option<&JsonOutputFormat>,
    _options: &Options,
) -> Result<AssistantMessage, ApiError> {
    Err(ApiError::HttpClientUnavailable("query_with_model"))
}

/// `executeNonStreamingRequest` 的客户端参数。
#[derive(Debug, Clone)]
pub struct NonStreamingClientOptions {
    pub model: String,
    pub source: String,
}

/// `executeNonStreamingRequest` 的重试参数。
#[derive(Debug, Clone)]
pub struct NonStreamingRetryOptions<'a> {
    pub model: &'a str,
    pub fallback_model: Option<&'a str>,
    pub thinking_config: &'a ThinkingConfig,
    pub fast_mode: bool,
    pub initial_consecutive_529_errors: u32,
    pub query_source: &'a QuerySource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_usage_ignores_zero_input_tokens() {
        let base = empty_usage();
        let part = BetaMessageDeltaUsage {
            input_tokens: Some(0),
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            output_tokens: Some(42),
            ..Default::default()
        };
        let updated = update_usage(&base, Some(&part));
        assert_eq!(updated.output_tokens, 42);
        assert_eq!(updated.input_tokens, 0); // 不被 0 覆盖
    }

    #[test]
    fn adjust_params_caps_thinking_budget() {
        let mut params = MessageStreamParams {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![],
            system: vec![],
            tools: vec![],
            tool_choice: None,
            betas: None,
            metadata: None,
            max_tokens: 100_000,
            thinking: Some(ThinkingParam::Enabled { budget_tokens: 90_000 }),
            temperature: None,
            context_management: None,
            output_config: None,
            speed: None,
            extra_body: Map::new(),
        };
        adjust_params_for_non_streaming(&mut params, MAX_NON_STREAMING_TOKENS);
        assert_eq!(params.max_tokens, MAX_NON_STREAMING_TOKENS);
        match params.thinking {
            Some(ThinkingParam::Enabled { budget_tokens }) => {
                assert_eq!(budget_tokens, MAX_NON_STREAMING_TOKENS - 1);
            }
            _ => panic!("expected enabled thinking"),
        }
    }

    #[test]
    fn sse_extraction_skips_done() {
        let raw = "event: message_start\ndata: {\"type\":\"message_start\"}\n\ndata: [DONE]\n\n";
        let payloads = extract_sse_data(raw);
        assert_eq!(payloads, vec!["{\"type\":\"message_start\"}"]);
    }

    #[test]
    fn parses_message_start_and_round_trips() {
        let json = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#;
        let event = parse_stream_event(json).expect("parse");
        match event {
            BetaRawMessageStreamEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_1");
                assert_eq!(message.usage.input_tokens, 10);
            }
            _ => panic!("expected message_start"),
        }
    }

    #[test]
    fn strips_media_items_beyond_limit() {
        let mk_msg = |i: usize| {
            Message::User(UserMessage {
                message: UserMessageBody {
                    content: MessageContent::Blocks(vec![json!({
                        "type": "image",
                        "source": {"data": i.to_string()},
                    })]),
                },
                typ: "user".to_string(),
                uuid: i.to_string(),
                timestamp: String::new(),
                is_meta: None,
            })
        };
        let messages = vec![mk_msg(0), mk_msg(1), mk_msg(2)];
        let stripped = strip_excess_media_items(&messages, 1);
        let remaining = stripped
            .into_iter()
            .filter_map(|m| match m {
                Message::User(u) => match u.message.content {
                    MessageContent::Blocks(b) => Some(b.len()),
                    _ => None,
                },
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(remaining, 1); // 最旧的两张被剥离
    }
}
