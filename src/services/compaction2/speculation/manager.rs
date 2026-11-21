//! `services/compaction2/speculation/manager.ts` — 异步投机压缩管理器
//! （COMPACTION_V2）。
//!
//! 纯带区决策在 `triggers/speculation.ts`；本模块拥有围绕一次后台
//! 摘要运行的状态机：
//!
//!   idle --maybeStart（带区内）--> running --runner 完成--> armed
//!   running --runner 失败 / 超时--> idle
//!   armed --consume（锚点仍存活）--> idle（结果 + 追加的消息）
//!   armed --consume（分支改写 / TTL 过期）/ 超时 / cancel --> idle（丢弃）
//!
//! 管理器永不阻塞 query 循环：`maybe_start` 只调度注入的 runner，
//! `consume` 绝不等待在途运行。transcript 在迭代之间只追加，因此当快照
//! 的最后一条消息仍是活跃锚点时提交运行；在其之后追加的消息随结果一并
//! 返回，调用方得以保住那一轮。锚点消失（分支改写/截断）的运行被丢弃
//! 而不是提交。模块刻意不持单例状态——调用方拥有实例并决定创建/取消
//! 时机。
//!
//! Rust 对齐说明：
//! - TS 的 `AbortController`/`AbortSignal` 用 `Arc<AtomicBool>` 表达，
//!   runner 通过 `Snapshot::is_aborted()` 观察取消。
//! - TS 的 `setTimeout` 超时与 `Promise.then` 后台推进用
//!   `tokio::spawn` + `tokio::time::timeout` 表达；`maybe_start` 需要
//!   调用方处于 tokio 运行时上下文（与 TS 的事件循环等价）。
//! - `useless`、`unref` 等 DOM/Node 特性无 Rust 对应，略去。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::services::compact::Message;
use crate::services::compaction2::contracts::{speculation_lead_tokens, CompactionOutcome};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// TS `SpeculationResult`：完整 outcome 或摘要字符串。
#[derive(Debug, Clone)]
pub enum SpeculationResult {
    Outcome(CompactionOutcome),
    Summary(String),
}

/// TS `DEFAULT_SPECULATION_TIMEOUT_MS`。
pub const DEFAULT_SPECULATION_TIMEOUT_MS: u64 = 120_000;

/// TS `SpeculationContext<TContext>`。
pub struct SpeculationContext<TContext = ()> {
    /// 活跃 transcript；运行开始时复制（冻结）。
    pub messages: Vec<Message>,
    pub token_count: i64,
    /// 当前模型/窗口的压缩阈值。
    pub threshold_tokens: i64,
    pub model: String,
    /// 捕获进快照的不透明调用方载荷（例如 query 的 toolUseContext/cache
    /// 参数）。放在 context 而非模块状态上，使后台运行永不读取后续迭代
    /// 的活跃值。
    pub runner_context: Option<TContext>,
}

/// TS `SpeculationSnapshot<TContext>`：交给 runner 的只读分支快照。
pub struct SpeculationSnapshot<TContext = ()> {
    /// 快照覆盖的最后一条消息 uuid；提交要求它仍是活跃锚点（同一位置）。
    /// 允许在其后追加消息。
    pub last_uuid: Option<String>,
    pub messages: Vec<Message>,
    pub token_count: i64,
    pub threshold_tokens: i64,
    pub model: String,
    pub started_at: i64,
    /// 运行被取消、取代或超时时置位。
    abort: Arc<AtomicBool>,
    pub runner_context: Option<TContext>,
}

impl<TContext> SpeculationSnapshot<TContext> {
    /// TS `snapshot.signal.aborted`。
    pub fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

/// TS `SpeculationRunner<TResult, TContext>`。
///
/// 返回 `None`（或失败）报告运行失败；管理器释放槽位，让后续带区进入
/// 得以重试。Rust 侧快照按 `Arc` 传入以支持 `'static` boxed future。
pub trait SpeculationRunner<TResult, TContext = ()>: Send + Sync {
    fn run(&self, snapshot: Arc<SpeculationSnapshot<TContext>>) -> BoxFuture<Option<TResult>>;
}

/// TS `SpeculationManagerOptions<TResult, TContext>`。
pub struct SpeculationManagerOptions<TResult, TContext = ()> {
    pub runner: Arc<dyn SpeculationRunner<TResult, TContext>>,
    pub now: Option<Box<dyn Fn() -> i64 + Send + Sync>>,
    /// 默认 false：调用方必须显式启用。
    pub enabled: Option<bool>,
    pub timeout_ms: Option<u64>,
    /// armed 结果等待消费者的最长时长，默认取 `timeout_ms`：若循环一直
    /// 不到达阈值，armed 结果会一直冻结 transcript 与 runner 上下文。
    /// 在 consume 时检查（`now - finishedAt`）。
    pub armed_ttl_ms: Option<u64>,
}

/// TS `SpeculationConsumption<TResult>`：armed 结果 + 快照之后追加的轮次。
pub struct SpeculationConsumption<TResult> {
    pub result: TResult,
    /// 快照最后一条消息之后到达的活跃消息。调用方应用结果后再追加这些，
    /// 快照后的轮次永不丢失。
    pub appended_messages: Vec<Message>,
}

/// TS `'idle' | 'running' | 'armed'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeculationState {
    Idle,
    Running,
    Armed,
}

struct RunSlot<TResult, TContext> {
    snapshot: Arc<SpeculationSnapshot<TContext>>,
    abort: Arc<AtomicBool>,
    armed: Option<TResult>,
    finished_at: Option<i64>,
}

enum ManagerState<TResult, TContext> {
    Idle,
    Running(RunSlot<TResult, TContext>),
}

struct ManagerInner<TResult, TContext = ()> {
    runner: Arc<dyn SpeculationRunner<TResult, TContext>>,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    enabled: bool,
    timeout_ms: u64,
    armed_ttl_ms: u64,
    state: Mutex<ManagerState<TResult, TContext>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// TS `findAppendedMessages(snapshot, live)`。
///
/// 在活跃 transcript 中定位快照之后追加的消息。快照不再是完整前缀
/// （分支改写、前段截断或锚点移动）时返回 `None`。
///
/// 锚点是快照的最后一条消息；它必须仍在同一位置。两侧都有 uuid 时，
/// 更早的 uuid 会被交叉校验。没有锚点 uuid 的快照永不提交：比较长度与
/// 存活 uuid 无法发现同长度的尾部改写——那会提交一份来自另一分支的
/// 摘要。
fn find_appended_messages<TContext>(
    snapshot: &SpeculationSnapshot<TContext>,
    live: &[Message],
) -> Option<Vec<Message>> {
    let snapshot_messages = &snapshot.messages;
    let anchor = snapshot.last_uuid.as_ref()?;
    let mut anchor_index = None;
    for (index, message) in live.iter().enumerate() {
        if message.uuid() == anchor {
            anchor_index = Some(index);
            break;
        }
    }
    let anchor_index = anchor_index?;
    if anchor_index + 1 != snapshot_messages.len() {
        return None;
    }
    for (index, before) in snapshot_messages.iter().enumerate() {
        let before_uuid = before.uuid();
        let Some(now) = live.get(index) else {
            return None;
        };
        let now_uuid = now.uuid();
        if before_uuid != now_uuid {
            return None;
        }
    }
    Some(live[anchor_index + 1..].to_vec())
}

/// TS `createSpeculationManager<TResult, TContext>(options)`。
pub fn create_speculation_manager<TResult: Send + 'static, TContext: Clone + Send + Sync + 'static>(
    options: SpeculationManagerOptions<TResult, TContext>,
) -> SpeculationManager<TResult, TContext> {
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_SPECULATION_TIMEOUT_MS);
    SpeculationManager {
        inner: Arc::new(ManagerInner {
            runner: options.runner,
            now: options.now.unwrap_or_else(|| {
                Box::new(|| chrono::Utc::now().timestamp_millis()) as Box<dyn Fn() -> i64 + Send + Sync>
            }),
            enabled: options.enabled.unwrap_or(false),
            timeout_ms,
            armed_ttl_ms: options.armed_ttl_ms.unwrap_or(timeout_ms),
            state: Mutex::new(ManagerState::Idle),
        }),
    }
}

/// TS `SpeculationManager<TResult, TContext>`。
pub struct SpeculationManager<TResult, TContext = ()> {
    inner: Arc<ManagerInner<TResult, TContext>>,
}

impl<TResult: Send + 'static, TContext: Clone + Send + Sync + 'static>
    SpeculationManager<TResult, TContext>
{
    /// TS `maybeStart(ctx)`：`tokenCount` 位于阈值前带区时启动一次后台运行。
    pub fn maybe_start(&self, ctx: &SpeculationContext<TContext>) -> bool {
        if !self.inner.enabled {
            return false;
        }
        // 一次只跑一个：在途或 armed 的运行拥有槽位。
        {
            let mut state = lock(&self.inner.state);
            if !matches!(*state, ManagerState::Idle) {
                return false;
            }
            let threshold_tokens = ctx.threshold_tokens.max(0);
            if threshold_tokens <= 0 {
                return false;
            }
            let lead_tokens = speculation_lead_tokens(threshold_tokens);
            let band_low_tokens = (threshold_tokens - lead_tokens).max(0);
            if ctx.token_count < band_low_tokens || ctx.token_count >= threshold_tokens {
                return false;
            }
            let abort = Arc::new(AtomicBool::new(false));
            let last_uuid = ctx.messages.last().map(|message| message.uuid().to_string());
            let snapshot = Arc::new(SpeculationSnapshot {
                last_uuid,
                messages: ctx.messages.clone(),
                token_count: ctx.token_count,
                threshold_tokens,
                model: ctx.model.clone(),
                started_at: (self.inner.now)(),
                abort: abort.clone(),
                runner_context: ctx.runner_context.clone(),
            });
            *state = ManagerState::Running(RunSlot {
                snapshot: snapshot.clone(),
                abort: abort.clone(),
                armed: None,
                finished_at: None,
            });

            let inner = self.inner.clone();
            let snapshot_for_task = snapshot;
            // 后台推进：runner 与 timeout 赛跑，永不阻塞 query 循环。
            tokio::spawn(async move {
                let run = tokio::time::timeout(
                    Duration::from_millis(inner.timeout_ms),
                    inner.runner.run(snapshot_for_task),
                )
                .await;
                let mut state = lock(&inner.state);
                let ManagerState::Running(slot) = &mut *state else {
                    // 槽位已被消费者取走或尚未启动。
                    return;
                };
                // 只有本运行的槽位才能被本任务武装（区分 superseded）。
                if !std::sync::Arc::ptr_eq(&slot.abort, &abort)
                    || abort.load(Ordering::Acquire)
                {
                    return;
                }
                match run {
                    Ok(Some(result)) => {
                        slot.finished_at = Some((inner.now)());
                        slot.armed = Some(result);
                    }
                    // runner 报告失败 / 超时：释放槽位以便稍后重试。
                    Ok(None) | Err(_) => *state = ManagerState::Idle,
                }
            });
        }
        true
    }

    /// TS `consume(ctx)`：在阈值处认领 armed 结果。
    ///
    /// 没有 armed 结果、运行仍在途、运行失败/超时，或快照最后一条消息
    /// 不再是活跃锚点（分支改写/截断）时返回 `None`。
    pub fn consume(&self, ctx: &SpeculationContext<TContext>) -> Option<SpeculationConsumption<TResult>> {
        if !self.inner.enabled {
            return None;
        }
        let slot = {
            let mut state = lock(&self.inner.state);
            let ManagerState::Running(slot) = &mut *state else {
                return None;
            };
            let slot = RunSlot {
                snapshot: slot.snapshot.clone(),
                abort: slot.abort.clone(),
                armed: slot.armed.take(),
                finished_at: slot.finished_at,
            };
            // 消费者从此时起拥有槽位（无论是否 armed）。
            *state = ManagerState::Idle;
            slot
        };
        let Some(result) = slot.armed else {
            // 阈值到达时仍在摘要：不阻塞，直接丢弃。
            slot.abort.store(true, Ordering::Release);
            return None;
        };
        if let Some(finished_at) = slot.finished_at {
            if finished_at - slot.snapshot.started_at > self.inner.timeout_ms as i64 {
                slot.abort.store(true, Ordering::Release);
                return None;
            }
            // 运行在预算内完成，但结果可能 armed 了很长时间（循环停留在
            // 阈值以下）。年龄在 consume 时从 `finishedAt` 起算，而不是
            // 只看运行时长：陈旧的预览描述的可能是不再相关的 transcript。
            if (self.inner.now)() - finished_at > self.inner.armed_ttl_ms as i64 {
                slot.abort.store(true, Ordering::Release);
                return None;
            }
        } else {
            slot.abort.store(true, Ordering::Release);
            return None;
        }
        let Some(appended_messages) = find_appended_messages(&slot.snapshot, &ctx.messages) else {
            slot.abort.store(true, Ordering::Release);
            return None;
        };
        Some(SpeculationConsumption {
            result,
            appended_messages,
        })
    }

    /// TS `cancel()`：中止并丢弃在途或 armed 的运行。
    pub fn cancel(&self) {
        let mut state = lock(&self.inner.state);
        if let ManagerState::Running(slot) = &*state {
            slot.abort.store(true, Ordering::Release);
        }
        *state = ManagerState::Idle;
    }

    /// TS `state()`。
    pub fn state(&self) -> SpeculationState {
        let state = lock(&self.inner.state);
        match &*state {
            ManagerState::Idle => SpeculationState::Idle,
            ManagerState::Running(ref slot) => {
                if slot.armed.is_some() {
                    SpeculationState::Armed
                } else {
                    SpeculationState::Running
                }
            }
        }
    }
}
