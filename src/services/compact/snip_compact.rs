//! `services/compact/snipCompact.ts` — snip 运行时（标记、按需剪除、nudge）。

use std::collections::HashSet;

use serde_json::Value;

use crate::services::compact::{Message, SnipMetadata};

/// 保守的字符/token 比值（混合代码文本）。
const CHARS_PER_TOKEN: usize = 4;

/// 触发 nudge 的最小消息数。
const SNIP_NUDGE_THRESHOLD: usize = 30;

/// TS `SNIP_NUDGE_TEXT`（逐字节搬运）。
pub const SNIP_NUDGE_TEXT: &str = "The conversation history is getting long. \
Consider using the /force-snip command or the snip tool to compress older messages, \
freeing context window space for continued work.";

/// TS `isSnipMarkerMessage(message)`：system + subtype==='snip_marker'。
pub fn is_snip_marker_message(message: &Message) -> bool {
    match message {
        Message::System(m) => m.subtype.as_deref() == Some("snip_marker"),
        _ => false,
    }
}

/// TS `estimateMessageTokens(message)`：序列化 `message.message.content`
/// 后按 ~4 字符/token 估算；system/attachment 无内层消息 → 最低 1 token。
fn estimate_message_tokens(message: &Message) -> usize {
    let mut chars: usize = 0;
    if let Some(content) = message.content() {
        match content {
            crate::services::compact::Content::Text(s) => chars = s.chars().count(),
            crate::services::compact::Content::Blocks(blocks) => {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        chars += text.chars().count();
                    } else if let Some(text) = block.get("content").and_then(Value::as_str) {
                        chars += text.chars().count();
                    } else {
                        chars += serde_json::to_string(block)
                            .map(|s| s.chars().count())
                            .unwrap_or(0);
                    }
                }
            }
        }
    }
    chars.div_ceil(CHARS_PER_TOKEN).max(1)
}

/// TS `snipCompactIfNeeded(messages, options?)` 的返回。
#[derive(Debug, Clone, PartialEq)]
pub struct SnipResult {
    pub messages: Vec<Message>,
    pub executed: bool,
    pub tokens_freed: usize,
    pub boundary_message: Option<Message>,
}

/// TS `snipCompactIfNeeded`：找到最后一条 `snip_boundary`，剪除其
/// `removedUuids` 列出的消息（边界消息本身保留）。无元数据时退化为
/// "保留边界及之后"。
pub fn snip_compact_if_needed(messages: &[Message], _force: bool) -> SnipResult {
    // 找最后一条 snip_boundary
    let mut boundary_idx: Option<usize> = None;
    let mut removed_uuids: Option<Vec<String>> = None;
    for (i, msg) in messages.iter().enumerate().rev() {
        if let Message::System(system) = msg {
            if system.subtype.as_deref() == Some("snip_boundary") {
                boundary_idx = Some(i);
                if let Some(SnipMetadata { removed_uuids: some }) = &system.snip_metadata {
                    removed_uuids = some.clone();
                }
                break;
            }
        }
    }

    let Some(boundary_idx) = boundary_idx else {
        return SnipResult {
            messages: messages.to_vec(),
            executed: false,
            tokens_freed: 0,
            boundary_message: None,
        };
    };

    let boundary_message = messages.get(boundary_idx).cloned();

    match &removed_uuids {
        Some(uuids) if !uuids.is_empty() => {
            let removed_set: HashSet<&str> = uuids.iter().map(|s| s.as_str()).collect();
            let mut kept = Vec::new();
            let mut tokens_freed = 0usize;
            for msg in messages {
                if removed_set.contains(msg.uuid()) {
                    tokens_freed += estimate_message_tokens(msg);
                    continue;
                }
                kept.push(msg.clone());
            }
            SnipResult {
                messages: kept,
                executed: true,
                tokens_freed,
                boundary_message,
            }
        }
        // 无 removedUuids：保留边界及之后所有消息。
        _ => SnipResult {
            messages: messages[boundary_idx..].to_vec(),
            executed: true,
            tokens_freed: 0,
            boundary_message,
        },
    }
}

/// TS `isSnipRuntimeEnabled()`：本模块只在 HISTORY_SNIP 开启时加载，恒 true。
pub fn is_snip_runtime_enabled() -> bool {
    true
}

/// TS `shouldNudgeForSnips(messages)`：消息数 >= 30。
pub fn should_nudge_for_snips(messages: &[Message]) -> bool {
    messages.len() >= SNIP_NUDGE_THRESHOLD
}
