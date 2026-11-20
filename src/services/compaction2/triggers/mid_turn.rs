//! `services/compaction2/triggers/midTurn.ts` — 工具循环内的中途维护。
//!
//! 与阈值检查同一谓词，在工具循环内下一个 provider 请求之前评估，
//! 受 `midTurnEnabled` 门控。

use crate::services::compaction2::contracts::TriggerReason;
use crate::services::compaction2::triggers::{
    evaluate_threshold_trigger, resolve_threshold_decision, ThresholdInput, TriggerDecision,
};

/// TS `evaluateMidTurnTrigger(input)`。
pub fn evaluate_mid_turn_trigger(input: &ThresholdInput) -> TriggerDecision {
    if !input.settings.mid_turn_enabled {
        let threshold = resolve_threshold_decision(input);
        return TriggerDecision {
            trigger: TriggerReason::MidTurn,
            should_compact: false,
            reason: "mid-turn-disabled".to_string(),
            threshold,
            promotion: None,
            payload: None,
        };
    }
    let decision = evaluate_threshold_trigger(input);
    TriggerDecision {
        trigger: TriggerReason::MidTurn,
        reason: if decision.should_compact {
            format!(
                "mid-turn context {} > threshold {}",
                decision.threshold.token_count, decision.threshold.threshold_tokens
            )
        } else {
            format!(
                "mid-turn context {} <= threshold {}",
                decision.threshold.token_count, decision.threshold.threshold_tokens
            )
        },
        ..decision
    }
}
