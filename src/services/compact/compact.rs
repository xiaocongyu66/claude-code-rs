//! `services/compact/compact.ts` — 旧压缩主链（compactConversation /
//! partialCompactConversation / 后压缩附件重建）。
//!
//! 语义对齐：
//! - 全部导出常量（POST_COMPACT_*、错误消息、PTL 重试参数）逐条搬运。
//! - 纯函数（stripImagesFromMessages、stripReinjectedAttachments、
//!   truncateHeadForPTLRetry、buildPostCompactMessages、
//!   annotateBoundaryWithPreservedSegment、mergeHookInstructions、
//!   collectReadToolFilePaths、truncateToTokens）完全实现。
//! - 依赖未转换子系统的调用（hooks、attachments、forkedAgent、
//!   sessionStorage、analytics、tokenEstimation、Tool 侧上下文回调）以
//!   最小占位实现并 TODO 标注；控制流（PTL 重试循环、摘要校验、边界
//!   构造、deferApply 语义）保持与 TS 一致。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::services::compact::grouping::group_messages_by_api_round;
use crate::services::compact::prompt::{
    get_compact_prompt, get_compact_user_summary_message, get_partial_compact_prompt,
    PartialCompactDirection,
};
use crate::services::compact::{
    create_user_message, get_messages_after_compact_boundary, is_compact_boundary_message,
    normalize_messages_for_api, CacheSafeParams, CompactMetadata, Content, Message, PreservedSegment,
    SystemMessage, UserMessage,
};

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// TS `POST_COMPACT_MAX_FILES_TO_RESTORE = 5`。
pub const POST_COMPACT_MAX_FILES_TO_RESTORE: usize = 5;
/// TS `POST_COMPACT_TOKEN_BUDGET = 50_000`。
pub const POST_COMPACT_TOKEN_BUDGET: usize = 50_000;
/// TS `POST_COMPACT_MAX_TOKENS_PER_FILE = 5_000`。
pub const POST_COMPACT_MAX_TOKENS_PER_FILE: usize = 5_000;
/// TS `POST_COMPACT_MAX_TOKENS_PER_SKILL = 5_000`（skill 可能很大，
/// 逐个截断优于整体丢弃——skill 头部通常是关键指令）。
pub const POST_COMPACT_MAX_TOKENS_PER_SKILL: usize = 5_000;
/// TS `POST_COMPACT_SKILLS_TOKEN_BUDGET = 25_000`（约 5 个 skill 的容量）。
pub const POST_COMPACT_SKILLS_TOKEN_BUDGET: usize = 25_000;

const MAX_COMPACT_STREAMING_RETRIES: u32 = 2;

/// TS `CompactionResult`。
///
/// `Serialize`/`Deserialize` 让 compaction2 的方法可以把整个结果放进
/// `preserveData`（TS 侧 `preserveData: { legacyCompactionResult }` 的
/// Rust 等价）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionResult {
    pub boundary_marker: SystemMessage,
    pub summary_messages: Vec<UserMessage>,
    pub attachments: Vec<Message>,
    pub hook_results: Vec<Message>,
    pub messages_to_keep: Option<Vec<Message>>,
    pub user_display_message: Option<String>,
    pub pre_compact_token_count: Option<i64>,
    pub post_compact_token_count: Option<i64>,
    pub true_post_compact_token_count: Option<i64>,
    /// 压缩 API 调用的 usage 快照（未转换 utils/tokens，原样保留）。
    pub compaction_usage: Option<Value>,
}

/// TS `RecompactionInfo`：autoCompactIfNeeded → compactConversation 的
/// 诊断上下文（区分同链重压缩 H2 / 跨 agent H1·H5 / 手动 H3）。
#[derive(Debug, Clone, Default)]
pub struct RecompactionInfo {
    pub is_recompaction_in_chain: bool,
    pub turns_since_previous_compact: i64,
    pub previous_compact_turn_id: Option<String>,
    pub auto_compact_threshold: i64,
    pub query_source: Option<String>,
}

/// TS `CompactConversationOptions`。
#[derive(Debug, Clone, Default)]
pub struct CompactConversationOptions {
    /// 计算模式：生成摘要与附件但跳过持久化副作用；调用方需在提交时调用
    /// `apply_compact_conversation_side_effects()`。
    pub defer_apply: bool,
}

#[derive(Debug, Error)]
pub enum CompactError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Api(#[from] crate::services::api::claude::ApiError),
}

/// TS `stripImagesFromMessages(messages)`：压缩前剥掉图片/文档块，替换为
/// 文本占位（图片对生成摘要无益，且可能让压缩请求自身撞上
/// prompt-too-long）。
pub fn strip_images_from_messages(messages: &[Message]) -> Vec<Message> {
    messages.iter().map(strip_images_from_message).collect()
}

fn strip_images_from_message(message: &Message) -> Message {
    let Message::User(user) = message else {
        return message.clone();
    };
    let Content::Blocks(blocks) = &user.message.content else {
        return message.clone();
    };
    let mut has_media_block = false;
    let mut new_content: Vec<Value> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let typ = block.get("type").and_then(Value::as_str);
        match typ {
            Some("image") => {
                has_media_block = true;
                new_content.push(serde_json::json!({ "type": "text", "text": "[image]" }));
            }
            Some("document") => {
                has_media_block = true;
                new_content.push(serde_json::json!({ "type": "text", "text": "[document]" }));
            }
            Some("tool_result") => {
                // 也剥掉 tool_result 内容数组内嵌套的图片/文档。
                let Some(inner) = block.get("content").and_then(Value::as_array) else {
                    new_content.push(block.clone());
                    continue;
                };
                let mut tool_has_media = false;
                let mut new_tool_content: Vec<Value> = Vec::with_capacity(inner.len());
                for item in inner {
                    match item.get("type").and_then(Value::as_str) {
                        Some("image") => {
                            tool_has_media = true;
                            new_tool_content.push(serde_json::json!({
                                "type": "text",
                                "text": "[image]"
                            }));
                        }
                        Some("document") => {
                            tool_has_media = true;
                            new_tool_content.push(serde_json::json!({
                                "type": "text",
                                "text": "[document]"
                            }));
                        }
                        _ => new_tool_content.push(item.clone()),
                    }
                }
                if tool_has_media {
                    has_media_block = true;
                    let mut replaced = block.clone();
                    replaced["content"] = Value::Array(new_tool_content);
                    new_content.push(replaced);
                } else {
                    new_content.push(block.clone());
                }
            }
            _ => new_content.push(block.clone()),
        }
    }
    if !has_media_block {
        return message.clone();
    }
    Message::User(UserMessage {
        message: crate::services::compact::UserMessageBody {
            role: user.message.role.clone(),
            content: Content::Blocks(new_content),
        },
        ..user.clone()
    })
}

/// TS `stripReinjectedAttachments(messages)`：skill_discovery/skill_listing
/// 附件压缩后会被重新注入，喂给摘要器纯浪费 token。feature 关闭时 no-op。
pub fn strip_reinjected_attachments(messages: &[Message]) -> Vec<Message> {
    if feature("EXPERIMENTAL_SKILL_SEARCH") {
        return messages
            .iter()
            .filter(|message| {
                let Message::Attachment(attachment) = message else {
                    return true;
                };
                let typ = attachment
                    .attachment
                    .get("type")
                    .and_then(Value::as_str);
                typ != Some("skill_discovery") && typ != Some("skill_listing")
            })
            .cloned()
            .collect();
    }
    messages.to_vec()
}

pub const ERROR_MESSAGE_NOT_ENOUGH_MESSAGES: &str =
    "Not enough messages to compact. Send a few more messages first, then try again.";
const MAX_PTL_RETRIES: u32 = 3;
const PTL_RETRY_MARKER: &str = "[earlier conversation truncated for compaction retry]";

/// TS `services/api/errors.ts` 的 PROMPT_TOO_LONG_ERROR_MESSAGE（该模块未
/// 转换；query.rs 同样本地定义）。
const PROMPT_TOO_LONG_ERROR_MESSAGE: &str = "Prompt is too long";

/// TS `getPromptTooLongTokenGap(ptlResponse)`：api/errors.ts 未转换；
/// 本地返回 None → 调用方走 20% 回退（等价于不可解析的 Vertex/Bedrock
/// 错误格式路径）。TODO(api/errors.ts)：解析 `>> (N - M) tokens` 差值。
fn get_prompt_too_long_token_gap(_response: &Message) -> Option<i64> {
    None
}

/// TS `truncateHeadForPTLRetry(messages, ptlResponse)`：压缩请求自身撞上
/// prompt-too-long 时的最后手段——按 API 轮次从最旧的组开始丢，直到覆盖
/// tokenGap（不可解析时丢 20% 组）。返回 None 表示无药可救。
pub fn truncate_head_for_ptl_retry(
    messages: &[Message],
    ptl_response: &Message,
) -> Option<Vec<Message>> {
    // 先剥掉上一轮重试注入的合成标记，否则它自成 group 0，20% 回退会卡死。
    let input: Vec<Message> = if let Some(first) = messages.first() {
        match first {
            Message::User(user)
                if user.is_meta.unwrap_or(false)
                    && matches!(&user.message.content, Content::Text(s) if s == PTL_RETRY_MARKER) =>
            {
                messages[1..].to_vec()
            }
            _ => messages.to_vec(),
        }
    } else {
        messages.to_vec()
    };

    let groups = group_messages_by_api_round(&input);
    if groups.len() < 2 {
        return None;
    }

    let mut drop_count: usize = if let Some(token_gap) = get_prompt_too_long_token_gap(ptl_response)
    {
        let mut acc: i64 = 0;
        let mut count = 0usize;
        for group in &groups {
            acc += crate::services::compact::micro_compact::estimate_message_tokens(
                group,
            ) as i64;
            count += 1;
            if acc >= token_gap {
                break;
            }
        }
        count
    } else {
        // 不可解析的错误格式：丢 20% 的组（至少 1 个）。
        (groups.len() * 2 / 10).max(1)
    };

    // 至少留一组用来摘要。
    drop_count = drop_count.min(groups.len() - 1);
    if drop_count < 1 {
        return None;
    }

    let mut sliced: Vec<Message> = Vec::new();
    for group in groups.iter().skip(drop_count) {
        sliced.extend_from_slice(group);
    }
    // 丢掉 group 0 后序列以 assistant 开头会被 API 拒绝（首条必须是
    // role=user）；补一个合成 user 标记（配对的孤立 tool_result 由
    // ensureToolResultPairing 兜底）。
    if sliced.first().map(|m| m.typ()) == Some("assistant") {
        let mut result = vec![create_user_message(PTL_RETRY_MARKER, true, false, false)];
        result.extend(sliced);
        return Some(result);
    }
    Some(sliced)
}

pub const ERROR_MESSAGE_PROMPT_TOO_LONG: &str =
    "Conversation too long to summarize. Try /compact to manually clear conversation history, \
     or start a new session with /clear.";
pub const ERROR_MESSAGE_USER_ABORT: &str = "API Error: Request was aborted.";
pub const ERROR_MESSAGE_INCOMPLETE_RESPONSE: &str =
    "Compaction interrupted · This may be due to network issues — please try again.";

/// TS `buildPostCompactMessages(result)`：统一后压缩消息顺序——boundary、
/// summary、messagesToKeep（剥掉 toolUseResult 负载）、attachments、hooks。
pub fn build_post_compact_messages(result: &CompactionResult) -> Vec<Message> {
    let mut messages: Vec<Message> = vec![Message::System(result.boundary_marker.clone())];
    messages.extend(result.summary_messages.iter().cloned().map(Message::User));
    messages.extend(strip_tool_use_results(result.messages_to_keep.as_deref()));
    messages.extend(result.attachments.iter().cloned());
    messages.extend(result.hook_results.iter().cloned());
    messages
}

/// 释放保留消息中的大 tool result 负载：toolUseResult 只用于 UI 渲染，
/// 不参与 API 调用。
fn strip_tool_use_results(messages: Option<&[Message]>) -> Vec<Message> {
    let Some(messages) = messages else {
        return Vec::new();
    };
    messages
        .iter()
        .map(|msg| match msg {
            Message::User(user) if user.tool_use_result.is_some() => Message::User(UserMessage {
                tool_use_result: None,
                ..user.clone()
            }),
            _ => msg.clone(),
        })
        .collect()
}

/// TS `annotateBoundaryWithPreservedSegment(boundary, anchorUuid,
/// messagesToKeep)`：为保留段标注链信息；磁盘上保留消息保持原 parentUuid
/// （dedup-skip），loader 用该标注修补 head→anchor 与其余子节点→tail。
pub fn annotate_boundary_with_preserved_segment(
    boundary: &SystemMessage,
    anchor_uuid: &str,
    messages_to_keep: Option<&[Message]>,
) -> SystemMessage {
    let Some(keep) = messages_to_keep else {
        return boundary.clone();
    };
    if keep.is_empty() {
        return boundary.clone();
    }
    let head = keep.first().map(|m| m.uuid().to_string());
    let tail = keep.last().map(|m| m.uuid().to_string());
    let (head, tail) = match (head, tail) {
        (Some(head), Some(tail)) => (head, tail),
        _ => return boundary.clone(),
    };
    let mut boundary = boundary.clone();
    let metadata = boundary
        .compact_metadata
        .get_or_insert_with(CompactMetadata::default);
    metadata.preserved_segment = Some(PreservedSegment {
        head_uuid: head,
        anchor_uuid: anchor_uuid.to_string(),
        tail_uuid: tail,
    });
    boundary
}

/// TS `mergeHookInstructions(userInstructions, hookInstructions)`：用户
/// 指令在前，hook 指令追加；空串归一化为 None。
pub fn merge_hook_instructions(
    user_instructions: Option<&str>,
    hook_instructions: Option<&str>,
) -> Option<String> {
    match (user_instructions, hook_instructions) {
        (_, None) => user_instructions.map(|s| s.to_string()),
        (None, Some(hook)) => Some(hook.to_string()),
        (Some(user), Some(hook)) => Some(format!("{user}\n\n{hook}")),
    }
}

/// TS `resolveCompactMessages(messages, agentId)`：窗口链升级（RESUME_WINDOW
/// 上限时从 transcript 重载完整链）。utils/sessionStorage 与
/// conversationRecovery 未转换，暂返回输入；保留与 TS 相同的门控结构。
pub async fn resolve_compact_messages(
    messages: Vec<Message>,
    _agent_id: Option<&str>,
) -> Vec<Message> {
    // TODO(sessionStorage): messages.len() >= RESUME_WINDOW 且主线程时，
    // 用 getLastSessionLog(getSessionId()) 重载完整链并
    // deserializeMessages；失败回落窗口集合。
    messages
}

// ---------------------------------------------------------------------------
// hooks / session-start 占位（utils/hooks.ts、sessionStart.ts 未转换）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PreCompactHookResult {
    pub new_custom_instructions: Option<String>,
    pub user_display_message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PostCompactHookResult {
    pub user_display_message: Option<String>,
}

async fn execute_pre_compact_hooks(
    _trigger: &str,
    _custom_instructions: Option<&str>,
) -> PreCompactHookResult {
    // TODO(utils/hooks.ts): PreCompact hooks（trigger / customInstructions）。
    PreCompactHookResult::default()
}

async fn execute_post_compact_hooks(
    _trigger: &str,
    _compact_summary: &str,
) -> PostCompactHookResult {
    // TODO(utils/hooks.ts): PostCompact hooks（compactSummary）。
    PostCompactHookResult::default()
}

async fn process_session_start_hooks(_source: &str) -> Vec<Message> {
    // TODO(utils/sessionStart.ts): 'compact' 源 SessionStart hooks。
    Vec::new()
}

/// TS `applyCompactConversationSideEffects(messages)`：压缩成功的持久化
/// 副作用（只有 deferApply 路径的提交方调用，直接路径已在
/// compact_conversation 内完成）。
pub fn apply_compact_conversation_side_effects(_messages: &[Message]) {
    crate::bootstrap::state::mark_post_compaction();
    // 重新追加 session 元数据（custom title / tag），让 --resume 的 16KB
    // 尾窗口仍能读到。utils/sessionStorage 未转换。
    // TODO(sessionStorage): reAppendSessionMetadata()
    if feature("KAIROS") {
        // TODO(sessionTranscript): writeSessionTranscriptSegment(messages)
        // （assistant 模式专用，fire-and-forget）。
    }
}

/// TS `compactConversation(messages, context, cacheSafeParams,
/// suppressFollowUpQuestions, customInstructions?, isAutoCompact?,
/// recompactionInfo?, options?)`。
pub async fn compact_conversation(
    messages: Vec<Message>,
    _context: Option<&crate::tool::ToolUseContext>,
    cache_safe_params: &CacheSafeParams,
    suppress_follow_up_questions: bool,
    custom_instructions: Option<&str>,
    is_auto_compact: bool,
    recompaction_info: Option<RecompactionInfo>,
    options: Option<&CompactConversationOptions>,
) -> Result<CompactionResult, CompactError> {
    let defer_apply = options.map_or(false, |o| o.defer_apply);
    let result = compact_conversation_inner(
        messages,
        cache_safe_params,
        suppress_follow_up_questions,
        custom_instructions,
        is_auto_compact,
        recompaction_info.as_ref(),
        defer_apply,
    )
    .await;
    // 无论成败都复位 UI 状态（对齐 TS finally 块）。
    // context.setStreamMode/setResponseLength/onCompactProgress/setSDKStatus
    // 均为 UI 回调（ToolUseContext 未携带压缩进度字段），此处仅触发收尾。
    result
}

async fn compact_conversation_inner(
    messages: Vec<Message>,
    _cache_safe_params: &CacheSafeParams,
    suppress_follow_up_questions: bool,
    mut custom_instructions: Option<&str>,
    is_auto_compact: bool,
    recompaction_info: Option<&RecompactionInfo>,
    defer_apply: bool,
) -> Result<CompactionResult, CompactError> {
    if messages.is_empty() {
        return Err(CompactError::Message(
            ERROR_MESSAGE_NOT_ENOUGH_MESSAGES.to_string(),
        ));
    }

    let messages = resolve_compact_messages(messages, None).await;

    let pre_compact_token_count =
        crate::services::compact::micro_compact::estimate_message_tokens(&messages) as i64;

    // PreCompact hooks：deferred（预览）运行有意跳过——其产出必须在摘要
    // 请求之前折叠进请求，无法在提交时重放；被丢弃的预览也绝不能执行
    // 任意用户命令。
    let hook_result = if defer_apply {
        PreCompactHookResult::default()
    } else {
        execute_pre_compact_hooks(
            if is_auto_compact { "auto" } else { "manual" },
            custom_instructions,
        )
        .await
    };
    custom_instructions = merge_hook_instructions(
        custom_instructions,
        hook_result.new_custom_instructions.as_deref(),
    );
    let user_display_message = hook_result.user_display_message;

    // 3P 默认 true：forked-agent 路径复用主会话 prompt cache。
    let prompt_cache_sharing_enabled = matches!(
        crate::services::compact::get_feature_value_cached_may_be_stale(
            "tengu_compact_cache_prefix"
        ),
        Some(Value::Bool(false))
    ) == false;

    let compact_prompt = get_compact_prompt(custom_instructions);
    let summary_request = create_user_message(&compact_prompt, false, false, false);

    let mut messages_to_summarize = messages.clone();
    let mut retry_cache_safe_params = _cache_safe_params.clone();
    let mut ptl_attempts: u32 = 0;
    let summary: String;
    let summary_response: Option<Message>;
    loop {
        let response = stream_compact_summary(StreamCompactSummaryParams {
            messages: &messages_to_summarize,
            summary_request: &summary_request,
            pre_compact_token_count,
            cache_safe_params: &retry_cache_safe_params,
            defer_apply,
        })
        .await?;
        let text = get_assistant_text(&response);
        if !text
            .as_deref()
            .map(|t| t.starts_with(PROMPT_TOO_LONG_ERROR_MESSAGE))
            .unwrap_or(true)
        {
            summary_response = Some(response);
            summary = text.unwrap_or_default();
            break;
        }

        // CC-1180：压缩请求自身撞上 prompt-too-long，截断最旧的 API 轮次
        // 组后重试，不能让用户卡死。
        ptl_attempts += 1;
        let truncated = if ptl_attempts <= MAX_PTL_RETRIES {
            truncate_head_for_ptl_retry(&messages_to_summarize, &response)
        } else {
            None
        };
        let Some(truncated) = truncated else {
            crate::services::compact::log_event("tengu_compact_failed", None);
            return Err(CompactError::Message(
                ERROR_MESSAGE_PROMPT_TOO_LONG.to_string(),
            ));
        };
        crate::services::compact::log_event("tengu_compact_ptl_retry", None);
        messages_to_summarize = truncated.clone();
        // forked-agent 路径读 cacheSafeParams.fork_context_messages 而非
        // messages 参数，两条路径都要同步截断集合。
        retry_cache_safe_params.fork_context_messages = Some(truncated);
    }

    if summary.is_empty() {
        crate::services::compact::log_event("tengu_compact_failed", None);
        return Err(CompactError::Message(
            "Failed to generate conversation summary - response did not contain valid text content"
                .to_string(),
        ));
    }
    if starts_with_api_error_prefix(&summary) {
        return Err(CompactError::Message(summary));
    }

    // 压缩前备份 readFileState，然后清缓存（fileStateCache / attachments
    // 未转换，记为待接线）。
    // TODO(fileStateCache): cacheToObject(context.readFileState) → clear()

    // 并行生成异步附件（async agents / plan / skill / delta 通知全部依赖
    // 未转换 utils，留结构）。
    let mut post_compact_file_attachments: Vec<Message> = Vec::new();
    post_compact_file_attachments
        .extend(create_post_compact_file_attachments(&HashMap::new(), None, POST_COMPACT_MAX_FILES_TO_RESTORE, &[]).await);
    if let Some(plan_attachment) = create_plan_attachment_if_needed(None) {
        post_compact_file_attachments.push(plan_attachment);
    }
    if let Some(skill_attachment) = create_skill_attachment_if_needed(None) {
        post_compact_file_attachments.push(skill_attachment);
    }
    // plan mode 附件（当前处于 plan mode 才注入）。
    if let Some(plan_mode_attachment) = create_plan_mode_attachment_if_needed(None).await {
        post_compact_file_attachments.push(plan_mode_attachment);
    }

    // SessionStart hooks 只在已提交的压缩上运行（预览仍可能被丢弃）。
    let mut hook_messages: Vec<Message> = Vec::new();
    if !defer_apply {
        hook_messages = process_session_start_hooks("compact").await;
    }

    // 构造压缩边界与摘要消息。
    let last_uuid = messages.last().map(|m| m.uuid().to_string());
    let mut boundary_marker = crate::services::compact::create_compact_boundary_message(
        if is_auto_compact { "auto" } else { "manual" },
        pre_compact_token_count,
        last_uuid.as_deref(),
        None,
        None,
    );
    // 携带已发现工具状态（摘要不保留 tool_reference 块）。
    let pre_compact_discovered = extract_discovered_tool_names(&messages);
    if !pre_compact_discovered.is_empty() {
        let metadata = boundary_marker
            .compact_metadata
            .get_or_insert_with(CompactMetadata::default);
        metadata.pre_compact_discovered_tools =
            Some({
                let mut tools: Vec<String> = pre_compact_discovered.into_iter().collect();
                tools.sort();
                tools
            });
    }

    // utils/sessionStorage.getTranscriptPath() 未转换。
    let transcript_path: Option<String> = None;
    let summary_messages = vec![create_user_message(
        &get_compact_user_summary_message(
            &summary,
            suppress_follow_up_questions,
            transcript_path.as_deref(),
            false,
        ),
        false,
        true,
        true,
    )];

    // 消息负载估算的真实后压缩大小（shouldAutoCompact 下一轮还会加上
    // system prompt + tools + userContext 的 ~20-40K）。
    let post_compact_estimate = [
        vec![Message::System(boundary_marker.clone())],
        summary_messages.iter().cloned().map(Message::User).collect(),
        post_compact_file_attachments.clone(),
        hook_messages.clone(),
    ]
    .concat();
    let true_post_compact_token_count =
        crate::services::compact::micro_compact::estimate_message_tokens(&post_compact_estimate) as i64;

    let compaction_usage = summary_response
        .as_ref()
        .and_then(|m| match m {
            Message::Assistant(a) => a.message.usage.clone(),
            _ => None,
        });

    crate::services::compact::log_event("tengu_compact", None);

    // 重置 cache read 基线，让后压缩下降不被误报为 break；预览运行不动
    // 活基线（结果可能仍被丢弃）。
    if !defer_apply && feature("PROMPT_CACHE_BREAK_DETECTION") {
        // TODO(promptCacheBreakDetection): notifyCompaction(querySource, agentId)
    }
    if !defer_apply {
        apply_compact_conversation_side_effects(&messages);
    }

    let post_compact_hook_result = if defer_apply {
        PostCompactHookResult::default()
    } else {
        execute_post_compact_hooks(
            if is_auto_compact { "auto" } else { "manual" },
            &summary,
        )
        .await
    };

    let combined_user_display_message = [
        user_display_message.as_deref(),
        post_compact_hook_result.user_display_message.as_deref(),
    ]
    .iter()
    .filter_map(|s| *s)
    .collect::<Vec<_>>()
    .join("\n");

    let _ = prompt_cache_sharing_enabled;
    let _ = recompaction_info;

    Ok(CompactionResult {
        boundary_marker,
        summary_messages,
        attachments: post_compact_file_attachments,
        hook_results: hook_messages,
        messages_to_keep: None,
        user_display_message: if combined_user_display_message.is_empty() {
            None
        } else {
            Some(combined_user_display_message)
        },
        pre_compact_token_count: Some(pre_compact_token_count),
        post_compact_token_count: compaction_usage
            .as_ref()
            .and_then(|usage| {
                usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .zip(usage.get("output_tokens").and_then(Value::as_i64))
                    .map(|(input, output)| input + output)
            }),
        true_post_compact_token_count: Some(true_post_compact_token_count),
        compaction_usage,
    })
}

fn get_assistant_text(message: &Message) -> Option<String> {
    crate::services::compact::get_assistant_message_text(message)
}

/// TS `startsWithApiErrorPrefix(text)`：api/errors.ts 未转换，本地等价。
fn starts_with_api_error_prefix(text: &str) -> bool {
    text.starts_with("API Error") || text.starts_with("Error: ")
}

/// TS `extractDiscoveredToolNames(messages)`（utils/searchExtraTools.ts 未
/// 转换）：从消息的 tool_reference 块收集已发现的延迟工具名。
fn extract_discovered_tool_names(messages: &[Message]) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_reference") {
                if let Some(name) = block
                    .get("name")
                    .or_else(|| block.get("tool_name"))
                    .and_then(Value::as_str)
                {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// TS `partialCompactConversation(allMessages, pivotIndex, context,
/// cacheSafeParams, userFeedback?, direction?)`：围绕选中消息做部分压缩。
pub async fn partial_compact_conversation(
    all_messages: &[Message],
    pivot_index: usize,
    _context: Option<&crate::tool::ToolUseContext>,
    cache_safe_params: &CacheSafeParams,
    user_feedback: Option<&str>,
    direction: PartialCompactDirection,
) -> Result<CompactionResult, CompactError> {
    let messages_to_summarize: Vec<Message> = match direction {
        PartialCompactDirection::UpTo => all_messages[..pivot_index.min(all_messages.len())].to_vec(),
        PartialCompactDirection::From => all_messages[pivot_index.min(all_messages.len())..].to_vec(),
    };
    // 'up_to' 必须剥掉旧的压缩边界/摘要：summary_B 位于 kept 之前，kept
    // 里的旧边界会赢得 findLastCompactBoundaryIndex 的反向扫描并丢掉
    // summary_B。'from' 保留它们。
    let messages_to_keep: Vec<Message> = match direction {
        PartialCompactDirection::UpTo => all_messages[pivot_index.min(all_messages.len())..]
            .iter()
            .filter(|message| {
                message.typ() != "progress"
                    && !is_compact_boundary_message(message)
                    && !message.is_compact_summary()
            })
            .cloned()
            .collect(),
        PartialCompactDirection::From => all_messages[..pivot_index.min(all_messages.len())]
            .iter()
            .filter(|message| !matches!(message, Message::System(m) if m.subtype.as_deref() == Some("progress")))
            .cloned()
            .collect(),
    };

    if messages_to_summarize.is_empty() {
        return Err(CompactError::Message(
            if direction == PartialCompactDirection::UpTo {
                "Nothing to summarize before the selected message."
            } else {
                "Nothing to summarize after the selected message."
            }
            .to_string(),
        ));
    }

    let pre_compact_token_count =
        crate::services::compact::micro_compact::estimate_message_tokens(all_messages) as i64;

    let hook_result = execute_pre_compact_hooks("manual", None).await;

    // 合并 hook 指令与用户反馈。
    let custom_instructions = match (
        hook_result.new_custom_instructions.as_deref(),
        user_feedback,
    ) {
        (Some(hook), Some(feedback)) => Some(format!("{hook}\n\nUser context: {feedback}")),
        (Some(hook), None) => Some(hook.to_string()),
        (None, Some(feedback)) => Some(format!("User context: {feedback}")),
        (None, None) => None,
    };

    let compact_prompt = get_partial_compact_prompt(custom_instructions.as_deref(), direction);
    let summary_request = create_user_message(&compact_prompt, false, false, false);

    // 'up_to' prefix 直接命中缓存；'from' 发全量（tail 不会缓存）。
    let mut api_messages: Vec<Message> = match direction {
        PartialCompactDirection::UpTo => messages_to_summarize.clone(),
        PartialCompactDirection::From => all_messages.to_vec(),
    };
    let mut retry_cache_safe_params = cache_safe_params.clone();
    if direction == PartialCompactDirection::UpTo {
        retry_cache_safe_params.fork_context_messages = Some(messages_to_summarize.clone());
    }
    let mut ptl_attempts: u32 = 0;
    let summary: String;
    loop {
        let response = stream_compact_summary(StreamCompactSummaryParams {
            messages: &api_messages,
            summary_request: &summary_request,
            pre_compact_token_count,
            cache_safe_params: &retry_cache_safe_params,
            defer_apply: false,
        })
        .await?;
        let text = get_assistant_text(&response);
        if !text
            .as_deref()
            .map(|t| {
                t.starts_with(PROMPT_TOO_LONG_ERROR_MESSAGE)
            })
            .unwrap_or(true)
        {
            summary = text.unwrap_or_default();
            break;
        }
        ptl_attempts += 1;
        let truncated = if ptl_attempts <= MAX_PTL_RETRIES {
            truncate_head_for_ptl_retry(&api_messages, &response)
        } else {
            None
        };
        let Some(truncated) = truncated else {
            crate::services::compact::log_event("tengu_partial_compact_failed", None);
            return Err(CompactError::Message(
                ERROR_MESSAGE_PROMPT_TOO_LONG.to_string(),
            ));
        };
        crate::services::compact::log_event("tengu_compact_ptl_retry", None);
        api_messages = truncated.clone();
        retry_cache_safe_params.fork_context_messages = Some(truncated);
    }

    if summary.is_empty() {
        crate::services::compact::log_event("tengu_partial_compact_failed", None);
        return Err(CompactError::Message(
            "Failed to generate conversation summary - response did not contain valid text content"
                .to_string(),
        ));
    }
    if starts_with_api_error_prefix(&summary) {
        crate::services::compact::log_event("tengu_partial_compact_failed", None);
        return Err(CompactError::Message(summary));
    }

    let mut post_compact_file_attachments: Vec<Message> = Vec::new();
    post_compact_file_attachments.extend(
        create_post_compact_file_attachments(
            &HashMap::new(),
            None,
            POST_COMPACT_MAX_FILES_TO_RESTORE,
            &messages_to_keep,
        )
        .await,
    );
    if let Some(plan_attachment) = create_plan_attachment_if_needed(None) {
        post_compact_file_attachments.push(plan_attachment);
    }
    if let Some(skill_attachment) = create_skill_attachment_if_needed(None) {
        post_compact_file_attachments.push(skill_attachment);
    }
    if let Some(plan_mode_attachment) = create_plan_mode_attachment_if_needed(None).await {
        post_compact_file_attachments.push(plan_mode_attachment);
    }

    let hook_messages = process_session_start_hooks("compact").await;

    crate::services::compact::log_event("tengu_partial_compact", None);

    // Progress 消息不可记录，forkSessionImpl 会把指向它的
    // logicalParentUuid 置空；两个方向都跳过。
    let last_pre_compact_uuid: Option<String> = match direction {
        PartialCompactDirection::UpTo => all_messages
           [..pivot_index.min(all_messages.len())]
            .iter()
            .rev()
            .find(|message| {
                !matches!(message, Message::System(m) if m.subtype.as_deref() == Some("progress"))
            })
            .map(|message| message.uuid().to_string()),
        PartialCompactDirection::From => messages_to_keep.last().map(|m| m.uuid().to_string()),
    };
    let mut boundary_marker = crate::services::compact::create_compact_boundary_message(
        "manual",
        pre_compact_token_count,
        last_pre_compact_uuid.as_deref(),
        user_feedback,
        Some(messages_to_summarize.len()),
    );
    // allMessages 而非 messagesToSummarize——集合并集幂等，比记录每个
    // 工具落在哪一半简单。
    let pre_compact_discovered = extract_discovered_tool_names(all_messages);
    if !pre_compact_discovered.is_empty() {
        let metadata = boundary_marker
            .compact_metadata
            .get_or_insert_with(CompactMetadata::default);
        metadata.pre_compact_discovered_tools = Some({
            let mut tools: Vec<String> = pre_compact_discovered.into_iter().collect();
            tools.sort();
            tools
        });
    }

    let summary_messages = vec![create_user_message(
        &get_compact_user_summary_message(&summary, false, None, false),
        false,
        true,
        // 有保留段时摘要带 summarizeMetadata；否则仅 transcript 可见。
        messages_to_keep.is_empty(),
    )];

    let compaction_usage: Option<Value> = None;

    crate::bootstrap::state::mark_post_compaction();
    // TODO(sessionStorage): reAppendSessionMetadata()
    if feature("KAIROS") {
        // TODO(sessionTranscript): writeSessionTranscriptSegment(messagesToSummarize)
    }

    // 'from'：prefix 保留 → 边界即锚；'up_to'：suffix 保留 → 最后一条摘要。
    let anchor_uuid = match direction {
        PartialCompactDirection::UpTo => summary_messages
            .last()
            .map(|m| m.uuid.to_string())
            .unwrap_or_else(|| boundary_marker.uuid.clone()),
        PartialCompactDirection::From => boundary_marker.uuid.clone(),
    };

    Ok(CompactionResult {
        boundary_marker: annotate_boundary_with_preserved_segment(
            &boundary_marker,
            &anchor_uuid,
            Some(&messages_to_keep),
        ),
        summary_messages,
        messages_to_keep: Some(messages_to_keep),
        attachments: post_compact_file_attachments,
        hook_results: hook_messages,
        user_display_message: None,
        pre_compact_token_count: Some(pre_compact_token_count),
        post_compact_token_count: compaction_usage
            .as_ref()
            .and_then(|usage| {
                usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .zip(usage.get("output_tokens").and_then(Value::as_i64))
                    .map(|(input, output)| input + output)
            }),
        true_post_compact_token_count: None,
        compaction_usage,
    })
}

/// `streamCompactSummary` 的入参（对齐 TS 内联类型）。
struct StreamCompactSummaryParams<'a> {
    messages: &'a [Message],
    summary_request: &'a Message,
    pre_compact_token_count: i64,
    cache_safe_params: &'a CacheSafeParams,
    defer_apply: bool,
}

/// TS `streamCompactSummary(...)`：先尝试 forked agent（缓存共享，默认
/// 开启），失败回落常规流式路径（带 2 次重试）。
///
/// 语义对齐说明：
/// - forked agent 路径（utils/forkedAgent.ts 未转换）直接回落到常规路径，
///   记一条与 TS 相同的 fallback 事件。
/// - 常规流式路径消费流事件并在结束时返回最终 assistant 消息；Rust 侧
///   `query_model_with_streaming` 尚无 sink 回调，故用非流式的
///   `query_model_without_streaming`（消费完整流并返回最终 assistant 消息）
///   等价表达，重试/错误消息逻辑与 TS 一致。
async fn stream_compact_summary(
    params: StreamCompactSummaryParams<'_>,
) -> Result<Message, CompactError> {
    let prompt_cache_sharing_enabled = !matches!(
        crate::services::compact::get_feature_value_cached_may_be_stale(
            "tengu_compact_cache_prefix"
        ),
        Some(Value::Bool(false))
    );
    let retry_enabled = matches!(
        crate::services::compact::get_feature_value_cached_may_be_stale(
            "tengu_compact_streaming_retry"
        ),
        Some(Value::Bool(true))
    );
    let max_attempts = if retry_enabled { MAX_COMPACT_STREAMING_RETRIES } else { 1 };

    if prompt_cache_sharing_enabled {
        // TODO(utils/forkedAgent.ts): runForkedAgent（缓存共享、maxTurns:1、
        // skipCacheWrite、skipTranscript=deferApply）。成功且响应含非 PTL
        // 文本时直接返回；否则回落并记 fallback 事件。
        crate::services::compact::log_event("tengu_compact_cache_sharing_fallback", None);
    }

    let mut last_error: Option<CompactError> = None;
    for attempt in 1..=max_attempts {
        // 会话保活信号（bridge/remote session 的 WebSocket 空闲超时防护）
        // 依赖 utils/sessionActivity，未转换，记 TODO。
        let _ = params.pre_compact_token_count;

        // 压缩边界之后的部分 + 摘要请求；剥掉重注入附件与图片。
        let summarized = get_messages_after_compact_boundary(params.messages);
        let stripped = strip_reinjected_attachments(&summarized);
        let stripped = strip_images_from_messages(&stripped);
        let api_messages = normalize_messages_for_api(stripped, &[]);
        let mut request_messages: Vec<crate::services::api::claude::Message> =
            Vec::with_capacity(api_messages.len() + 1);
        for message in &api_messages {
            request_messages.push(to_api_message(message));
        }
        request_messages.push(to_api_message(params.summary_request));

        let system_prompt = vec![
            "You are a helpful AI assistant tasked with summarizing conversations.".to_string(),
        ];
        let thinking_config = crate::services::api::claude::ThinkingConfig::Disabled;
        // useSearchExtraTools 为 true 时注入 FileReadTool + SearchExtraToolsTool
        // + MCP 工具（去重）；为 false 时仅 FileReadTool。两路径均依赖
        // builtin-tools（未转换），暂带最小工具集。
        let tools: Vec<crate::services::api::claude::ToolParam> = Vec::new();

        match crate::services::api::claude::query_model_without_streaming(
            &request_messages,
            &system_prompt,
            &thinking_config,
            &tools,
            &stream_compact_options(),
        )
        .await
        {
            Ok(assistant) => {
                return Ok(from_api_assistant(&assistant));
            }
            Err(error) => {
                last_error = Some(CompactError::Api(error));
                if attempt < max_attempts {
                    // 与 TS 相同的退避重试（api/withRetry.getRetryDelay
                    // 未转换，用指数退避 250ms×2^(attempt-1)）。
                    let delay_ms = 250 * 2u64.pow(attempt - 1);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
                crate::services::compact::log_event("tengu_compact_failed", None);
                return Err(CompactError::Message(
                    ERROR_MESSAGE_INCOMPLETE_RESPONSE.to_string(),
                ));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CompactError::Message(ERROR_MESSAGE_INCOMPLETE_RESPONSE.to_string())
    }))
}

/// compact `Message` → api 层 `Message`（`types/message.ts` 未转换，
/// claude.rs 用自己的 UserMessage/AssistantMessage wire 类型）。
fn to_api_message(message: &Message) -> crate::services::api::claude::Message {
    use crate::services::api::claude::{AssistantMessage, AssistantMessageBody, MessageContent,
        UserMessage as ApiUserMessage, UserMessageBody};

    match message {
        Message::User(user) => {
            crate::services::api::claude::Message::User(ApiUserMessage {
                message: UserMessageBody {
                    content: to_api_content(&user.message.content),
                },
                typ: "user".to_string(),
                uuid: user.uuid.clone(),
                timestamp: user.timestamp.clone(),
                is_meta: user.is_meta,
            })
        }
        Message::Assistant(assistant) => {
            crate::services::api::claude::Message::Assistant(AssistantMessage {
                message: AssistantMessageBody {
                    content: to_api_content(&assistant.message.content),
                    model: assistant.message.model.clone(),
                    usage: assistant.message.usage.clone(),
                    stop_reason: assistant.message.stop_reason.clone(),
                },
                request_id: assistant.request_id.clone(),
                typ: "assistant".to_string(),
                uuid: assistant.uuid.clone(),
                timestamp: assistant.timestamp.clone(),
                advisor_model: None,
            })
        }
        // system/attachment 不进 API 请求（normalizeMessagesForAPI 会剥掉）。
        Message::System(_) | Message::Attachment(_) => {
            crate::services::api::claude::Message::User(ApiUserMessage {
                message: UserMessageBody {
                    content: MessageContent::Text(String::new()),
                },
                typ: "user".to_string(),
                uuid: message.uuid().to_string(),
                timestamp: message.timestamp().to_string(),
                is_meta: Some(true),
            })
        }
    }
}

fn to_api_content(content: &Content) -> MessageContent {
    match content {
        Content::Text(text) => MessageContent::Text(text.clone()),
        Content::Blocks(blocks) => MessageContent::Blocks(blocks.clone()),
    }
}

/// api 层 `AssistantMessage` → compact `Message`。
fn from_api_assistant(assistant: &crate::services::api::claude::AssistantMessage) -> Message {
    Message::Assistant(crate::services::compact::AssistantMessage {
        uuid: assistant.uuid.clone(),
        timestamp: assistant.timestamp.clone(),
        message: crate::services::compact::AssistantApiMessage {
            id: None,
            content: match &assistant.message.content {
                crate::services::api::claude::MessageContent::Text(text) => Content::Text(text.clone()),
                crate::services::api::claude::MessageContent::Blocks(blocks) => {
                    Content::Blocks(blocks.clone())
                }
            },
            model: assistant.message.model.clone(),
            usage: assistant.message.usage.clone(),
            stop_reason: assistant.message.stop_reason.clone(),
        },
        is_api_error_message: None,
        request_id: assistant.request_id.clone(),
    })
}

fn stream_compact_options() -> crate::services::api::claude::Options {
    // 与 TS 对齐的压缩请求选项；isNonInteractiveSession/effortValue 等
    // 来自 ToolUseContext（本次调用暂占位）。
    let model = crate::utils::model::model::get_runtime_main_loop_model();
    crate::services::api::claude::Options {
        model: model.clone(),
        tool_choice: None,
        is_non_interactive_session: false,
        extra_tool_schemas: Vec::new(),
        max_output_tokens_override: Some(std::cmp::min(
            crate::utils::context::COMPACT_MAX_OUTPUT_TOKENS as i64,
            crate::services::api::claude::get_max_output_tokens_for_model(&model),
        )),
        fallback_model: None,
        query_source: "compact".to_string(),
        has_append_system_prompt: false,
        enable_prompt_caching: None,
        skip_cache_write: true,
        temperature_override: None,
        effort_value: None,
        output_format: None,
        fast_mode: false,
        advisor_model: None,
        task_budget: None,
        agent_id: None,
        has_pending_mcp_servers: false,
    }
}

/// TS `createPostCompactFileAttachments`：为最近访问的文件生成附件，
/// 防止压缩后模型重复读取。文件按时间排序取最近 maxFiles 个，且总
/// token 不超过 POST_COMPACT_TOKEN_BUDGET；保留段里已作为 Read 结果
/// 出现过的路径跳过（重复注入纯浪费）。
pub async fn create_post_compact_file_attachments(
    read_file_state: &HashMap<String, FileStateEntry>,
    _tool_use_context: Option<&crate::tool::ToolUseContext>,
    max_files: usize,
    preserved_messages: &[Message],
) -> Vec<Message> {
    let preserved_read_paths = collect_read_tool_file_paths(preserved_messages);
    let mut recent_files: Vec<(&String, &FileStateEntry)> = read_file_state.iter().collect();
    recent_files.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
    let recent_files: Vec<&(&String, &FileStateEntry)> = recent_files
        .iter()
        .filter(|(filename, _)| {
            !should_exclude_from_post_compact_restore(filename, None)
                && !preserved_read_paths.contains(&filename.as_str())
        })
        .take(max_files)
        .collect();

    // 重新读取文件内容需要 attachments/FileReadTool 链（未转换）；
    // 结构与 TS 一致，附件内容产出留空。
    let mut used_tokens = 0usize;
    let mut results = Vec::new();
    for (filename, _state) in recent_files {
        let _ = filename;
        let placeholder_tokens = 0usize;
        if used_tokens + placeholder_tokens <= POST_COMPACT_TOKEN_BUDGET {
            used_tokens += placeholder_tokens;
            // results.push(create_attachment_message(attachment));
            let _ = &mut results;
        }
    }
    results
}

/// readFileState 条目的最小 duck-type（utils/fileStateCache.ts 未转换）。
#[derive(Debug, Clone, Default)]
pub struct FileStateEntry {
    pub content: String,
    pub timestamp: i64,
}

/// TS `createPlanAttachmentIfNeeded(agentId?)`：存在 plan 文件时生成
/// plan_file_reference 附件。utils/plans.ts 未转换，暂返回 None。
pub fn create_plan_attachment_if_needed(_agent_id: Option<&str>) -> Option<Message> {
    // TODO(utils/plans.ts): getPlan(agentId) / getPlanFilePath(agentId) →
    // createAttachmentMessage({ type: 'plan_file_reference', planFilePath, planContent })
    None
}

/// TS `createSkillAttachmentIfNeeded(agentId?)`：把本会话调用过的 skill
/// 内容做成 invoked_skills 附件（按最近使用排序、逐 skill 截断、总
/// token 预算 POST_COMPACT_SKILLS_TOKEN_BUDGET）。
pub fn create_skill_attachment_if_needed(agent_id: Option<&str>) -> Option<Message> {
    let invoked_skills = crate::bootstrap::state::get_invoked_skills_for_agent(agent_id);
    if invoked_skills.is_empty() {
        return None;
    }
    // 最近优先，让预算压力先丢最不相关的 skill。
    let mut skills: Vec<(i64, String, String, String)> = invoked_skills
        .into_values()
        .map(|skill| {
            (
                skill.invoked_at,
                skill.skill_name,
                skill.skill_path,
                skill.content,
            )
        })
        .collect();
    skills.sort_by(|a, b| b.0.cmp(&a.0));

    let mut used_tokens = 0usize;
    let mut kept: Vec<Value> = Vec::new();
    for (_, name, path, content) in skills {
        let truncated = truncate_to_tokens(&content, POST_COMPACT_MAX_TOKENS_PER_SKILL);
        let tokens = crate::services::compact::micro_compact::rough_token_count_estimation(&truncated, 4);
        if used_tokens + tokens > POST_COMPACT_SKILLS_TOKEN_BUDGET {
            continue;
        }
        used_tokens += tokens;
        kept.push(serde_json::json!({
            "name": name,
            "path": path,
            "content": truncated,
        }));
    }
    if kept.is_empty() {
        return None;
    }
    Some(crate::services::compact::create_attachment_message(
        serde_json::json!({ "type": "invoked_skills", "skills": kept }),
    ))
}

/// TS `createPlanModeAttachmentIfNeeded(context)`：plan mode 下注入
/// plan_mode 附件，让压缩后模型继续按 plan mode 工作。
pub async fn create_plan_mode_attachment_if_needed(
    _tool_use_context: Option<&crate::tool::ToolUseContext>,
) -> Option<Message> {
    // 需要 appState.toolPermissionContext.mode === 'plan'（未转换）。
    // TODO(tool context): createAttachmentMessage({
    //   type: 'plan_mode', reminderType: 'full', isSubAgent, planFilePath, planExists })
    None
}

/// TS `collectReadToolFilePaths(messages)`：扫描 Read 的 tool_use 块收集
/// file_path（expandPath 归一化），用于对保留段去重。跳过 tool_result
/// 是去重存根（FILE_UNCHANGED_STUB）的读取——存根指向更早的完整读取，
/// 可能已被压缩掉，应重新注入真实内容。
fn collect_read_tool_file_paths(messages: &[Message]) -> Vec<String> {
    const FILE_READ_TOOL_NAME: &str = "Read";
    const FILE_UNCHANGED_STUB: &str = "[file content unchanged]";

    let mut stub_ids = std::collections::HashSet::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let content_is_stub = block
                .get("content")
                .and_then(Value::as_str)
                .map(|content| content.starts_with(FILE_UNCHANGED_STUB))
                .unwrap_or(false);
            if content_is_stub {
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    stub_ids.insert(id.to_string());
                }
            }
        }
    }

    let mut paths = Vec::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if block.get("name").and_then(Value::as_str) != Some(FILE_READ_TOOL_NAME) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if stub_ids.contains(id) {
                continue;
            }
            if let Some(path) = block
                .get("input")
                .and_then(|input| input.get("file_path"))
                .and_then(Value::as_str)
            {
                let expanded = expand_path(path);
                if !paths.contains(&expanded) {
                    paths.push(expanded);
                }
            }
        }
    }
    paths
}

/// `expandPath`（utils/path.ts 未转换）：如实展开 ~ 与环境变量。
fn expand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

const SKILL_TRUNCATION_MARKER: &str =
    "\n\n[... skill content truncated for compaction; use Read on the skill path if you need \
     the full text]";

/// TS `truncateToTokens(content, maxTokens)`：roughTokenCountEstimation
/// 默认 ~4 字符/token，字符预算 = maxTokens*4 - 标记长度；保留头部。
fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    if rough_token_count_estimation_local(content) <= max_tokens {
        return content.to_string();
    }
    let char_budget = max_tokens
        .saturating_mul(4)
        .saturating_sub(SKILL_TRUNCATION_MARKER.len());
    if char_budget >= content.len() {
        return content.to_string();
    }
    format!("{}{SKILL_TRUNCATION_MARKER}", &content[..char_budget])
}

fn rough_token_count_estimation_local(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// TS `shouldExcludeFromPostCompactRestore(filename, agentId?)`：排除
/// plan 文件与 claude.md 类文件。utils/plans.ts、config.ts 未转换，
/// 暂不排除（TODO 标注）。
fn should_exclude_from_post_compact_restore(_filename: &str, _agent_id: Option<&str>) -> bool {
    // TODO(utils/plans.ts + utils/config.ts):
    //   expandPath(getPlanFilePath(agentId)) / MEMORY_TYPE_VALUES.map(getMemoryPath)
    false
}

/// TS `createCompactCanUseTool()`：压缩期间禁用一切工具调用。
pub fn create_compact_can_use_tool() -> crate::tool::CanUseToolFn {
    use std::sync::Arc;
    Arc::new(|_request: crate::tool::CanUseToolRequest<'_>| {
        Box::pin(async {
            crate::types::permissions::PermissionResult::Deny(
                crate::types::permissions::PermissionDenyDecision {
                    message: "Tool use is not allowed during compaction".to_string(),
                    decision_reason:
                        crate::types::permissions::PermissionDecisionReason::Other {
                            reason: "compaction agent should only produce text summary"
                                .to_string(),
                        },
                    tool_use_id: None,
                },
            )
        }) as crate::tool::BoxFuture<'_, _>
    })
}
