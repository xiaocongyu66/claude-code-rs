//! message_queue_types.rs — 消息队列操作类型。
//!
//! 重写自 `src/types/messageQueueTypes.ts`（Auto-generated stub）。TS 的
//! `QueueOperation = 'enqueue' | 'dequeue' | 'remove' | string` 本质上接受任意
//! 字符串（三个常量值仅为提示），映射为透明序列化的字符串 newtype。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// 队列操作名称。wire 格式为裸字符串，三个已知值以常量形式提供。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueueOperation(pub String);

impl QueueOperation {
    pub const ENQUEUE: &'static str = "enqueue";
    pub const DEQUEUE: &'static str = "dequeue";
    pub const REMOVE: &'static str = "remove";

    pub fn new(op: impl Into<String>) -> Self {
        Self(op.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QueueOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 队列操作消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueOperationMessage {
    pub r#type: String,
    pub operation: QueueOperation,
    pub timestamp: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 索引签名兜底字段。
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}
