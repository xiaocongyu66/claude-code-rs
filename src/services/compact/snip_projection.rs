//! `services/compact/snipProjection.ts` — snip 边界判定与"已被剪除"视图投影。

use std::collections::HashSet;

use crate::services::compact::{Message, SnipMetadata};

/// TS `isSnipBoundaryMessage(message)`：system + subtype==='snip_boundary'。
pub fn is_snip_boundary_message(message: &Message) -> bool {
    match message {
        Message::System(m) => m.subtype.as_deref() == Some("snip_boundary"),
        _ => false,
    }
}

/// TS `projectSnippedView(messages)`：收集所有 snip 边界的 removedUuids，
/// 过滤掉对应消息（边界消息本身保留）。无移除项时原样返回。
pub fn project_snipped_view(messages: &[Message]) -> Vec<Message> {
    let mut removed_set = HashSet::new();
    for msg in messages {
        if let Message::System(system) = msg {
            if let Some(SnipMetadata {
                removed_uuids: Some(uuids),
            }) = &system.snip_metadata
            {
                for uuid in uuids {
                    removed_set.insert(uuid.clone());
                }
            }
        }
    }
    if removed_set.is_empty() {
        return messages.to_vec();
    }
    messages
        .iter()
        .filter(|msg| !removed_set.contains(msg.uuid()))
        .cloned()
        .collect()
}
