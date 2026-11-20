//! `services/compaction2/contracts.ts` — oh-my-pi 压缩端口（COMPACTION_V2）
//! 的冻结契约。
//!
//! 每个工作流（reducers / triggers / methods / tokenizers / harness）都
//! 以本文件为准。仅在跨工作流评审后修改：legacy 压缩路径在 parity 验证
//! 通过前仍留在 feature flag 之后。
//!
//! 语义对齐：类型与常量逐条搬运；`Message` 使用 `services::compact` 的
//! 鸭子类型模型（TS 侧共用 `types/message.ts`）。

use crate::services::compact::Message;

/// TS `TriggerReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerReason {
    Manual,
    Overflow,
    Incomplete,
    Threshold,
    MidTurn,
    Idle,
}

impl TriggerReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerReason::Manual => "manual",
            TriggerReason::Overflow => "overflow",
            TriggerReason::Incomplete => "incomplete",
            TriggerReason::Threshold => "threshold",
            TriggerReason::MidTurn => "midTurn",
            TriggerReason::Idle => "idle",
        }
    }
}

/// TS `CompactionMethodName`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompactionMethodName {
    Remote,
    Snapcompact,
    Handoff,
    Shake,
    Soft,
}

impl CompactionMethodName {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactionMethodName::Remote => "remote",
            CompactionMethodName::Snapcompact => "snapcompact",
            CompactionMethodName::Handoff => "handoff",
            CompactionMethodName::Shake => "shake",
            CompactionMethodName::Soft => "soft",
        }
    }
}

/// TS `EntryKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Message,
    Compaction,
    BranchSummary,
}

/// TS `EntryView`：CCH 消息数组在 oh-my-pi 条目术语下的只读投影。
/// 消息数组仍是 REPL/transcript 的真源；本视图只携带稳定的切割点 id 与
/// 每条目的 token 计数。
#[derive(Debug, Clone)]
pub struct EntryView {
    pub id: String,
    pub kind: EntryKind,
    pub message: Option<Message>,
    pub tokens: Option<i64>,
}

/// TS `CutPoint`。
#[derive(Debug, Clone)]
pub struct CutPoint {
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    pub boundary_message: crate::services::compact::SystemMessage,
}

/// TS `CompactionSettings`。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionSettings {
    pub method_order: Vec<CompactionMethodName>,
    /// 保留最近的上下文 token 不动。
    pub keep_recent_tokens: i64,
    /// 显式预留；undefined = max(floor(window*0.15), 16384)。
    pub reserve_tokens: Option<i64>,
    pub threshold_tokens: Option<i64>,
    pub threshold_percent: Option<i64>,
    pub mid_turn_enabled: bool,
    pub auto_continue: bool,
    pub async_enabled: bool,
    pub idle_enabled: bool,
    pub idle_threshold_tokens: i64,
    pub idle_timeout_seconds: i64,
}

/// TS `DEFAULT_COMPACTION_SETTINGS`。
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    method_order: DEFAULT_METHOD_ORDER,
    keep_recent_tokens: 20_000,
    reserve_tokens: None,
    threshold_tokens: None,
    threshold_percent: None,
    mid_turn_enabled: true,
    auto_continue: true,
    async_enabled: true,
    idle_enabled: false,
    idle_threshold_tokens: 200_000,
    idle_timeout_seconds: 300,
};

const DEFAULT_METHOD_ORDER: [CompactionMethodName; 5] = [
    CompactionMethodName::Remote,
    CompactionMethodName::Snapcompact,
    CompactionMethodName::Handoff,
    CompactionMethodName::Shake,
    CompactionMethodName::Soft,
];

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS.clone()
    }
}

/// TS `CompactionContext`。`token_count` / `estimate_entry_tokens` /
/// `now` 通过函数指针注入（Rust 无闭包捕获 trait object 的便捷等价，
/// 保持与 TS 注入语义一致）。
pub struct CompactionContext {
    /// 当前模型 id（已知时为 provider/model-id 形式）。
    pub model: String,
    pub trigger: TriggerReason,
    pub messages: Vec<Message>,
    pub settings: CompactionSettings,
    /// 规范上下文大小估算；与 legacy 路径同一函数。
    pub token_count: Box<dyn Fn(&[Message]) -> i64 + Send + Sync>,
    pub estimate_entry_tokens: Box<dyn Fn(&EntryView) -> i64 + Send + Sync>,
    pub abort_signal: Option<AbortSignalRef>,
    pub agent_id: Option<String>,
    pub now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl CompactionContext {
    pub fn token_count_of(&self, messages: &[Message]) -> i64 {
        (self.token_count)(messages)
    }

    pub fn estimate_entry_tokens_of(&self, entry: &EntryView) -> i64 {
        (self.estimate_entry_tokens)(entry)
    }

    pub fn now_ms(&self) -> i64 {
        (self.now)()
    }
}

/// `AbortSignal` 的可注入引用（Rust 侧无 DOM AbortSignal；用 trait 对象
/// 表达"是否已取消"）。
pub trait AbortSignalRef: Send + Sync {
    fn aborted(&self) -> bool;
}

/// 常闭（永不取消）的 AbortSignal 占位。
pub struct NeverAbort;
impl AbortSignalRef for NeverAbort {
    fn aborted(&self) -> bool {
        false
    }
}

/// TS `CompactionOutcome`。
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    Compacted {
        method: CompactionMethodName,
        cut_point: CutPoint,
        tokens_saved: i64,
        headroom_tokens: i64,
        summary: Option<String>,
        preserve_data: Option<serde_json::Map<String, serde_json::Value>>,
        warning: Option<String>,
    },
    Reduced {
        method: CompactionMethodName,
        messages: Vec<Message>,
        tokens_saved: i64,
        headroom_tokens: i64,
        warning: Option<String>,
    },
    Skipped {
        method: CompactionMethodName,
        reason: String,
    },
    Failed {
        method: CompactionMethodName,
        reason: String,
    },
}

impl CompactionOutcome {
    pub fn method(&self) -> CompactionMethodName {
        match self {
            CompactionOutcome::Compacted { method, .. }
            | CompactionOutcome::Reduced { method, .. }
            | CompactionOutcome::Skipped { method, .. }
            | CompactionOutcome::Failed { method, .. } => *method,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            CompactionOutcome::Compacted { .. } => "compacted",
            CompactionOutcome::Reduced { .. } => "reduced",
            CompactionOutcome::Skipped { .. } => "skipped",
            CompactionOutcome::Failed { .. } => "failed",
        }
    }
}

/// TS `CompactionMethod`。
///
/// `run` 手动脱糖为 `Pin<Box<dyn Future>>`（Cargo 无 async-trait 依赖，
/// 且 Rust 原生 async-fn-in-trait 不支持 dyn 分发）。
pub trait CompactionMethod: Send + Sync {
    fn name(&self) -> CompactionMethodName;
    fn is_available(&self, ctx: &CompactionContext) -> bool;
    fn run<'a>(
        &'a self,
        ctx: &'a CompactionContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = CompactionOutcome> + Send + 'a>,
    >;
}

/// TS `GuardResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct GuardResult {
    pub ok: bool,
    pub reason: Option<String>,
    pub headroom_tokens: Option<i64>,
}

/// 压缩后进度护卫阈值（oh-my-pi：阈值 80% 以内才算解除压力）。
pub const PROGRESS_HEADROOM_RATIO: f64 = 0.8;

/// 投机压缩带：lead = clamp(threshold × 0.125, 8192, 32000)。
pub fn speculation_lead_tokens(threshold: i64) -> i64 {
    std::cmp::min(32_000, std::cmp::max(8_192, (threshold as f64 * 0.125).floor() as i64))
}

/// TS `resolveReserveTokens(contextWindow, configured?)`。
pub fn resolve_reserve_tokens(context_window: i64, configured: Option<i64>) -> i64 {
    let proportional = (context_window as f64 * 0.15).floor() as i64;
    let reserve = std::cmp::max(proportional, configured.unwrap_or(16_384));
    // 小窗口上*默认*下限可能吃掉几乎整个窗口，阈值变得极小而每次都被
    // 护卫挡住。显式 reserve 是用户意图，永不下调。
    if configured.is_none() && reserve >= context_window - proportional {
        return std::cmp::max(1, proportional);
    }
    reserve
}
