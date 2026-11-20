//! `services/compaction2/triggers/types.ts` — 触发器公共类型。

use crate::services::compaction2::contracts::{CompactionSettings, TriggerReason};
use crate::services::compaction2::guards::payload::PayloadGuardDecision;

/// 所有触发器共享的阈值算术。`headroom_tokens` 有符号：负值表示上下文
/// 已越过阈值。
#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdDecision {
    pub window_tokens: i64,
    pub reserve_tokens: i64,
    pub threshold_tokens: i64,
    pub token_count: i64,
    pub headroom_tokens: i64,
    pub progress_target_tokens: i64,
    pub over_threshold: bool,
}

/// TS `PromotionResult`。
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionResult {
    Promoted { model: String, reason: Option<String> },
    Skipped { reason: String },
}

/// TS `TriggerDecision`。
#[derive(Debug, Clone)]
pub struct TriggerDecision {
    pub trigger: TriggerReason,
    pub should_compact: bool,
    pub reason: String,
    pub threshold: ThresholdDecision,
    pub promotion: Option<PromotionResult>,
    pub payload: Option<PayloadGuardDecision>,
}

/// TS `ThresholdInput`。
#[derive(Debug, Clone)]
pub struct ThresholdInput {
    pub token_count: i64,
    pub window_tokens: i64,
    pub settings: CompactionSettings,
}
