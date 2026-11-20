//! `services/compaction2/triggers/threshold.ts` — 阈值解析与回合末维护触发。

use crate::services::compaction2::contracts::{
    resolve_reserve_tokens, CompactionSettings, PROGRESS_HEADROOM_RATIO,
};
use crate::services::compaction2::triggers::types::{ThresholdDecision, ThresholdInput, TriggerDecision};

/// TS `resolveThresholdDecision(input)`。
///
/// 默认策略是 `window - reserve`，reserve =
/// `max(floor(window*0.15), configuredReserve ?? 16384)`（契约
/// `resolve_reserve_tokens`）。显式 `threshold_tokens` /
/// `threshold_percent` 设置优先生效（镜像 oh-my-pi 的阈值策略）。
pub fn resolve_threshold_decision(input: &ThresholdInput) -> ThresholdDecision {
    let window_tokens = std::cmp::max(0, input.window_tokens);
    let token_count = std::cmp::max(0, input.token_count);

    if window_tokens <= 0 {
        return ThresholdDecision {
            window_tokens,
            reserve_tokens: 0,
            threshold_tokens: 0,
            token_count,
            headroom_tokens: 0,
            progress_target_tokens: 0,
            over_threshold: false,
        };
    }

    let reserve_tokens = resolve_reserve_tokens(window_tokens, input.settings.reserve_tokens);
    let fixed = input.settings.threshold_tokens;
    let percent = input.settings.threshold_percent;

    let threshold_tokens = if let Some(fixed) = fixed {
        if fixed.is_finite() && fixed > 0 {
            std::cmp::min(window_tokens - 1, std::cmp::max(1, fixed))
        } else {
            std::cmp::max(1, window_tokens - reserve_tokens)
        }
    } else if let Some(percent) = percent {
        if percent.is_finite() && percent > 0 {
            let clamped = std::cmp::min(99, std::cmp::max(1, percent));
            std::cmp::max(1, (window_tokens as f64 * (clamped as f64 / 100.0)).floor() as i64)
        } else {
            std::cmp::max(1, window_tokens - reserve_tokens)
        }
    } else {
        std::cmp::max(1, window_tokens - reserve_tokens)
    };

    ThresholdDecision {
        window_tokens,
        reserve_tokens,
        threshold_tokens,
        token_count,
        headroom_tokens: threshold_tokens - token_count,
        progress_target_tokens: (threshold_tokens as f64 * PROGRESS_HEADROOM_RATIO).floor() as i64,
        // oh-my-pi 的 shouldCompact() 用严格 `>`：恰好在阈值上下一个请求
        // 还装得下，不需要维护轮。
        over_threshold: token_count > threshold_tokens,
    }
}

/// TS `evaluateThresholdTrigger(input)`：回合末阈值维护触发。
pub fn evaluate_threshold_trigger(input: &ThresholdInput) -> TriggerDecision {
    let threshold = resolve_threshold_decision(input);
    TriggerDecision {
        trigger: crate::services::compaction2::contracts::TriggerReason::Threshold,
        should_compact: threshold.over_threshold,
        reason: if threshold.over_threshold {
            format!(
                "context {} > threshold {}",
                threshold.token_count, threshold.threshold_tokens
            )
        } else {
            format!(
                "context {} <= threshold {}",
                threshold.token_count, threshold.threshold_tokens
            )
        },
        threshold,
        promotion: None,
        payload: None,
    }
}

/// 触发器共享的设置类型简写。
pub type TriggerSettings = CompactionSettings;
