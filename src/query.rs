//! query.rs — 主查询 API（重写自 TS `src/query.ts`）。
//!
//! 语义对齐说明：
//! - TS 的 `query()` 是 async generator（yield StreamEvent / Message …，return
//!   Terminal）。Rust 侧无 `futures` 依赖，改为 sink 回调模型：
//!   `query(params, sink)` 把每次 yield 映射为 `sink.emit(QueryEvent)`，
//!   生成器 return 映射为 `Ok(Terminal)`；throw 映射为 `Err(QueryError)`。
//! - turn 循环结构、流式消费、工具分发、中断处理、用量跟踪与 TS 对齐；
//!   依赖未转换的子系统（compaction / reactiveCompact / contextCollapse /
//!   speculation / langfuse / autonomy 队列 / stop hooks / 流式工具执行器 /
//!   toolUseSummary）以占位或最小实现表达，见文件尾「占位清单」。
//! - 消息模型使用 `services::api::claude` 的 wire 类型（`types/message.ts`
//!   未转换），工具上下文使用 `tool::ToolUseContext`。

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::bootstrap::state::{
    add_to_total_cost_state, get_current_turn_token_budget, get_turn_output_tokens,
    increment_budget_continuation_count, ModelUsage,
};
use crate::constants::query_source::QuerySource;
use crate::services::api::claude::{
    self, accumulate_usage, empty_usage, ApiError, AssistantMessage, AssistantMessageBody, Message,
    MessageContent, NonNullableUsage, Options, SystemPrompt, TaskBudget, TextBlockParam,
    ThinkingConfig, ToolParam,
};
use crate::tool::{
    find_tool_by_name, BoxFuture, CanUseToolFn, QueryChainTracking, Tool, ToolError,
    ToolUseContext, Tools,
};
use crate::types::permissions::PermissionMode;
use crate::utils::log::log_error_dyn;
use crate::utils::model::model::{
    get_effective_context_tokens, get_runtime_main_loop_model, render_model_name,
};

// ---------------------------------------------------------------------------
// 常量（TS 对应项未转换 → 本地常量）
// ---------------------------------------------------------------------------

/// TS `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT = 3`。
const MAX_OUTPUT_TOKENS_RECOVERY_LIMIT: u32 = 3;

/// submit-interrupt 的 abort reason（对齐 TS `signal.reason !== 'interrupt'`）。
const INTERRUPT_REASON: &str = "interrupt";

/// TS `services/api/errors.ts` 的 PROMPT_TOO_LONG_ERROR_MESSAGE。
const PROMPT_TOO_LONG_ERROR_MESSAGE: &str = "Prompt is too long";

/// max_output_tokens 恢复注入的元消息（对齐 TS query.ts 原文）。
const MAX_OUTPUT_TOKENS_RECOVERY_MESSAGE: &str =
    "Output token limit hit. Resume directly — no apology, no recap of what you were doing. \
     Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";

// ---------------------------------------------------------------------------
// 参数 / 事件 / 返回
// ---------------------------------------------------------------------------

/// `QueryParams`：对齐 TS `query()` 的入参（deps 除外，见 `QueryDeps`）。
pub struct QueryParams {
    pub messages: Vec<Message>,
    pub system_prompt: SystemPrompt,
    pub user_context: HashMap<String, String>,
    pub system_context: HashMap<String, String>,
    pub can_use_tool: CanUseToolFn,
    pub tool_use_context: ToolUseContext,
    pub fallback_model: Option<String>,
    pub query_source: QuerySource,
    pub max_output_tokens_override: Option<i64>,
    pub max_turns: Option<u32>,
    pub skip_cache_write: bool,
    /// API task_budget 的 `total`（wire `remaining` 由循环内部累计维护）。
    pub task_budget: Option<i64>,
    /// 注入依赖；`None` 时使用 `ProductionQueryDeps`（对齐 TS `productionDeps()`）。
    pub deps: Option<Arc<dyn QueryDeps>>,
}

/// 生成器 yield 项的最小等价。
#[derive(Debug, Clone, PartialEq)]
pub enum QueryEvent {
    /// `stream_request_start`。
    StreamRequestStart,
    /// 流式产出的助手消息（含回放/恢复路径再投放的错误消息）。
    AssistantMessage(AssistantMessage),
    /// 用户消息（中断、恢复提示、孤儿 tool_result 等）。
    UserMessage(claude::UserMessage),
    /// 墓碑：模型回退时需从 UI / transcript 移除的孤儿助手消息。
    Tombstone(AssistantMessage),
    /// 系统提示（模型回退通知等）。
    SystemMessage(SystemNotice),
    /// 附件消息（max_turns_reached / hook_stopped_continuation）。
    Attachment(AttachmentKind),
    /// 工具执行结果消息。
    ToolResult(claude::UserMessage),
}

/// sink 契约：每次 `yield` 一次调用。`Send` 以兼容跨线程驱动。
pub trait QuerySink: Send {
    fn emit(&mut self, event: QueryEvent);
}

#[derive(Debug, Clone, PartialEq)]
pub struct SystemNotice {
    /// 对齐 TS `createSystemMessage(text, level)` 的 level（如 "warning"）。
    pub level: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttachmentKind {
    MaxTurnsReached { max_turns: u32, turn_count: u32 },
    HookStoppedContinuation,
}

/// turn 结束原因（对齐 TS `query/transitions.ts` 的 `Terminal`）。
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalReason {
    Completed,
    AbortedStreaming,
    AbortedTools,
    ModelError(String),
    MaxTurns { turn_count: u32 },
    BlockingLimit,
    PromptTooLong,
    ImageError,
    StopHookPrevented,
    HookStopped,
    RapidRefillBreaker,
}

/// `query()` 的 return 值；携带本 query 累计的 usage（用量跟踪）。
#[derive(Debug, Clone, PartialEq)]
pub struct Terminal {
    pub reason: TerminalReason,
    pub usage: NonNullableUsage,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("API error: {0}")]
    Api(#[from] ApiError),
    #[error("tool error: {0}")]
    Tool(String),
}

// ---------------------------------------------------------------------------
// 依赖注入（`query/deps.ts`）
// ---------------------------------------------------------------------------

/// 一次模型请求（对齐 TS `deps.callModel({...})` 的载荷）。
pub struct ModelRequest<'a> {
    /// 已注入 userContext 的会话消息。
    pub messages: Vec<Message>,
    /// 已构建的系统提示块。
    pub system: Vec<TextBlockParam>,
    pub thinking_config: &'a ThinkingConfig,
    /// 由 `Tools` 转换的工具 schema。
    pub tools: Vec<ToolParam>,
    pub options: Options,
}

/// 流式请求结果：deps 负责把流式事件实时推入 sink，循环只消费聚合结果。
pub struct ModelStreamOutcome {
    pub assistant_messages: Vec<AssistantMessage>,
    pub tool_use_blocks: Vec<ToolUseBlock>,
    /// 对齐 TS `streamingFallbackOccured`：流内回退已发生，循环需投放墓碑。
    pub streaming_fallback_occurred: bool,
    /// 最后一条助手消息是否为 API 错误合成消息（对齐 TS `isApiErrorMessage`）。
    pub last_is_api_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub trait QueryDeps: Send + Sync {
    /// 对齐 TS `deps.uuid()`。
    fn uuid(&self) -> String {
        Uuid::new_v4().to_string()
    }
    /// 对齐 TS `deps.callModel`：驱动流式请求，把 AssistantMessage/StreamEvent
    /// 实时推入 sink，结束后返回聚合结果。
    fn call_model<'a>(
        &'a self,
        req: ModelRequest<'a>,
        sink: &'a mut dyn QuerySink,
    ) -> BoxFuture<'a, Result<ModelStreamOutcome, ApiError>>;
}

/// 生产依赖（对齐 TS `productionDeps()`）。
///
/// 占位：HTTP 传输层未就绪（`claude::query_model_with_streaming` 为 reqwest
/// 占位）。待其就绪后，此处应改为驱动 SSE 流（`extract_sse_data` +
/// `parse_stream_event` + `StreamAccumulator`），按 `StreamEventOutcome` 把
/// 助手消息实时推入 sink。
pub struct ProductionQueryDeps;

impl QueryDeps for ProductionQueryDeps {
    fn call_model<'a>(
        &'a self,
        req: ModelRequest<'a>,
        _sink: &'a mut dyn QuerySink,
    ) -> BoxFuture<'a, Result<ModelStreamOutcome, ApiError>> {
        Box::pin(async move {
            let system: SystemPrompt = req.system.iter().map(|b| b.text.clone()).collect();
            claude::query_model_with_streaming(
                &req.messages,
                &system,
                req.thinking_config,
                &req.tools,
                &req.options,
            )
            .await?;
            Ok(ModelStreamOutcome {
                assistant_messages: Vec::new(),
                tool_use_blocks: Vec::new(),
                streaming_fallback_occurred: false,
                last_is_api_error: false,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// 循环状态
// ---------------------------------------------------------------------------

/// 循环迭代间的可变状态（对齐 TS `type State`）。
#[allow(dead_code)]
struct LoopState {
    messages: Vec<Message>,
    max_output_tokens_override: Option<i64>,
    max_output_tokens_recovery_count: u32,
    has_attempted_reactive_compact: bool,
    stop_hook_active: bool,
    turn_count: u32,
    /// 上一次 continue 的原因（对齐 TS `State.transition`，便于断言恢复路径）。
    transition: Option<ContinueReason>,
    /// 本 query 累计 usage。
    cumulative_usage: NonNullableUsage,
}

/// TS `query/transitions.ts` 的 `Continue` 最小等价。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum ContinueReason {
    NextTurn,
    MaxOutputTokensRecovery { attempt: u32 },
    TokenBudgetContinuation,
    StopHookBlocking,
    ReactiveCompactRetry,
    ModelFallback,
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 主查询 API：驱动 turn 循环直至 Terminal。
///
/// 对齐 TS `query()`：外层包装的 langfuse trace / autonomy 队列 finalize /
/// 推测压缩取消等副作用均属未转换子系统（见文件尾占位清单），此处不再包裹。
pub async fn query(
    mut params: QueryParams,
    sink: &mut dyn QuerySink,
) -> Result<Terminal, QueryError> {
    let deps: Arc<dyn QueryDeps> = match params.deps.take() {
        Some(d) => d,
        None => Arc::new(ProductionQueryDeps),
    };
    query_loop(&mut params, &deps, sink).await
}

async fn query_loop(
    params: &mut QueryParams,
    deps: &Arc<dyn QueryDeps>,
    sink: &mut dyn QuerySink,
) -> Result<Terminal, QueryError> {
    let system_prompt = params.system_prompt.clone();
    let user_context = params.user_context.clone();
    let system_context = params.system_context.clone();
    let fallback_model = params.fallback_model.clone();
    let query_source = params.query_source.clone();
    let max_turns = params.max_turns;
    let skip_cache_write = params.skip_cache_write;
    let task_budget_total = params.task_budget;

    let mut state = LoopState {
        messages: std::mem::take(&mut params.messages),
        max_output_tokens_override: params.max_output_tokens_override,
        max_output_tokens_recovery_count: 0,
        has_attempted_reactive_compact: false,
        stop_hook_active: false,
        turn_count: 1,
        transition: None,
        cumulative_usage: empty_usage(),
    };
    // task_budget.remaining 跨压缩边界累计（压缩模块未转换 → 恒为 None）。
    let mut task_budget_remaining: Option<i64> = None;

    loop {
        sink.emit(QueryEvent::StreamRequestStart);

        // 每轮迭代克隆上下文；变更在 continue 前写回 params（对齐 TS 对
        // 同一对象的就地修改）。
        let mut ctx = params.tool_use_context.clone();
        let query_tracking = match &ctx.query_tracking {
            Some(t) => QueryChainTracking {
                chain_id: t.chain_id.clone(),
                depth: t.depth + 1,
            },
            None => QueryChainTracking {
                chain_id: deps.uuid(),
                depth: 0,
            },
        };
        ctx.query_tracking = Some(query_tracking);

        let messages_for_query = state.messages.clone();
        let full_system_prompt = append_system_context(&system_prompt, &system_context);

        let app_state = (params.tool_use_context.get_app_state)();
        let permission_mode = permission_mode_from_state(&app_state);
        let mut current_model = get_runtime_main_loop_model(
            permission_mode,
            &ctx.options.main_loop_model,
            // doesMostRecentAssistantMessageExceed200k 未转换 → 恒不超限。
            false,
        )
        .to_string();

        // 阻塞上限保护（auto-compact 未转换，无压缩让位逻辑；compact /
        // session_memory fork 查询跳过，否则会自死锁）。
        if query_source != "compact" && query_source != "session_memory" {
            let estimated = estimate_tokens(&messages_for_query);
            if estimated > get_effective_context_tokens(&current_model) {
                sink.emit(QueryEvent::AssistantMessage(create_assistant_api_error(
                    PROMPT_TOO_LONG_ERROR_MESSAGE,
                )));
                return Ok(terminal(TerminalReason::BlockingLimit, &state));
            }
        }

        let mut assistant_messages: Vec<AssistantMessage> = Vec::new();
        let mut tool_results: Vec<claude::UserMessage> = Vec::new();
        let mut tool_use_blocks: Vec<ToolUseBlock> = Vec::new();
        let mut needs_follow_up = false;
        let mut last_is_api_error = false;

        // ---- 流式请求 + 模型回退重试 ----
        let mut attempt_with_fallback = true;
        while attempt_with_fallback {
            attempt_with_fallback = false;

            let options = build_options(
                &current_model,
                &ctx,
                &query_source,
                &fallback_model,
                state.max_output_tokens_override,
                skip_cache_write,
                task_budget_total.map(|total| TaskBudget {
                    total,
                    remaining: task_budget_remaining,
                }),
            );
            let system_blocks = system_prompt_blocks(&full_system_prompt);
            let req_messages = prepend_user_context(&messages_for_query, &user_context);

            match deps
                .call_model(
                    ModelRequest {
                        messages: req_messages,
                        system: system_blocks,
                        thinking_config: &ctx.options.thinking_config,
                        tools: tool_params(&ctx.options.tools),
                        options,
                    },
                    sink,
                )
                .await
            {
                Ok(outcome) => {
                    // 流内回退：前次局部消息的签名非法，投放墓碑后整体重来。
                    if outcome.streaming_fallback_occurred {
                        for msg in &assistant_messages {
                            sink.emit(QueryEvent::Tombstone(msg.clone()));
                        }
                        for orphan in
                            missing_tool_result_messages(&assistant_messages, "Model fallback triggered")
                        {
                            sink.emit(QueryEvent::UserMessage(orphan));
                        }
                        assistant_messages.clear();
                        tool_results.clear();
                        tool_use_blocks.clear();
                        needs_follow_up = false;
                    }
                    last_is_api_error = outcome.last_is_api_error;
                    for msg in &outcome.assistant_messages {
                        sink.emit(QueryEvent::AssistantMessage(msg.clone()));
                        assistant_messages.push(msg.clone());
                        if let Some(u) = &msg.message.usage {
                            state.cumulative_usage =
                                accumulate_usage(&state.cumulative_usage, u);
                            record_usage_to_state(u, &current_model);
                        }
                        let blocks = extract_tool_use_blocks(msg);
                        if !blocks.is_empty() {
                            needs_follow_up = true;
                            tool_use_blocks.extend(blocks);
                        }
                    }
                }
                Err(ApiError::FallbackTriggered) => {
                    if let Some(fb) = fallback_model.as_deref() {
                        for msg in &assistant_messages {
                            sink.emit(QueryEvent::Tombstone(msg.clone()));
                        }
                        for orphan in
                            missing_tool_result_messages(&assistant_messages, "Model fallback triggered")
                        {
                            sink.emit(QueryEvent::UserMessage(orphan));
                        }
                        assistant_messages.clear();
                        tool_results.clear();
                        tool_use_blocks.clear();
                        needs_follow_up = false;

                        let original = current_model.clone();
                        current_model = fb.to_string();
                        ctx.options.main_loop_model = fb.to_string();
                        params.tool_use_context.options.main_loop_model = fb.to_string();

                        // stripSignatureBlocks（USER_TYPE=ant 专用）未转换 → 跳过。
                        sink.emit(QueryEvent::SystemMessage(SystemNotice {
                            level: "warning".to_string(),
                            text: format!(
                                "Switched to {} due to high demand for {}",
                                render_model_name(fb),
                                render_model_name(&original)
                            ),
                        }));
                        state.transition = Some(ContinueReason::ModelFallback);
                        attempt_with_fallback = true;
                        continue;
                    }
                    return Ok(terminal(
                        TerminalReason::ModelError(ApiError::FallbackTriggered.to_string()),
                        &state,
                    ));
                }
                Err(e) => {
                    log_error_dyn(Box::new(e.clone()));
                    let error_message = e.to_string();
                    // 已产出 tool_use 却中断：补齐 tool_result，避免 API 状态损坏。
                    for orphan in missing_tool_result_messages(&assistant_messages, &error_message) {
                        sink.emit(QueryEvent::UserMessage(orphan));
                    }
                    sink.emit(QueryEvent::AssistantMessage(create_assistant_api_error(
                        &error_message,
                    )));
                    return Ok(terminal(
                        TerminalReason::ModelError(error_message),
                        &state,
                    ));
                }
            }
        }

        // ---- 流式期间被中断 ----
        if ctx.abort_controller.aborted() {
            for orphan in missing_tool_result_messages(&assistant_messages, "Interrupted by user") {
                sink.emit(QueryEvent::UserMessage(orphan));
            }
            if ctx.abort_controller.reason().as_deref() != Some(INTERRUPT_REASON) {
                sink.emit(QueryEvent::UserMessage(create_user_interruption_message(false)));
            }
            return Ok(terminal(TerminalReason::AbortedStreaming, &state));
        }

        if !needs_follow_up {
            let last = assistant_messages.last();

            // prompt-too-long 恢复链（collapse drain / reactiveCompact / V2）
            // 未转换 → 直接投放被扣留的错误并退出。
            if let Some(last) = last {
                if is_prompt_too_long(last) {
                    sink.emit(QueryEvent::AssistantMessage(last.clone()));
                    return Ok(terminal(TerminalReason::PromptTooLong, &state));
                }
                // max_output_tokens 恢复：注入元消息让模型续写，最多 3 次。
                if is_max_output_tokens(last) {
                    if state.max_output_tokens_recovery_count < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT {
                        let mut next = messages_for_query;
                        next.extend(assistant_messages.iter().cloned().map(Message::Assistant));
                        next.push(Message::User(new_user_message(
                            MessageContent::Text(MAX_OUTPUT_TOKENS_RECOVERY_MESSAGE.to_string()),
                            true,
                        )));
                        state.messages = next;
                        state.max_output_tokens_recovery_count += 1;
                        state.transition = Some(ContinueReason::MaxOutputTokensRecovery {
                            attempt: state.max_output_tokens_recovery_count,
                        });
                        params.tool_use_context = ctx.clone();
                        continue;
                    }
                    // 恢复耗尽 → 投放被扣留的错误。
                    sink.emit(QueryEvent::AssistantMessage(last.clone()));
                }
                if last_is_api_error {
                    return Ok(terminal(
                        TerminalReason::ModelError(
                            last.message
                                .stop_reason
                                .clone()
                                .unwrap_or_else(|| "api_error".to_string()),
                        ),
                        &state,
                    ));
                }
            }

            // TOKEN_BUDGET 最小等价：未达预算则注入 nudge 续跑，否则结束。
            if let Some(budget) = get_current_turn_token_budget() {
                if get_turn_output_tokens() < budget {
                    increment_budget_continuation_count();
                    let mut next = messages_for_query;
                    next.extend(assistant_messages.iter().cloned().map(Message::Assistant));
                    next.push(Message::User(new_user_message(
                        MessageContent::Text(format!(
                            "Continue — {}/{} turn tokens used.",
                            get_turn_output_tokens(),
                            budget
                        )),
                        true,
                    )));
                    state.messages = next;
                    state.transition = Some(ContinueReason::TokenBudgetContinuation);
                    params.tool_use_context = ctx.clone();
                    continue;
                }
            }

            // stop hooks（handleStopHooks）未转换 → 无钩子拦截，直接完成。
            state.transition = None;
            return Ok(terminal(TerminalReason::Completed, &state));
        }

        // ---- 工具执行（streamingToolExecutor 未转换 → 串行 runTools 等价）----
        let (tool_result_messages, should_prevent_continuation) =
            run_tools(&tool_use_blocks, &ctx.options.tools, &params.can_use_tool, &ctx, sink)
                .await;
        tool_results.extend(tool_result_messages);

        // 工具执行期间被中断。
        if ctx.abort_controller.aborted() {
            if ctx.abort_controller.reason().as_deref() != Some(INTERRUPT_REASON) {
                sink.emit(QueryEvent::UserMessage(create_user_interruption_message(true)));
            }
            let next_turn_count = state.turn_count + 1;
            if let Some(max) = max_turns {
                if next_turn_count > max {
                    sink.emit(QueryEvent::Attachment(AttachmentKind::MaxTurnsReached {
                        max_turns: max,
                        turn_count: next_turn_count,
                    }));
                }
            }
            return Ok(terminal(TerminalReason::AbortedTools, &state));
        }

        if should_prevent_continuation {
            return Ok(terminal(TerminalReason::HookStopped, &state));
        }

        // 工具间刷新 MCP 工具（对齐 TS refreshTools）。Vec 无廉价同一性比较 →
        // 总是赋值刷新结果。
        if let Some(refresh) = ctx.options.refresh_tools.clone() {
            ctx.options.tools = refresh();
        }

        let next_turn_count = state.turn_count + 1;
        if let Some(max) = max_turns {
            if next_turn_count > max {
                sink.emit(QueryEvent::Attachment(AttachmentKind::MaxTurnsReached {
                    max_turns: max,
                    turn_count: next_turn_count,
                }));
                return Ok(terminal(
                    TerminalReason::MaxTurns {
                        turn_count: next_turn_count,
                    },
                    &state,
                ));
            }
        }

        let mut next_messages = messages_for_query;
        next_messages.extend(assistant_messages.into_iter().map(Message::Assistant));
        next_messages.extend(tool_results.into_iter().map(Message::User));

        state.messages = next_messages;
        state.turn_count = next_turn_count;
        state.max_output_tokens_recovery_count = 0;
        state.has_attempted_reactive_compact = false;
        state.max_output_tokens_override = None;
        state.transition = Some(ContinueReason::NextTurn);
        params.tool_use_context = ctx;
    }
}

fn terminal(reason: TerminalReason, state: &LoopState) -> Terminal {
    Terminal {
        reason,
        usage: state.cumulative_usage.clone(),
    }
}

// ---------------------------------------------------------------------------
// 工具执行（`services/tools/toolOrchestration.ts` 的 runTools 等价）
// ---------------------------------------------------------------------------

/// 串行执行 tool_use 块，把 tool_result 用户消息推入 sink 并返回。
///
/// 返回 (tool_result 消息列表, hook 是否要求终止续跑)。
async fn run_tools(
    tool_use_blocks: &[ToolUseBlock],
    tools: &Tools,
    can_use_tool: &CanUseToolFn,
    ctx: &ToolUseContext,
    sink: &mut dyn QuerySink,
) -> (Vec<claude::UserMessage>, bool) {
    let mut results = Vec::new();
    let mut should_prevent_continuation = false;

    for block in tool_use_blocks {
        if ctx.abort_controller.aborted() {
            break;
        }
        let outcome: Result<crate::tool::ToolResult<Value>, ToolError> = match find_tool_by_name(
            tools,
            &block.name,
        ) {
            None => Err(ToolError::Generic {
                message: format!("Tool not found: {}", block.name),
            }),
            Some(tool) => {
                let parent_message = Value::Null;
                tool.call(&block.input, ctx, can_use_tool, &parent_message, None)
                    .await
            }
        };

        let (content, is_error) = match outcome {
            Ok(res) => {
                for m in res.new_messages.unwrap_or_default() {
                    if m.get("type").and_then(Value::as_str) == Some("attachment")
                        && m
                            .get("attachment")
                            .and_then(|a| a.get("type"))
                            .and_then(Value::as_str)
                            == Some("hook_stopped_continuation")
                    {
                        should_prevent_continuation = true;
                    }
                }
                // ToolResult.data 序列化为 tool_result 内容（对齐 TS 渲染管线
                // 落盘前内容）；context_modifier 未转换 → 不应用。
                let body = serde_json::to_string(&res.data)
                    .unwrap_or_else(|_| "{}".to_string());
                (body, false)
            }
            Err(ToolError::Aborted) => ("Tool call aborted".to_string(), true),
            Err(e) => (e.to_string(), true),
        };

        let msg = new_user_message(
            MessageContent::Blocks(vec![json!({
                "type": "tool_result",
                "tool_use_id": block.id,
                "content": content,
                "is_error": is_error,
            })]),
            false,
        );
        sink.emit(QueryEvent::ToolResult(msg.clone()));
        results.push(msg);
    }

    (results, should_prevent_continuation)
}

// ---------------------------------------------------------------------------
// 消息构造助手（`utils/messages.ts` 未转换 → 本地最小等价）
// ---------------------------------------------------------------------------


fn new_user_message(content: MessageContent, is_meta: bool) -> claude::UserMessage {
    claude::UserMessage {
        message: claude::UserMessageBody { content },
        typ: "user".to_string(),
        uuid: Uuid::new_v4().to_string(),
        timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        is_meta: if is_meta { Some(true) } else { None },
    }
}

/// 对齐 TS `createAssistantAPIErrorMessage`（API 错误合成助手消息）。
fn create_assistant_api_error(content: &str) -> AssistantMessage {
    AssistantMessage {
        message: AssistantMessageBody {
            content: MessageContent::Blocks(vec![json!({
                "type": "text",
                "text": content,
            })]),
            model: None,
            usage: Some(empty_usage()),
            stop_reason: Some("error".to_string()),
        },
        request_id: None,
        typ: "assistant".to_string(),
        uuid: Uuid::new_v4().to_string(),
        timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        advisor_model: None,
    }
}

/// 对齐 TS `createUserInterruptionMessage`（结构化 subtype 未转换 → 文本）。
fn create_user_interruption_message(tool_use: bool) -> claude::UserMessage {
    let text = if tool_use {
        "[Request interrupted by user while running tools]"
    } else {
        "[Request interrupted by user]"
    };
    new_user_message(MessageContent::Text(text.to_string()), true)
}

/// 对齐 TS `yieldMissingToolResultBlocks`：为已产出的 tool_use 补错误结果。
fn missing_tool_result_messages(
    assistant_messages: &[AssistantMessage],
    error_message: &str,
) -> Vec<claude::UserMessage> {
    let mut out = Vec::new();
    for am in assistant_messages {
        let blocks = match &am.message.content {
            MessageContent::Blocks(v) => v,
            _ => continue,
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                out.push(new_user_message(
                    MessageContent::Blocks(vec![json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": error_message,
                        "is_error": true,
                    })]),
                    true,
                ));
            }
        }
    }
    out
}

/// 从助手消息抽取 tool_use 块（id 已解析为 JSON input）。
fn extract_tool_use_blocks(msg: &AssistantMessage) -> Vec<ToolUseBlock> {
    let blocks = match &msg.message.content {
        MessageContent::Blocks(v) => v,
        _ => return Vec::new(),
    };
    blocks
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            let id = b.get("id").and_then(Value::as_str)?.to_string();
            let name = b.get("name").and_then(Value::as_str)?.to_string();
            let input = b
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            Some(ToolUseBlock { id, name, input })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 判定助手（TS `isPromptTooLongMessage` / `isWithheldMaxOutputTokens` …）
// ---------------------------------------------------------------------------

/// wire 消息无 `apiError` 字段 → 按文本判定（占位，偏差见文件尾）。
fn is_prompt_too_long(msg: &AssistantMessage) -> bool {
    assistant_text(msg)
        .to_lowercase()
        .contains("prompt is too long")
}

/// 对齐 `apiError === 'max_output_tokens'`：wire 侧等价 stop_reason。
fn is_max_output_tokens(msg: &AssistantMessage) -> bool {
    msg.message.stop_reason.as_deref() == Some("max_tokens")
}

fn assistant_text(msg: &AssistantMessage) -> String {
    let blocks = match &msg.message.content {
        MessageContent::Blocks(v) => v,
        MessageContent::Text(s) => return s.clone(),
    };
    blocks
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                b.get("text").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// 请求组装助手（`utils/api.ts` / `claude.ts` 的本地等价）
// ---------------------------------------------------------------------------

fn build_options(
    current_model: &str,
    ctx: &ToolUseContext,
    query_source: &QuerySource,
    fallback_model: &Option<String>,
    max_output_tokens_override: Option<i64>,
    skip_cache_write: bool,
    task_budget: Option<TaskBudget>,
) -> Options {
    Options {
        model: current_model.to_string(),
        tool_choice: None,
        is_non_interactive_session: ctx.options.is_non_interactive_session,
        extra_tool_schemas: Vec::new(),
        max_output_tokens_override,
        fallback_model: fallback_model.clone(),
        query_source: query_source.clone(),
        has_append_system_prompt: ctx.options.append_system_prompt.is_some(),
        enable_prompt_caching: None,
        skip_cache_write,
        temperature_override: None,
        effort_value: None,
        output_format: None,
        fast_mode: false,
        advisor_model: None,
        task_budget,
        agent_id: ctx.agent_id.as_ref().map(|a| a.0.clone()),
        has_pending_mcp_servers: false,
    }
}

/// `Tools` → `Vec<ToolParam>`（`claude.ts` 的 schema 转换本地等价）。
fn tool_params(tools: &[Arc<dyn Tool>]) -> Vec<ToolParam> {
    tools
        .iter()
        .map(|t| {
            let schema = t.input_schema();
            let mut schema_value = Map::new();
            schema_value.insert(
                "type".to_string(),
                Value::String(schema.schema_type.clone()),
            );
            if let Some(props) = &schema.properties {
                if let Ok(v) = serde_json::to_value(props) {
                    schema_value.insert("properties".to_string(), v);
                }
            }
            for (k, v) in &schema.extra {
                schema_value.insert(k.clone(), v.clone());
            }
            ToolParam {
                typ: "custom".to_string(),
                name: Some(t.name().to_string()),
                model: None,
                description: None,
                input_schema: Some(Value::Object(schema_value)),
                cache_control: None,
                extra: Map::new(),
            }
        })
        .collect()
}

fn system_prompt_blocks(system_prompt: &SystemPrompt) -> Vec<TextBlockParam> {
    system_prompt
        .iter()
        .map(|s| TextBlockParam {
            typ: "text".to_string(),
            text: s.clone(),
            cache_control: None,
        })
        .collect()
}

/// 对齐 `utils/api.ts` `prependUserContext`（未转换 → 本地最小等价）。
fn prepend_user_context(messages: &[Message], user_context: &HashMap<String, String>) -> Vec<Message> {
    if user_context.is_empty() {
        return messages.to_vec();
    }
    let text = user_context
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::with_capacity(messages.len() + 1);
    out.push(Message::User(new_user_message(
        MessageContent::Text(text),
        true,
    )));
    out.extend_from_slice(messages);
    out
}

/// 对齐 `utils/api.ts` `appendSystemContext`（未转换 → 本地最小等价）。
fn append_system_context(
    system_prompt: &SystemPrompt,
    system_context: &HashMap<String, String>,
) -> SystemPrompt {
    if system_context.is_empty() {
        return system_prompt.clone();
    }
    let mut out = system_prompt.clone();
    for (k, v) in system_context {
        out.push(format!("{k}: {v}"));
    }
    out
}

fn permission_mode_from_state(app_state: &Value) -> PermissionMode {
    let mode = app_state
        .get("toolPermissionContext")
        .and_then(|c| c.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("default");
    match mode {
        "plan" => PermissionMode::Plan,
        "acceptEdits" => PermissionMode::AcceptEdits,
        "bypassPermissions" => PermissionMode::BypassPermissions,
        "dontAsk" => PermissionMode::DontAsk,
        "auto" => PermissionMode::Auto,
        "bubble" => PermissionMode::Bubble,
        _ => PermissionMode::Default,
    }
}

/// `tokenCountWithEstimation` 未转换 → crude JSON 长度估算（≈4 字符/token），
/// 仅用于阻塞上限保护。
fn estimate_tokens(messages: &[Message]) -> i64 {
    // Message 未实现 Serialize → 用 Debug 表示的长度做 crude 估算（≈4 字符/token），
    // 仅用于阻塞上限保护。
    let len = format!("{messages:?}").len();
    (len as i64) / 4
}

/// 用量跟踪：把每次响应的 usage 记入 bootstrap 全局统计。
/// 成本暂记 0.0（`calculateUSDCost` 未转换）。
fn record_usage_to_state(usage: &NonNullableUsage, model: &str) {
    add_to_total_cost_state(
        0.0,
        ModelUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            web_search_requests: usage.server_tool_use.web_search_requests,
            cost_usd: 0.0,
            context_window: 0,
            max_output_tokens: 0,
        },
        model,
    );
}

// ---------------------------------------------------------------------------
// 占位清单（未转换依赖 → 当前行为偏差）
// ---------------------------------------------------------------------------
// 1. call_model：`ProductionQueryDeps` 透传 `claude::query_model_with_streaming`
//    的 reqwest 占位错误；HTTP 层就绪前 query 必以 ModelError 终止。
// 2. 自动压缩链（autoCompact / snip / microcompact / compactionV2 /
//    contextCollapse / reactiveCompact / speculation）全部未转换：无阈值压缩、
//    无 prompt-too-long 恢复、无 V2 提升；阻塞上限改用 crude token 估算 +
//    硬上限（无压缩让位，长会话可能过早 BlockingLimit）。
// 3. stop hooks（handleStopHooks）：未转换 → 无 PreToolUse/postTool 拦截，
//    `stop_hook_active` / `StopHookPrevented` / `StopHookBlocking` 恢复路径
//    均不可达。
// 4. 流式工具执行器（StreamingToolExecutor）未转换 → 工具串行执行，
//    无流内并发与 getRemainingResults 合成结果。
// 5. toolUseSummary / attachments / memory+skill 预取 / autonomy 队列 /
//    langfuse / analytics（logEvent）：全部未转换 → 不产出对应事件。
// 6. max_output_tokens 的 64k 升级（ESCALATED_MAX_TOKENS + statsig 门控）
//    未转换 → 仅保留 3 次元消息恢复。
// 7. task_budget.remaining 跨压缩累计无意义（无压缩）；中断消息为文本占位
//    （TS 结构化 subtype 未转换）；`isApiErrorMessage` 经 deps 标记传递；
//    prompt-too-long 判定按文本匹配（无 wire apiError 字段）。
// 8. 签名块剥离（USER_TYPE=ant fallback 路径）未转换。
