//! `services/compaction2/triggers/promotion.rs` — 上下文升级：换到严格更大
//! 上下文的模型而不是压缩。W9 只交付决策；调用方（W8）拥有真正的模型
//! 切换与重试。

use std::collections::HashMap;
use std::sync::Arc;

use crate::services::compaction2::contracts::CompactionContext;
use crate::services::compaction2::triggers::overflow::ContextPromotion;
use crate::services::compaction2::triggers::{PromotionResult, TriggerDecision};

/// TS `PromotionCandidate`。
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionCandidate {
    pub model: String,
    pub window_tokens: i64,
}

/// TS `PromotionCandidateSource`。
#[derive(Debug, Clone, Default)]
pub struct PromotionCandidateSource {
    /// 模型 → 上下文窗口映射（如 settings.contextWindowOverrides）。
    pub window_overrides: Option<HashMap<String, i64>>,
    /// 显式已知的可用模型。
    pub candidates: Option<Vec<PromotionCandidate>>,
}

/// TS `collectPromotionCandidates(source)`：两个来源归并为去重列表，
/// 同一模型出现两次时保留更大窗口。
pub fn collect_promotion_candidates(source: &PromotionCandidateSource) -> Vec<PromotionCandidate> {
    let mut by_model: HashMap<String, PromotionCandidate> = HashMap::new();
    if let Some(overrides) = &source.window_overrides {
        for (model, window_tokens) in overrides {
            if model.is_empty() || !window_tokens.is_finite() || *window_tokens <= 0 {
                continue;
            }
            by_model.insert(
                model.clone(),
                PromotionCandidate {
                    model: model.clone(),
                    window_tokens: *window_tokens,
                },
            );
        }
    }
    if let Some(candidates) = &source.candidates {
        for candidate in candidates {
            if candidate.model.is_empty() || !candidate.window_tokens.is_finite() || candidate.window_tokens <= 0 {
                continue;
            }
            match by_model.get(&candidate.model) {
                Some(existing) if existing.window_tokens >= candidate.window_tokens => {}
                _ => {
                    by_model.insert(
                        candidate.model.clone(),
                        PromotionCandidate {
                            model: candidate.model.clone(),
                            window_tokens: candidate.window_tokens,
                        },
                    );
                }
            }
        }
    }
    by_model.into_values().collect()
}

/// TS `selectPromotion(currentModel, currentWindow, candidates?)`：选出
/// 窗口严格大于当前窗口的最大候选。当前模型永不升级。无候选（含窗口
/// 未知）返回 None。
pub fn select_promotion(
    current_model: &str,
    current_window: i64,
    candidates: Option<&[PromotionCandidate]>,
) -> Option<PromotionCandidate> {
    if !current_window.is_finite() || current_window <= 0 {
        return None;
    }
    let candidates = candidates.unwrap_or(&[]);
    let mut best: Option<&PromotionCandidate> = None;
    for candidate in candidates {
        if candidate.model.is_empty() || candidate.model == current_model {
            continue;
        }
        if !candidate.window_tokens.is_finite() || candidate.window_tokens <= current_window {
            continue;
        }
        match best {
            Some(best) if best.window_tokens >= candidate.window_tokens => {}
            _ => best = Some(candidate),
        }
    }
    best.cloned()
}

/// TS `applyPromotionDecision(decision, promotion)`：把升级决策合并进本要
/// 压缩的触发决策。升级成功压制压缩（调用方换模型并重试一次）；其余
/// 决策原样附上，保持 skip 原因可见。
pub fn apply_promotion_decision(
    decision: &TriggerDecision,
    promotion: &PromotionResult,
) -> TriggerDecision {
    match promotion {
        PromotionResult::Promoted { model, .. } => TriggerDecision {
            should_compact: false,
            reason: format!("context-promoted: {model}"),
            promotion: Some(promotion.clone()),
            ..decision.clone()
        },
        PromotionResult::Skipped { .. } => TriggerDecision {
            promotion: Some(promotion.clone()),
            ..decision.clone()
        },
    }
}

/// TS `PromotionAttempt`。
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionAttempt {
    /// 溢出且重试失败时要还原的模型。
    pub original_model: String,
    /// 一次性重试用的更大窗口模型。
    pub target_model: String,
}

/// TS `PromotionRetryDecision`。
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionRetryDecision {
    Retry {
        original_model: String,
        target_model: String,
    },
    Revert {
        original_model: String,
        target_model: String,
        reason: String,
    },
    Skip {
        reason: String,
    },
}

/// TS `PromotionRetryInput`。
pub struct PromotionRetryInput<'a> {
    /// 产生溢出的模型。
    pub current_model: &'a str,
    /// 溢出触发器返回的决策。
    pub promotion: Option<&'a PromotionResult>,
    /// 本 query 已开始过的一次性升级重试。
    pub attempt: Option<&'a PromotionAttempt>,
    /// 进行中的重试再次撞上同一溢出。
    pub retry_failed: bool,
}

/// TS `decidePromotionRetry(input)`。
///
/// 升级是一次性的：已有 attempt 则永不重复。若该 attempt 再次溢出，
/// 调用方必须还原到原模型并上浮原错误。候选缺失、等于当前模型或并不
/// 严格更大时跳过升级，让溢出路径落回压缩。
pub fn decide_promotion_retry(input: &PromotionRetryInput<'_>) -> PromotionRetryDecision {
    if let Some(attempt) = input.attempt {
        if input.retry_failed {
            return PromotionRetryDecision::Revert {
                original_model: attempt.original_model.clone(),
                target_model: attempt.target_model.clone(),
                reason: "promotion-retry-failed".to_string(),
            };
        }
        return PromotionRetryDecision::Skip {
            reason: "promotion-already-attempted".to_string(),
        };
    }

    match input.promotion {
        Some(PromotionResult::Promoted { model, .. })
            if !model.is_empty() && model != input.current_model =>
        {
            PromotionRetryDecision::Retry {
                original_model: input.current_model.to_string(),
                target_model: model.clone(),
            }
        }
        Some(PromotionResult::Skipped { reason }) => PromotionRetryDecision::Skip {
            reason: reason.clone(),
        },
        Some(PromotionResult::Promoted { .. }) => PromotionRetryDecision::Skip {
            reason: "promotion-target-is-current-model".to_string(),
        },
        None => PromotionRetryDecision::Skip {
            reason: "no-promotion-candidate".to_string(),
        },
    }
}

/// TS `CreateContextPromotionOptions`（Rust 侧把 `resolveWindow` 收紧为
/// `Arc` 闭包以拥有 'static）。
pub struct CreateContextPromotionOptions {
    pub source: PromotionCandidateSource,
    /// 解析当前模型的有效窗口。
    pub resolve_window: Option<Arc<dyn Fn(&str) -> i64 + Send + Sync>>,
}

/// TS `createContextPromotion(options)`：把 `select_promotion` 适配到
/// 溢出触发器的 `ContextPromotion` 接口。升级只是决策：报告目标模型，
/// 不切换任何东西。
pub fn create_context_promotion(
    options: CreateContextPromotionOptions,
) -> Box<dyn ContextPromotion> {
    let candidates = collect_promotion_candidates(&options.source);
    Box::new(ContextPromotionAdapter {
        candidates,
        resolve_window: options.resolve_window,
    })
}

struct ContextPromotionAdapter {
    candidates: Vec<PromotionCandidate>,
    resolve_window: Option<Arc<dyn Fn(&str) -> i64 + Send + Sync>>,
}

impl ContextPromotionAdapter {
    fn target_for(&self, ctx: &CompactionContext) -> Option<PromotionCandidate> {
        let current_window = self
            .resolve_window
            .as_ref()
            .map(|resolve| resolve(&ctx.model))
            .unwrap_or(0);
        select_promotion(&ctx.model, current_window, Some(&self.candidates))
    }
}

impl ContextPromotion for ContextPromotionAdapter {
    fn is_available(&self, ctx: &CompactionContext) -> bool {
        self.target_for(ctx).is_some()
    }

    fn promote(
        &self,
        ctx: &CompactionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PromotionResult> + Send + '_>> {
        let target = self.target_for(ctx);
        Box::pin(async move {
            match target {
                Some(target) => PromotionResult::Promoted {
                    model: target.model,
                    reason: Some(format!("larger-window:{}", target.window_tokens)),
                },
                None => PromotionResult::Skipped {
                    reason: "no-larger-context-candidate".to_string(),
                },
            }
        })
    }
}
