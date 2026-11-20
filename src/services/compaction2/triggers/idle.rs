//! `services/compaction2/triggers/idle.ts` — 空闲维护决策（纯函数）。
//!
/// 调用方拥有时钟与"未在流式/未在压缩"的前置条件。

use crate::services::compaction2::contracts::TriggerReason;
use crate::services::compaction2::triggers::{
    resolve_threshold_decision, ThresholdInput, TriggerDecision,
};

/// TS `IdleInput`（now 注入）。
pub struct IdleInput<'a> {
    pub threshold: &'a ThresholdInput,
    pub last_activity_at: Option<i64>,
    /// 注入时钟；本模块不 arm 任何计时器。
    pub now: Option<i64>,
}

/// TS `evaluateIdleTrigger(input)`：纯决策。
pub fn evaluate_idle_trigger(input: &IdleInput<'_>) -> TriggerDecision {
    let threshold = resolve_threshold_decision(input.threshold);
    if !input.threshold.settings.idle_enabled {
        return TriggerDecision {
            trigger: TriggerReason::Idle,
            should_compact: false,
            reason: "idle-disabled".to_string(),
            threshold,
            promotion: None,
            payload: None,
        };
    }
    if input.threshold.token_count < input.threshold.settings.idle_threshold_tokens {
        return TriggerDecision {
            trigger: TriggerReason::Idle,
            should_compact: false,
            reason: "below-idle-threshold".to_string(),
            threshold,
            promotion: None,
            payload: None,
        };
    }
    let now = input.now.unwrap_or_else(|| {
        chrono::Utc::now().timestamp_millis()
    });
    let last_activity_at = input.last_activity_at.unwrap_or(now);
    let elapsed_seconds = std::cmp::max(0, now - last_activity_at) / 1000;
    if elapsed_seconds < input.threshold.settings.idle_timeout_seconds {
        return TriggerDecision {
            trigger: TriggerReason::Idle,
            should_compact: false,
            reason: "idle-timeout-not-elapsed".to_string(),
            threshold,
            promotion: None,
            payload: None,
        };
    }
    TriggerDecision {
        trigger: TriggerReason::Idle,
        should_compact: true,
        reason: "idle".to_string(),
        threshold,
        promotion: None,
        payload: None,
    }
}
