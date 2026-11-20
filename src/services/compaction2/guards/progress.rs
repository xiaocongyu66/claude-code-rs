//! `services/compaction2/guards/progress.ts` — 压缩后进度护卫。
//!
//! 一次压缩只有在残留上下文落在 `PROGRESS_HEADROOM_RATIO`（80%）以内
//! 时才算解除阈值压力。

use crate::services::compaction2::contracts::{GuardResult, PROGRESS_HEADROOM_RATIO};

/// TS `evaluateProgressGuard(thresholdTokens, afterTokens)`。
pub fn evaluate_progress_guard(threshold_tokens: i64, after_tokens: i64) -> GuardResult {
    let threshold = std::cmp::max(0, threshold_tokens);
    let after = std::cmp::max(0, after_tokens);
    let target = (threshold as f64 * PROGRESS_HEADROOM_RATIO).floor() as i64;
    let headroom_tokens = threshold - after;
    let ok = after <= target;
    GuardResult {
        ok,
        headroom_tokens: Some(headroom_tokens),
        reason: if ok {
            None
        } else {
            Some(format!(
                "post-compaction context {after} exceeds {target} (80% of {threshold})"
            ))
        },
    }
}
