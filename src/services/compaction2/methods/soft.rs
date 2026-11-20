//! `services/compaction2/methods/soft.ts` — 软压缩（经 legacy
//! `compactConversation` 原地摘要）。
//!
//! 保留 `/compact [instructions]` 语义：手动运行保留追问流程并透传
//! focus 文本。

use std::future::Future;
use std::pin::Pin;

use crate::services::compact::compact::CompactionResult;
use crate::services::compact::Message;
use crate::services::compaction2::contracts::{
    CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome, CutPoint,
    TriggerReason,
};

/// TS `SoftCompactRequest`。
#[derive(Debug, Clone)]
pub struct SoftCompactRequest {
    pub messages: Vec<Message>,
    pub instructions: Option<String>,
    pub trigger: TriggerReason,
    pub is_auto_compact: bool,
}

/// TS `SoftCompactRun`。
#[derive(Debug, Clone)]
pub struct SoftCompactRun {
    pub result: CompactionResult,
    pub tokens_before: i64,
    pub tokens_after: i64,
}

/// 注入式 legacy 运行器。W2 把 `compactConversation` 挡在本模块外，
/// 方法链决策测试保持纯；生产侧注入调用既有 legacy 实现的运行器。
pub trait SoftCompactRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        request: &'a SoftCompactRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SoftCompactRun, String>> + Send + 'a>>;
}

/// TS `SoftMethodDeps`。
pub struct SoftMethodDeps {
    pub runner: Box<dyn SoftCompactRunner>,
    /// `/compact [instructions]` 的 focus 文本。
    pub instructions: Option<String>,
    pub resolve_threshold_tokens: Option<
        Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>,
    >,
}

/// TS `createSoftMethod(deps?)`。
pub fn create_soft_method(deps: Option<SoftMethodDeps>) -> Box<dyn CompactionMethod> {
    struct SoftMethod {
        runner: Option<Box<dyn SoftCompactRunner>>,
        instructions: Option<String>,
        resolve_threshold_tokens: Option<
            Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>,
        >,
    }

    impl CompactionMethod for SoftMethod {
        fn name(&self) -> CompactionMethodName {
            CompactionMethodName::Soft
        }

        fn is_available(&self, ctx: &CompactionContext) -> bool {
            self.runner.is_some() && !ctx.messages.is_empty()
        }

        fn run<'a>(
            &'a self,
            ctx: &'a CompactionContext,
        ) -> Pin<Box<dyn Future<Output = CompactionOutcome> + Send + 'a>> {
            Box::pin(async move {
                let Some(runner) = self.runner.as_ref() else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Soft,
                        reason: "soft-runner-not-configured".to_string(),
                    };
                };
                let request = SoftCompactRequest {
                    messages: ctx.messages.clone(),
                    instructions: self.instructions.clone(),
                    trigger: ctx.trigger,
                    is_auto_compact: ctx.trigger != TriggerReason::Manual,
                };
                let run = match runner.run(&request).await {
                    Ok(run) => run,
                    Err(reason) => {
                        return CompactionOutcome::Failed {
                            method: CompactionMethodName::Soft,
                            reason,
                        }
                    }
                };
                let tokens_saved = std::cmp::max(0, run.tokens_before - run.tokens_after);
                let threshold = self
                    .resolve_threshold_tokens
                    .as_ref()
                    .and_then(|resolve| resolve(ctx));
                let headroom_tokens = threshold
                    .map(|threshold| threshold - run.tokens_after)
                    .unwrap_or(0);
                let first_kept_entry_id = run
                    .result
                    .messages_to_keep
                    .as_ref()
                    .and_then(|keep| keep.first().map(|message| message.uuid().to_string()))
                    .unwrap_or_default();
                let cut_point = CutPoint {
                    first_kept_entry_id,
                    tokens_before: run.tokens_before,
                    boundary_message: run.result.boundary_marker.clone(),
                };
                let mut preserve_data = serde_json::Map::new();
                // 完整 legacy 结果随 preserveData 返回（orchestrator 用
                // `getLegacyCompactionResult` 直接取回，保留 messagesToKeep /
                // 附件 / hook 结果）。
                if let Ok(value) = serde_json::to_value(&run.result) {
                    preserve_data.insert("legacyCompactionResult".to_string(), value);
                }
                CompactionOutcome::Compacted {
                    method: CompactionMethodName::Soft,
                    cut_point,
                    tokens_saved,
                    headroom_tokens,
                    summary: run.result.user_display_message.clone(),
                    preserve_data: Some(preserve_data),
                    warning: None,
                }
            })
        }
    }

    Box::new(SoftMethod {
        runner: deps.map(|deps| deps.runner),
        instructions: deps.map(|deps| deps.instructions).unwrap_or(None),
        resolve_threshold_tokens: deps.and_then(|deps| deps.resolve_threshold_tokens),
    })
}
