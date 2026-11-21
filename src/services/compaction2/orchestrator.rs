//! `services/compaction2/orchestrator.ts` — oh-my-pi 压缩编排器（COMPACTION_V2）。
//!
//! 语义对齐说明：
//! - 逐函数搬运 TS：`runCompactionV2` 决策图、会话日志
//!   （`persistCompactionEntry`）、提交路径（`applyCompactionOutcome` /
//!   `commitLegacyOutcomeSideEffects`）、三个生产适配器
//!   （query / manual / idle），以及 `resolveSettings` 的四源合并优先级。
//! - 注入式依赖（计数 / 窗口 / 时钟 / trace / 门控）以 `Arc<dyn Fn>`
//!   表达；方法链以 `Box<dyn CompactionMethod>` 分发（contracts.rs 的
//!   既有约定）。
//! - TS `try/catch` 包 `method.run()` 的失败路径在 Rust 侧不存在：
//!   `CompactionMethod::run` 以 `CompactionOutcome::Failed` 表达失败，
//!   语义等价。
//! - 尚未转换的 crate 级依赖按仓内惯例 TODO 存根（对齐 shakeReducer.rs /
//!   sessionMemoryCompact.rs 的 TODO 风格）：SessionMemory 的
//!   `setLastSummarizedMessageId`、utils/hooks 的
//!   `executePostCompactHooks`、`methods/remote/transport.ts` 的
//!   `buildRemoteTransport`。子模块（session / snapcompact 等）按
//!   compaction2/mod.rs 已声明的挂载点引用。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::constants::query_source::QuerySource;
use crate::services::compact::auto_compact::{
    get_auto_compact_threshold, get_effective_context_window_size, is_auto_compact_enabled,
    MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES,
};
use crate::services::compact::compact::{
    apply_compact_conversation_side_effects, build_post_compact_messages, compact_conversation,
    CompactConversationOptions, CompactError, CompactionResult, RecompactionInfo,
};
use crate::services::compact::micro_compact::estimate_message_tokens;
use crate::services::compact::post_compact_cleanup::run_post_compact_cleanup;
use crate::services::compact::{
    get_feature_value_cached_may_be_stale, CacheSafeParams, Content, Message, create_user_message,
};
use crate::services::compaction2::contracts::{
    AbortSignalRef, CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome,
    CompactionSettings, CutPoint, EntryView, GuardResult, TriggerReason,
    DEFAULT_COMPACTION_SETTINGS,
};
use crate::services::compaction2::guards::payload::PayloadRejectionEvidence;
use crate::services::compaction2::guards::progress::evaluate_progress_guard;
use crate::services::compaction2::methods::handoff::{
    create_handoff_method, HandoffMethodDeps, HandoffSummary, HandoffSummaryRequest,
    HandoffSummaryValue,
};
use crate::services::compaction2::methods::registry::{index_methods, is_compaction_method_name, resolve_method_order};
use crate::services::compaction2::methods::remote::{
    create_remote_method, is_remote_compaction_enabled, RemoteMethodDeps,
};
use crate::services::compaction2::methods::shake::{create_shake_method, ShakeMethodDeps, ShakeReducer};
use crate::services::compaction2::methods::snapcompact::create_snapcompact_method;
use crate::services::compaction2::methods::soft::{
    create_soft_method, SoftCompactRequest, SoftCompactRun, SoftMethodDeps,
};
use crate::services::compaction2::reducers::shake_reducer::create_shake_reducer;
use crate::services::compaction2::session::display::{annotate_compaction_result, CompactionMethodMetadata};
use crate::services::compaction2::session::entries::{
    append_compaction_entry, bound_attachment_meta_pair, create_compaction_session_entry,
    project_attachment_meta, CompactionEntryAttachmentMeta, CompactionSessionEntry,
    CreateCompactionSessionEntryInput,
};
use crate::services::compaction2::tokens::context_count::{
    count_context_tokens, CountContextTokensOptions,
};
use crate::services::compaction2::tokens::estimate_tokens;
use crate::services::compaction2::tokens::message_count::count_messages_for_threshold;
use crate::services::compaction2::triggers::idle::{evaluate_idle_trigger, IdleInput};
use crate::services::compaction2::triggers::incomplete::evaluate_incomplete_trigger;
use crate::services::compaction2::triggers::manual::evaluate_manual_trigger;
use crate::services::compaction2::triggers::mid_turn::evaluate_mid_turn_trigger;
use crate::services::compaction2::triggers::overflow::{
    evaluate_overflow_trigger, try_context_promotion, ContextPromotion, OverflowInput,
};
use crate::services::compaction2::triggers::promotion::{
    apply_promotion_decision, collect_promotion_candidates, create_context_promotion,
    CreateContextPromotionOptions, PromotionCandidateSource,
};
use crate::services::compaction2::triggers::threshold::evaluate_threshold_trigger;
use crate::services::compaction2::triggers::types::{
    PromotionResult, ThresholdDecision, ThresholdInput, TriggerDecision,
};
use crate::tool::ToolUseContext;
use crate::utils::abort_controller::CancellationToken;
use crate::utils::model::providers::get_api_provider;
use crate::utils::settings::settings::{get_compaction_settings, get_initial_settings};
use crate::utils::settings::types::CompactionSettings as CompactionSettingsSchema;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// TS `CompactionV2Tracking`。
#[derive(Debug, Clone, Default)]
pub struct CompactionV2Tracking {
    pub compacted: bool,
    pub turn_counter: i64,
    pub consecutive_failures: i64,
}

/// TS `DecisionOutcome`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionOutcome {
    Selected,
    Skipped,
    Failed,
    Compacted,
    Reduced,
    GuardBlocked,
}

/// TS `DecisionTraceEntry`。
#[derive(Debug, Clone)]
pub struct DecisionTraceEntry {
    pub trigger: TriggerReason,
    pub method: Option<CompactionMethodName>,
    pub outcome: DecisionOutcome,
    pub reason: String,
    pub tokens_saved: Option<i64>,
    pub headroom_tokens: Option<i64>,
    pub warning: Option<String>,
    pub at: i64,
}

/// TS `Partial<CompactionSettings>`（逐字段 Option；`resolveSettings` 按源合并）。
#[derive(Debug, Clone, Default)]
pub struct CompactionSettingsOverride {
    pub method_order: Option<Vec<CompactionMethodName>>,
    pub keep_recent_tokens: Option<i64>,
    pub reserve_tokens: Option<i64>,
    pub threshold_tokens: Option<i64>,
    pub threshold_percent: Option<i64>,
    pub mid_turn_enabled: Option<bool>,
    pub auto_continue: Option<bool>,
    pub async_enabled: Option<bool>,
    pub idle_enabled: Option<bool>,
    pub idle_threshold_tokens: Option<i64>,
    pub idle_timeout_seconds: Option<i64>,
}

/// TS `CompactionV2Request`。
pub struct CompactionV2Request {
    pub model: String,
    pub trigger: TriggerReason,
    pub messages: Vec<Message>,
    pub settings: Option<CompactionSettingsOverride>,
    /// 有效上下文窗口；默认 `getEffectiveContextWindowSize(model)`。
    pub window_tokens: Option<i64>,
    /// 规范上下文大小估算；默认 `tokenCountWithEstimation`。
    pub token_count: Option<i64>,
    pub stop_reason: Option<String>,
    pub overflow_detected: bool,
    pub payload_rejection: Option<PayloadRejectionEvidence>,
    pub last_activity_at: Option<i64>,
    pub instructions: Option<String>,
    pub abort_signal: Option<Box<dyn AbortSignalRef>>,
    pub agent_id: Option<String>,
}

/// TS `CompactionV2Options`。注入闭包统一为 `Arc<dyn Fn>`（可跨 await 共享）。
#[derive(Default)]
pub struct CompactionV2Options {
    pub methods: Option<Vec<Box<dyn CompactionMethod>>>,
    pub soft: Option<SoftMethodDeps>,
    pub handoff: Option<HandoffMethodDeps>,
    pub shake: Option<Box<dyn ShakeReducer>>,
    pub remote: Option<RemoteMethodDeps>,
    pub promotion: Option<Box<dyn ContextPromotion>>,
    /// `Some(false)` 抑制所有触发器路径上的上下文升级（一次性重试已用尽时）。
    pub allow_promotion: Option<bool>,
    pub resolve_window_tokens: Option<Arc<dyn Fn(&str) -> i64 + Send + Sync>>,
    pub count_tokens: Option<Arc<dyn Fn(&[Message]) -> i64 + Send + Sync>>,
    pub estimate_entry_tokens: Option<Arc<dyn Fn(&EntryView) -> i64 + Send + Sync>>,
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    pub on_trace: Option<Arc<dyn Fn(&DecisionTraceEntry) + Send + Sync>>,
    /// 拒绝调用方无法渲染的 compacted 结果（如缺 legacy 结果），使其回落到下一方法。
    pub can_accept_outcome: Option<Arc<dyn Fn(&CompactionOutcome) -> bool + Send + Sync>>,
    /// 方法运行前的门控（如断路器跳过 API 方法）。
    pub can_run_method: Option<Arc<dyn Fn(CompactionMethodName) -> bool + Send + Sync>>,
    /// settings.json 的 `settings.compaction`；优先级低于显式请求设置。
    pub compaction_settings: Option<CompactionSettingsSchema>,
    /// `CLAUDE_CODE_COMPACTION_*` 覆盖的环境映射；默认读进程环境。
    pub env: Option<HashMap<String, Option<String>>>,
    /// 只计算模式（投机压缩）：跑出可用结果但跳过日志与全部提交期副作用。
    pub defer_apply: bool,
    /// 日志写入器覆盖（测试）；缺省由 feature flag 门控 `persistCompactionEntry`。
    pub persist:
        Option<Box<dyn for<'a> Fn(&'a CompactionOutcome, &'a [Message]) -> BoxFuture<'a, ()> + Send + Sync>>,
    /// 压缩方 query source，透传给提交期清理。
    pub query_source: Option<QuerySource>,
    /// handoff 提交期 transcript 写入的测试缝。
    pub apply_side_effects: Option<Arc<dyn Fn(&[Message]) + Send + Sync>>,
    /// handoff 提交期 reset/cleanup 的测试缝。
    pub apply_deps: Option<ApplyCompactionOutcomeDeps>,
}

/// TS `CompactionV2Result`。
pub struct CompactionV2Result {
    pub trigger: TriggerDecision,
    pub threshold: ThresholdDecision,
    pub promotion: Option<PromotionResult>,
    pub outcome: Option<CompactionOutcome>,
    pub trace: Vec<DecisionTraceEntry>,
    pub should_compact: bool,
    /// `deferApply: true` 的只计算预览结果；调用方负责 `applyCompactionOutcome`。
    pub deferred_apply: Option<DeferredCompactionApply>,
}

/// TS `DeferredCompactionApply`。
pub struct DeferredCompactionApply {
    pub outcome: CompactionOutcome,
    /// 摘要所依据的 transcript（冻结快照）。
    pub messages: Vec<Message>,
}

/// TS `DefaultMethodOptions`。
pub struct DefaultMethodOptions {
    pub soft: Option<SoftMethodDeps>,
    pub handoff: Option<HandoffMethodDeps>,
    pub shake: Option<Box<dyn ShakeReducer>>,
    pub remote: Option<RemoteMethodDeps>,
    pub instructions: Option<String>,
    pub resolve_threshold_tokens: Option<Arc<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>>,
}

/// TS `createDefaultMethods()`：默认方法链 remote → snapcompact → handoff → shake → soft。
pub fn create_default_methods(options: DefaultMethodOptions) -> Vec<Box<dyn CompactionMethod>> {
    // SoftMethodDeps / ShakeMethodDeps 的 resolve_threshold_tokens 要的是
    // `Box<dyn Fn>`；共享的 `Arc` 闭包在此逐个装箱。
    let box_resolve = |shared: &Option<
        Arc<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>,
    >| -> Option<Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>> {
        shared.clone().map(|shared| {
            Box::new(move |ctx: &CompactionContext| shared(ctx))
                as Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>
        })
    };
    let shared = options.resolve_threshold_tokens.clone();
    let soft = options.soft.map(|soft| SoftMethodDeps {
        runner: soft.runner,
        instructions: options.instructions.clone().or(soft.instructions),
        resolve_threshold_tokens: if shared.is_some() {
            box_resolve(&shared)
        } else {
            soft.resolve_threshold_tokens
        },
    });
    vec![
        create_remote_method(options.remote),
        create_snapcompact_method(None),
        create_handoff_method(options.handoff),
        create_shake_method(Some(ShakeMethodDeps {
            reducer: options.shake,
            resolve_threshold_tokens: box_resolve(&shared),
        })),
        create_soft_method(soft),
    ]
}

/// TS `buildCompactionContext(input)`。
pub struct BuildCompactionContextInput {
    pub model: String,
    pub trigger: TriggerReason,
    pub messages: Vec<Message>,
    pub settings: CompactionSettings,
    pub count_tokens: Arc<dyn Fn(&[Message]) -> i64 + Send + Sync>,
    pub estimate_entry_tokens: Option<Arc<dyn Fn(&EntryView) -> i64 + Send + Sync>>,
    pub abort_signal: Option<Box<dyn AbortSignalRef>>,
    pub agent_id: Option<String>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

pub fn build_compaction_context(input: BuildCompactionContextInput) -> CompactionContext {
    let model = input.model.clone();
    let estimate_entry_tokens = input.estimate_entry_tokens.unwrap_or_else(|| {
        Arc::new(move |entry: &EntryView| default_estimate_entry_tokens(entry, &model))
    });
    let count_tokens = input.count_tokens.clone();
    let now = input.now.clone();
    CompactionContext {
        model: input.model,
        trigger: input.trigger,
        messages: input.messages,
        settings: input.settings,
        token_count: Box::new(move |messages: &[Message]| count_tokens(messages)),
        estimate_entry_tokens: Box::new(move |entry: &EntryView| estimate_entry_tokens(entry)),
        abort_signal: input.abort_signal,
        agent_id: input.agent_id,
        now: Box::new(move || now()),
    }
}

/// `CancellationToken` → contracts `AbortSignalRef` 的适配器（对齐 TS
/// `request.toolUseContext.abortController.signal`）。
struct CancellationTokenSignal(Arc<CancellationToken>);

impl AbortSignalRef for CancellationTokenSignal {
    fn aborted(&self) -> bool {
        self.0.aborted()
    }
}

/// TS `runCompactionV2(request, options)`。
///
/// 为一个触发决策跑配置好的方法链：不可用方法跳过，失败方法回落下一偏好，
/// compacted/reduced 结果须先过进度护卫（阈值 80% 以内）才接受。每个决策
/// 都记入 trace。
pub async fn run_compaction_v2(
    request: CompactionV2Request,
    options: CompactionV2Options,
) -> CompactionV2Result {
    let CompactionV2Options {
        methods,
        soft,
        handoff,
        shake,
        remote,
        promotion,
        allow_promotion,
        resolve_window_tokens,
        count_tokens,
        estimate_entry_tokens,
        now,
        on_trace,
        can_accept_outcome,
        can_run_method,
        compaction_settings,
        env,
        defer_apply,
        persist,
        query_source,
        apply_side_effects,
        apply_deps,
    } = options;

    let settings = resolve_settings(
        request.settings.as_ref(),
        CompactionSettingsSources {
            compaction: compaction_settings.as_ref(),
            env,
        },
    );
    let count_tokens: Arc<dyn Fn(&[Message]) -> i64 + Send + Sync> =
        count_tokens.unwrap_or_else(|| Arc::new(default_count_tokens));
    let token_count = request
        .token_count
        .unwrap_or_else(|| count_tokens(&request.messages));
    let resolve_window: Arc<dyn Fn(&str) -> i64 + Send + Sync> = resolve_window_tokens
        .clone()
        .unwrap_or_else(|| Arc::new(default_resolve_window_tokens));
    let window_tokens = request
        .window_tokens
        .unwrap_or_else(|| resolve_window(&request.model));
    let now: Arc<dyn Fn() -> i64 + Send + Sync> =
        now.unwrap_or_else(|| Arc::new(default_now));

    let threshold_input = ThresholdInput {
        token_count,
        window_tokens,
        settings: settings.clone(),
    };
    let ctx = build_compaction_context(BuildCompactionContextInput {
        model: request.model.clone(),
        trigger: request.trigger,
        messages: request.messages.clone(),
        settings,
        count_tokens: count_tokens.clone(),
        estimate_entry_tokens,
        abort_signal: request.abort_signal,
        agent_id: request.agent_id.clone(),
        now: now.clone(),
    });

    let mut trace: Vec<DecisionTraceEntry> = Vec::new();

    // 升级先于压缩：`allow_promotion: false` 时全路径抑制（一次性升级重试
    // 已用尽时回落压缩而非循环）。
    let promotion_source: Option<Box<dyn ContextPromotion>> = match allow_promotion {
        Some(false) => None,
        _ => promotion,
    };

    let mut trigger_decision = match request.trigger {
        TriggerReason::Manual => evaluate_manual_trigger(&threshold_input),
        TriggerReason::Threshold => evaluate_threshold_trigger(&threshold_input),
        TriggerReason::MidTurn => evaluate_mid_turn_trigger(&threshold_input),
        TriggerReason::Incomplete => {
            evaluate_incomplete_trigger(&threshold_input, request.stop_reason.as_deref())
        }
        TriggerReason::Idle => evaluate_idle_trigger(&IdleInput {
            threshold: &threshold_input,
            last_activity_at: request.last_activity_at,
            now: Some(now()),
        }),
        TriggerReason::Overflow => {
            evaluate_overflow_trigger(&OverflowInput {
                token_count,
                window_tokens,
                settings: &ctx.settings,
                ctx: Some(&ctx),
                overflow_detected: request.overflow_detected,
                payload_rejection: request.payload_rejection.clone(),
                promotion: promotion_source.as_deref(),
            })
            .await
        }
    };

    // omp 对所有自动触发器（不止 overflow）都把升级排在压缩前：更大窗口的
    // 模型完全不必摘要历史。overflow 触发器已自行决策；阈值/不完整触发器
    // 在同一可用性检查下镜像该决策（无独立候选选择）。升级成功压制方法链，
    // 调用方换模型并重试一次。
    if matches!(
        request.trigger,
        TriggerReason::Threshold | TriggerReason::Incomplete
    ) && trigger_decision.should_compact
    {
        let promotion_result =
            try_context_promotion(Some(&ctx), promotion_source.as_deref()).await;
        trigger_decision = apply_promotion_decision(&trigger_decision, &promotion_result);
    }

    if !trigger_decision.should_compact {
        record(
            &mut trace,
            &on_trace,
            request.trigger,
            now(),
            TraceParts {
                method: None,
                outcome: DecisionOutcome::Skipped,
                reason: trigger_decision.reason.clone(),
                tokens_saved: None,
                headroom_tokens: Some(trigger_decision.threshold.headroom_tokens),
                warning: None,
            },
        );
        return build_result(trigger_decision, trace, None, None);
    }

    let order_strings: Vec<String> = ctx
        .settings
        .method_order
        .iter()
        .map(|method| method.as_str().to_string())
        .collect();
    let order = resolve_method_order(Some(&order_strings));
    let methods: Vec<Box<dyn CompactionMethod>> = match methods {
        Some(methods) => methods,
        None => {
            let threshold_tokens = trigger_decision.threshold.threshold_tokens;
            let resolve_threshold_tokens: Arc<
                dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync,
            > = Arc::new(move |_| Some(threshold_tokens));
            create_default_methods(DefaultMethodOptions {
                soft,
                handoff,
                shake,
                remote,
                instructions: request.instructions.clone(),
                resolve_threshold_tokens: Some(resolve_threshold_tokens),
            })
        }
    };
    let indexed = index_methods(&methods);

    for name in order {
        let Some(&position) = indexed.get(&name) else {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::Skipped,
                    reason: "method-not-registered".to_string(),
                    ..default_trace_parts()
                },
            );
            continue;
        };
        if let Some(can_run_method) = &can_run_method {
            if !can_run_method(name) {
                record(
                    &mut trace,
                    &on_trace,
                    request.trigger,
                    now(),
                    TraceParts {
                        method: Some(name),
                        outcome: DecisionOutcome::Skipped,
                        reason: "circuit-open".to_string(),
                        ..default_trace_parts()
                    },
                );
                continue;
            }
        }
        if trigger_decision
            .payload
            .as_ref()
            .map(|payload| payload.exclude_media_methods)
            .unwrap_or(false)
            && name == CompactionMethodName::Snapcompact
        {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::Skipped,
                    reason: "payload-rejection-excludes-media".to_string(),
                    ..default_trace_parts()
                },
            );
            continue;
        }
        let method = &methods[position];
        if !method.is_available(&ctx) {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::Skipped,
                    reason: "unavailable".to_string(),
                    ..default_trace_parts()
                },
            );
            continue;
        }

        let mut outcome = method.run(&ctx).await;

        if let CompactionOutcome::Skipped { reason, .. } = &outcome {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::Skipped,
                    reason: reason.clone(),
                    ..default_trace_parts()
                },
            );
            continue;
        }
        if let CompactionOutcome::Failed { reason, .. } = &outcome {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::Failed,
                    reason: reason.clone(),
                    ..default_trace_parts()
                },
            );
            continue;
        }

        let tokens_saved = outcome_tokens_saved(&outcome);
        let method_headroom = outcome_headroom_tokens(&outcome);
        let after_tokens = std::cmp::max(0, token_count - tokens_saved);
        let threshold_tokens = trigger_decision.threshold.threshold_tokens;
        // 未知窗口（阈值 0）无法做进度检查；那里的溢出恢复只要求结果装得下窗口。
        let progress = if threshold_tokens > 0 {
            evaluate_progress_guard(threshold_tokens, after_tokens)
        } else {
            GuardResult {
                ok: true,
                reason: None,
                headroom_tokens: method_headroom,
            }
        };
        let headroom_tokens = progress.headroom_tokens.or(method_headroom);
        let kind = outcome_kind(&outcome);
        // reduced 结果尚无副作用，护卫失败直接回落；非 soft 的 compacted 方法
        // 也未提交 legacy 摘要，同样回落。soft 是例外——其摘要与清理已经发生，
        // 丢弃会丢失副作用，故保留并上浮护卫告警。
        if !progress.ok
            && (kind == DecisionOutcome::Reduced
                || (kind == DecisionOutcome::Compacted && name != CompactionMethodName::Soft))
        {
            record(
                &mut trace,
                &on_trace,
                request.trigger,
                now(),
                TraceParts {
                    method: Some(name),
                    outcome: DecisionOutcome::GuardBlocked,
                    reason: progress.reason.clone().unwrap_or_else(|| "progress-guard".to_string()),
                    tokens_saved: Some(tokens_saved),
                    headroom_tokens,
                    warning: None,
                },
            );
            continue;
        }
        // 只能消费 legacy CompactionResult 的调用方传 `canAcceptOutcome`；
        // 无法渲染的 compacted 结果回落到下一方法（soft），避免返回空操作。
        if kind == DecisionOutcome::Compacted {
            if let Some(can_accept) = &can_accept_outcome {
                if !can_accept(&outcome) {
                    record(
                        &mut trace,
                        &on_trace,
                        request.trigger,
                        now(),
                        TraceParts {
                            method: Some(name),
                            outcome: DecisionOutcome::Skipped,
                            reason: "unrenderable-compacted-outcome".to_string(),
                            tokens_saved: Some(tokens_saved),
                            headroom_tokens,
                            warning: None,
                        },
                    );
                    continue;
                }
            }
        }
        if !progress.ok && kind == DecisionOutcome::Compacted {
            set_compacted_warning(
                &mut outcome,
                progress.reason.unwrap_or_else(|| "progress-guard".to_string()),
            );
        }

        record(
            &mut trace,
            &on_trace,
            request.trigger,
            now(),
            TraceParts {
                method: Some(name),
                outcome: kind,
                reason: if kind == DecisionOutcome::Compacted {
                    "compacted".to_string()
                } else {
                    "reduced".to_string()
                },
                tokens_saved: Some(tokens_saved),
                headroom_tokens,
                warning: outcome_warning(&outcome),
            },
        );

        let deferred_apply = if kind == DecisionOutcome::Compacted {
            if defer_apply {
                // 预览运行原样返回结果；提交路径再写日志并施加 legacy 副作用。
                Some(DeferredCompactionApply {
                    outcome: outcome.clone(),
                    messages: request.messages.clone(),
                })
            } else if name == CompactionMethodName::Handoff {
                // handoff 摘要恒用 deferApply（其内部运行器在结果仍可能被
                // 护卫拦截时不得写盘）。结果已接受，现在提交那些写入。
                let mut commit_deps = apply_deps.unwrap_or_default();
                if commit_deps.apply_side_effects.is_none() {
                    commit_deps.apply_side_effects = apply_side_effects;
                }
                commit_legacy_outcome_side_effects(
                    &outcome,
                    &request.messages,
                    query_source.as_deref(),
                    &commit_deps,
                    // 直跑 handoff 路径无活跃工具上下文：缓存清理由
                    // compactConversation 自己的克隆处理完成。
                    None,
                    Some(if request.trigger == TriggerReason::Manual {
                        PostCompactTrigger::Manual
                    } else {
                        PostCompactTrigger::Auto
                    }),
                )
                .await;
                persist_outcome(&persist, &outcome, &request.messages).await;
                None
            } else {
                persist_outcome(&persist, &outcome, &request.messages).await;
                None
            }
        } else {
            None
        };

        return build_result(trigger_decision, trace, Some(outcome), deferred_apply);
    }

    record(
        &mut trace,
        &on_trace,
        request.trigger,
        now(),
        TraceParts {
            method: None,
            outcome: DecisionOutcome::Skipped,
            reason: "all-methods-unavailable-or-failed".to_string(),
            tokens_saved: None,
            headroom_tokens: Some(trigger_decision.threshold.headroom_tokens),
            warning: None,
        },
    );
    build_result(trigger_decision, trace, None, None)
}

fn build_result(
    trigger_decision: TriggerDecision,
    trace: Vec<DecisionTraceEntry>,
    outcome: Option<CompactionOutcome>,
    deferred_apply: Option<DeferredCompactionApply>,
) -> CompactionV2Result {
    let threshold = trigger_decision.threshold.clone();
    let promotion = trigger_decision.promotion.clone();
    let should_compact = trigger_decision.should_compact;
    CompactionV2Result {
        trigger: trigger_decision,
        threshold,
        promotion,
        outcome,
        trace,
        should_compact,
        deferred_apply,
    }
}

struct TraceParts {
    method: Option<CompactionMethodName>,
    outcome: DecisionOutcome,
    reason: String,
    tokens_saved: Option<i64>,
    headroom_tokens: Option<i64>,
    warning: Option<String>,
}

fn default_trace_parts() -> TraceParts {
    TraceParts {
        method: None,
        outcome: DecisionOutcome::Skipped,
        reason: String::new(),
        tokens_saved: None,
        headroom_tokens: None,
        warning: None,
    }
}

/// TS `runCompactionV2` 内部的 `record(entry)`：补 trigger 与 at 后落 trace。
fn record(
    trace: &mut Vec<DecisionTraceEntry>,
    on_trace: &Option<Arc<dyn Fn(&DecisionTraceEntry) + Send + Sync>>,
    trigger: TriggerReason,
    at: i64,
    part: TraceParts,
) {
    let entry = DecisionTraceEntry {
        trigger,
        at,
        method: part.method,
        outcome: part.outcome,
        reason: part.reason,
        tokens_saved: part.tokens_saved,
        headroom_tokens: part.headroom_tokens,
        warning: part.warning,
    };
    trace.push(entry.clone());
    if let Some(on_trace) = on_trace {
        on_trace(&entry);
    }
}

async fn persist_outcome(
    persist: &Option<Box<dyn for<'a> Fn(&'a CompactionOutcome, &'a [Message]) -> BoxFuture<'a, ()> + Send + Sync>>,
    outcome: &CompactionOutcome,
    messages: &[Message],
) {
    if let Some(persist) = persist {
        let _ = persist(outcome, messages).await;
    } else if feature("COMPACTION_V2") {
        let _ = persist_compaction_entry(
            Some(outcome),
            messages,
            PersistCompactionEntryOptions::default(),
        )
        .await;
    }
}

fn outcome_tokens_saved(outcome: &CompactionOutcome) -> i64 {
    match outcome {
        CompactionOutcome::Compacted { tokens_saved, .. }
        | CompactionOutcome::Reduced { tokens_saved, .. } => *tokens_saved,
        _ => 0,
    }
}

fn outcome_headroom_tokens(outcome: &CompactionOutcome) -> Option<i64> {
    match outcome {
        CompactionOutcome::Compacted { headroom_tokens, .. }
        | CompactionOutcome::Reduced { headroom_tokens, .. } => Some(*headroom_tokens),
        _ => None,
    }
}

fn outcome_kind(outcome: &CompactionOutcome) -> DecisionOutcome {
    match outcome {
        CompactionOutcome::Compacted { .. } => DecisionOutcome::Compacted,
        CompactionOutcome::Reduced { .. } => DecisionOutcome::Reduced,
        CompactionOutcome::Skipped { .. } => DecisionOutcome::Skipped,
        CompactionOutcome::Failed { .. } => DecisionOutcome::Failed,
    }
}

fn outcome_warning(outcome: &CompactionOutcome) -> Option<String> {
    match outcome {
        CompactionOutcome::Compacted { warning, .. }
        | CompactionOutcome::Reduced { warning, .. } => warning.clone(),
        _ => None,
    }
}

fn set_compacted_warning(outcome: &mut CompactionOutcome, warning: String) {
    if let CompactionOutcome::Compacted { warning: slot, .. } = outcome {
        *slot = Some(warning);
    }
}

// ---------------------------------------------------------------------------
// 会话日志（COMPACTION_V2）。compacted 结果作为独立 `compaction` JSONL 行
// 持久化，resume 时据其重建。legacy transcript 写入不受影响；flag 关闭时
// 什么都不写。
// ---------------------------------------------------------------------------

/// TS `PersistCompactionEntryOptions`。
#[derive(Default)]
pub struct PersistCompactionEntryOptions {
    /// `Some(false)` 模拟 flag 关闭（测试 / 直调）。
    pub enabled: Option<bool>,
    pub append:
        Option<Box<dyn for<'a> Fn(&'a CompactionSessionEntry) -> BoxFuture<'a, ()> + Send + Sync>>,
    pub uuid: Option<Box<dyn Fn() -> String + Send + Sync>>,
    pub now: Option<Box<dyn Fn() -> i64 + Send + Sync>>,
}

/// TS `persistCompactionEntry(outcome, messages, options?)`。
///
/// 持久化成功的压缩结果。soft 恒有摘要（legacy 摘要消息）；remote/snapcompact
/// 仅在 provider 返回摘要时记日志。失败吞掉——日志绝不能让压缩本身失败。
pub async fn persist_compaction_entry(
    outcome: Option<&CompactionOutcome>,
    messages: &[Message],
    options: PersistCompactionEntryOptions,
) -> Option<CompactionSessionEntry> {
    if options.enabled == Some(false) {
        return None;
    }
    let CompactionOutcome::Compacted {
        method,
        cut_point,
        preserve_data,
        ..
    } = outcome?
    else {
        return None;
    };
    let summary = resolve_compaction_summary(outcome)?;
    let legacy = get_legacy_compaction_result(outcome);
    let metas = bound_attachment_meta_pair(
        &project_attachment_metas(
            legacy
                .as_ref()
                .map(|result| result.attachments.as_slice())
                .unwrap_or(&[]),
        ),
        &project_attachment_metas(
            legacy
                .as_ref()
                .map(|result| result.hook_results.as_slice())
                .unwrap_or(&[]),
        ),
    );
    let entry = create_compaction_session_entry(CreateCompactionSessionEntryInput {
        summary,
        first_kept_entry_id: cut_point.first_kept_entry_id.clone(),
        tokens_before: cut_point.tokens_before,
        method: *method,
        preserve_data: slim_preserve_data(preserve_data.as_ref()),
        attachments: metas.attachments,
        hook_results: metas.hook_results,
        parent_uuid: messages.last().map(|message| message.uuid().to_string()),
        uuid: options.uuid.as_ref().map(|uuid| uuid()),
        timestamp: options.now.as_ref().map(|now| {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now())
                .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_default()
        }),
    });
    match &options.append {
        Some(append) => {
            let _ = append(&entry).await;
        }
        None => {
            let _ = append_compaction_entry(&entry).await;
        }
    }
    Some(entry)
}

/// TS `ApplyCompactionOutcomeContext`。
#[derive(Default)]
pub struct ApplyCompactionOutcomeContext {
    /// 预览摘要所依据的 transcript。
    pub messages: Option<Vec<Message>>,
    /// 压缩方 query source（清理范围 + 日志元数据）。
    pub query_source: Option<QuerySource>,
    /// 覆盖 soft 路径的通知旗标（提交测试用）。
    pub apply_side_effects: Option<Arc<dyn Fn(&[Message]) + Send + Sync>>,
    /// 活跃工具上下文：预览跑在克隆上（克隆侧缓存已清），提交在这里清活跃
    /// 缓存，与直跑 `compactConversation` 路径一致。
    pub tool_use_context: Option<Arc<ToolUseContext>>,
    /// 递延 PostCompact hooks 收到的 trigger；默认 'auto'。
    pub trigger: Option<PostCompactTrigger>,
}

/// TS `'manual' | 'auto'`（PostCompact hooks 的 trigger 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCompactTrigger {
    Manual,
    Auto,
}

/// TS `executePostCompactHooks` 的入参 data。
pub struct PostCompactHookData {
    pub trigger: PostCompactTrigger,
    pub compact_summary: String,
}

/// TS `executePostCompactHooks` 的返回值。
#[derive(Debug, Clone, Default)]
pub struct PostCompactHookResult {
    pub user_display_message: Option<String>,
}

/// TS `ApplyCompactionOutcomeDeps`（可注入的提交期依赖，测试用）。
#[derive(Default)]
pub struct ApplyCompactionOutcomeDeps {
    pub persist: Option<
        Box<
            dyn for<'a> Fn(
                    &'a CompactionOutcome,
                    &'a [Message],
                ) -> BoxFuture<'a, Option<CompactionSessionEntry>>
                + Send
                + Sync,
        >,
    >,
    pub reset_last_summarized_message_id: Option<Box<dyn Fn() + Send + Sync>>,
    pub cleanup: Option<Box<dyn Fn(Option<&str>) + Send + Sync>>,
    pub apply_side_effects: Option<Arc<dyn Fn(&[Message]) + Send + Sync>>,
    /// 跑预览跳过的 PostCompact hooks。
    pub execute_post_compact_hooks: Option<
        Box<
            dyn for<'a> Fn(&'a PostCompactHookData, Option<&'a CancellationToken>) -> BoxFuture<'a, PostCompactHookResult>
                + Send
                + Sync,
        >,
    >,
}

/// TS `applyCompactionOutcome(outcome, ctx?, deps?)`。
///
/// 提交预览结果：施加与直跑路径（compactConversation / soft runner）相同的
/// 效果，再写日志。soft 与 legacy-runner handoff 拥有 legacy
/// transcript/session 写入（会话元数据、transcript 段、压缩后标记、
/// summarized-id 复位、清理）；其余 compacted 方法只写日志。每个被接受的
/// 预览恰调一次——被丢弃的预览永不进入本函数。
pub async fn apply_compaction_outcome(
    outcome: Option<&CompactionOutcome>,
    ctx: ApplyCompactionOutcomeContext,
    deps: ApplyCompactionOutcomeDeps,
) -> Option<CompactionSessionEntry> {
    if !matches!(outcome, Some(CompactionOutcome::Compacted { .. })) {
        return None;
    }
    let messages: Vec<Message> = ctx.messages.clone().unwrap_or_default();
    let mut merged_deps = deps;
    if merged_deps.apply_side_effects.is_none() {
        merged_deps.apply_side_effects = ctx.apply_side_effects;
    }
    commit_legacy_outcome_side_effects(
        outcome?,
        &messages,
        ctx.query_source.as_deref(),
        &merged_deps,
        ctx.tool_use_context.as_deref(),
        ctx.trigger,
    )
    .await;
    match merged_deps.persist {
        Some(persist) => persist(outcome?, &messages).await,
        None => {
            persist_compaction_entry(
                outcome,
                &messages,
                PersistCompactionEntryOptions::default(),
            )
            .await
        }
    }
}

/// TS `commitLegacyOutcomeSideEffects(...)`。
///
/// 经 `compactConversation`（soft）或其 soft-runner 适配器（handoff）摘要的
/// 方法拥有的提交期写入。预览跳过了 transcript 写入、summarized-id 复位、
/// 清理、活跃缓存清零与 PostCompact hooks；接受的 outcome 恰调一次，被
/// 丢弃的预览永不进入。remote/snapcompact 直跑路径从不执行这些，提前返回。
async fn commit_legacy_outcome_side_effects(
    outcome: &CompactionOutcome,
    messages: &[Message],
    query_source: Option<&str>,
    deps: &ApplyCompactionOutcomeDeps,
    tool_use_context: Option<&ToolUseContext>,
    trigger: Option<PostCompactTrigger>,
) {
    if !is_legacy_backed_outcome(outcome) {
        return;
    }
    match &deps.apply_side_effects {
        Some(apply_side_effects) => apply_side_effects(messages),
        None => apply_compact_conversation_side_effects(messages),
    }
    match &deps.reset_last_summarized_message_id {
        Some(reset) => reset(),
        None => set_last_summarized_message_id(),
    }
    match &deps.cleanup {
        Some(cleanup) => cleanup(query_source),
        None => run_post_compact_cleanup(query_source),
    }
    if let Some(tool_use_context) = tool_use_context {
        // 预览跑在克隆上；在这里清活跃缓存，与直跑 compactConversation 一致。
        // TODO(utils/file_state_cache.ts): readFileState.clear()；ToolUseContext 以
        // 共享引用传入，真实清理待 file_state_cache 接通后补上。
        if let Some(paths) = &tool_use_context.loaded_nested_memory_paths {
            let _ = paths;
        }
    }
    // PreCompact hooks 不重放：它们塑造已运行的摘要请求（见 compact.ts）。
    // PostCompact 收到摘要，现在恰跑一次。
    if let Some(summary) = resolve_compaction_summary(Some(outcome)) {
        let data = PostCompactHookData {
            trigger: trigger.unwrap_or(PostCompactTrigger::Auto),
            compact_summary: summary,
        };
        match &deps.execute_post_compact_hooks {
            Some(execute) => {
                let _ = execute(&data, tool_use_context.map(|context| &*context.abort_controller)).await;
            }
            None => {
                let _ = execute_post_compact_hooks(
                    &data,
                    tool_use_context.map(|context| &*context.abort_controller),
                )
                .await;
            }
        }
    }
}

/// TS `isLegacyBackedOutcome(outcome)`。
///
/// soft 直接拥有 legacy 写入；handoff 同样走该运行器（deferApply），但只有
/// 真跑了 legacy runner 的 handoff 才拥有 transcript 写入——返回裸文档的
/// 自定义摘要器不算。
fn is_legacy_backed_outcome(outcome: &CompactionOutcome) -> bool {
    let CompactionOutcome::Compacted {
        method,
        preserve_data,
        ..
    } = outcome
    else {
        return false;
    };
    if *method == CompactionMethodName::Soft {
        return true;
    }
    if *method != CompactionMethodName::Handoff {
        return false;
    }
    match preserve_data {
        Some(data) => match data.get("legacyCompactionResult") {
            Some(Value::Object(candidate)) => {
                candidate.contains_key("boundaryMarker") && candidate.contains_key("summaryMessages")
            }
            _ => false,
        },
        None => false,
    }
}

/// TS `resolveCompactionSummary(outcome)`。
///
/// soft 的 `summary` 是展示消息；真正的摘要文本在 legacy 摘要消息里，那才是
/// resume 要注入的。
fn resolve_compaction_summary(outcome: Option<&CompactionOutcome>) -> Option<String> {
    let legacy = get_legacy_compaction_result(outcome);
    if let Some(legacy) = &legacy {
        if let Some(first) = legacy.summary_messages.first() {
            if let Content::Text(text) = &first.message.content {
                if !text.is_empty() {
                    return Some(text.clone());
                }
            }
        }
    }
    if let Some(CompactionOutcome::Compacted { summary, .. }) = outcome {
        if let Some(summary) = summary {
            if !summary.is_empty() {
                return Some(summary.clone());
            }
        }
    }
    None
}

/// TS `projectAttachmentMetas(messages?)`。
fn project_attachment_metas(messages: &[Message]) -> Vec<CompactionEntryAttachmentMeta> {
    messages.iter().filter_map(project_attachment_meta).collect()
}

/// TS `slimPreserveData(data?)`：记日志前瘦身 preserveData——
/// - 丢掉完整 legacy CompactionResult（其 `messagesToKeep` 与磁盘上已有的
///   transcript 字节重复）；
/// - 丢 snapcompact `frames` 的 base64，只保留其数量：行保持小体积，文本
///   模式 resume 仍可取 archive 的 `text`/`textHead`/`textTail`。
fn slim_preserve_data(data: Option<&Map<String, Value>>) -> Option<Map<String, Value>> {
    let data = data?;
    let mut rest = Map::new();
    for (key, value) in data {
        if key == "legacyCompactionResult" {
            continue;
        }
        let value = if key == "snapcompact" {
            slim_snapcompact_archive(value)
        } else {
            value.clone()
        };
        rest.insert(key.clone(), value);
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn slim_snapcompact_archive(value: &Value) -> Value {
    let Value::Object(archive) = value else {
        return value.clone();
    };
    let Some(Value::Array(frames)) = archive.get("frames") else {
        return value.clone();
    };
    let mut rest = archive.clone();
    rest.remove("frames");
    rest.insert("frameCount".to_string(), Value::from(frames.len()));
    Value::Object(rest)
}

// ---------------------------------------------------------------------------
// 生产适配器。以下内容仅在 COMPACTION_V2 打开时可达；legacy 模块按需加载，
// 纯决策图及其测试不触碰 legacy 压缩依赖链。
// ---------------------------------------------------------------------------

/// TS `CompactionV2QueryRequest`。
pub struct CompactionV2QueryRequest {
    pub messages: Vec<Message>,
    pub model: String,
    pub trigger: TriggerReason,
    pub token_count: i64,
    pub tool_use_context: Arc<ToolUseContext>,
    pub cache_safe_params: Arc<CacheSafeParams>,
    pub query_source: Option<QuerySource>,
    pub instructions: Option<String>,
    pub stop_reason: Option<String>,
    pub overflow_detected: bool,
    pub payload_rejection: Option<PayloadRejectionEvidence>,
    pub last_activity_at: Option<i64>,
    pub tracking: Option<CompactionV2Tracking>,
    /// 触发器调优（如 midTurnEnabled）；默认与 DEFAULT_COMPACTION_SETTINGS 一致。
    pub settings: Option<CompactionSettingsOverride>,
    /// `Some(false)` 抑制所有触发器路径上的上下文升级（一次性升级重试已
    /// 用尽时，threshold / incomplete / overflow 全部回落压缩）。
    pub allow_promotion: Option<bool>,
    /// 只计算预览（投机压缩）：跳过日志与全部提交期副作用，成功时返回
    /// `deferred_apply` 载荷。
    pub defer_apply: bool,
    /// 向触发器报告的上下文大小（代替实测计数）。投机运行摘要的是阈值前
    /// 的快照，但仍须跑阈值链。
    pub token_count_override: Option<i64>,
}

/// TS `CompactionV2QueryResult`。
#[derive(Default)]
pub struct CompactionV2QueryResult {
    pub was_compacted: bool,
    pub compaction_result: Option<CompactionResult>,
    pub consecutive_failures: Option<i64>,
    /// 本请求一次性使用的机械削减（shake）结果。
    pub reduced_messages: Option<Vec<Message>>,
    /// 触发本趟的上下文升级决策（threshold / incomplete / overflow 适用时）。
    pub promotion: Option<PromotionResult>,
    /// 削减掉的 token（驱动一次性 UI 提示）。
    pub reduced_tokens_saved: Option<i64>,
    pub reduced_method: Option<CompactionMethodName>,
    /// `deferApply: true` 时存在：调用方经 `applyCompactionOutcome` 负责提交。
    pub deferred_apply: Option<DeferredCompactionApply>,
}

/// TS `runCompactionV2ForQuery(request)`。
///
/// query 循环适配器：用 legacy soft runner 跑 v2 链，返回与
/// `autoCompactIfNeeded` 同构的结果，query.ts 的既有结果处理不动。
pub async fn run_compaction_v2_for_query(
    request: CompactionV2QueryRequest,
) -> CompactionV2QueryResult {
    // 与 legacy shouldAutoCompact 同样的护卫：不在 compact / session_memory
    // fork 内压缩（compact fork 以 querySource='compact' 跑 query()，否则
    // 永远在此分支里打转），并尊重 DISABLE_COMPACT /
    // DISABLE_AUTO_COMPACT / autoCompactEnabled。
    if !is_auto_compact_enabled() {
        return CompactionV2QueryResult {
            was_compacted: false,
            ..Default::default()
        };
    }
    let compaction_settings = get_compaction_settings();
    let env = default_compaction_env();
    if !is_compaction_enabled_in_settings(compaction_settings.as_ref(), &env) {
        return CompactionV2QueryResult {
            was_compacted: false,
            ..Default::default()
        };
    }
    // 断路器：API 侧方法连续失败到这个数后跳过。本地削减器（shake/snapcompact）
    // 保持可用——它们不需要模型调用，是仅剩的恢复手段。
    let circuit_open = request
        .tracking
        .as_ref()
        .map(|tracking| tracking.consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES)
        .unwrap_or(false);
    if request.query_source.as_deref() == Some("compact")
        || request.query_source.as_deref() == Some("session_memory")
    {
        return CompactionV2QueryResult {
            was_compacted: false,
            ..Default::default()
        };
    }
    // 其余 legacy shouldAutoCompact 抑制项对齐：context collapse 开启时拥有
    // 头室问题；reactive-only 模式把溢出恢复交给 API 错误路径。
    if feature("CONTEXT_COLLAPSE") {
        if request.query_source.as_deref() == Some("marble_origami") {
            return CompactionV2QueryResult {
                was_compacted: false,
                ..Default::default()
            };
        }
        if crate::services::contextCollapse::is_context_collapse_enabled() {
            return CompactionV2QueryResult {
                was_compacted: false,
                ..Default::default()
            };
        }
    }
    if feature("REACTIVE_COMPACT")
        && matches!(
            get_feature_value_cached_may_be_stale("tengu_cobalt_raccoon"),
            Some(Value::Bool(true))
        )
    {
        return CompactionV2QueryResult {
            was_compacted: false,
            ..Default::default()
        };
    }
    let window_tokens = get_effective_context_window_size(&request.model);
    let auto_compact_threshold = get_auto_compact_threshold(&request.model);
    // 优先显式覆盖（快照低于阈值的预览运行）；否则用真实 tokenizer，没有
    // tokenizer 数据时回落到调用方的 usage 锚定估算。
    let token_count = request.token_count_override.unwrap_or_else(|| {
        let context_tokens = count_context_tokens(
            &request.messages,
            &request.model,
            &CountContextTokensOptions::default(),
        )
        .tokens;
        count_messages_for_threshold(&request.messages, &request.model, context_tokens) as i64
    });

    // remote 压缩是 opt-in。provider 绑定的 transport 懒构建，flag 关闭路径
    // 永不触碰鉴权；transport 缺失保持既有的 lane-unavailable /
    // transport-missing 跳过。
    let remote = if is_remote_compaction_enabled(&env) {
        Some(create_query_remote_deps(&request.model))
    } else {
        None
    };
    let soft_input = LegacySoftDepsInput {
        tool_use_context: request.tool_use_context.clone(),
        cache_safe_params: request.cache_safe_params.clone(),
        query_source: request.query_source.clone(),
        token_count,
        auto_compact_threshold,
        tracking: request.tracking.clone(),
        defer_apply: request.defer_apply,
        compact_conversation: None,
        run_post_compact_cleanup: None,
        set_last_summarized_message_id: None,
    };
    let soft = create_legacy_soft_deps(soft_input.clone());
    let promotion = if request.allow_promotion == Some(false) {
        None
    } else {
        create_query_promotion()
    };

    let result = run_compaction_v2(
        CompactionV2Request {
            model: request.model.clone(),
            trigger: request.trigger,
            messages: request.messages.clone(),
            window_tokens: Some(window_tokens),
            token_count: Some(token_count),
            stop_reason: request.stop_reason.clone(),
            overflow_detected: request.overflow_detected,
            payload_rejection: request.payload_rejection.clone(),
            last_activity_at: request.last_activity_at,
            instructions: request.instructions.clone(),
            settings: request.settings.clone(),
            abort_signal: Some(Box::new(CancellationTokenSignal(
                request.tool_use_context.abort_controller.clone(),
            ))),
            agent_id: request
                .tool_use_context
                .agent_id
                .as_ref()
                .map(|agent_id| agent_id.0.clone()),
        },
        CompactionV2Options {
            soft: Some(soft),
            handoff: Some(create_legacy_handoff_deps(
                soft_input,
                request.instructions.as_deref(),
            )),
            shake: Some(create_shake_reducer(None)),
            remote,
            // 升级只是决策；模型切换/重试归调用方。两个旋钮都设上：升级
            // 用尽后跳过 settings 读取，门控仍作用于每个触发器。
            promotion,
            allow_promotion: Some(request.allow_promotion != Some(false)),
            // query 循环只认 legacy CompactionResult；无法渲染的
            // remote/snapcompact 结果回落到 soft。
            can_accept_outcome: Some(Arc::new(|outcome: &CompactionOutcome| {
                !matches!(outcome, CompactionOutcome::Compacted { .. })
                    || get_legacy_compaction_result(Some(outcome)).is_some()
            })),
            can_run_method: Some(Arc::new(move |name| {
                !circuit_open
                    || name == CompactionMethodName::Snapcompact
                    || name == CompactionMethodName::Shake
            })),
            resolve_window_tokens: Some(Arc::new(|model: &str| {
                get_effective_context_window_size(model)
            })),
            compaction_settings,
            env: Some(env),
            defer_apply: request.defer_apply,
            query_source: request.query_source.clone(),
            ..Default::default()
        },
    )
    .await;

    if let Some(CompactionOutcome::Reduced {
        messages,
        tokens_saved,
        method,
        ..
    }) = result.outcome.as_ref()
    {
        return CompactionV2QueryResult {
            was_compacted: false,
            reduced_messages: Some(messages.clone()),
            reduced_tokens_saved: Some(*tokens_saved),
            reduced_method: Some(*method),
            // 成功的本地削减复位失败计数。
            consecutive_failures: Some(0),
            promotion: result.promotion.clone(),
            ..Default::default()
        };
    }
    if let Some(outcome) = result.outcome.as_ref() {
        if let CompactionOutcome::Compacted {
            method,
            tokens_saved,
            warning,
            ..
        } = outcome
        {
            if let Some(legacy_result) = get_legacy_compaction_result(Some(outcome)) {
                let deferred_apply = if request.defer_apply {
                    Some(DeferredCompactionApply {
                        outcome: outcome.clone(),
                        messages: request.messages.clone(),
                    })
                } else {
                    None
                };
                return CompactionV2QueryResult {
                    was_compacted: true,
                    compaction_result: Some(annotate_compaction_result(
                        legacy_result,
                        *method,
                        method_metadata(*tokens_saved, warning.clone()),
                    )),
                    promotion: result.promotion.clone(),
                    deferred_apply,
                    ..Default::default()
                };
            }
        }
    }
    if result.should_compact {
        // 需要过阈值但没有任何方法产出可用结果：上浮累计失败计数，让
        // query.ts 的断路器停止重试（对齐 autoCompactIfNeeded）。
        let consecutive_failures = request
            .tracking
            .as_ref()
            .map(|tracking| tracking.consecutive_failures)
            .unwrap_or(0)
            + 1;
        return CompactionV2QueryResult {
            was_compacted: false,
            consecutive_failures: Some(consecutive_failures),
            promotion: result.promotion.clone(),
            ..Default::default()
        };
    }
    CompactionV2QueryResult {
        was_compacted: false,
        promotion: result.promotion.clone(),
        ..Default::default()
    }
}

/// TS `ManualCompactionV2Request`。
pub struct ManualCompactionV2Request {
    pub messages: Vec<Message>,
    pub model: String,
    pub tool_use_context: Arc<ToolUseContext>,
    pub cache_safe_params: Arc<CacheSafeParams>,
    pub query_source: Option<QuerySource>,
    /// `/compact [instructions]` 的 focus 文本。
    pub instructions: Option<String>,
    pub settings: Option<CompactionSettingsOverride>,
}

/// TS `runManualCompactionV2(request)`。
///
/// 手动 `/compact` 适配器。与自动触发器不同：不查 `isAutoCompactEnabled`
/// （用户明确要求）、不排除 `compact` query source（命令本身可能带着它）。
/// 仍尊重中止与方法链自身的可用性护卫。
///
/// 链未产出 legacy `CompactionResult`（全部方法失败或只跑了机械削减）时
/// 返回 None，调用方回落 legacy 命令路径。
pub async fn run_manual_compaction_v2(
    request: ManualCompactionV2Request,
) -> Option<CompactionResult> {
    if request.tool_use_context.abort_controller.aborted() {
        return None;
    }
    let token_count = default_count_tokens(&request.messages);
    let auto_compact_threshold = get_auto_compact_threshold(&request.model);
    let soft_input = LegacySoftDepsInput {
        tool_use_context: request.tool_use_context.clone(),
        cache_safe_params: request.cache_safe_params.clone(),
        query_source: request.query_source.clone(),
        token_count,
        auto_compact_threshold,
        tracking: None,
        defer_apply: false,
        compact_conversation: None,
        run_post_compact_cleanup: None,
        set_last_summarized_message_id: None,
    };
    let soft = create_legacy_soft_deps(soft_input.clone());
    let result = run_compaction_v2(
        CompactionV2Request {
            model: request.model.clone(),
            trigger: TriggerReason::Manual,
            messages: request.messages.clone(),
            window_tokens: Some(get_effective_context_window_size(&request.model)),
            token_count: Some(token_count),
            stop_reason: None,
            overflow_detected: false,
            payload_rejection: None,
            last_activity_at: None,
            instructions: request.instructions.clone(),
            settings: request.settings.clone(),
            abort_signal: Some(Box::new(CancellationTokenSignal(
                request.tool_use_context.abort_controller.clone(),
            ))),
            agent_id: request
                .tool_use_context
                .agent_id
                .as_ref()
                .map(|agent_id| agent_id.0.clone()),
        },
        CompactionV2Options {
            soft: Some(soft),
            handoff: Some(create_legacy_handoff_deps(
                soft_input,
                request.instructions.as_deref(),
            )),
            shake: Some(create_shake_reducer(None)),
            // `/compact` 必须返回 legacy CompactionResult：没有 legacy 结果的
            // 非 soft compacted 结果回落到 soft（soft 恒带）。
            can_accept_outcome: Some(Arc::new(|outcome: &CompactionOutcome| {
                !matches!(outcome, CompactionOutcome::Compacted { .. })
                    || get_legacy_compaction_result(Some(outcome)).is_some()
            })),
            resolve_window_tokens: Some(Arc::new(|model: &str| {
                get_effective_context_window_size(model)
            })),
            compaction_settings: get_compaction_settings(),
            query_source: request.query_source.clone(),
            ..Default::default()
        },
    )
    .await;
    let outcome = result.outcome.as_ref()?;
    let CompactionOutcome::Compacted {
        method,
        tokens_saved,
        warning,
        ..
    } = outcome
    else {
        return None;
    };
    let legacy_result = get_legacy_compaction_result(Some(outcome))?;
    Some(annotate_compaction_result(
        legacy_result,
        *method,
        method_metadata(*tokens_saved, warning.clone()),
    ))
}

/// TS `IdleCompactionV2Request`。
pub struct IdleCompactionV2Request {
    pub messages: Vec<Message>,
    pub model: String,
    pub tool_use_context: Arc<ToolUseContext>,
    pub cache_safe_params: Arc<CacheSafeParams>,
    pub query_source: Option<QuerySource>,
    pub last_activity_at: i64,
    pub settings: Option<CompactionSettingsOverride>,
}

/// TS `IdleCompactionV2Result`。
pub struct IdleCompactionV2Result {
    pub compacted: bool,
    pub messages: Option<Vec<Message>>,
    pub trace: Vec<DecisionTraceEntry>,
}

/// TS `runIdleCompactionV2(request)`。
///
/// 空闲维护适配器。调用方仅在空闲维护显式启用时调用，故这里强制
/// `idleEnabled`；否则默认设置会让触发器恒为 no-op。
pub async fn run_idle_compaction_v2(
    request: IdleCompactionV2Request,
) -> IdleCompactionV2Result {
    let nothing = || IdleCompactionV2Result {
        compacted: false,
        messages: None,
        trace: Vec::new(),
    };
    // 空闲仍是自动压缩：即便环境 opt-in 打开，也尊重
    // DISABLE_COMPACT / DISABLE_AUTO_COMPACT / autoCompactEnabled。
    if !is_auto_compact_enabled() {
        return nothing();
    }
    let compaction_settings = get_compaction_settings();
    let env = default_compaction_env();
    if !is_compaction_enabled_in_settings(compaction_settings.as_ref(), &env) {
        return nothing();
    }
    // 显式 idleEnabled:false 胜过环境 opt-in。
    if compaction_settings
        .as_ref()
        .and_then(|settings| settings.idle_enabled)
        == Some(false)
    {
        return nothing();
    }
    let token_count = default_count_tokens(&request.messages);
    let auto_compact_threshold = get_auto_compact_threshold(&request.model);
    let soft_input = LegacySoftDepsInput {
        tool_use_context: request.tool_use_context.clone(),
        cache_safe_params: request.cache_safe_params.clone(),
        query_source: request.query_source.clone(),
        token_count,
        auto_compact_threshold,
        tracking: None,
        defer_apply: false,
        compact_conversation: None,
        run_post_compact_cleanup: None,
        set_last_summarized_message_id: None,
    };
    let soft = create_legacy_soft_deps(soft_input.clone());
    let mut settings = request.settings.clone().unwrap_or_default();
    // TS `request.settings?.idleEnabled ?? true`：undefined 才补 true。
    if settings.idle_enabled.is_none() {
        settings.idle_enabled = Some(true);
    }
    let result = run_compaction_v2(
        CompactionV2Request {
            model: request.model.clone(),
            trigger: TriggerReason::Idle,
            messages: request.messages.clone(),
            settings: Some(settings),
            window_tokens: Some(get_effective_context_window_size(&request.model)),
            token_count: Some(token_count),
            stop_reason: None,
            overflow_detected: false,
            payload_rejection: None,
            last_activity_at: Some(request.last_activity_at),
            instructions: None,
            abort_signal: Some(Box::new(CancellationTokenSignal(
                request.tool_use_context.abort_controller.clone(),
            ))),
            agent_id: request
                .tool_use_context
                .agent_id
                .as_ref()
                .map(|agent_id| agent_id.0.clone()),
        },
        CompactionV2Options {
            soft: Some(soft),
            handoff: Some(create_legacy_handoff_deps(soft_input, None)),
            query_source: request.query_source.clone(),
            resolve_window_tokens: Some(Arc::new(|model: &str| {
                get_effective_context_window_size(model)
            })),
            compaction_settings,
            ..Default::default()
        },
    )
    .await;

    if let Some(outcome) = result.outcome.as_ref() {
        if let CompactionOutcome::Compacted {
            method,
            tokens_saved,
            warning,
            ..
        } = outcome
        {
            if let Some(legacy_result) = get_legacy_compaction_result(Some(outcome)) {
                return IdleCompactionV2Result {
                    compacted: true,
                    messages: Some(build_post_compact_messages(&annotate_compaction_result(
                        legacy_result,
                        *method,
                        method_metadata(*tokens_saved, warning.clone()),
                    ))),
                    trace: result.trace,
                };
            }
        }
    }
    IdleCompactionV2Result {
        compacted: false,
        messages: None,
        trace: result.trace,
    }
}

/// TS `createQueryRemoteDeps(model)`。
///
/// 解析 provider 并构建其 transport。除 `buildRemoteTransport` 同步解析的
/// 内容外不读任何凭证；Google token 在返回的 fetch 内按请求获取。
fn create_query_remote_deps(_model: &str) -> RemoteMethodDeps {
    // TS 以 try/catch 包 getAPIProvider()，失败时退回纯 env 依赖；Rust 的
    // get_api_provider() 不会失败，恒有 provider。
    let provider = get_api_provider();
    let mut env = HashMap::new();
    for key in [
        "CCH_REMOTE_COMPACTION",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ] {
        env.insert(key.to_string(), std::env::var(key).ok());
    }
    // TODO(methods/remote/transport.ts)：buildRemoteTransport({provider,
    //   model, env}) → { fetch, apiKey, baseUrl }。其前 remote lane 保持
    //   transport-missing 不可用（与 bedrock lane 的既有降级一致）。
    RemoteMethodDeps {
        env: Some(env),
        fetch: None,
        api_key: None,
        base_url: None,
        provider: Some(provider.as_str().to_string()),
        open_ai_version: None,
        instructions: None,
        resolve_provider: None,
        resolve_base_url: None,
        resolve_api_key: None,
        is_available: None,
    }
}

/// TS `createQueryPromotion()`。
///
/// 升级候选来自 `contextWindowOverrides`：这些条目是用户对某模型真实窗口
/// 的显式意图。无覆盖 → 无升级，overflow 路径对现有用户保持不变。
fn create_query_promotion() -> Option<Box<dyn ContextPromotion>> {
    let overrides = get_initial_settings().context_window_overrides?;
    let source = PromotionCandidateSource {
        window_overrides: Some(overrides),
        candidates: None,
    };
    let candidates = collect_promotion_candidates(&source);
    if candidates.is_empty() {
        return None;
    }
    Some(create_context_promotion(CreateContextPromotionOptions {
        source,
        resolve_window: Some(Arc::new(|model: &str| {
            get_effective_context_window_size(model)
        })),
    }))
}

/// 自动触发器尊重 `compaction.enabled`（默认 true）；显式
/// CLAUDE_CODE_COMPACTION_ENABLED 优先。手动 `/compact` 不受此门控。
fn is_compaction_enabled_in_settings(
    settings: Option<&CompactionSettingsSchema>,
    env: &HashMap<String, Option<String>>,
) -> bool {
    if let Some(from_env) = env_boolean(env, "CLAUDE_CODE_COMPACTION_ENABLED") {
        return from_env;
    }
    if let Some(settings) = settings {
        if settings.enabled == Some(false) {
            return false;
        }
    }
    true
}

/// TS `LegacySoftDepsInput`。
#[derive(Clone)]
pub struct LegacySoftDepsInput {
    pub tool_use_context: Arc<ToolUseContext>,
    pub cache_safe_params: Arc<CacheSafeParams>,
    pub query_source: Option<QuerySource>,
    pub token_count: i64,
    pub auto_compact_threshold: i64,
    pub tracking: Option<CompactionV2Tracking>,
    /// 只计算预览：`compactConversation` 跳过 session/transcript 写入，
    /// 下面的清理/复位推迟到提交路径。
    pub defer_apply: bool,
    /// 测试缝；生产用 legacy 模块。
    pub compact_conversation: Option<CompactConversationFn>,
    pub run_post_compact_cleanup: Option<Arc<dyn Fn(Option<&str>) + Send + Sync>>,
    pub set_last_summarized_message_id: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// `compact_conversation` 的可注入签名（对齐 TS `typeof compactConversation`）。
pub type CompactConversationFn = Arc<
    dyn for<'a> Fn(
            Vec<Message>,
            Option<&'a ToolUseContext>,
            &'a CacheSafeParams,
            bool,
            Option<&'a str>,
            bool,
            Option<&'a RecompactionInfo>,
            Option<&'a CompactConversationOptions>,
        ) -> BoxFuture<'a, Result<CompactionResult, CompactError>>
        + Send
        + Sync,
>;

/// 默认 `compact_conversation` 的 HRTB 包装。
fn compact_conversation_wrapped<'a>(
    messages: Vec<Message>,
    context: Option<&'a ToolUseContext>,
    cache_safe_params: &'a CacheSafeParams,
    suppress_follow_up_questions: bool,
    custom_instructions: Option<&'a str>,
    is_auto_compact: bool,
    recompaction_info: Option<&'a RecompactionInfo>,
    options: Option<&'a CompactConversationOptions>,
) -> BoxFuture<'a, Result<CompactionResult, CompactError>> {
    Box::pin(compact_conversation(
        messages,
        context,
        cache_safe_params,
        suppress_follow_up_questions,
        custom_instructions,
        is_auto_compact,
        recompaction_info.cloned(),
        options,
    ))
}

/// TS `createLegacySoftDeps(input)`。
///
/// 围绕 legacy `compactConversation` 路径构建 soft-method 运行器。
///
/// 直跑保留历史行为：先摘要，再立即复位 summarized-message id 并做压缩后
/// 清理。预览（deferApply）只摘要；那些写入发生在结果提交的
/// `applyCompactionOutcome` 里。
pub fn create_legacy_soft_deps(input: LegacySoftDepsInput) -> SoftMethodDeps {
    let compact_conversation = input
        .compact_conversation
        .clone()
        .unwrap_or_else(|| Arc::new(compact_conversation_wrapped));
    SoftMethodDeps {
        runner: Box::new(LegacySoftRunner {
            tool_use_context: input.tool_use_context,
            cache_safe_params: input.cache_safe_params,
            query_source: input.query_source,
            token_count: input.token_count,
            auto_compact_threshold: input.auto_compact_threshold,
            tracking: input.tracking,
            defer_apply: input.defer_apply,
            compact_conversation,
            run_post_compact_cleanup: input.run_post_compact_cleanup,
            set_last_summarized_message_id: input.set_last_summarized_message_id,
        }),
        instructions: None,
        resolve_threshold_tokens: None,
    }
}

struct LegacySoftRunner {
    tool_use_context: Arc<ToolUseContext>,
    cache_safe_params: Arc<CacheSafeParams>,
    query_source: Option<QuerySource>,
    token_count: i64,
    auto_compact_threshold: i64,
    tracking: Option<CompactionV2Tracking>,
    defer_apply: bool,
    compact_conversation: CompactConversationFn,
    run_post_compact_cleanup: Option<Arc<dyn Fn(Option<&str>) + Send + Sync>>,
    set_last_summarized_message_id: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl crate::services::compaction2::methods::soft::SoftCompactRunner for LegacySoftRunner {
    fn run<'a>(
        &'a self,
        request: &'a SoftCompactRequest,
    ) -> BoxFuture<'a, Result<SoftCompactRun, String>> {
        Box::pin(async move {
            let is_auto_compact = request.trigger != TriggerReason::Manual;
            let recompaction_info = RecompactionInfo {
                is_recompaction_in_chain: self
                    .tracking
                    .as_ref()
                    .map(|tracking| tracking.compacted)
                    .unwrap_or(false),
                turns_since_previous_compact: self
                    .tracking
                    .as_ref()
                    .map(|tracking| tracking.turn_counter)
                    .unwrap_or(-1),
                previous_compact_turn_id: None,
                auto_compact_threshold: self.auto_compact_threshold,
                query_source: self.query_source.clone(),
            };
            let result = match (self.compact_conversation)(
                request.messages.clone(),
                Some(&self.tool_use_context),
                &self.cache_safe_params,
                is_auto_compact,
                request.instructions.as_deref(),
                is_auto_compact,
                Some(&recompaction_info),
                Some(&CompactConversationOptions {
                    defer_apply: self.defer_apply,
                }),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => return Err(error.to_string()),
            };
            if !self.defer_apply {
                match &self.set_last_summarized_message_id {
                    Some(reset) => reset(),
                    None => set_last_summarized_message_id(),
                }
                match &self.run_post_compact_cleanup {
                    Some(cleanup) => cleanup(self.query_source.as_deref()),
                    None => run_post_compact_cleanup(self.query_source.as_deref()),
                }
            }
            let tokens_before = result.pre_compact_token_count.unwrap_or(self.token_count);
            let tokens_after = result
                .true_post_compact_token_count
                .or(result.post_compact_token_count)
                .unwrap_or(0);
            Ok(SoftCompactRun {
                result,
                tokens_before,
                tokens_after,
            })
        })
    }
}

/// TS `extractLegacySummaryText(result)`：把 legacy 摘要消息拼成交接文档正文。
fn extract_legacy_summary_text(result: &CompactionResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in &result.summary_messages {
        match &message.message.content {
            Content::Text(text) => parts.push(text.clone()),
            Content::Blocks(blocks) => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
        }
    }
    let summary = parts.join("\n").trim().to_string();
    if !summary.is_empty() {
        return summary;
    }
    result
        .user_display_message
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_default()
}

/// TS `createLegacyHandoffDeps(input, instructions?)`。
///
/// handoff 复用 soft 运行器，不引入第二个 API 客户端。运行器把 handoff
/// 提示词当作 `/compact` 自定义指令接收；其摘要消息成为交接文档。
///
/// 内部运行恒为只计算（deferApply: true）：结果仍可能被进度护卫拒绝，
/// 被拒的 handoff 不得留下任何 transcript/hook/cleanup 痕迹。完整 legacy
/// 结果随结果的 `preserveData` 返回，接受的 handoff 保留 messagesToKeep、
/// 附件与 hook 结果，编排器恰提交一次其副作用。
pub fn create_legacy_handoff_deps(
    input: LegacySoftDepsInput,
    instructions: Option<&str>,
) -> HandoffMethodDeps {
    let soft = Arc::new(create_legacy_soft_deps(LegacySoftDepsInput {
        defer_apply: true,
        ..input
    }));
    HandoffMethodDeps {
        summarize: Some(Box::new(
            move |ctx: &CompactionContext, request: &HandoffSummaryRequest| {
                let soft = soft.clone();
                Box::pin(async move {
                    let run = match soft.runner.run(&SoftCompactRequest {
                        messages: ctx.messages.clone(),
                        instructions: Some(request.instructions.clone()),
                        trigger: ctx.trigger,
                        is_auto_compact: true,
                    })
                    .await
                    {
                        Ok(run) => run,
                        Err(reason) => return Err(reason),
                    };
                    let summary = extract_legacy_summary_text(&run.result);
                    if summary.is_empty() {
                        return Err("handoff summary was empty".to_string());
                    }
                    Ok(HandoffSummaryValue::Summary(HandoffSummary {
                        document: summary,
                        legacy_compaction_result: serde_json::to_value(&run.result).ok(),
                    }))
                })
            },
        )),
        instructions: instructions.map(str::to_string),
        is_available: None,
    }
}

/// TS `methodMetadata(outcome)`：摘要方法的展示元数据。
fn method_metadata(tokens_saved: i64, warning: Option<String>) -> CompactionMethodMetadata {
    CompactionMethodMetadata {
        tokens_saved: Some(tokens_saved),
        warning,
    }
}

/// TS `getLegacyCompactionResult(outcome?)`。
fn get_legacy_compaction_result(outcome: Option<&CompactionOutcome>) -> Option<CompactionResult> {
    let CompactionOutcome::Compacted {
        preserve_data,
        summary,
        cut_point,
        tokens_saved,
        ..
    } = outcome?
    else {
        return None;
    };
    if let Some(data) = preserve_data {
        if let Some(candidate) = data.get("legacyCompactionResult") {
            if candidate.get("boundaryMarker").is_some()
                && candidate.get("summaryMessages").is_some()
            {
                if let Ok(result) = serde_json::from_value::<CompactionResult>(candidate.clone()) {
                    return Some(result);
                }
            }
        }
    }
    // remote/snapcompact 结果带摘要与切割点但没有 legacy 结果。构造
    // query 循环/transcript 能理解的最小形状，免得结果被丢弃、soft 再跑
    // 一次摘要调用。
    if let Some(summary) = summary {
        if !summary.is_empty() {
            return Some(build_legacy_result_from_outcome(summary, cut_point, *tokens_saved));
        }
    }
    None
}

/// TS `buildLegacyResultFromOutcome(outcome)`。
fn build_legacy_result_from_outcome(
    summary: &str,
    cut_point: &CutPoint,
    tokens_saved: i64,
) -> CompactionResult {
    let tokens_before = cut_point.tokens_before;
    let tokens_after = std::cmp::max(0, tokens_before - tokens_saved);
    CompactionResult {
        boundary_marker: cut_point.boundary_message.clone(),
        summary_messages: vec![create_user_message(summary, false, true, true)],
        attachments: Vec::new(),
        hook_results: Vec::new(),
        messages_to_keep: None,
        user_display_message: None,
        pre_compact_token_count: Some(tokens_before),
        post_compact_token_count: Some(tokens_after),
        true_post_compact_token_count: Some(tokens_after),
        compaction_usage: None,
    }
}

fn default_count_tokens(messages: &[Message]) -> i64 {
    // 对齐 TS `tokenCountWithEstimation`；仓内等价是 micro_compact 的估算
    // （autoCompact/compact 链同款）。
    estimate_message_tokens(messages) as i64
}

fn default_resolve_window_tokens(model: &str) -> i64 {
    get_effective_context_window_size(model)
}

fn default_now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// TS `defaultEstimateEntryTokens(entry, model?)`。
fn default_estimate_entry_tokens(entry: &EntryView, model: &str) -> i64 {
    if let Some(tokens) = entry.tokens {
        return tokens;
    }
    let Some(message) = &entry.message else {
        return 0;
    };
    let Ok(serialized) = serde_json::to_string(message) else {
        return 0;
    };
    if serialized.is_empty() {
        return 0;
    }
    if !model.is_empty() {
        return estimate_tokens(&serialized, model).tokens as i64;
    }
    ((serialized.len() as f64) / 4.0).ceil() as i64
}

/// TS `CompactionSettingsSources`。
#[derive(Default)]
pub struct CompactionSettingsSources<'a> {
    /// settings.json 的原始 `settings.compaction`。
    pub compaction: Option<&'a CompactionSettingsSchema>,
    /// 环境映射；默认读进程环境。
    pub env: Option<HashMap<String, Option<String>>>,
}

/// TS `resolveSettings(input, sources)`。
///
/// 优先级（高 → 低）：
///   1. 显式请求设置
///   2. CLAUDE_CODE_COMPACTION_* 环境变量（仅当设置时）
///   3. settings.json 的 settings.compaction
///   4. DEFAULT_COMPACTION_SETTINGS
///
/// 非法值逐字段跳过：坏的请求值回落到下一源，而不是毒化整个策略。
pub fn resolve_settings(
    input: Option<&CompactionSettingsOverride>,
    sources: CompactionSettingsSources<'_>,
) -> CompactionSettings {
    let global = sources.compaction;
    let env = sources.env.unwrap_or_else(default_compaction_env);

    let method_order = input
        .and_then(|input| {
            input
                .method_order
                .clone()
                .filter(|order| !order.is_empty())
        })
        .or_else(|| env_method_order(&env, "CLAUDE_CODE_COMPACTION_METHOD_ORDER"))
        .or_else(|| {
            global.and_then(|global| {
                global
                    .method_order
                    .as_ref()
                    .and_then(|names| sanitize_method_order(names))
            })
        })
        .unwrap_or_else(|| DEFAULT_COMPACTION_SETTINGS.method_order.to_vec());

    CompactionSettings {
        keep_recent_tokens: input
            .and_then(|input| input.keep_recent_tokens)
            .and_then(as_non_negative)
            .or_else(|| {
                env_integer(&env, "CLAUDE_CODE_COMPACTION_KEEP_RECENT_TOKENS", as_non_negative)
            })
            .or_else(|| global.and_then(|global| global.keep_recent_tokens).and_then(as_non_negative))
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.keep_recent_tokens),
        reserve_tokens: input
            .and_then(|input| input.reserve_tokens)
            .and_then(as_positive)
            .or_else(|| env_integer(&env, "CLAUDE_CODE_COMPACTION_RESERVE_TOKENS", as_positive))
            .or_else(|| global.and_then(|global| global.reserve_tokens).and_then(as_positive)),
        threshold_tokens: input
            .and_then(|input| input.threshold_tokens)
            .and_then(as_positive)
            .or_else(|| env_integer(&env, "CLAUDE_CODE_COMPACTION_THRESHOLD_TOKENS", as_positive))
            .or_else(|| global.and_then(|global| global.threshold_tokens).and_then(as_positive)),
        threshold_percent: input
            .and_then(|input| input.threshold_percent)
            .and_then(as_threshold_percent)
            .or_else(|| {
                env_integer(&env, "CLAUDE_CODE_COMPACTION_THRESHOLD_PERCENT", as_threshold_percent)
            })
            .or_else(|| {
                global
                    .and_then(|global| global.threshold_percent)
                    .and_then(as_threshold_percent)
            }),
        mid_turn_enabled: input
            .and_then(|input| input.mid_turn_enabled)
            .or_else(|| env_boolean(&env, "CLAUDE_CODE_COMPACTION_MID_TURN_ENABLED"))
            .or_else(|| global.and_then(|global| global.mid_turn_enabled))
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.mid_turn_enabled),
        auto_continue: input
            .and_then(|input| input.auto_continue)
            .or_else(|| env_boolean(&env, "CLAUDE_CODE_COMPACTION_AUTO_CONTINUE"))
            .or_else(|| global.and_then(|global| global.auto_continue))
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.auto_continue),
        async_enabled: input
            .and_then(|input| input.async_enabled)
            .or_else(|| env_boolean(&env, "CLAUDE_CODE_COMPACTION_ASYNC_ENABLED"))
            .or_else(|| global.and_then(|global| global.async_enabled))
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.async_enabled),
        idle_enabled: input
            .and_then(|input| input.idle_enabled)
            .or_else(|| env_boolean(&env, "CLAUDE_CODE_COMPACTION_IDLE_ENABLED"))
            .or_else(|| global.and_then(|global| global.idle_enabled))
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.idle_enabled),
        idle_threshold_tokens: input
            .and_then(|input| input.idle_threshold_tokens)
            .and_then(as_positive)
            .or_else(|| {
                env_integer(&env, "CLAUDE_CODE_COMPACTION_IDLE_THRESHOLD_TOKENS", as_positive)
            })
            .or_else(|| {
                global
                    .and_then(|global| global.idle_threshold_tokens)
                    .and_then(as_positive)
            })
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.idle_threshold_tokens),
        idle_timeout_seconds: input
            .and_then(|input| input.idle_timeout_seconds)
            .and_then(as_idle_timeout)
            .or_else(|| {
                env_integer(&env, "CLAUDE_CODE_COMPACTION_IDLE_TIMEOUT_SECONDS", as_idle_timeout)
            })
            .or_else(|| {
                global
                    .and_then(|global| global.idle_timeout_seconds)
                    .and_then(as_idle_timeout)
            })
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS.idle_timeout_seconds),
        method_order,
    }
}

/// TS `asPositiveInt(value)`：正整数校验。
fn as_positive(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

/// TS `asNonNegativeInt(value)`：非负整数校验。
fn as_non_negative(value: i64) -> Option<i64> {
    (value >= 0).then_some(value)
}

/// TS `asThresholdPercent(value)`：`asPositiveInt` 且 ≤ 99。
fn as_threshold_percent(value: i64) -> Option<i64> {
    (1..=99).contains(&value).then_some(value)
}

/// TS `asIdleTimeout(value)`：整数且 ≥ 60 秒。
fn as_idle_timeout(value: i64) -> Option<i64> {
    (value >= 60).then_some(value)
}

/// TS `sanitizeMethodOrder(value)`：过滤非法方法名；全被过滤时返回 None。
fn sanitize_method_order(value: &[String]) -> Option<Vec<CompactionMethodName>> {
    let filtered: Vec<CompactionMethodName> = value
        .iter()
        .filter_map(|name| is_compaction_method_name(name))
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn env_value(env: &HashMap<String, Option<String>>, name: &str) -> Option<String> {
    let raw = env.get(name)?.clone()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn env_boolean(env: &HashMap<String, Option<String>>, name: &str) -> Option<bool> {
    let raw = env_value(env, name)?.to_ascii_lowercase();
    match raw.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_integer(
    env: &HashMap<String, Option<String>>,
    name: &str,
    guard: fn(i64) -> Option<i64>,
) -> Option<i64> {
    env_value(env, name)
        .and_then(|raw| raw.parse::<i64>().ok())
        .and_then(guard)
}

fn env_method_order(
    env: &HashMap<String, Option<String>>,
    name: &str,
) -> Option<Vec<CompactionMethodName>> {
    let raw = env_value(env, name)?;
    let names: Vec<String> = raw
        .split(',')
        .map(|part| part.trim().to_string())
        .collect();
    sanitize_method_order(&names)
}

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// `CLAUDE_CODE_COMPACTION_*` 相关键的进程环境投影。
fn default_compaction_env() -> HashMap<String, Option<String>> {
    let mut env = HashMap::new();
    for key in [
        "CLAUDE_CODE_COMPACTION_ENABLED",
        "CLAUDE_CODE_COMPACTION_METHOD_ORDER",
        "CLAUDE_CODE_COMPACTION_KEEP_RECENT_TOKENS",
        "CLAUDE_CODE_COMPACTION_RESERVE_TOKENS",
        "CLAUDE_CODE_COMPACTION_THRESHOLD_TOKENS",
        "CLAUDE_CODE_COMPACTION_THRESHOLD_PERCENT",
        "CLAUDE_CODE_COMPACTION_MID_TURN_ENABLED",
        "CLAUDE_CODE_COMPACTION_AUTO_CONTINUE",
        "CLAUDE_CODE_COMPACTION_ASYNC_ENABLED",
        "CLAUDE_CODE_COMPACTION_IDLE_ENABLED",
        "CLAUDE_CODE_COMPACTION_IDLE_THRESHOLD_TOKENS",
        "CLAUDE_CODE_COMPACTION_IDLE_TIMEOUT_SECONDS",
        "CCH_REMOTE_COMPACTION",
    ] {
        env.insert(key.to_string(), std::env::var(key).ok());
    }
    env
}

// ---------------------------------------------------------------------------
// 尚未转换依赖的 TODO 存根（对齐 shakeReducer.rs 的 TODO 风格）。
// ---------------------------------------------------------------------------

/// TODO(services/SessionMemory/sessionMemoryUtils.ts)：
/// `setLastSummarizedMessageId(undefined)`。SessionMemory 未转换，暂为 no-op。
fn set_last_summarized_message_id() {}

/// TODO(utils/hooks)：`executePostCompactHooks(data, signal?)`。hooks 引擎
/// 尚未暴露 PostCompact 执行入口，暂返回空结果（不 panic、不写盘）。
fn execute_post_compact_hooks<'a>(
    data: &'a PostCompactHookData,
    signal: Option<&'a CancellationToken>,
) -> BoxFuture<'a, PostCompactHookResult> {
    let _ = (data, signal);
    Box::pin(async move { PostCompactHookResult::default() })
}
