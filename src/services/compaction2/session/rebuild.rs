//! `services/compaction2/session/rebuild.ts` — resume 重建。
//!
//! 给定日志会话条目（compaction 行 + transcript 消息），在最近一次压缩处
//! 重建模型上下文：
//!
//!   compact 边界 → 摘要 → 恢复的 archive/附件/hook 结果 →
//!   firstKeptEntryId 之后的消息
//!
//! 没有可用压缩行（缺失、损坏、或其切点不再可解析）时返回 `None`。
//! 调用方保持 legacy transcript 链——它本来就从 legacy
//! `compact_boundary` 消息开始。

use crate::services::compact::{
    create_user_message, now_ts, uuid_v4, Message, UserMessage,
};
use crate::services::compaction2::contracts::CompactionMethodName;
use crate::services::compaction2::session::display::{
    create_compaction_boundary_message, CompactionBoundaryTrigger,
};
use crate::services::compaction2::session::entries::{
    parse_compaction_session_entry, sanitize_attachment_meta_pair, CompactionEntryAttachmentMeta,
    CompactionSessionEntry,
};

/// TS `RebuildEntry = Message | CompactionSessionEntry`。
#[derive(Debug, Clone)]
pub enum RebuildEntry {
    Message(Message),
    Compaction(CompactionSessionEntry),
}

impl RebuildEntry {
    fn typ(&self) -> &'static str {
        match self {
            RebuildEntry::Message(message) => message.typ(),
            RebuildEntry::Compaction(_) => "compaction",
        }
    }
}

/// TS `findLatestCompaction(entries)`。
///
/// 最新压缩行胜出。最新行损坏时返回 `None`，而不是悄悄复活更旧的压缩
/// （那会丢掉更新的轮次）。
fn find_latest_compaction(entries: &[RebuildEntry]) -> Option<(CompactionSessionEntry, usize)> {
    for (index, candidate) in entries.iter().enumerate().rev() {
        if candidate.typ() != "compaction" {
            continue;
        }
        let RebuildEntry::Compaction(entry) = candidate else {
            return None;
        };
        return parse_compaction_session_entry(&serde_json::to_value(entry).ok()?)
            .map(|entry| (entry, index));
    }
    None
}

/// legacy 压缩前导（边界 + 摘要）可能位于日志行与保留尾部之间——当调用方
/// 把日志行与一条已被边界截断的链拼在一起时。跳过它，空切点才不会在
/// 重建时重复标记。
fn is_legacy_compaction_preamble(entry: &RebuildEntry) -> bool {
    match entry {
        RebuildEntry::Message(Message::System(system)) => {
            system.subtype.as_deref() == Some("compact_boundary")
        }
        RebuildEntry::Message(Message::User(user)) => user.is_compact_summary.unwrap_or(false),
        _ => false,
    }
}

/// TS `findFirstKeptIndex(entries, firstKeptEntryId, compactionIndex)`。
///
/// `''` 表示"压缩行之前没有保留任何消息"，尾部从行后第一个非前导条目
/// 开始。非空 id 必须仍然解析到一条 transcript 消息；否则该行陈旧/损坏
/// → `None`。
fn find_first_kept_index(
    entries: &[RebuildEntry],
    first_kept_entry_id: &str,
    compaction_index: usize,
) -> Option<usize> {
    if first_kept_entry_id.is_empty() {
        let mut index = compaction_index + 1;
        while index < entries.len() && is_legacy_compaction_preamble(&entries[index]) {
            index += 1;
        }
        return Some(index);
    }
    for (index, entry) in entries.iter().enumerate() {
        if let RebuildEntry::Message(message) = entry {
            if message.uuid() == first_kept_entry_id {
                return Some(index);
            }
        }
    }
    None
}

fn content_blocks(message: &Message) -> Vec<&serde_json::Map<String, serde_json::Value>> {
    let Some(blocks) = message.content() else {
        return Vec::new();
    };
    let crate::services::compact::Content::Blocks(blocks) = blocks else {
        return Vec::new();
    };
    blocks.iter().filter_map(|block| block.as_object()).collect()
}

/// TS `alignToolPairing(messages)`。
///
/// 落在 tool_result 上的切点会孤立其配对的 tool_use（它在边界之前）。
/// 丢掉这些开头的 user 消息，重建的上下文永不以悬空 tool_result 开头。
/// 反向情形——assistant tool_use 的 result 在尾部更后面——已经配对，
/// 保持不动。
fn align_tool_pairing(messages: Vec<Message>) -> Vec<Message> {
    use std::collections::HashSet;

    let mut kept_tool_use_ids: HashSet<String> = HashSet::new();
    for message in &messages {
        for block in content_blocks(message) {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use") {
                if let Some(id) = block.get("id").and_then(serde_json::Value::as_str) {
                    kept_tool_use_ids.insert(id.to_string());
                }
            }
        }
    }

    let mut start = 0usize;
    while start < messages.len() {
        let message = &messages[start];
        let orphaned = match message {
            Message::User(_) => content_blocks(message).iter().any(|block| {
                block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                    && block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .map(|id| !kept_tool_use_ids.contains(id))
                        .unwrap_or(false)
            }),
            _ => false,
        };
        if !orphaned {
            break;
        }
        start += 1;
    }
    if start == 0 {
        messages
    } else {
        messages[start..].to_vec()
    }
}

/// TS `restoredAttachmentBody(meta)`。
///
/// 只重建 API normalizer 能消费的形状；其余降级为文本提醒——损坏的日志行
/// 不能让消息 normalization 崩溃。
fn restored_attachment_body(meta: &CompactionEntryAttachmentMeta) -> Option<serde_json::Value> {
    let text = meta.text.clone().unwrap_or_default();
    let hook_name = meta.name.clone().unwrap_or_else(|| "SessionStart".to_string());
    let hook_event = "SessionStart";
    let tool_use_id = meta
        .tool_use_id
        .as_ref()
        .map(|id| serde_json::json!({ "toolUseID": id }))
        .unwrap_or_else(|| serde_json::json!({}));
    let body = match meta.typ.as_str() {
        "file" | "image" | "pdf" | "notebook" | "pdf_reference" | "compact_file_reference" => {
            // 文件字节刻意不入日志；指向路径（与 legacy
            // `compact_file_reference` 回落输出的文本相同）。
            let path = meta.path.as_ref().filter(|path| !path.is_empty())?;
            serde_json::json!({
                "type": "compact_file_reference",
                "filename": path,
            })
        }
        "plan_file_reference" => serde_json::json!({
            "type": "plan_file_reference",
            "planFilePath": meta.path.clone().unwrap_or_default(),
            "planContent": text,
        }),
        "invoked_skills" => serde_json::json!({
            "type": "invoked_skills",
            "skills": [{
                "name": meta.name.clone().unwrap_or_else(|| "restored-skill".to_string()),
                "path": meta.path.clone().unwrap_or_default(),
                "content": text,
            }],
        }),
        "hook_additional_context" => {
            let content = if text.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::Value::String(text.clone())]
            };
            let mut body = serde_json::json!({
                "type": "hook_additional_context",
                "content": content,
                "hookName": hook_name,
                "hookEvent": hook_event,
            });
            if let Some(map) = body.as_object_mut() {
                if let Some(id) = tool_use_id.as_object() {
                    map.extend(id.iter().map(|(key, value)| (key.clone(), value.clone())));
                }
            }
            body
        }
        "hook_success" => {
            let mut body = serde_json::json!({
                "type": "hook_success",
                "content": text,
                "hookName": hook_name,
                "hookEvent": hook_event,
            });
            if let Some(map) = body.as_object_mut() {
                if let Some(id) = tool_use_id.as_object() {
                    map.extend(id.iter().map(|(key, value)| (key.clone(), value.clone())));
                }
            }
            body
        }
        "hook_stopped_continuation" => {
            let mut body = serde_json::json!({
                "type": "hook_stopped_continuation",
                "message": text,
                "hookName": hook_name,
                "hookEvent": hook_event,
            });
            if let Some(map) = body.as_object_mut() {
                if let Some(id) = tool_use_id.as_object() {
                    map.extend(id.iter().map(|(key, value)| (key.clone(), value.clone())));
                }
            }
            body
        }
        "critical_system_reminder" => serde_json::json!({
            "type": "critical_system_reminder",
            "content": text,
        }),
        _ => serde_json::json!({
            "type": "critical_system_reminder",
            "content": restore_notice(meta),
        }),
    };
    Some(body)
}

fn restore_notice(meta: &CompactionEntryAttachmentMeta) -> String {
    let header = match meta.path.as_ref() {
        Some(path) if !path.is_empty() => format!("[restored {}: {path}]", meta.typ),
        _ => format!("[restored {}]", meta.typ),
    };
    match meta.text.as_ref() {
        Some(text) if !text.is_empty() => format!("{header}\n{text}"),
        _ => header,
    }
}

/// TS `restoreAttachment(meta, timestamp)`。
fn restore_attachment(meta: &CompactionEntryAttachmentMeta, timestamp: &str) -> Option<Message> {
    let attachment = restored_attachment_body(meta)?;
    Some(Message::Attachment(crate::services::compact::AttachmentMessage {
        uuid: uuid_v4(),
        timestamp: timestamp.to_string(),
        attachment,
    }))
}

/// TS `snapcompactArchiveText(preserveData)`。
///
/// snapcompact 归档在 `frames` 被剔除出日志后仍保留有界源文本，resume
/// 可以用文本模式重注入归档历史（图片归档回落到同一段文本）。
fn snapcompact_archive_text(
    preserve_data: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let archive = preserve_data?.get("snapcompact")?;
    let record = archive.as_object()?;
    if let Some(text) = record.get("text").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let head = record
        .get("textHead")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let tail = record
        .get("textTail")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let text = format!("{head}{tail}");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// TS `rebuildMessagesFromEntries(entries)`。
///
/// `entries` 可以是任意顺序，只要最新压缩行存在、且其 `firstKeptEntryId`
/// 解析到同一集合中的一条消息。
pub fn rebuild_messages_from_entries(entries: &[RebuildEntry]) -> Option<Vec<Message>> {
    let (latest_entry, compaction_index) = find_latest_compaction(entries)?;

    let start_index =
        find_first_kept_index(entries, &latest_entry.first_kept_entry_id, compaction_index)?;
    let kept: Vec<Message> = align_tool_pairing(
        entries[start_index..]
            .iter()
            .filter_map(|entry| match entry {
                RebuildEntry::Message(message) => Some(message.clone()),
                _ => None,
            })
            .collect(),
    );

    let last_summarized = entries[..start_index].iter().rev().find_map(|entry| {
        match entry {
            RebuildEntry::Message(message) => Some(message),
            _ => None,
        }
    });

    // 经 display 构造器建立（只有 crypto，不拉 transcript 链），resume 渲染
    // 与活动轮同一标记——包括只存在于日志行里的方法。
    let timestamp = now_ts();
    let mut boundary =
        create_compaction_boundary_message(&crate::services::compaction2::session::display::CompactionBoundaryInput {
            method: parse_method(&latest_entry.method)?,
            trigger: CompactionBoundaryTrigger::Auto,
            pre_tokens: latest_entry.tokens_before,
            tokens_saved: None,
            warning: None,
            uuid: None,
            timestamp: Some(timestamp.clone()),
        });
    if let Some(last_summarized) = last_summarized {
        boundary.logical_parent_uuid = Some(last_summarized.uuid().to_string());
    }
    let summary: UserMessage = create_user_message(
        &latest_entry.summary,
        false,
        true,
        true,
    );

    // 恢复顺序对齐 legacy 压缩后布局：摘要、归档 snapcompact 文本、附件、
    // hook 结果，然后保留尾部。
    let mut restored: Vec<Message> = Vec::new();
    if let Some(archive_text) = snapcompact_archive_text(latest_entry.preserve_data.as_ref()) {
        let mut archive_message =
            create_user_message(&format!("<snapcompact-archive>\n{archive_text}\n</snapcompact-archive>"), true, false, false);
        archive_message.uuid = uuid_v4();
        archive_message.timestamp = timestamp.clone();
        restored.push(Message::User(archive_message));
    }
    let meta = sanitize_attachment_meta_pair(
        latest_entry
            .attachments
            .as_ref()
            .and_then(|attachments| serde_json::to_value(attachments).ok())
            .as_ref(),
        latest_entry
            .hook_results
            .as_ref()
            .and_then(|hook_results| serde_json::to_value(hook_results).ok())
            .as_ref(),
    );
    for attachment in meta.attachments.unwrap_or_default() {
        if let Some(message) = restore_attachment(&attachment, &timestamp) {
            restored.push(message);
        }
    }
    for hook in meta.hook_results.unwrap_or_default() {
        if let Some(message) = restore_attachment(&hook, &timestamp) {
            restored.push(message);
        }
    }

    // 恢复的消息是 `attachment`/meta user 行，不含 tool_result 块；且
    // `alignToolPairing` 已在保留尾部上跑过，插入它们不会孤立
    // tool_use/tool_result 对。
    let mut rebuilt = Vec::with_capacity(2 + restored.len() + kept.len());
    rebuilt.push(Message::System(boundary));
    rebuilt.push(Message::User(summary));
    rebuilt.extend(restored);
    rebuilt.extend(kept);
    Some(rebuilt)
}

fn parse_method(value: &str) -> Option<CompactionMethodName> {
    match value {
        "remote" => Some(CompactionMethodName::Remote),
        "snapcompact" => Some(CompactionMethodName::Snapcompact),
        "handoff" => Some(CompactionMethodName::Handoff),
        "shake" => Some(CompactionMethodName::Shake),
        "soft" => Some(CompactionMethodName::Soft),
        _ => None,
    }
}
