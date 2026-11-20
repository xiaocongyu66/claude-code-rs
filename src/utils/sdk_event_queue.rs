//! sdk_event_queue.rs — SDK 系统事件队列（task_started / progress /
//! notification / session_state_changed）。
//!
//! 重写自 `src/utils/sdkEventQueue.ts`。语义对齐：
//!
//! - 仅在非交互（headless/streaming）会话中入队；TUI 模式直接丢弃
//!   （不然会堆积到上限而永远不被读取）。
//! - 队列上限 1000，溢出时丢掉最旧的一条（对齐 TS `queue.shift()`）。
//! - `drain_sdk_events` 取出全部事件，并为每条补 `uuid`（随机 v4）与
//!   `session_id`（当前会话）。
//! - `emit_task_terminated_sdk` 是任务进入终态时的收尾事件捷径。
//!
//! 偏差：TS 的事件联合类型在 Rust 侧落成带 `subtype` 标签的枚举；
//! 序列化字段名与 TS 完全一致（snake_case，由 serde rename 统一）。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::state;
use crate::types::ids::SessionId;

/// 任务用量摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
}

/// 任务开始事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStartedEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub subtype: String,
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub description: String,
    pub task_type: Option<String>,
    pub workflow_name: Option<String>,
    pub prompt: Option<String>,
}

/// 任务进度事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgressEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub subtype: String,
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub description: String,
    pub usage: SdkUsage,
    pub last_tool_name: Option<String>,
    pub summary: Option<String>,
    /// 工作流状态变更增量批次；客户端按 `${type}:${index}` upsert 再按
    /// phaseIndex 分组重建阶段树。载荷结构未在 TS 侧强类型化，这里用
    /// serde_json::Value 保留。
    pub workflow_progress: Option<Vec<serde_json::Value>>,
}

/// 任务终态通知（前台 agent 完成、未被转入后台时发出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotificationSdkEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub subtype: String,
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub status: String,
    pub output_file: String,
    pub summary: String,
    pub usage: Option<SdkUsage>,
}

/// 会话状态变化事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStateChangedEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub subtype: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype")]
pub enum SdkEvent {
    #[serde(rename = "task_started")]
    TaskStarted(TaskStartedEvent),
    #[serde(rename = "task_progress")]
    TaskProgress(TaskProgressEvent),
    #[serde(rename = "task_notification")]
    TaskNotification(TaskNotificationSdkEvent),
    #[serde(rename = "session_state_changed")]
    SessionStateChanged(SessionStateChangedEvent),
}

const MAX_QUEUE_SIZE: usize = 1000;

/// 进程内队列（对齐 TS module-level `queue`）。
static QUEUE: std::sync::Mutex<VecDeque<SdkEvent>> = std::sync::Mutex::new(VecDeque::new());

/// 入队一条 SDK 事件。非交互会话外直接丢弃。
pub fn enqueue_sdk_event(event: SdkEvent) {
    // SDK 事件只在 headless/streaming 模式被消费（drain）。
    // TUI 模式会堆积到上限且永不读取。
    if !state::get_is_non_interactive_session() {
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.len() >= MAX_QUEUE_SIZE {
        queue.pop_front();
    }
    queue.push_back(event);
}

/// 取出全部事件，为每条补 uuid 与 session_id。
pub fn drain_sdk_events() -> Vec<EnrichedSdkEvent> {
    let mut queue = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.is_empty() {
        return Vec::new();
    }
    let events: Vec<_> = queue.drain(..).collect();
    drop(queue);
    events
        .into_iter()
        .map(|e| EnrichedSdkEvent {
            event: e,
            uuid: Uuid::new_v4().to_string(),
            session_id: state::get_session_id().0,
        })
        .collect()
}

/// 带 uuid / session_id 的事件（对齐 TS `SdkEvent & { uuid; session_id }`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedSdkEvent {
    #[serde(flatten)]
    pub event: SdkEvent,
    pub uuid: String,
    pub session_id: String,
}

/// 任务进入终态时发出 task_notification 事件。
///
/// registerTask 总会发 task_started；这是收尾。用于不经由
/// enqueuePendingNotification-<task-id> 路径设置终态的出口（那些路径
/// 会经 print.ts 的 XML 解析发出同一事件，重复调用会双发）。
pub fn emit_task_terminated_sdk(
    task_id: &str,
    status: &str,
    tool_use_id: Option<&str>,
    summary: Option<&str>,
    output_file: Option<&str>,
    usage: Option<SdkUsage>,
) {
    enqueue_sdk_event(SdkEvent::TaskNotification(TaskNotificationSdkEvent {
        event_type: "system".to_string(),
        subtype: "task_notification".to_string(),
        task_id: task_id.to_string(),
        tool_use_id: tool_use_id.map(str::to_string),
        status: status.to_string(),
        output_file: output_file.unwrap_or("").to_string(),
        summary: summary.unwrap_or("").to_string(),
        usage,
    }));
}

/// 当前会话 ID（供调用方拿到 SessionId 类型而非 String）。
pub fn current_session_id() -> SessionId {
    state::get_session_id()
}
