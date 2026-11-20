//! `services/compact/sessionMemoryCompact.ts` — 会话记忆压缩（实验）。
//!
//! 语义对齐：
//! - 配置（minTokens / minTextBlockMessages / maxTokens 默认 10k/5/40k）
//!   与 GrowthBook 合并逻辑逐条搬运。
//! - 纯函数（hasTextBlocks / adjustIndexToPreserveAPIInvariants /
//!   calculateMessagesToKeepIndex）完全实现。
//! - `trySessionMemoryCompaction` 的 I/O 依赖（SessionMemory prompts/
//!   utils、sessionStorage、hooks）以占位实现并 TODO 标注。

use std::sync::Mutex;

use crate::services::compact::compact::{annotate_boundary_with_preserved_segment, CompactionResult};
use crate::services::compact::micro_compact::estimate_message_tokens;
use crate::services::compact::prompt::get_compact_user_summary_message;
use crate::services::compact::{create_compact_boundary_message, create_user_message, is_compact_boundary_message, Content, Message};
use crate::utils::env_utils::is_env_truthy;

/// TS `SessionMemoryCompactConfig`。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMemoryCompactConfig {
    /// 压缩后最少保留的 token 数。
    pub min_tokens: usize,
    /// 至少保留的含文本块消息数。
    pub min_text_block_messages: usize,
    /// 压缩后最多保留的 token 数（硬上限）。
    pub max_tokens: usize,
}

/// TS `DEFAULT_SM_COMPACT_CONFIG`。
pub const DEFAULT_SM_COMPACT_CONFIG: SessionMemoryCompactConfig = SessionMemoryCompactConfig {
    min_tokens: 10_000,
    min_text_block_messages: 5,
    max_tokens: 40_000,
};

static SM_COMPACT_CONFIG: Mutex<SessionMemoryCompactConfig> = Mutex::new(DEFAULT_SM_COMPACT_CONFIG);
static CONFIG_INITIALIZED: Mutex<bool> = Mutex::new(false);

/// TS `setSessionMemoryCompactConfig(config)`。
pub fn set_session_memory_compact_config(config: &SessionMemoryCompactConfig) {
    if let Ok(mut current) = SM_COMPACT_CONFIG.lock() {
        *current = config.clone();
    }
}

/// TS `getSessionMemoryCompactConfig()`。
pub fn get_session_memory_compact_config() -> SessionMemoryCompactConfig {
    SM_COMPACT_CONFIG
        .lock()
        .map(|config| config.clone())
        .unwrap_or(DEFAULT_SM_COMPACT_CONFIG)
}

/// TS `resetSessionMemoryCompactConfig()`。
pub fn reset_session_memory_compact_config() {
    if let Ok(mut config) = SM_COMPACT_CONFIG.lock() {
        *config = DEFAULT_SM_COMPACT_CONFIG;
    }
    if let Ok(mut initialized) = CONFIG_INITIALIZED.lock() {
        *initialized = false;
    }
}

/// TS `initSessionMemoryCompactConfig()`：每会话只从 GrowthBook 拉一次；
/// 只有显式正数覆盖默认值。
async fn init_session_memory_compact_config() {
    {
        let Ok(mut initialized) = CONFIG_INITIALIZED.lock() else {
            return;
        };
        if *initialized {
            return;
        }
        *initialized = true;
    }
    let remote = crate::services::compact::get_feature_value_cached_may_be_stale(
        "tengu_sm_compact_config",
    );
    let remote: Option<SessionMemoryCompactConfig> = remote
        .and_then(|value| serde_json::from_value(value).ok());
    let defaults = DEFAULT_SM_COMPACT_CONFIG;
    let merged = match remote {
        Some(remote) => SessionMemoryCompactConfig {
            min_tokens: if remote.min_tokens > 0 {
                remote.min_tokens
            } else {
                defaults.min_tokens
            },
            min_text_block_messages: if remote.min_text_block_messages > 0 {
                remote.min_text_block_messages
            } else {
                defaults.min_text_block_messages
            },
            max_tokens: if remote.max_tokens > 0 {
                remote.max_tokens
            } else {
                defaults.max_tokens
            },
        },
        None => defaults,
    };
    set_session_memory_compact_config(&merged);
}

/// TS `shouldUseSessionMemoryCompaction()`：env 覆盖 > 双 GrowthBook gate。
pub fn should_use_session_memory_compaction() -> bool {
    if is_env_truthy(
        std::env::var("ENABLE_CLAUDE_CODE_SM_COMPACT")
            .ok()
            .as_deref(),
    ) {
        return true;
    }
    if is_env_truthy(
        std::env::var("DISABLE_CLAUDE_CODE_SM_COMPACT")
            .ok()
            .as_deref(),
    ) {
        return false;
    }
    let session_memory_flag = matches!(
        crate::services::compact::get_feature_value_cached_may_be_stale("tengu_session_memory"),
        Some(serde_json::Value::Bool(true))
    );
    let sm_compact_flag = matches!(
        crate::services::compact::get_feature_value_cached_may_be_stale("tengu_sm_compact"),
        Some(serde_json::Value::Bool(true))
    );
    session_memory_flag && sm_compact_flag
}

/// TS `hasTextBlocks(message)`。
pub fn has_text_blocks(message: &Message) -> bool {
    match message {
        Message::Assistant(assistant) => match &assistant.message.content {
            Content::Blocks(blocks) => blocks
                .iter()
                .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text")),
            Content::Text(_) => false,
        },
        Message::User(user) => match &user.message.content {
            Content::Text(text) => !text.is_empty(),
            Content::Blocks(blocks) => blocks
                .iter()
                .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text")),
        },
        _ => false,
    }
}

/// user 消息的 tool_result 块 id 集合。
fn get_tool_result_ids(message: &Message) -> Vec<String> {
    let Message::User(user) = message else {
        return Vec::new();
    };
    let Content::Blocks(blocks) = &user.message.content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| {
            block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
        })
        .filter_map(|block| {
            block
                .get("tool_use_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| id.to_string())
        })
        .collect()
}

/// assistant 消息是否含指定 id 的 tool_use 块。
fn has_tool_use_with_ids(message: &Message, tool_use_ids: &std::collections::HashSet<String>) -> bool {
    let Message::Assistant(assistant) = message else {
        return false;
    };
    let Content::Blocks(blocks) = &assistant.message.content else {
        return false;
    };
    blocks.iter().any(|block| {
        block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
            && block
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(|id| tool_use_ids.contains(id))
                .unwrap_or(false)
    })
}

/// TS `adjustIndexToPreserveAPIInvariants(messages, startIndex)`：
/// 回退起点，避免切开 tool_use/tool_result 配对、以及保留段 assistant
/// 与前置同 message.id 分块（thinking 合并依赖）。
pub fn adjust_index_to_preserve_api_invariants(
    messages: &[Message],
    start_index: usize,
) -> usize {
    if start_index == 0 || start_index >= messages.len() {
        return start_index;
    }

    let mut adjusted_index = start_index;

    // 步骤 1：处理 tool_use/tool_result 配对。
    // 收集保留段（startIndex..）内所有 tool_result id。
    let mut all_tool_result_ids: Vec<String> = Vec::new();
    for message in &messages[start_index..] {
        all_tool_result_ids.extend(get_tool_result_ids(message));
    }

    if !all_tool_result_ids.is_empty() {
        // 收集保留段内已有的 tool_use id。
        let mut tool_use_ids_in_kept_range = std::collections::HashSet::new();
        for message in &messages[adjusted_index..] {
            let Message::Assistant(assistant) = message else {
                continue;
            };
            let Content::Blocks(blocks) = &assistant.message.content else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use") {
                    if let Some(id) = block.get("id").and_then(serde_json::Value::as_str) {
                        tool_use_ids_in_kept_range.insert(id.to_string());
                    }
                }
            }
        }

        // 只找保留段里缺失配对的 tool_use。
        let mut needed_tool_use_ids: std::collections::HashSet<String> = all_tool_result_ids
            .into_iter()
            .filter(|id| !tool_use_ids_in_kept_range.contains(id))
            .collect();

        // 向前找含这些 tool_use 的 assistant 消息。
        for i in (0..adjusted_index).rev() {
            if needed_tool_use_ids.is_empty() {
                break;
            }
            let message = &messages[i];
            if has_tool_use_with_ids(message, &needed_tool_use_ids) {
                adjusted_index = i;
                if let Message::Assistant(assistant) = message {
                    let Content::Blocks(blocks) = &assistant.message.content else {
                        continue;
                    };
                    for block in blocks {
                        if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
                        {
                            if let Some(id) =
                                block.get("id").and_then(serde_json::Value::as_str)
                            {
                                needed_tool_use_ids.remove(id);
                            }
                        }
                    }
                }
            }
        }
    }

    // 步骤 2：处理与保留段 assistant 共享 message.id 的前置分块
    // （normalizeMessagesForAPI 会按 message.id 合并 thinking 块）。
    let mut message_ids_in_kept_range = std::collections::HashSet::new();
    for message in &messages[adjusted_index..] {
        if let Message::Assistant(assistant) = message {
            if let Some(id) = &assistant.message.id {
                message_ids_in_kept_range.insert(id.clone());
            }
        }
    }

    for i in (0..adjusted_index).rev() {
        let message = &messages[i];
        if let Message::Assistant(assistant) = message {
            if let Some(id) = &assistant.message.id {
                if message_ids_in_kept_range.contains(id) {
                    adjusted_index = i;
                }
            }
        }
    }

    adjusted_index
}

/// TS `calculateMessagesToKeepIndex(messages, lastSummarizedIndex)`：
/// 从 lastSummarizedIndex+1 起回扩到满足 minTokens 与
/// minTextBlockMessages，maxTokens 封顶；不切开工具配对；不低于最后一个
/// 压缩边界（边界处的磁盘链有断点，绕过它会漏删保留消息）。
pub fn calculate_messages_to_keep_index(
    messages: &[Message],
    last_summarized_index: isize,
) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let config = get_session_memory_compact_config();

    // 从 lastSummarizedIndex 之后一条开始；未找到（-1）时起点为结尾
    // （不保留任何消息）。
    let mut start_index: usize = if last_summarized_index >= 0 {
        (last_summarized_index as usize + 1).min(messages.len())
    } else {
        messages.len()
    };

    let mut total_tokens = 0usize;
    let mut text_block_message_count = 0usize;
    for message in &messages[start_index..] {
        total_tokens += estimate_message_tokens(std::slice::from_ref(message));
        if has_text_blocks(message) {
            text_block_message_count += 1;
        }
    }

    // 已达 max 上限。
    if total_tokens >= config.max_tokens {
        return adjust_index_to_preserve_api_invariants(messages, start_index);
    }

    // 已同时满足两个下限。
    if total_tokens >= config.min_tokens
        && text_block_message_count >= config.min_text_block_messages
    {
        return adjust_index_to_preserve_api_invariants(messages, start_index);
    }

    // 向前回扩直到满足双下限或撞 max；floor 卡在最后一个压缩边界。
    let floor = messages
        .iter()
        .position(|m| is_compact_boundary_message(m))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let mut i = start_index;
    while i > floor {
        i -= 1;
        let message = &messages[i];
        total_tokens += estimate_message_tokens(std::slice::from_ref(message));
        if has_text_blocks(message) {
            text_block_message_count += 1;
        }
        start_index = i;

        if total_tokens >= config.max_tokens {
            break;
        }
        if total_tokens >= config.min_tokens
            && text_block_message_count >= config.min_text_block_messages
        {
            break;
        }
    }

    adjust_index_to_preserve_api_invariants(messages, start_index)
}

/// 从会话记忆构造 CompactionResult（不调用模型；SM-compact 无压缩 API
/// 调用，postCompactTokenCount 与 truePostCompactTokenCount 同值）。
fn create_compaction_result_from_session_memory(
    messages: &[Message],
    session_memory: &str,
    messages_to_keep: &[Message],
    hook_results: &[Message],
    transcript_path: Option<&str>,
    _agent_id: Option<&str>,
) -> CompactionResult {
    let pre_compact_token_count = estimate_message_tokens(messages) as i64;

    let mut boundary_marker = create_compact_boundary_message(
        "auto",
        pre_compact_token_count,
        messages.last().map(|m| m.uuid()),
        None,
        None,
    );

    let pre_compact_discovered = extract_discovered_tool_names(messages);
    if !pre_compact_discovered.is_empty() {
        let metadata = boundary_marker
            .compact_metadata
            .get_or_insert_with(crate::services::compact::CompactMetadata::default);
        let mut tools: Vec<String> = pre_compact_discovered.into_iter().collect();
        tools.sort();
        metadata.pre_compact_discovered_tools = Some(tools);
    }

    // 截断超长 section，防止会话记忆吃掉整个后压缩 token 预算。
    let (truncated_content, was_truncated) =
        truncate_session_memory_for_compact(session_memory, config_max_tokens());

    let mut summary_content = get_compact_user_summary_message(
        &truncated_content,
        true,
        transcript_path,
        true,
    );

    if was_truncated {
        // TODO(utils/permissions/filesystem.ts): getSessionMemoryPath()
        let memory_path = "<session-memory>";
        summary_content.push_str(&format!(
            "\n\nSome session memory sections were truncated for length. The full session memory \
             can be viewed at: {memory_path}"
        ));
    }

    let summary_messages = vec![create_user_message(&summary_content, false, true, true)];

    // SM-compact 无压缩 API 调用：两个 token 字段收敛为同一估算值。
    let keep_estimate = estimate_message_tokens(
        &summary_messages
            .iter()
            .cloned()
            .map(crate::services::compact::Message::User)
            .collect::<Vec<_>>(),
    ) as i64;

    CompactionResult {
        boundary_marker: annotate_boundary_with_preserved_segment(
            &boundary_marker,
            summary_messages
                .last()
                .map(|m| m.uuid.as_str())
                .unwrap_or(boundary_marker.uuid.as_str()),
            Some(messages_to_keep),
        ),
        summary_messages,
        attachments: Vec::new(),
        hook_results: hook_results.to_vec(),
        messages_to_keep: Some(messages_to_keep.to_vec()),
        pre_compact_token_count: Some(pre_compact_token_count),
        post_compact_token_count: Some(keep_estimate),
        true_post_compact_token_count: Some(keep_estimate),
        compaction_usage: None,
    }
}

/// `SessionMemory/prompts.ts`（未转换）默认上限。
fn config_max_tokens() -> usize {
    get_session_memory_compact_config().max_tokens
}

/// TS `truncateSessionMemoryForCompact(sessionMemory)`（SessionMemory/
/// prompts.ts 未转换）：按 maxTokens/4 字符预算截断超长 section。
fn truncate_session_memory_for_compact(
    session_memory: &str,
    max_tokens: usize,
) -> (String, bool) {
    // TODO(SessionMemory/prompts.ts): 按 section 分割、逐段截断并标记。
    let char_budget = max_tokens.saturating_mul(4);
    if session_memory.len() <= char_budget {
        return (session_memory.to_string(), false);
    }
    let truncated = session_memory[..char_budget].to_string();
    (truncated, true)
}

/// TS `extractDiscoveredToolNames`（utils/searchExtraTools.ts 未转换）。
fn extract_discovered_tool_names(
    messages: &[Message],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_reference") {
                if let Some(name) = block
                    .get("name")
                    .or_else(|| block.get("tool_name"))
                    .and_then(serde_json::Value::as_str)
                {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// TS `trySessionMemoryCompaction(messages, agentId?, autoCompactThreshold?)`。
///
/// 两种场景：
/// 1. `lastSummarizedMessageId` 已设置：只保留该 id 之后的消息；
/// 2. 恢复的会话：id 未设但会话记忆非空 → 保留全部消息、以记忆为摘要。
pub async fn try_session_memory_compaction(
    messages: &[Message],
    _agent_id: Option<&str>,
    auto_compact_threshold: Option<i64>,
) -> Option<CompactionResult> {
    if !should_use_session_memory_compaction() {
        return None;
    }

    init_session_memory_compact_config().await;

    // 等待进行中的会话记忆抽取（带超时）。SessionMemory 未转换 → 无记忆
    // 内容，直接走 legacy compact 兜底。
    let last_summarized_message_id: Option<String> = None; // TODO(SessionMemory)
    let session_memory: Option<String> = None; // TODO(SessionMemory)

    let Some(_session_memory) = session_memory else {
        crate::services::compact::log_event("tengu_sm_compact_no_session_memory", None);
        return None;
    };

    let last_summarized_index: isize = match &last_summarized_message_id {
        Some(id) => {
            let found = messages.iter().position(|message| message.uuid() == id);
            match found {
                Some(index) => index as isize,
                None => {
                    // 摘要 id 在当前消息中不存在（消息被修改）→ 无法确定
                    // 摘要/未摘要边界，回落 legacy compact。
                    crate::services::compact::log_event(
                        "tengu_sm_compact_summarized_id_not_found",
                        None,
                    );
                    return None;
                }
            }
        }
        None => {
            // 恢复会话：记忆有内容但边界未知，起点设为最后一条消息。
            crate::services::compact::log_event("tengu_sm_compact_resumed_session", None);
            messages.len() as isize - 1
        }
    };

    let start_index = calculate_messages_to_keep_index(messages, last_summarized_index);
    // 过滤保留段里的旧压缩边界：REPL 剪枝后再次 yield 旧边界会触发
    // 二次剪枝，丢掉新边界与摘要。
    let messages_to_keep: Vec<Message> = messages[start_index..]
        .iter()
        .filter(|message| !is_compact_boundary_message(message))
        .cloned()
        .collect();

    // SessionStart hooks 恢复 CLAUDE.md 等上下文（未转换）。
    let hook_results: Vec<Message> = Vec::new();

    let compaction_result = create_compaction_result_from_session_memory(
        messages,
        // TODO(SessionMemory): 真实记忆内容
        "",
        &messages_to_keep,
        &hook_results,
        None,
        _agent_id,
    );

    let post_compact_messages =
        crate::services::compact::compact::build_post_compact_messages(&compaction_result);
    let post_compact_token_count = estimate_message_tokens(&post_compact_messages) as i64;

    // 仅在给定阈值时检查（autocompact 路径）。
    if let Some(threshold) = auto_compact_threshold {
        if post_compact_token_count >= threshold {
            crate::services::compact::log_event(
                "tengu_sm_compact_threshold_exceeded",
                None,
            );
            return None;
        }
    }

    Some(CompactionResult {
        post_compact_token_count: Some(post_compact_token_count),
        true_post_compact_token_count: Some(post_compact_token_count),
        ..compaction_result
    })
}
