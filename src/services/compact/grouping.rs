//! `services/compact/grouping.ts` — 按 API 轮次分组消息。
//!
//! 语义对齐：一个 assistant 响应（相同 `message.id`）为一组；边界在新
//! assistant 响应开始时触发。边界只由 `lastAssistantId` 门控——格式错误
//! 的悬挂 tool_use 不卡死门（交由 claude.ts 的 `ensureToolResultPairing`
//! 在 API 时修复），与 TS 注释一致。

use crate::services::compact::Message;

/// TS `groupMessagesByApiRound(messages)`。
pub fn group_messages_by_api_round(messages: &[Message]) -> Vec<Vec<Message>> {
    let mut groups: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();
    // 最近见过的 assistant 的 API 响应 id；流式分块共享同一 id。
    let mut last_assistant_id: Option<String> = None;

    for msg in messages {
        let is_new_assistant_round = msg.typ() == "assistant"
            && msg.assistant_api_id() != last_assistant_id.as_deref()
            && !current.is_empty();
        if is_new_assistant_round {
            groups.push(std::mem::take(&mut current));
            current.push(msg.clone());
        } else {
            current.push(msg.clone());
        }
        if msg.typ() == "assistant" {
            last_assistant_id = msg.assistant_api_id().map(|s| s.to_string());
        }
    }

    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::compact::{AssistantApiMessage, AssistantMessage, Content};

    fn assistant(uuid: &str, api_id: Option<&str>) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            message: AssistantApiMessage {
                id: api_id.map(|s| s.to_string()),
                content: Content::Text("hi".to_string()),
                model: None,
                usage: None,
                stop_reason: None,
            },
            is_api_error_message: None,
            request_id: None,
        })
    }

    #[test]
    fn same_api_id_stays_in_one_group() {
        let messages = vec![
            assistant("a1", Some("resp1")),
            assistant("a2", Some("resp1")),
            assistant("a3", Some("resp2")),
        ];
        let groups = group_messages_by_api_round(&messages);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
    }
}
