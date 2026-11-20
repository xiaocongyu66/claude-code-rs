//! `services/compaction2/triggers/speculation.rs` — 投机压缩纯带决策。
//!
//! 在 `[threshold - lead, threshold)` 带内触发，其中
//! `lead = clamp(threshold * 0.125, 8192, 32000)`（契约
//! `speculation_lead_tokens`）。W2 只交付纯决策——无计时器、无网络、
//! 无后台任务。

use crate::services::compaction2::contracts::{
    speculation_lead_tokens, CompactionMethodName, CompactionSettings,
};

/// 会调 LLM、因此值得投机的方法。
pub const SPECULATION_METHODS: [CompactionMethodName; 3] = [
    CompactionMethodName::Remote,
    CompactionMethodName::Handoff,
    CompactionMethodName::Soft,
];

/// TS `SpeculationInput`。
pub struct SpeculationInput<'a> {
    pub threshold_tokens: i64,
    pub token_count: i64,
    pub settings: &'a CompactionSettings,
    /// 当前 `isAvailable()` 为真的方法。
    pub available_methods: Option<&'a [CompactionMethodName]>,
}

/// TS `SpeculationDecision`。
#[derive(Debug, Clone, PartialEq)]
pub struct SpeculationDecision {
    pub eligible: bool,
    pub lead_tokens: i64,
    pub band_low_tokens: i64,
    pub band_high_tokens: i64,
    pub in_band: bool,
    pub method: Option<CompactionMethodName>,
    pub reason: String,
}

/// TS `evaluateSpeculation(input)`。
pub fn evaluate_speculation(input: &SpeculationInput<'_>) -> SpeculationDecision {
    let threshold_tokens = std::cmp::max(0, input.threshold_tokens);
    let lead_tokens = speculation_lead_tokens(threshold_tokens);
    let band_high_tokens = threshold_tokens;
    let band_low_tokens = std::cmp::max(0, threshold_tokens - lead_tokens);
    let in_band = input.token_count >= band_low_tokens && input.token_count < band_high_tokens;

    let default_methods: Vec<CompactionMethodName> = SPECULATION_METHODS.to_vec();
    let available = input.available_methods.unwrap_or(&default_methods);
    let method = input.settings.method_order.iter().find(|candidate| {
        SPECULATION_METHODS.contains(candidate) && available.contains(candidate)
    });

    if !input.settings.async_enabled {
        return SpeculationDecision {
            eligible: false,
            lead_tokens,
            band_low_tokens,
            band_high_tokens,
            in_band,
            method: None,
            reason: "async-disabled".to_string(),
        };
    }
    if !in_band {
        return SpeculationDecision {
            eligible: false,
            lead_tokens,
            band_low_tokens,
            band_high_tokens,
            in_band,
            method: None,
            reason: if input.token_count < band_low_tokens {
                "below-band".to_string()
            } else {
                "at-or-above-threshold".to_string()
            },
        };
    }
    let Some(method) = method else {
        return SpeculationDecision {
            eligible: false,
            lead_tokens,
            band_low_tokens,
            band_high_tokens,
            in_band,
            method: None,
            reason: "no-llm-backed-method-available".to_string(),
        };
    };
    SpeculationDecision {
        eligible: true,
        lead_tokens,
        band_low_tokens,
        band_high_tokens,
        in_band,
        method: Some(*method),
        reason: format!("speculate-{}", method.as_str()),
    }
}
