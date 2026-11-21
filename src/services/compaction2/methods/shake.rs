//! `services/compaction2/methods/shake.ts` — shake 方法（注入式 reducer 接口）。
//!
//! W2 定义契约与 null 降级路径；实际机械剔除由 reducers/shake.rs 提供。

use std::future::Future;
use std::pin::Pin;

use crate::services::compaction2::contracts::{
    CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome,
};
use crate::services::compact::Message;

/// TS `ShakeReduction`。
#[derive(Debug, Clone, PartialEq)]
pub struct ShakeReduction {
    pub messages: Vec<Message>,
    pub tokens_saved: i64,
    pub warning: Option<String>,
}

/// 注入式 reducer 接口（W1）。Rust 侧用 boxed future 支持 dyn 分发。
pub trait ShakeReducer: Send + Sync {
    fn reduce<'a>(
        &'a self,
        ctx: &'a CompactionContext,
    ) -> Pin<Box<dyn Future<Output = Option<ShakeReduction>> + Send + 'a>>;
}

/// TS `ShakeMethodDeps`。
pub struct ShakeMethodDeps {
    pub reducer: Option<Box<dyn ShakeReducer>>,
    pub resolve_threshold_tokens: Option<
        Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>,
    >,
}

/// TS `createShakeMethod(deps?)`。
pub fn create_shake_method(deps: Option<ShakeMethodDeps>) -> Box<dyn CompactionMethod> {
    struct ShakeMethod {
        reducer: Option<Box<dyn ShakeReducer>>,
        resolve_threshold_tokens: Option<
            Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>,
        >,
    }

    impl CompactionMethod for ShakeMethod {
        fn name(&self) -> CompactionMethodName {
            CompactionMethodName::Shake
        }

        fn is_available(&self, _ctx: &CompactionContext) -> bool {
            self.reducer.is_some()
        }

        fn run<'a>(
            &'a self,
            ctx: &'a CompactionContext,
        ) -> Pin<Box<dyn Future<Output = CompactionOutcome> + Send + 'a>> {
            Box::pin(async move {
                let Some(reducer) = self.reducer.as_ref() else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Shake,
                        reason: "no-shake-reducer".to_string(),
                    };
                };
                let Some(reduction) = reducer.reduce(ctx).await else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Shake,
                        reason: "shake-noop".to_string(),
                    };
                };
                let after_tokens = ctx.token_count_of(&reduction.messages);
                let threshold = self
                    .resolve_threshold_tokens
                    .as_ref()
                    .and_then(|resolve| resolve(ctx));
                CompactionOutcome::Reduced {
                    method: CompactionMethodName::Shake,
                    messages: reduction.messages,
                    tokens_saved: std::cmp::max(0, reduction.tokens_saved),
                    headroom_tokens: threshold
                        .map(|threshold| threshold - after_tokens)
                        .unwrap_or(0),
                    warning: reduction.warning,
                }
            })
        }
    }

    let (reducer, resolve_threshold_tokens) = match deps {
        Some(deps) => (deps.reducer, deps.resolve_threshold_tokens),
        None => (None, None),
    };
    Box::new(ShakeMethod {
        reducer,
        resolve_threshold_tokens,
    })
}
