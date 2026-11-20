//! `services/compact/autoCompact.ts` — 自动压缩触发与 2.9 断路器。
//!
//! 语义对齐：
//! - 全部阈值常量（buffer、warning/error、max output 预留、断路器窗口）
//!   逐条搬运。
//! - 纯决策函数（getEffectiveContextWindowSize / getAutocompactBufferTokens /
//!   estimateMaxTurnGrowth / detectPrefixOverflow /
//!   computeConsecutiveRapidRefills / evaluateRapidRefillBreaker /
//!   getAutoCompactThreshold / calculateTokenWarningState /
//!   isAutoCompactEnabled / shouldAutoCompact）完全实现。
//! - `autoCompactIfNeeded` 的压缩执行链调用本目录已转换模块。

use serde_json::Value;

use crate::services::compact::compact::{
    compact_conversation, CompactionResult, ERROR_MESSAGE_USER_ABORT, RecompactionInfo,
};
use crate::services::compact::micro_compact::estimate_message_tokens;
use crate::services::compact::post_compact_cleanup::run_post_compact_cleanup;
use crate::services::compact::session_memory_compact::try_session_memory_compaction;
use crate::services::compact::Message;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::errors::has_exactErrorMessage;
use crate::utils::model::model::get_effective_context_tokens;

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 压缩期间为输出预留的 token（基于 p99.99 17,387 的观测值）。
const MAX_OUTPUT_TOKENS_FOR_SUMMARY: i64 = 20_000;

/// `getMaxOutputTokensForModel`（api/claude）的本地转发。
fn get_max_output_tokens_for_model(model: &str) -> i64 {
    crate::services::api::claude::get_max_output_tokens_for_model(model)
}

/// TS `getEffectiveContextWindowSize(model)`：上下文窗口减去输出预留；
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` 可缩窗口。
pub fn get_effective_context_window_size(model: &str) -> i64 {
    let reserved_tokens_for_summary = std::cmp::min(
        get_max_output_tokens_for_model(model),
        MAX_OUTPUT_TOKENS_FOR_SUMMARY,
    );
    let mut context_window = get_effective_context_tokens(model).max(0) as i64;

    if let Ok(auto_compact_window) = std::env::var("CLAUDE_CODE_AUTO_COMPACT_WINDOW") {
        if let Ok(parsed) = auto_compact_window.trim().parse::<i64>() {
            if parsed > 0 {
                context_window = std::cmp::min(context_window, parsed);
            }
        }
    }

    context_window - reserved_tokens_for_summary
}

/// TS `AutoCompactTrackingState`。
#[derive(Debug, Clone, Default)]
pub struct AutoCompactTrackingState {
    pub compacted: bool,
    pub turn_counter: i64,
    /// 每轮唯一 id。
    pub turn_id: String,
    /// 连续自动压缩失败计数（断路器用）。
    pub consecutive_failures: Option<i64>,
    /// 2.9（official 2.1.284）：连续"上一个压缩后 <RAPID_REFILL_TURN_WINDOW
    /// 轮内重新填满"计数；达到 MAX_CONSECUTIVE_RAPID_REFILLS 触发断路。
    pub consecutive_rapid_refills: Option<i64>,
}

/// TS `AUTOCOMPACT_BUFFER_TOKENS = 13_000`。
pub const AUTOCOMPACT_BUFFER_TOKENS: i64 = 13_000;
/// TS `WARNING_THRESHOLD_BUFFER_TOKENS = 20_000`。
pub const WARNING_THRESHOLD_BUFFER_TOKENS: i64 = 20_000;
/// TS `ERROR_THRESHOLD_BUFFER_TOKENS = 20_000`。
pub const ERROR_THRESHOLD_BUFFER_TOKENS: i64 = 20_000;
/// TS `MANUAL_COMPACT_BUFFER_TOKENS = 3_000`。
pub const MANUAL_COMPACT_BUFFER_TOKENS: i64 = 3_000;

/// 单轮工具结果增长的保守估计（典型 5-10K，大读可达 20K+）。
const TOOL_RESULT_GROWTH_ESTIMATE: i64 = 15_000;

/// TS `getAutocompactBufferTokens(model)`：上下文感知的压缩缓冲——窗口
/// 越大单轮产出越多，需要更大余量。
pub fn get_autocompact_buffer_tokens(model: &str) -> i64 {
    let effective_window = get_effective_context_window_size(model);
    if effective_window >= 800_000 {
        50_000
    } else if effective_window >= 400_000 {
        30_000
    } else {
        AUTOCOMPACT_BUFFER_TOKENS
    }
}

/// TS `estimateMaxTurnGrowth(model)`：单轮可能的最大 token 增长（用于
/// API 调用前的预测性检查）。
pub fn estimate_max_turn_growth(model: &str) -> i64 {
    let max_output = std::cmp::min(
        get_max_output_tokens_for_model(model),
        MAX_OUTPUT_TOKENS_FOR_SUMMARY,
    );
    max_output + TOOL_RESULT_GROWTH_ESTIMATE
}

/// 连续失败达到该次数后停止自动压缩（1,279 个会话出现 50+ 连续失败，
/// 浪费 ~250K API 调用/天）。
pub const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: i64 = 3;

// ---------------------------------------------------------------------------
// 2.9 — official 2.1.284 autocompact protections（AUTO_COMPACT_BREAKER
// 门控；flag 关闭时字节等价于 2.9 之前）。常量取自官方 bundle。
// ---------------------------------------------------------------------------

/// 官方 byte 8681495：`var qct=3`（"<3 turns each"）；上一次压缩后在该
/// 窗口内重新填满阈值即计为一次 refill。
pub const RAPID_REFILL_TURN_WINDOW: i64 = 3;

/// 官方 byte 8681595 lFn：`n>=3?"trip":"proceed"`。
pub const MAX_CONSECUTIVE_RAPID_REFILLS: i64 = 3;

/// 官方 byte 8681801 nwr（query 循环 kind="rapid_refill_breaker_tripped"
/// 分支原文透传）。
pub const AUTOCOMPACT_THRASHING_ERROR_MESSAGE: &str =
    "Autocompact is thrashing: the context refilled to the limit within 3 turns of the previous \
     compact, 3 times in a row. A file being read or a tool output is likely too large for the \
     context window. Try reading in smaller chunks, or use /clear to start fresh.";

/// TS `PrefixOverflowCheck`。
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixOverflowCheck {
    /// 估算的固定前缀 token：usage 请求总数 − 消息内容估算 − snipTokensFreed。
    /// 这是压缩无法缩小的部分（system prompt、工具定义、MCP 工具）。
    pub prefix_tokens: i64,
    pub threshold_tokens: i64,
    pub total_input_tokens: i64,
    pub messages_estimate: i64,
    pub snip_tokens_freed: i64,
    pub document_block_count: i64,
    pub image_block_count: i64,
}

/// TS `detectPrefixOverflow(input)`：纯函数。None 表示压缩仍可救场；
/// Some 表示固定前缀单独就已超过 autocompact 阈值。
pub fn detect_prefix_overflow(
    total_input_tokens: i64,
    messages_estimate: i64,
    threshold_tokens: i64,
    snip_tokens_freed: Option<i64>,
    document_block_count: Option<i64>,
    image_block_count: Option<i64>,
) -> Option<PrefixOverflowCheck> {
    let prefix_tokens = std::cmp::max(
        0,
        total_input_tokens - snip_tokens_freed.unwrap_or(0) - messages_estimate,
    );
    if prefix_tokens <= threshold_tokens {
        return None;
    }
    Some(PrefixOverflowCheck {
        prefix_tokens,
        threshold_tokens,
        total_input_tokens,
        messages_estimate,
        snip_tokens_freed: snip_tokens_freed.unwrap_or(0),
        document_block_count: document_block_count.unwrap_or(0),
        image_block_count: image_block_count.unwrap_or(0),
    })
}

/// 官方 Oko 的媒体计数器：source 既非 text 也非 content 的文档块（PDF
/// 等）与图片块，递归进 tool_result 内容数组。仅遥测用。
fn count_overflow_media_blocks(content: &[Value], counts: &mut (i64, i64)) {
    for block in content {
        let typ = block.get("type").and_then(Value::as_str);
        if typ == Some("document") {
            let source_kind = block
                .get("source")
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str);
            if source_kind != Some("text") && source_kind != Some("content") {
                counts.0 += 1;
            }
        } else if typ == Some("image") {
            counts.1 += 1;
        } else if typ == Some("tool_result") {
            if let Some(Value::Array(inner)) = block.get("content") {
                count_overflow_media_blocks(inner, counts);
            }
        }
    }
}

fn gather_prefix_overflow_media_blocks(messages: &[Message]) -> (i64, i64) {
    let mut counts = (0i64, 0i64);
    for message in messages {
        if let Some(crate::services::compact::Content::Blocks(blocks)) = message.content() {
            count_overflow_media_blocks(blocks, &mut counts);
        }
    }
    counts
}

/// TS `getCurrentUsage(messages)`（utils/tokens.ts 未转换）：取最后一条
/// assistant 消息 usage 的 input_tokens。
fn get_current_usage(messages: &[Message]) -> Option<i64> {
    let last_assistant = messages.iter().rev().find(|m| m.typ() == "assistant")?;
    let Message::Assistant(assistant) = last_assistant else {
        return None;
    };
    let usage = assistant.message.usage.as_ref()?;
    let input = usage.get("input_tokens").and_then(Value::as_i64)?;
    Some(input)
}

/// TS `buildPrefixOverflowCheck(messages, model, snipTokensFreed?)`：
/// 从活跃状态组装 detectPrefixOverflow 的输入。无 usage 锚点（官方 Pke）
/// 时返回 None——无服务端基线就无法分离固定前缀。
fn build_prefix_overflow_check(
    messages: &[Message],
    model: &str,
    snip_tokens_freed: Option<i64>,
) -> Option<PrefixOverflowCheck> {
    let usage = get_current_usage(messages)?;
    let (document_block_count, image_block_count) =
        gather_prefix_overflow_media_blocks(messages);
    // messagesEstimate 用 roughTokenCountEstimationForMessages 等价（本地
    // 4 字符/token 的 usage 锚外估算）。
    let messages_estimate = estimate_message_tokens(messages) as i64;
    detect_prefix_overflow(
        usage,
        messages_estimate,
        get_auto_compact_threshold(model),
        snip_tokens_freed,
        Some(document_block_count),
        Some(image_block_count),
    )
}

/// TS `computeConsecutiveRapidRefills(tracking?)`：官方 kNn
/// （byte 8681566）：
/// `tracking?.compacted === true && tracking.turnCounter < 3 ? (tracking?.
///  consecutiveRapidRefills ?? 0) + 1 : 0`。
pub fn compute_consecutive_rapid_refills(
    tracking: Option<&AutoCompactTrackingState>,
) -> i64 {
    match tracking {
        Some(tracking)
            if tracking.compacted && tracking.turn_counter < RAPID_REFILL_TURN_WINDOW =>
        {
            tracking.consecutive_rapid_refills.unwrap_or(0) + 1
        }
        _ => 0,
    }
}

/// TS `evaluateRapidRefillBreaker(tracking?)`：官方 lFn
/// （byte 8681595）：`{ action: n>=3?"trip":"proceed",
/// consecutiveRapidRefills: n }`。与 consecutiveFailures 断路器独立——
/// 它统计的是"成功但立刻重新填满"的压缩。
pub fn evaluate_rapid_refill_breaker(
    tracking: Option<&AutoCompactTrackingState>,
) -> (&'static str, i64) {
    let consecutive_rapid_refills = compute_consecutive_rapid_refills(tracking);
    let action = if consecutive_rapid_refills >= MAX_CONSECUTIVE_RAPID_REFILLS {
        "trip"
    } else {
        "proceed"
    };
    (action, consecutive_rapid_refills)
}

/// TS `getAutoCompactThreshold(model)`：有效窗口 − 缓冲；
/// `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`（0-100）可调低阈值便于测试。
pub fn get_auto_compact_threshold(model: &str) -> i64 {
    let effective_context_window = get_effective_context_window_size(model);

    let autocompact_threshold =
        effective_context_window - get_autocompact_buffer_tokens(model);

    if let Ok(env_percent) = std::env::var("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        if let Ok(parsed) = env_percent.trim().parse::<f64>() {
            if parsed > 0.0 && parsed <= 100.0 {
                let percentage_threshold = (effective_context_window as f64
                    * (parsed / 100.0))
                    .floor() as i64;
                return std::cmp::min(percentage_threshold, autocompact_threshold);
            }
        }
    }

    autocompact_threshold
}

/// TS `calculateTokenWarningState(tokenUsage, model)` 的返回。
#[derive(Debug, Clone, PartialEq)]
pub struct TokenWarningState {
    pub percent_left: i64,
    pub is_above_warning_threshold: bool,
    pub is_above_error_threshold: bool,
    pub is_above_auto_compact_threshold: bool,
    pub is_at_blocking_limit: bool,
}

/// TS `calculateTokenWarningState(tokenUsage, model)`。
pub fn calculate_token_warning_state(token_usage: i64, model: &str) -> TokenWarningState {
    let auto_compact_threshold = get_auto_compact_threshold(model);
    let threshold = if is_auto_compact_enabled() {
        auto_compact_threshold
    } else {
        get_effective_context_window_size(model)
    };

    let percent_left = std::cmp::max(
        0,
        ((((threshold - token_usage) as f64) / threshold as f64) * 100.0).round() as i64,
    );

    let warning_threshold = threshold - WARNING_THRESHOLD_BUFFER_TOKENS;
    let error_threshold = threshold - ERROR_THRESHOLD_BUFFER_TOKENS;

    let is_above_warning_threshold = token_usage >= warning_threshold;
    let is_above_error_threshold = token_usage >= error_threshold;

    let is_above_auto_compact_threshold =
        is_auto_compact_enabled() && token_usage >= auto_compact_threshold;

    let actual_context_window = get_effective_context_window_size(model);
    let default_blocking_limit = actual_context_window - MANUAL_COMPACT_BUFFER_TOKENS;

    // 测试可覆盖。
    let blocking_limit = match std::env::var("CLAUDE_CODE_BLOCKING_LIMIT_OVERRIDE") {
        Ok(raw) => raw.trim().parse::<i64>().ok().filter(|parsed| *parsed > 0),
        Err(_) => None,
    }
    .unwrap_or(default_blocking_limit);

    let is_at_blocking_limit = token_usage >= blocking_limit;

    TokenWarningState {
        percent_left,
        is_above_warning_threshold,
        is_above_error_threshold,
        is_above_auto_compact_threshold,
        is_at_blocking_limit,
    }
}

/// TS `isAutoCompactEnabled()`：`DISABLE_COMPACT` /
/// `DISABLE_AUTO_COMPACT` env 或 settings 开关。
pub fn is_auto_compact_enabled() -> bool {
    if is_env_truthy(std::env::var("DISABLE_COMPACT").ok().as_deref()) {
        return false;
    }
    if is_env_truthy(std::env::var("DISABLE_AUTO_COMPACT").ok().as_deref()) {
        return false;
    }
    // settings 的 autoCompactEnabled（utils/config.ts 未转换；默认 true）。
    // TODO(utils/config.ts): getGlobalConfig().autoCompactEnabled
    true
}

/// TS `shouldAutoCompact(messages, model, querySource?, snipTokensFreed?)`。
pub async fn should_auto_compact(
    messages: &[Message],
    model: &str,
    query_source: Option<&str>,
    snip_tokens_freed: i64,
) -> bool {
    // 递归保护：session_memory 与 compact 是 fork agent，会死锁。
    if query_source == Some("session_memory") || query_source == Some("compact") {
        return false;
    }
    // marble_origami 是 ctx-agent：它的上下文爆掉后 autocompact 触发
    // runPostCompactCleanup → resetContextCollapse 会摧毁主线程的已提交
    // 日志（fork 间共享的模块级状态）。
    if feature("CONTEXT_COLLAPSE") && query_source == Some("marble_origami") {
        return false;
    }

    if !is_auto_compact_enabled() {
        return false;
    }

    // reactive-only 模式：压制主动 autocompact，让 reactive compact 接
    // API 的 prompt-too-long。
    if feature("REACTIVE_COMPACT")
        && matches!(
            crate::services::compact::get_feature_value_cached_may_be_stale(
                "tengu_cobalt_raccoon"
            ),
            Some(Value::Bool(true))
        )
    {
        return false;
    }

    // context-collapse 模式：collapse 是开启时的上下文管理系统（90%
    // 提交 / 95% 阻断）；autocompact 在有效窗口 ~93% 处触发会与 collapse
    // 竞争并通常抢先，湮灭本要保留的细粒度上下文。
    if feature("CONTEXT_COLLAPSE")
        && crate::services::contextCollapse::is_context_collapse_enabled()
    {
        return false;
    }

    let token_count = estimate_message_tokens(messages) as i64 - snip_tokens_freed;
    let threshold = get_auto_compact_threshold(model);
    let effective_window = get_effective_context_window_size(model);

    let _ = (threshold, effective_window);

    let state = calculate_token_warning_state(token_count, model);
    state.is_above_auto_compact_threshold
}

/// 2.9 断路器门控：FEATURE_AUTO_COMPACT_BREAKER env 镜像优先，其次是
/// 编译期 feature()（生产构建中被 DCE）。
fn is_auto_compact_breaker_enabled() -> bool {
    if is_env_truthy(std::env::var("FEATURE_AUTO_COMPACT_BREAKER").ok().as_deref()) {
        return true;
    }
    feature("AUTO_COMPACT_BREAKER")
}

/// TS `autoCompactIfNeeded(...)` 的返回。
#[derive(Debug, Clone, Default)]
pub struct AutoCompactResult {
    pub was_compacted: bool,
    pub compaction_result: Option<CompactionResult>,
    pub consecutive_failures: Option<i64>,
    /// 2.9：成功压缩时回传 refill 计数（调用方重装 tracking）。
    pub consecutive_rapid_refills: Option<i64>,
    /// 2.9：rapid-refill 断路跳闸时携带，调用方产出 thrashing 错误并
    /// 结束循环。
    pub rapid_refill_breaker: Option<RapidRefillBreaker>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RapidRefillBreaker {
    pub consecutive_rapid_refills: i64,
    pub turns_since_previous_compact: i64,
}

/// TS `autoCompactIfNeeded(messages, toolUseContext, cacheSafeParams,
/// querySource?, tracking?, snipTokensFreed?)`。
pub async fn auto_compact_if_needed(
    messages: &[Message],
    tool_use_context: Option<&crate::tool::ToolUseContext>,
    cache_safe_params: &crate::services::compact::CacheSafeParams,
    query_source: Option<&str>,
    tracking: Option<&AutoCompactTrackingState>,
    snip_tokens_freed: Option<i64>,
) -> AutoCompactResult {
    if is_env_truthy(std::env::var("DISABLE_COMPACT").ok().as_deref()) {
        return AutoCompactResult::default();
    }

    // 断路器：连续失败 N 次后停止重试。
    if let Some(consecutive_failures) = tracking.and_then(|t| t.consecutive_failures) {
        if consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES {
            return AutoCompactResult::default();
        }
    }

    let model = tool_use_context
        .map(|ctx| ctx.options.main_loop_model.clone())
        .unwrap_or_else(|| {
            crate::utils::model::model::get_runtime_main_loop_model(
                crate::types::permissions::PermissionMode::Default,
                &crate::utils::model::model::get_default_main_loop_model(),
                false,
            )
        });
    let should_compact = should_auto_compact(
        messages,
        &model,
        query_source,
        snip_tokens_freed.unwrap_or(0),
    )
    .await;

    if !should_compact {
        return AutoCompactResult::default();
    }

    // -- 2.9: official 2.1.284 autocompact protections（flag 门控）。
    // 护卫顺序对齐官方 autocompact 生成器（byte 9533695）：
    // consecutiveFailures 断路器（上面）→ 阈值检查（shouldAutoCompact）
    // → prefix-overflow 预检（遥测 + lFn 落空） → rapid-refill 断路 → 压缩。
    let mut consecutive_rapid_refills: Option<i64> = None;
    if is_auto_compact_breaker_enabled() {
        // (1) prefix-overflow 预检。official 平级注记：2.1.284 把它作为
        // 影子遥测——官方 jvr 计算 Oko、记录 tengu_auto_compact_prefix_
        // overflow（wouldHaveBlocked: true）后**落空继续**；CCH 在断路
        // 评估后额外激活该行为：固定前缀（服务端请求总数 − 消息估算 −
        // snipTokensFreed）单独超过阈值时不再尝试压缩（见 (2b)）。
        let overflow = build_prefix_overflow_check(messages, &model, snip_tokens_freed);
        if overflow.is_some() {
            crate::services::compact::log_event("tengu_auto_compact_prefix_overflow", None);
        }

        // (2a) rapid-refill 断路（官方 lFn；byte 9534953 落闸日志 kind
        // "rapid_refill_breaker_tripped"）。阈值检查通过后无条件评估——
        // 落空早退会吞掉同轮的跳闸。
        let (action, refill) = evaluate_rapid_refill_breaker(tracking);
        consecutive_rapid_refills = Some(refill);
        if action == "trip" {
            return AutoCompactResult {
                was_compacted: false,
                rapid_refill_breaker: Some(RapidRefillBreaker {
                    consecutive_rapid_refills: refill,
                    turns_since_previous_compact: tracking
                        .map(|t| t.turn_counter)
                        .unwrap_or(-1),
                }),
                ..Default::default()
            };
        }

        // (2b) CCH 激活的跳过：固定前缀单独超过阈值——没有压缩能装下，
        // 直接返回不尝试。API 调用照常进行，prompt-too-long 走常规路径。
        if overflow.is_some() {
            return AutoCompactResult {
                consecutive_rapid_refills,
                ..Default::default()
            };
        }
    }

    let recompaction_info = RecompactionInfo {
        is_recompaction_in_chain: tracking.map(|t| t.compacted).unwrap_or(false),
        turns_since_previous_compact: tracking.map(|t| t.turn_counter).unwrap_or(-1),
        previous_compact_turn_id: tracking.map(|t| t.turn_id.clone()),
        auto_compact_threshold: get_auto_compact_threshold(&model),
        query_source: query_source.map(|s| s.to_string()),
    };

    // 实验：优先尝试会话记忆压缩。
    if let Some(session_memory_result) = try_session_memory_compaction(
        messages,
        tool_use_context.and_then(|ctx| ctx.agent_id.as_ref().map(|a| a.0.as_str())),
        Some(recompaction_info.auto_compact_threshold),
    )
    .await
    {
        // 压缩剪枝了消息，旧摘要 id 在 REPL 替换后不再存在。
        // TODO(SessionMemory): setLastSummarizedMessageId(undefined)
        run_post_compact_cleanup(query_source);
        // 重置 cache read 基线，让后压缩下降不被误报为 break
        // （compactConversation 内部做了；SM-compact 没做）。
        if feature("PROMPT_CACHE_BREAK_DETECTION") {
            // TODO(promptCacheBreakDetection): notifyCompaction(querySource ?? 'compact', agentId)
        }
        crate::bootstrap::state::mark_post_compaction();
        return AutoCompactResult {
            was_compacted: true,
            compaction_result: Some(session_memory_result),
            consecutive_rapid_refills,
            ..Default::default()
        };
    }

    let result = compact_conversation(
        messages.to_vec(),
        tool_use_context,
        cache_safe_params,
        /* suppress_follow_up_questions */ true,
        /* custom_instructions */ None,
        /* is_auto_compact */ true,
        Some(recompaction_info),
        None,
    )
    .await;

    match result {
        Ok(compaction_result) => {
            // 旧压缩替换全部消息，旧摘要 id 不再存在。
            // TODO(SessionMemory): setLastSummarizedMessageId(undefined)
            run_post_compact_cleanup(query_source);
            AutoCompactResult {
                was_compacted: true,
                compaction_result: Some(compaction_result),
                consecutive_failures: Some(0), // 成功重置失败计数
                consecutive_rapid_refills,
                ..Default::default()
            }
        }
        Err(error) => {
            if !has_exactErrorMessage(&error, ERROR_MESSAGE_USER_ABORT) {
                crate::utils::log::log_error(error);
            }
            // 累加失败计数；达到阈值时记断路日志（下一轮直接跳过）。
            let prev_failures = tracking.and_then(|t| t.consecutive_failures).unwrap_or(0);
            let next_failures = prev_failures + 1;
            AutoCompactResult {
                was_compacted: false,
                consecutive_failures: Some(next_failures),
                ..Default::default()
            }
        }
    }
}
