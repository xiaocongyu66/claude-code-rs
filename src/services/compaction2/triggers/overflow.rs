//! `services/compaction2/triggers/overflow.ts` — 溢出触发 + 上下文升级。

use crate::services::compaction2::contracts::{CompactionContext, TriggerReason};
use crate::services::compaction2::guards::payload::{evaluate_payload_guard, PayloadGuardDecision, PayloadRejectionEvidence};
use crate::services::compaction2::triggers::{
    resolve_threshold_decision, PromotionResult, ThresholdInput, TriggerDecision,
};

/// 上下文升级：换到配置的更大上下文模型，避免摘要任何内容。
/// W2 只交付接口；默认实现缺席并报告 `skipped`。
pub trait ContextPromotion: Send + Sync {
    fn is_available(&self, ctx: &CompactionContext) -> bool;
    fn promote(&self, ctx: &CompactionContext)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = PromotionResult> + Send + '_>>;
}

/// TS `OverflowInput`。
pub struct OverflowInput<'a> {
    pub token_count: i64,
    pub window_tokens: i64,
    pub settings: &'a crate::services::compaction2::contracts::CompactionSettings,
    pub ctx: Option<&'a CompactionContext>,
    pub overflow_detected: bool,
    pub payload_rejection: Option<PayloadRejectionEvidence>,
    pub promotion: Option<&'a dyn ContextPromotion>,
}

/// TS `tryContextPromotion(ctx, promotion)`。
pub async fn try_context_promotion(
    ctx: Option<&CompactionContext>,
    promotion: Option<&dyn ContextPromotion>,
) -> PromotionResult {
    let Some(promotion) = promotion else {
        return PromotionResult::Skipped {
            reason: "no-promotion-configured".to_string(),
        };
    };
    let Some(ctx) = ctx else {
        return PromotionResult::Skipped {
            reason: "no-compaction-context".to_string(),
        };
    };
    if !promotion.is_available(ctx) {
        return PromotionResult::Skipped {
            reason: "promotion-unavailable".to_string(),
        };
    }
    promotion.promote(ctx).await
}

/// TS `evaluateOverflowTrigger(input)`。
///
/// 证据要么是显式溢出信号（provider 拒了 prompt），要么是 provider 用量
/// 超过窗口。不证明 token 溢出的载荷拒绝阻断压缩而非升级/摘要。
pub async fn evaluate_overflow_trigger(input: &OverflowInput<'_>) -> TriggerDecision {
    let threshold_input = ThresholdInput {
        token_count: input.token_count,
        window_tokens: input.window_tokens,
        settings: input.settings.clone(),
    };
    let threshold = resolve_threshold_decision(&threshold_input);
    let payload: Option<PayloadGuardDecision> = input
        .payload_rejection
        .as_ref()
        .map(|evidence| {
            evaluate_payload_guard(&PayloadRejectionEvidence {
                context_window: Some(input.window_tokens),
                token_count: Some(input.token_count),
                ..evidence.clone()
            })
        });

    let overflow_detected = input.overflow_detected
        || (input.window_tokens > 0 && input.token_count > input.window_tokens);

    if !overflow_detected {
        return TriggerDecision {
            trigger: TriggerReason::Overflow,
            should_compact: false,
            reason: "no-overflow-evidence".to_string(),
            threshold,
            promotion: None,
            payload,
        };
    }

    if let Some(payload) = &payload {
        if payload.block_compaction {
            return TriggerDecision {
                trigger: TriggerReason::Overflow,
                should_compact: false,
                reason: format!("payload-rejection: {}", payload.reason),
                threshold,
                promotion: None,
                payload: payload.clone(),
            };
        }
    }

    let promotion = try_context_promotion(input.ctx, input.promotion).await;
    if let PromotionResult::Promoted { model, .. } = &promotion {
        return TriggerDecision {
            trigger: TriggerReason::Overflow,
            should_compact: false,
            reason: format!("context-promoted: {model}"),
            threshold,
            promotion: Some(promotion),
            payload,
        };
    }

    TriggerDecision {
        trigger: TriggerReason::Overflow,
        should_compact: true,
        reason: "overflow".to_string(),
        threshold,
        promotion: Some(promotion),
        payload,
    }
}
