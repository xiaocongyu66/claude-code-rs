//! index.rs — analytics 事件入口（重写自 `src/services/analytics/index.ts`）。
//!
//! DESIGN（对齐 TS）：本模块**无任何依赖**以避免 import cycle。事件先入
//! 有界队列，直到 `attach_analytics_sink()` 在应用初始化时被调用后由
//! sink 路由到 Datadog / 1P event logging。
//!
//! 职责：
//! - `strip_proto_fields`：剥掉 `_PROTO_*`（PII-tagged）键，供非 1P sink
//!   扇出前清洗。
//! - 有界事件队列（`MAX_QUEUED_EVENTS = 5000`，满时丢最旧）。
//! - `log_event` / `log_event_async`：无 sink 时入队，attach 后排空。
//! - `attach_analytics_sink`：幂等挂载；USERTYPE=ant 会额外记录
//!   `analytics_sink_attached`（含排队事件数）。
//!
//! 已知偏差：
//! - TS 用 `queueMicrotask` 异步排空以避免阻塞启动路径；Rust 侧无等价
//!   的宿主微任务调度，attach 后**同步**排空（语义等价：attach 发生在
//!   启动路径完成之后，早期事件顺序保持不变）。
//! - TS 的两个 marker 类型（`AnalyticsMetadata_I_VERIFIED_*`，类型为
//!   `never`）是纯类型层意图标记，Rust 无对应物，以文档注释保留。
//! - 上报本身由 sink 决定；当前 sink 实现为 no-op（见 sink.rs 偏差）。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

/// logEvent 元数据值。
///
/// TS 的声明类型是 `boolean | number | undefined`，但运行期存在经
/// `AnalyticsMetadata_I_VERIFIED_*` 标记强转的字符串值（如
/// `tengu_internal_record_permission_context` 的 namespace /
/// toolPermissionContext）；Rust 侧按运行期形态包含 String。
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    Bool(bool),
    Num(f64),
    String(String),
}

/// logEvent 元数据（TS 内部类型 `LogEventMetadata`）。
pub type LogEventMetadata = HashMap<String, Option<MetadataValue>>;

/// 异步 sink 方法的返回 future 类型。
pub type SinkFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// analytics 后端 sink 接口（TS `AnalyticsSink`）。
pub trait AnalyticsSink: Send + Sync {
    /// 同步记录事件。
    fn log_event(&self, event_name: &str, metadata: &LogEventMetadata);
    /// 异步记录事件。
    fn log_event_async(&self, event_name: String, metadata: LogEventMetadata) -> SinkFuture;
}

/// 从 destined for general-access storage 的负载中剥掉 `_PROTO_*` 键。
///
/// 无 `_PROTO_` 键时按 TS 语义返回「同一对象」——Rust 无引用共享，返回
/// 克隆（调用方不依赖原地修改，行为等价）。
pub fn strip_proto_fields(metadata: &LogEventMetadata) -> LogEventMetadata {
    let mut result = metadata.clone();
    result.retain(|key, _| !key.starts_with("_PROTO_"));
    result
}

/// 队列上限（TS `MAX_QUEUED_EVENTS`）。
const MAX_QUEUED_EVENTS: usize = 5_000;

/// 测试可见的队列上限常量（TS `__MAX_QUEUED_EVENTS_FOR_TESTING`）。
pub const MAX_QUEUED_EVENTS_FOR_TESTING: usize = MAX_QUEUED_EVENTS;

struct QueuedEvent {
    event_name: String,
    metadata: LogEventMetadata,
    is_async: bool,
}

static EVENT_QUEUE: Lazy<Mutex<VecDeque<QueuedEvent>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static SINK: Lazy<Mutex<Option<Arc<dyn AnalyticsSink>>>> = Lazy::new(|| Mutex::new(None));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn enqueue_event(event: QueuedEvent) {
    let mut queue = lock(&EVENT_QUEUE);
    // 有界：满时 drop 最旧（对齐 TS splice(0, len - MAX + 1)）
    while queue.len() >= MAX_QUEUED_EVENTS {
        queue.pop_front();
    }
    queue.push_back(event);
}

/// 测试专用：当前队列长度（TS `__getEventQueueLengthForTesting`）。
pub fn get_event_queue_length_for_testing() -> usize {
    lock(&EVENT_QUEUE).len()
}

/// 挂载 analytics sink；幂等（已挂载时 no-op）。挂载时把队列中积压的
/// 事件排空。
pub fn attach_analytics_sink(new_sink: Arc<dyn AnalyticsSink>) {
    let mut sink_slot = lock(&SINK);
    if sink_slot.is_some() {
        return;
    }
    *sink_slot = Some(new_sink);
    // 排空期间持锁：与 TS 的 microtask 排空不同（同步排空，见模块偏差），
    // 但事件顺序与「attach 后尽快排空」语义一致。
    let queued_events: Vec<QueuedEvent> = {
        let mut queue = lock(&EVENT_QUEUE);
        queue.drain(..).collect()
    };
    if queued_events.is_empty() {
        return;
    }
    let sink = match sink_slot.as_ref() {
        Some(sink) => sink.clone(),
        None => return,
    };
    // ant 记录队列规模，帮助排查 analytics 初始化时序
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        let mut meta = LogEventMetadata::new();
        meta.insert(
            "queued_event_count".to_string(),
            Some(MetadataValue::Num(queued_events.len() as f64)),
        );
        sink.log_event("analytics_sink_attached", &meta);
    }
    for event in queued_events {
        if event.is_async {
            let future = sink.log_event_async(event.event_name, event.metadata);
            // 同步驱动 future（无宿主运行时的等价排空；TS 用 microtask）
            futures_drive(future);
        } else {
            sink.log_event(&event.event_name, &event.metadata);
        }
    }
}

/// 阻塞驱动一个 `SinkFuture` 到完成（排空路径使用）。
fn futures_drive(mut future: SinkFuture) {
    use std::task::{Context, Poll, Wake, Waker};
    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: std::sync::Arc<Self>) {}
    }
    let waker = Waker::from(std::sync::Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(()) = future.as_mut().poll(&mut cx) {
            break;
        }
        // sink 实现为 fire-and-forget 时 future 一次就绪；保守自旋等待
        std::thread::yield_now();
    }
}

/// 记录事件（同步）。无 sink 时入队，待 attach 后排空。
pub fn log_event(event_name: &str, metadata: &LogEventMetadata) {
    {
        let sink_slot = lock(&SINK);
        if let Some(sink) = sink_slot.as_ref() {
            sink.log_event(event_name, metadata);
            return;
        }
    }
    enqueue_event(QueuedEvent {
        event_name: event_name.to_string(),
        metadata: metadata.clone(),
        is_async: false,
    });
}

/// 记录事件（异步）。无 sink 时入队，待 attach 后排空。
pub async fn log_event_async(event_name: &str, metadata: &LogEventMetadata) {
    let sink: Option<Arc<dyn AnalyticsSink>> = lock(&SINK).as_ref().cloned();
    if let Some(sink) = sink {
        sink.log_event_async(event_name.to_string(), metadata.clone()).await;
        return;
    }
    enqueue_event(QueuedEvent {
        event_name: event_name.to_string(),
        metadata: metadata.clone(),
        is_async: true,
    });
}

/// 重置 analytics 状态（仅测试）。
pub fn reset_for_testing() {
    *lock(&SINK) = None;
    lock(&EVENT_QUEUE).clear();
}
