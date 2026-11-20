//! `services/compaction2/tokens/messageCount.ts` — 消息级精确 token 计数。
//!
//! V2 阈值与阻断检查优先用 `CCH_TOKENIZER_DIR` 的真实词表。内容按块序
//! 列化（与 `roughTokenCountEstimationForBlock` 同规则：text/
//! tool_result/tool_use/thinking/图片/JSON 兜底），逐片用
//! `estimateTokens` 计数。模型无 tokenizer 时调用方回落到既有的
//! usage 锚定 / cl100k 估算。

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::tokens::{estimate_tokens, resolve_tokenizer};

/// 与 `roughTokenCountEstimationForBlock` 一致的保守图片计费。
const IMAGE_TOKEN_ESTIMATE: usize = 2000;

/// TS `countMessagesWithTokenizer(messages, model)`：无 tokenizer 数据时
/// 返回 None。
pub fn count_messages_with_tokenizer(messages: &[Message], model: &str) -> Option<usize> {
    if resolve_tokenizer(model).is_none() {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| count_message(message, model))
            .sum(),
    )
}

/// TS `countMessagesForThreshold(messages, model, fallback)`：阈值面向的
/// 计数——有精确 tokenizer 用精确，否则用调用方提供的 fallback。
pub fn count_messages_for_threshold(
    messages: &[Message],
    model: &str,
    fallback: usize,
) -> usize {
    count_messages_with_tokenizer(messages, model).unwrap_or(fallback)
}

fn count_message(message: &Message, model: &str) -> usize {
    match message {
        Message::User(user) => count_content(&user.message.content, model),
        Message::Assistant(assistant) => count_content(&assistant.message.content, model),
        Message::Attachment(_) => {
            // 附件归一化为 API user 消息后计费；utils/messages.ts 未转换，
            // 摘要层只认 attachment 原体的文本字段（对齐 normalizeAttachmentForAPI
            // 的退化文本）。
            0
        }
        Message::System(_) => 0,
    }
}

fn count_content(content: &Content, model: &str) -> usize {
    match content {
        Content::Text(text) => count_text(text, model),
        Content::Blocks(blocks) => blocks
            .iter()
            .map(|block| count_block(block, model))
            .sum(),
    }
}

fn count_block(block: &Value, model: &str) -> usize {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| count_text(text, model))
            .unwrap_or(0),
        Some("image") | Some("document") => IMAGE_TOKEN_ESTIMATE,
        Some("tool_result") => block
            .get("content")
            .map(|content| count_content_value(content, model))
            .unwrap_or(0),
        Some("tool_use") => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            count_text(&format!("{name}{}", stringify(&input)), model)
        }
        Some("thinking") => block
            .get("thinking")
            .and_then(Value::as_str)
            .map(|text| count_text(text, model))
            .unwrap_or(0),
        Some("redacted_thinking") => block
            .get("data")
            .and_then(Value::as_str)
            .map(|text| count_text(text, model))
            .unwrap_or(0),
        _ => count_text(&stringify(block), model),
    }
}

fn count_content_value(content: &Value, model: &str) -> usize {
    match content {
        Value::String(text) => count_text(text, model),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| count_block(block, model))
            .sum(),
        _ => 0,
    }
}

/// 与 `jsonStringify`（JSON.stringify + slow-op 记账）同义。
fn stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn count_text(text: &str, model: &str) -> usize {
    estimate_tokens(text, model).tokens
}
