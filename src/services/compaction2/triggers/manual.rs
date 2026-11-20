//! `services/compaction2/triggers/manual.ts` — 手动 /compact。

use crate::services::compaction2::contracts::TriggerReason;
use crate::services::compaction2::triggers::{resolve_threshold_decision, ThresholdInput, TriggerDecision};

/// TS `evaluateManualTrigger(input)`：手动 `/compact [instructions]`
/// 总是运行；阈值只随附给进度护卫与决策追踪。
pub fn evaluate_manual_trigger(input: &ThresholdInput) -> TriggerDecision {
    TriggerDecision {
        trigger: TriggerReason::Manual,
        should_compact: true,
        reason: "manual-compact".to_string(),
        threshold: resolve_threshold_decision(input),
        promotion: None,
        payload: None,
    }
}
