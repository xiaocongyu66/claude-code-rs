//! `services/compaction2/triggers/incomplete.ts` — 不完整输出恢复。

use crate::services::compaction2::contracts::TriggerReason;
use crate::services::compaction2::triggers::{
    resolve_threshold_decision, ThresholdInput, TriggerDecision,
};

/// TS `isIncompleteStop(stopReason?)`：Anthropic `length` / OpenAI
/// `max_tokens` / `model_context_window_exceeded`。
pub fn is_incomplete_stop(stop_reason: Option<&str>) -> bool {
    matches!(
        stop_reason,
        Some("length") | Some("max_tokens") | Some("model_context_window_exceeded")
    )
}

/// TS `evaluateIncompleteTrigger(input)`。
///
/// 压缩不能抬高输出上限，因此只有在窗口未知或上下文超过阈值时才触发
/// （oh-my-pi 的"above threshold (or unknown window)"规则）。
pub fn evaluate_incomplete_trigger(
    input: &ThresholdInput,
    stop_reason: Option<&str>,
) -> TriggerDecision {
    let threshold = resolve_threshold_decision(input);
    let incomplete = is_incomplete_stop(stop_reason);
    let window_unknown = input.window_tokens <= 0;
    let should_compact =
        incomplete && (window_unknown || input.token_count > threshold.threshold_tokens);

    TriggerDecision {
        trigger: TriggerReason::Incomplete,
        should_compact,
        reason: if !incomplete {
            "not-incomplete-stop".to_string()
        } else if should_compact {
            "incomplete-output-over-threshold".to_string()
        } else {
            "incomplete-output-below-threshold".to_string()
        },
        threshold,
        promotion: None,
        payload: None,
    }
}
