//! sink.rs — analytics sink 路由实现（重写自
//! `src/services/analytics/sink.ts`）。
//!
//! 职责（对齐 TS）：
//! - Datadog 门（`tengu_log_datadog`）：未初始化时回落上一会话缓存值。
//! - `logEventImpl`：先按采样配置采样（命中则附加 sample_rate），再扇出
//!   到 Datadog（先 `_PROTO_*` 剥离）与 1P（保留全负载，由 exporter 提
//!   升 `_PROTO_*` 到 proto 字段）。
//! - 启动期门初始化（`initializeAnalyticsGates`）与幂等 sink 挂载
//!   （`initializeAnalyticsSink`）。
//!
//! 已知偏差：
//! - Datadog 扇出为异步（reqwest 风格 async fn）；`log_event` 同步接口
//!   内无法 `.await`——Datadog 路径改为 fire-and-forget 后台线程驱动
//!   （std::thread + 阻塞未来），避免阻塞事件路径；事件顺序不保证跨
//!   sink，与 TS 的 Promise 语义一致（TS 也是 fire-and-forget）。
//! - `log_event_async`：两个 sink 均 fire-and-forget，直接同步执行后
//!   返回（对齐 TS 注释的接口保留）。

use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use super::datadog::track_datadog_event;
use super::first_party_event_logger::{log_event_to_1p, should_sample_event};
use super::growthbook::check_statsig_feature_gate_cached_may_be_stale;
use super::index::{attach_analytics_sink, strip_proto_fields, AnalyticsSink, LogEventMetadata, MetadataValue, SinkFuture};
use super::sink_killswitch::{is_sink_killed, SinkName};

/// Datadog 门名（TS `DATADOG_GATE_NAME`）。
const DATADOG_GATE_NAME: &str = "tengu_log_datadog";

/// 模块级门状态：未初始化（None）时回落上一会话的缓存值。
static IS_DATADOG_GATE_ENABLED: Mutex<Option<bool>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Datadog 跟踪是否启用（TS `shouldTrackDatadog`）。
fn should_track_datadog() -> bool {
    if is_sink_killed(SinkName::Datadog) {
        return false;
    }
    {
        let cached = lock(&IS_DATADOG_GATE_ENABLED);
        if let Some(enabled) = *cached {
            return enabled;
        }
    }
    // 回落上一会话缓存值（TS try/catch → false）
    check_statsig_feature_gate_cached_may_be_stale(DATADOG_GATE_NAME)
}

/// 同步实现（TS `logEventImpl`）。
fn log_event_impl(event_name: &str, metadata: &LogEventMetadata) {
    // 采样：0 → 未选中记录；正数 → 附加 sample_rate
    let sample_result = should_sample_event(event_name);
    if sample_result == Some(0.0) {
        return;
    }

    let mut metadata_with_sample_rate = metadata.clone();
    if let Some(sample_rate) = sample_result {
        metadata_with_sample_rate.insert(
            "sample_rate".to_string(),
            Some(MetadataValue::Num(sample_rate)),
        );
    }

    if should_track_datadog() {
        // Datadog 是通用后端：扇出前剥 _PROTO_*（未脱敏 PII 仅给 1P 特权列）
        let stripped = strip_proto_fields(&metadata_with_sample_rate);
        // fire-and-forget：独立线程跑异步 track（事件路径不阻塞）
        let event_name_owned = event_name.to_string();
        std::thread::spawn(move || {
            // 在独立线程用极简 waker 驱动 future 到完成
            let fut = async move {
                track_datadog_event(&event_name_owned, &stripped).await;
            };
            let _ = drive(fut);
        });
    }

    // 1P 收全负载（含 _PROTO_*：exporter 自己解构路由）
    log_event_to_1p(event_name, &metadata_with_sample_rate);
}

/// 阻塞驱动一个 future 到完成（供 fire-and-forget 后台线程使用）。
fn drive<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: std::sync::Arc<Self>) {}
    }
    let waker = Waker::from(std::sync::Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::yield_now();
    }
}

/// 异步实现（TS `logEventAsyncImpl`）：两个 sink 均 fire-and-forget，
/// 直接包装同步实现以保持 sink 接口契约。
fn log_event_async_impl(event_name: String, metadata: LogEventMetadata) {
    log_event_impl(&event_name, &metadata);
}

/// 分析门 sink（TS `AnalyticsSink` 的实现骨架）。
pub struct AnalyticsSinkImpl;

impl AnalyticsSink for AnalyticsSinkImpl {
    fn log_event(&self, event_name: &str, metadata: &LogEventMetadata) {
        log_event_impl(event_name, metadata);
    }

    fn log_event_async(&self, event_name: String, metadata: LogEventMetadata) -> SinkFuture {
        log_event_async_impl(event_name, metadata);
        Box::pin(async {})
    }
}

/// 启动期门初始化（setupBackend 调用；早期事件用上一会话缓存值）。
pub fn initialize_analytics_gates() {
    *lock(&IS_DATADOG_GATE_ENABLED) =
        Some(check_statsig_feature_gate_cached_may_be_stale(DATADOG_GATE_NAME));
}

/// 挂载 analytics sink（幂等；早期事件会在挂载时排空）。
pub fn initialize_analytics_sink() {
    attach_analytics_sink(Arc::new(AnalyticsSinkImpl) as Arc<dyn AnalyticsSink>);
}
