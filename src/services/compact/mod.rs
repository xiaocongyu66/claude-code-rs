//! `services/compact` — 旧压缩实现（auto compact / microcompact / snip）。
//!
//! 本目录是 TS `src/services/compact/` 的 Rust 重写。模块根额外承载：
//! - `message` 鸭子类型模型：`types/message.ts` 未转换，这里定义最小可用的
//!   `Message` 联合（user/assistant/system/attachment），字段名与 wire 层
//!   camelCase 对齐；compaction2 亦复用本模型（TS 侧两处都 import 同一
//!   types/message.js）。
//! - `utils/messages.ts`（未转换）中压缩链需要的消息工具函数的本地等价物
//!   （createUserMessage / createCompactBoundaryMessage /
//!   isCompactBoundaryMessage / getMessagesAfterCompactBoundary /
//!   getAssistantMessageText / getLastAssistantMessage /
//!   normalizeMessagesForAPI 透传）。
//! - `services/analytics`（未转换）的 `logEvent` / GrowthBook
//!   `getFeatureValue_CACHED_MAY_BE_STALE` 占位（默认值语义见各调用点）。
//! - `utils/forkedAgent.ts`（未转换）的 `CacheSafeParams` 最小 duck-type。

pub mod api_microcompact;
pub mod auto_compact;
pub mod cached_microcompact;
pub mod compact;
pub mod compact_warning_hook;
pub mod compact_warning_state;
pub mod grouping;
pub mod micro_compact;
pub mod post_compact_cleanup;
pub mod prompt;
pub mod reactive_compact;
pub mod session_memory_compact;
pub mod snip_compact;
pub mod snip_projection;
pub mod time_based_mc_config;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// 鸭子类型 Message 模型（types/message.ts 未转换）
// ---------------------------------------------------------------------------

/// 消息内容：字符串或内容块数组（块以 `serde_json::Value` 保留原始 wire
/// 结构——块形状随 provider 变化，且压缩链只做结构判定/复制）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

impl Content {
    /// 是否为数组形式。
    pub fn is_blocks(&self) -> bool {
        matches!(self, Content::Blocks(_))
    }

    /// 块数组引用（字符串内容返回 None）。
    pub fn blocks(&self) -> Option<&Vec<Value>> {
        match self {
            Content::Blocks(blocks) => Some(blocks),
            Content::Text(_) => None,
        }
    }

    /// 块数组可变引用。
    pub fn blocks_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Content::Blocks(blocks) => Some(blocks),
            Content::Text(_) => None,
        }
    }

    /// 字符串内容引用。
    pub fn text(&self) -> Option<&str> {
        match self {
            Content::Text(s) => Some(s),
            Content::Blocks(_) => None,
        }
    }
}

/// `SystemCompactBoundaryMessage.compactMetadata`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_tokens: Option<i64>,
    /// `annotateBoundaryWithPreservedSegment` 写入的保留段链信息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegment>,
    /// 压缩前已发现的延迟工具名（compaction2 方法盖章携带）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_compact_discovered_tools: Option<Vec<String>>,
    /// compaction2 方法名（display.ts 盖章）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// remote 方法标记。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_saved: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `compactMetadata.preservedSegment`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreservedSegment {
    pub head_uuid: String,
    pub anchor_uuid: String,
    pub tail_uuid: String,
}

/// `snipMetadata`（snip 边界标记）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnipMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_uuids: Option<Vec<String>>,
}

/// `summarizeMetadata`（partial compact 摘要消息携带）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarizeMetadata {
    pub messages_summarized: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    pub direction: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageBody {
    pub role: String,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub uuid: String,
    pub timestamp: String,
    pub message: UserMessageBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_meta: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_compact_summary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_visible_in_transcript_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_result: Option<Value>,
    /// partial compact 摘要消息携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize_metadata: Option<SummarizeMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantApiMessage {
    /// API 响应 id（API-round 分组边界依据）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// provider usage 块原样保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub uuid: String,
    pub timestamp: String,
    pub message: AssistantApiMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_api_error_message: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub uuid: String,
    pub timestamp: String,
    /// `compact_boundary` / `snip_boundary` / `informational` / `snip_marker`…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_meta: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_metadata: Option<CompactMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snip_metadata: Option<SnipMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentMessage {
    pub uuid: String,
    pub timestamp: String,
    /// 附件体原样保留（file / plan_file_reference / invoked_skills /
    /// task_status / hook_* / …）。
    pub attachment: Value,
}

/// TS `Message` 联合（user / assistant / system / attachment；hook 结果在
/// TS 里通常也是 attachment 消息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    System(SystemMessage),
    Attachment(AttachmentMessage),
}

impl Message {
    pub fn typ(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::System(_) => "system",
            Message::Attachment(_) => "attachment",
        }
    }

    pub fn uuid(&self) -> &str {
        match self {
            Message::User(m) => &m.uuid,
            Message::Assistant(m) => &m.uuid,
            Message::System(m) => &m.uuid,
            Message::Attachment(m) => &m.uuid,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Message::User(m) => &m.timestamp,
            Message::Assistant(m) => &m.timestamp,
            Message::System(m) => &m.timestamp,
            Message::Attachment(m) => &m.timestamp,
        }
    }

    /// user/assistant 内层消息内容。
    pub fn content(&self) -> Option<&Content> {
        match self {
            Message::User(m) => Some(&m.message.content),
            Message::Assistant(m) => Some(&m.message.content),
            _ => None,
        }
    }

    pub fn content_mut(&mut self) -> Option<&mut Content> {
        match self {
            Message::User(m) => Some(&mut m.message.content),
            Message::Assistant(m) => Some(&mut m.message.content),
            _ => None,
        }
    }

    /// assistant 的 API 响应 id（不含 SYNTHETIC 语义，调用方自行过滤）。
    pub fn assistant_api_id(&self) -> Option<&str> {
        match self {
            Message::Assistant(m) => m.message.id.as_deref(),
            _ => None,
        }
    }

    pub fn is_compact_summary(&self) -> bool {
        match self {
            Message::User(m) => m.is_compact_summary.unwrap_or(false),
            _ => false,
        }
    }

    pub fn is_meta(&self) -> bool {
        match self {
            Message::User(m) => m.is_meta.unwrap_or(false),
            Message::System(m) => m.is_meta.unwrap_or(false),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// utils/messages.ts 的本地等价物（未转换依赖）
// ---------------------------------------------------------------------------

pub fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// TS `createUserMessage({ content, isMeta?, isCompactSummary?,
/// isVisibleInTranscriptOnly? })`。
pub fn create_user_message(
    content: &str,
    is_meta: bool,
    is_compact_summary: bool,
    is_visible_in_transcript_only: bool,
) -> Message {
    Message::User(UserMessage {
        uuid: uuid_v4(),
        timestamp: now_ts(),
        message: UserMessageBody {
            role: "user".into(),
            content: Content::Text(content.to_string()),
        },
        is_meta: if is_meta { Some(true) } else { None },
        is_compact_summary: if is_compact_summary { Some(true) } else { None },
        is_visible_in_transcript_only: if is_visible_in_transcript_only {
            Some(true)
        } else {
            None
        },
        parent_uuid: None,
        tool_use_result: None,
        summarize_metadata: None,
    })
}

/// TS `createCompactBoundaryMessage(trigger, preTokens, lastPreCompactUuid?,
/// userFeedback?, messagesSummarized?)`。TS 实现见 utils/messages.ts（未
/// 转换）：system + subtype=compact_boundary + compactMetadata，userFeedback
/// 与 messagesSummarized 为 partial compact 扩展字段。
pub fn create_compact_boundary_message(
    trigger: &str,
    pre_tokens: i64,
    last_pre_compact_uuid: Option<&str>,
    user_feedback: Option<&str>,
    messages_summarized: Option<usize>,
) -> SystemMessage {
    let mut metadata = CompactMetadata {
        trigger: Some(trigger.to_string()),
        pre_tokens: Some(pre_tokens),
        ..Default::default()
    };
    if let Some(fb) = user_feedback {
        metadata.warning = Some(fb.to_string());
    }
    if let Some(count) = messages_summarized {
        // 与 TS 一致：partial compact 把 userFeedback/messagesSummarized 放
        // 在 summarizeMetadata 侧（见 sessionMemoryCompact 的用法），边界
        // 本体只保留 trigger/preTokens。这里保留位置以便未来对齐。
        metadata.tokens_saved = Some(count as i64);
    }
    SystemMessage {
        uuid: uuid_v4(),
        timestamp: now_ts(),
        subtype: Some("compact_boundary".to_string()),
        content: "Conversation compacted".to_string(),
        level: Some("info".to_string()),
        is_meta: Some(false),
        compact_metadata: Some(metadata),
        snip_metadata: None,
        logical_parent_uuid: last_pre_compact_uuid.map(|s| s.to_string()),
    }
}

/// TS `isCompactBoundaryMessage(message)`。
pub fn is_compact_boundary_message(message: &Message) -> bool {
    match message {
        Message::System(m) => m.subtype.as_deref() == Some("compact_boundary"),
        _ => false,
    }
}

/// TS `getMessagesAfterCompactBoundary(messages)`：从最后一条压缩边界之后
/// 截取（边界本身不保留）。未找到边界时返回全集。
pub fn get_messages_after_compact_boundary(messages: &[Message]) -> Vec<Message> {
    let mut idx = None;
    for (i, message) in messages.iter().enumerate() {
        if is_compact_boundary_message(message) {
            idx = Some(i);
        }
    }
    match idx {
        Some(i) => messages[i + 1..].to_vec(),
        None => messages.to_vec(),
    }
}

/// TS `getAssistantMessageText(message)`：拼接 assistant 消息的 text 块。
pub fn get_assistant_message_text(message: &Message) -> Option<String> {
    let content = message.content()?;
    match content {
        Content::Text(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Content::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(""))
            }
        }
    }
}

/// TS `getLastAssistantMessage(messages)`。
pub fn get_last_assistant_message(messages: &[Message]) -> Option<&Message> {
    messages.iter().rev().find(|m| m.typ() == "assistant")
}

/// TS `normalizeMessagesForAPI(messages, tools)`：未转换（utils/messages.ts），
/// 本模块按"透传 + 注记"处理；压缩链调用点保持同一签名。
pub fn normalize_messages_for_api(messages: Vec<Message>, _tools: &[Value]) -> Vec<Message> {
    // TODO(utils/messages.ts): 合并同 id 的流式 assistant 分块、补齐
    // tool_result 配对等。压缩请求侧目前不依赖这些变换。
    messages
}

// ---------------------------------------------------------------------------
// analytics / GrowthBook 占位（services/analytics 未转换）
// ---------------------------------------------------------------------------

/// TS `logEvent(name, properties)` —— 压缩链埋点占位（不落盘）。
pub fn log_event(_name: &str, _properties: Option<HashMap<String, Value>>) {}

/// TS `getFeatureValue_CACHED_MAY_BE_STALE<T>(key, defaultValue)`。
/// 未转换 analytics 层；返回 `None` 表示"未命中，调用方用默认值"。
pub fn get_feature_value_cached_may_be_stale(_key: &str) -> Option<Value> {
    None
}

// ---------------------------------------------------------------------------
// utils/forkedAgent.ts 的最小 duck-type
// ---------------------------------------------------------------------------

/// TS `CacheSafeParams`：缓存共享（forked agent）参数。未转换依赖，按压缩
/// 链用到的字段最小建模；`fork_context_messages` 是 PTL 重试链需要回写的
/// 截断后消息集合。
#[derive(Debug, Clone, Default)]
pub struct CacheSafeParams {
    /// TS `toolUseContext`：压缩 fork 的工具上下文。
    pub tool_use_context: Option<crate::tool::ToolUseContext>,
    /// PTL 重试时回写的截断消息集合（compact.ts 唯一读写的字段）。
    pub fork_context_messages: Option<Vec<Message>>,
}

/// TS `Attachment`（utils/attachments.ts 未转换）：附件体原样保留。
pub type Attachment = Value;

/// `Attachment` 工厂（对齐 `createAttachmentMessage`）。
pub fn create_attachment_message(attachment: Attachment) -> Message {
    Message::Attachment(AttachmentMessage {
        uuid: uuid_v4(),
        timestamp: now_ts(),
        attachment,
    })
}
