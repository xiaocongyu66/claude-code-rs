//! `services/compact/reactiveCompact.ts` — 响应式压缩（API 错误后兜底）。
//!
//! 语义对齐：`ENABLE_REACTIVE_COMPACT_ONLY` / settings.compact.
//! reactiveOnlyMode 开关；PROMPT_TOO_LONG / 媒体尺寸错误的识别；紧急压缩
//! 重试。`compactConversation` 已在本目录 `compact.rs` 实现。

use crate::services::compact::compact::{compact_conversation, CompactionResult, RecompactionInfo};
use crate::services::compact::{CacheSafeParams, Message};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::log::log_error;
use crate::utils::settings::settings::get_compaction_settings;

/// TS `isReactiveOnlyMode()`：settings 或 env 显式开启响应式-only 压缩。
pub fn is_reactive_only_mode() -> bool {
    if is_env_truthy(std::env::var("ENABLE_REACTIVE_COMPACT_ONLY").ok().as_deref()) {
        return true;
    }
    // settings.json: { "compact": { "reactiveOnlyMode": true } }
    //（reactiveOnlyMode 不是 V2 建模字段，落在 CompactionSettings.extra）
    get_compaction_settings()
        .and_then(|compaction| compaction.extra.get("reactiveOnlyMode"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// TS `reactiveCompactOnPromptTooLong(messages, cacheSafeParams, options)`。
pub async fn reactive_compact_on_prompt_too_long(
    messages: Vec<Message>,
    cache_safe_params: &CacheSafeParams,
    options: &ReactiveCompactOptions,
) -> ReactiveCompactResult {
    let params = cache_safe_params;
    let result = compact_conversation(
        messages,
        params.tool_use_context.as_ref(),
        params,
        /* suppress_follow_up_questions */ true,
        options.custom_instructions.as_deref(),
        /* is_auto_compact */ true,
        Some(RecompactionInfo {
            is_recompaction_in_chain: false,
            turns_since_previous_compact: 0,
            auto_compact_threshold: 0,
            previous_compact_turn_id: None,
            query_source: Some("compact".to_string()),
        }),
        None,
    )
    .await;
    match result {
        Ok(result) => ReactiveCompactResult {
            ok: true,
            reason: None,
            result: Some(result),
        },
        Err(error) => {
            log_error(error);
            ReactiveCompactResult {
                ok: false,
                reason: Some("compact error".to_string()),
                result: None,
            }
        }
    }
}

/// TS `options` 参数（customInstructions / trigger）。
#[derive(Debug, Clone, Default)]
pub struct ReactiveCompactOptions {
    pub custom_instructions: Option<String>,
    pub trigger: Option<String>,
}

/// TS 返回 `{ ok, reason?, result? }`。
#[derive(Debug, Clone)]
pub struct ReactiveCompactResult {
    pub ok: bool,
    pub reason: Option<String>,
    pub result: Option<CompactionResult>,
}

/// TS `isReactiveCompactEnabled()`：`DISABLE_COMPACT` 关闭时为 false。
pub fn is_reactive_compact_enabled() -> bool {
    if is_env_truthy(std::env::var("DISABLE_COMPACT").ok().as_deref()) {
        return false;
    }
    true
}

/// TS `isWithheldPromptTooLong(message)`：assistant API 错误且消息为
/// prompt-too-long。
pub fn is_withheld_prompt_too_long(message: &Message) -> bool {
    let Message::Assistant(assistant) = message else {
        return false;
    };
    if !assistant.is_api_error_message.unwrap_or(false) {
        return false;
    }
    is_prompt_too_long_message(message)
}

/// TS `isWithheldMediaSizeError(message)`。
pub fn is_withheld_media_size_error(message: &Message) -> bool {
    let Message::Assistant(assistant) = message else {
        return false;
    };
    if !assistant.is_api_error_message.unwrap_or(false) {
        return false;
    }
    is_media_size_error_message(message)
}

fn assistant_text(message: &Message) -> Option<&str> {
    match message {
        Message::Assistant(assistant) => match &assistant.message.content {
            crate::services::compact::Content::Text(s) => Some(s.as_str()),
            crate::services::compact::Content::Blocks(_) => None,
        },
        _ => None,
    }
}

/// TS `isPromptTooLongMessage(message)`（api/errors.ts 未转换；本地等价）。
fn is_prompt_too_long_message(message: &Message) -> bool {
    let Some(text) = assistant_text(message) else {
        return false;
    };
    text.contains("prompt is too long") || text.contains("Prompt is too long")
}

/// TS `isMediaSizeErrorMessage(message)`。
fn is_media_size_error_message(message: &Message) -> bool {
    let Some(text) = assistant_text(message) else {
        return false;
    };
    text.contains("media size") || text.contains("image size")
}

/// TS `tryReactiveCompact({ hasAttempted, aborted, messages, cacheSafeParams })`。
pub async fn try_reactive_compact(
    has_attempted: bool,
    aborted: bool,
    messages: Vec<Message>,
    cache_safe_params: &CacheSafeParams,
) -> Option<CompactionResult> {
    if has_attempted || aborted {
        return None;
    }
    match compact_conversation(
        messages,
        cache_safe_params.tool_use_context.as_ref(),
        cache_safe_params,
        true,
        None,
        true,
        Some(RecompactionInfo {
            is_recompaction_in_chain: false,
            turns_since_previous_compact: 0,
            auto_compact_threshold: 0,
            previous_compact_turn_id: None,
            query_source: None,
        }),
        None,
    )
    .await
    {
        Ok(result) => Some(result),
        Err(error) => {
            // 紧急压缩失败：记录并吞掉，让 query 循环继续。
            log_error(error);
            None
        }
    }
}
