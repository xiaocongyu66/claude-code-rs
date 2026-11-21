//! `services/compaction2/methods/handoff.ts` — 移交文档压缩。
//!
//! 把会话摘要成结构化移交文档并从中继续：文档替换被切的上下文前缀，
//! 后续会话无需完整历史也能接手目标、文件、todo 与约束。
//!
//! 模型调用经 `summarize(ctx, { instructions })` 注入；orchestrator 复用
//! legacy soft-compaction 运行器，不引入第二个 API 客户端。溢出按构造
//! 跳过本方法：要摘要的正是放不下的同一批输入。

use std::future::Future;
use std::pin::Pin;

use crate::services::compaction2::contracts::{
    CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome,
};
use crate::services::compaction2::methods::remote::compute_remote_cut;

/// `preserveData` 中移交文档的键。
pub const HANDOFF_PRESERVE_KEY: &str = "handoff";
/// 移交文档版本。
pub const HANDOFF_DOCUMENT_VERSION: i64 = 1;

/// TS `DEFAULT_HANDOFF_INSTRUCTIONS`（逐字节搬运）。
pub const DEFAULT_HANDOFF_INSTRUCTIONS: &str = "Write a handoff document for a fresh session \
that must continue this work. Return Markdown with these sections: ## Recent goals ## Key files \
## Todo ## Constraints Preserve concrete file paths, commands, decisions and anything the user \
explicitly required. Be concise and do not invent facts.";

/// TS `HandoffSummaryRequest`。
#[derive(Debug, Clone)]
pub struct HandoffSummaryRequest {
    pub instructions: String,
}

/// TS `HandoffSummary`。自定义/无 `run` 的嵌入方可返回裸字符串；
/// orchestrator 的 legacy 运行器同时返回完整 `CompactionResult`，
/// 已提交的结果得以保留 `messagesToKeep`、附件与 hook 结果。
#[derive(Debug, Clone)]
pub struct HandoffSummary {
    pub document: String,
    /// 完整 legacy CompactionResult（类型擦除以保持 contracts 独立）。
    pub legacy_compaction_result: Option<serde_json::Value>,
}

/// TS `HandoffSummaryValue`。
#[derive(Debug, Clone)]
pub enum HandoffSummaryValue {
    Document(String),
    Summary(HandoffSummary),
}

/// TS `HandoffMethodDeps`。
pub struct HandoffMethodDeps {
    /// 生成移交文档；由 orchestrator 注入（soft 路径）。
    pub summarize: Option<
        Box<
            dyn for<'a> Fn(
                    &'a CompactionContext,
                    &'a HandoffSummaryRequest,
                ) -> Pin<
                    Box<
                        dyn Future<Output = Result<HandoffSummaryValue, String>> + Send + 'a,
                    >,
                > + Send
                + Sync,
        >,
    >,
    /// `/compact [instructions]` focus 文本，附在基础提示词后。
    pub instructions: Option<String>,
    pub is_available: Option<Box<dyn Fn(&CompactionContext) -> bool + Send + Sync>>,
}

/// TS `buildHandoffInstructions(custom?)`。
pub fn build_handoff_instructions(custom: Option<&str>) -> String {
    let focus = custom.map(str::trim).filter(|focus| !focus.is_empty());
    match focus {
        None => DEFAULT_HANDOFF_INSTRUCTIONS.to_string(),
        Some(focus) => format!("{DEFAULT_HANDOFF_INSTRUCTIONS}\n\nAdditional focus:\n{focus}"),
    }
}

/// TS `HandoffDocument`（`preserveData` 载荷）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HandoffDocument {
    pub version: i64,
    pub document: String,
}

/// TS `createHandoffMethod(deps?)`。
pub fn create_handoff_method(deps: Option<HandoffMethodDeps>) -> Box<dyn CompactionMethod> {
    struct HandoffMethod {
        summarize: Option<
            Box<
                dyn for<'a> Fn(
                        &'a CompactionContext,
                        &'a HandoffSummaryRequest,
                    ) -> Pin<
                        Box<dyn Future<Output = Result<HandoffSummaryValue, String>> + Send + 'a>,
                    > + Send
                    + Sync,
            >,
        >,
        instructions: Option<String>,
        is_available: Option<Box<dyn Fn(&CompactionContext) -> bool + Send + Sync>>,
    }

    impl CompactionMethod for HandoffMethod {
        fn name(&self) -> CompactionMethodName {
            CompactionMethodName::Handoff
        }

        fn is_available(&self, ctx: &CompactionContext) -> bool {
            if ctx.trigger == crate::services::compaction2::contracts::TriggerReason::Overflow {
                return false;
            }
            if self.summarize.is_none() {
                return false;
            }
            match &self.is_available {
                Some(is_available) => is_available(ctx),
                None => true,
            }
        }

        fn run<'a>(
            &'a self,
            ctx: &'a CompactionContext,
        ) -> Pin<Box<dyn Future<Output = CompactionOutcome> + Send + 'a>> {
            Box::pin(async move {
                let Some(summarize) = self.summarize.as_ref() else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Handoff,
                        reason: "handoff-not-available".to_string(),
                    };
                };
                let Ok(summarized) = summarize(
                    ctx,
                    &HandoffSummaryRequest {
                        instructions: build_handoff_instructions(self.instructions.as_deref()),
                    },
                )
                .await
                else {
                    return CompactionOutcome::Failed {
                        method: CompactionMethodName::Handoff,
                        reason: "handoff-summarize-failed".to_string(),
                    };
                };
                let (document, legacy_result) = match summarized {
                    HandoffSummaryValue::Document(document) => (document, None),
                    HandoffSummaryValue::Summary(summary) => {
                        (summary.document, summary.legacy_compaction_result)
                    }
                };
                let document = document.trim().to_string();
                if document.is_empty() {
                    return CompactionOutcome::Failed {
                        method: CompactionMethodName::Handoff,
                        reason: "handoff-empty-document".to_string(),
                    };
                }

                // 复用 remote 的切点：保留尾部与其他摘要方法一致，
                // 工具配对不破。
                let cut = compute_remote_cut(ctx);
                if cut.messages.is_empty() {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Handoff,
                        reason: "handoff-empty-prefix".to_string(),
                    };
                }

                let threshold = ctx.settings.threshold_tokens.unwrap_or(0);
                let preserve = HandoffDocument {
                    version: HANDOFF_DOCUMENT_VERSION,
                    document: document.clone(),
                };
                let mut preserve_data = serde_json::Map::new();
                if let Ok(value) = serde_json::to_value(&preserve) {
                    preserve_data.insert(HANDOFF_PRESERVE_KEY.to_string(), value);
                }
                if let Some(legacy_result) = legacy_result {
                    preserve_data.insert("legacyCompactionResult".to_string(), legacy_result);
                }

                CompactionOutcome::Compacted {
                    method: CompactionMethodName::Handoff,
                    cut_point: cut.cut_point.clone(),
                    tokens_saved: std::cmp::max(
                        0,
                        cut.cut_point.tokens_before - cut.tokens_after,
                    ),
                    headroom_tokens: if threshold > 0 {
                        threshold - cut.tokens_after
                    } else {
                        0
                    },
                    summary: Some(document),
                    preserve_data: Some(preserve_data),
                    warning: None,
                }
            })
        }
    }

    let (summarize, instructions, is_available) = match deps {
        Some(deps) => (deps.summarize, deps.instructions, deps.is_available),
        None => (None, None, None),
    };
    Box::new(HandoffMethod {
        summarize,
        instructions,
        is_available,
    })
}
