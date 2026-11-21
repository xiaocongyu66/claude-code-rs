//! `services/compaction2/session/entries.ts` — 压缩会话条目（JSONL 行）。
//!
//! 一次压缩作为独立 JSONL 行（`type: 'compaction'`）写在普通 transcript
//! 消息旁边。该行刻意*不*参与链：`parentUuid` 只是溯源，早于 V2 的读取方
//! 直接跳过未知 `type`。
//!
//! resume 时 `rebuild.rs` 消费最新行重建模型上下文（边界 + 摘要 + 保留
//! 尾部）。行缺失或损坏时调用方回落 legacy `compact_boundary` 消息，所以
//! 写这一行永不改变 legacy resume 路径。

use serde_json::{Map, Value};

use crate::services::compact::{now_ts, uuid_v4, Content, Message};
use crate::services::compaction2::contracts::CompactionMethodName;

/// TS `COMPACTION_SESSION_ENTRY_TYPE`。
pub const COMPACTION_SESSION_ENTRY_TYPE: &str = "compaction";

/// TS `CompactionEntryAttachmentMeta`。
///
/// legacy `buildPostCompactMessages` 在摘要后追加的压缩后附件 / hook 结果
/// 的有界元数据。日志永不携带完整文件内容或 base64 图片：resume 时这些
/// 被恢复为降级的纯文本消息（见 `rebuild.rs`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactionEntryAttachmentMeta {
    /// `attachment.type`（或非 attachment hook 的消息 type）。
    #[serde(rename = "type")]
    pub typ: String,
    /// 文件路径 / 文件名 / plan 路径（附件有其一时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// hook 或 skill 名（附件有其一时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 有界短文本（content / message / stdout / skill 文本）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 附件绑定的 tool use id（若有）。
    #[serde(rename = "toolUseId", skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// TS `MAX_ATTACHMENT_META_ITEM_BYTES`：单个附件/hook 元数据条目的预算。
pub const MAX_ATTACHMENT_META_ITEM_BYTES: usize = 4 * 1024;
/// TS `MAX_ATTACHMENT_META_TOTAL_BYTES`：`attachments` + `hookResults` 合计预算。
pub const MAX_ATTACHMENT_META_TOTAL_BYTES: usize = 32 * 1024;

/// TS `CompactionSessionEntry`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSessionEntry {
    #[serde(rename = "type")]
    pub typ: String,
    pub uuid: String,
    /// 溯源链接：压缩前最后一条 transcript 消息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    /// resume 时作为 compact summary user message 注入的摘要。
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_summary: Option<String>,
    /// 切点后保留的第一条 transcript 消息。
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_data: Option<Map<String, Value>>,
    /// 有界压缩后附件（见 CompactionEntryAttachmentMeta）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<CompactionEntryAttachmentMeta>>,
    /// 有界压缩后 hook 结果。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_results: Option<Vec<CompactionEntryAttachmentMeta>>,
    pub timestamp: String,
}

/// `CompactionMethodName` 是否是合法的日志方法名（wire 格式为小写字符串）。
fn is_method_name(value: &str) -> bool {
    matches!(
        value,
        "remote" | "snapcompact" | "handoff" | "shake" | "soft"
    )
}

fn method_to_wire(method: CompactionMethodName) -> &'static str {
    method.as_str()
}

/// TS `isCompactionSessionEntry(value)`。
///
/// 读取方与 `rebuildMessagesFromEntries` 共用的结构校验器。刻意保持零
/// 依赖（不 import compaction2 其余部分），以便解析裸 JSONL 时直接使用。
pub fn is_compaction_session_entry(value: &Value) -> bool {
    parse_compaction_session_entry(value).is_some()
}

/// 结构校验 + 解析：失败/缺字段/格式非法返回 `None`。
pub fn parse_compaction_session_entry(value: &Value) -> Option<CompactionSessionEntry> {
    serde_json::from_value::<CompactionSessionEntry>(value.clone())
        .ok()
        .filter(|entry| {
            entry.typ == COMPACTION_SESSION_ENTRY_TYPE
                && is_method_name(&entry.method)
                && chrono::DateTime::<chrono::FixedOffset>::parse_from_rfc3339(&entry.timestamp)
                    .is_ok()
        })
}

fn serialized_bytes<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(0)
}

/// 在 UTF-8 字节预算内截断，不切开码点。
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let bytes = character.len_utf8();
        if used + bytes > max_bytes {
            break;
        }
        used += bytes;
        out.push(character);
    }
    out
}

fn first_string(record: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(Value::String(value)) = record.get(*key) {
            if !value.is_empty() {
                return Some(value.clone());
            }
        }
    }
    None
}

/// `textFromContent(value)`：字符串或内容块数组 → 拼接文本。
fn text_from_json_content(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                let Some(record) = item.as_object() else {
                    continue;
                };
                if record.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(Value::String(text)) = record.get("text") {
                        parts.push(text.clone());
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// `textFromContent(message.message?.content)`：CCH 内层 content 投影。
fn text_from_message_content(content: Option<&Content>) -> Option<String> {
    let Some(content) = content else {
        return None;
    };
    match content {
        Content::Text(text) if !text.is_empty() => Some(text.clone()),
        Content::Blocks(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

fn text_from_skills(value: Option<&Value>) -> Option<String> {
    let Some(Value::Array(items)) = value else {
        return None;
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        let Some(skill) = item.as_object() else {
            continue;
        };
        let name = skill.get("name").and_then(Value::as_str);
        let path = skill.get("path").and_then(Value::as_str);
        let content = skill.get("content").and_then(Value::as_str);
        if name.is_none() && path.is_none() && content.is_none() {
            continue;
        }
        let header = match (name, path) {
            (Some(name), Some(path)) => format!("### {name} ({path})"),
            (Some(name), None) => format!("### {name}"),
            (None, Some(path)) => format!("### skill ({path})"),
            (None, None) => "### skill".to_string(),
        };
        parts.push(format!("{header}\n{}", content.unwrap_or("")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// TS `projectAttachmentMeta(message)`。
///
/// 把一条压缩后消息降为有界元数据：只有 type、path、name 与短文本。
/// 嵌套载荷（文件内容、图片 base64、tool 结果）永不拷贝。
pub fn project_attachment_meta(message: &Message) -> Option<CompactionEntryAttachmentMeta> {
    if let Message::Attachment(attachment) = message {
        let Value::Object(record) = &attachment.attachment else {
            return None;
        };
        let Some(Value::String(typ)) = record.get("type") else {
            return None;
        };
        let first_skill = match record.get("skills") {
            Some(Value::Array(skills)) => skills.first().and_then(Value::as_object),
            _ => None,
        };
        let path = first_string(record, &["path", "filename", "filePath", "planFilePath", "uri"])
            .or_else(|| {
                first_skill
                    .and_then(|skill| skill.get("path"))
                    .and_then(Value::as_str)
                    .map(String::from)
            });
        let name = first_string(record, &["hookName", "name", "agentType"]).or_else(|| {
            first_skill
                .and_then(|skill| skill.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        });
        let text = text_from_json_content(record.get("content"))
            .or_else(|| text_from_skills(record.get("skills")))
            .or_else(|| first_string(record, &["planContent", "message", "stdout", "stderr"]));
        let tool_use_id = record
            .get("toolUseID")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(String::from);
        return Some(CompactionEntryAttachmentMeta {
            typ: typ.clone(),
            path,
            name,
            text,
            tool_use_id,
        });
    }

    // hook 结果通常是 attachment 消息，但为其他消息类型保留文本投影，
    // 而不是整体丢弃 hook 输出。
    let text = text_from_message_content(message.content()).or_else(|| {
        let value = serde_json::to_value(message).ok()?;
        let Value::Object(record) = value else {
            return None;
        };
        first_string(&record, &["content", "message"])
    })?;
    Some(CompactionEntryAttachmentMeta {
        typ: message.typ().to_string(),
        path: None,
        name: None,
        text: Some(text),
        tool_use_id: None,
    })
}

/// TS `boundAttachmentMeta(meta)`：执行单条预算，丢弃无法表示的字段。
pub fn bound_attachment_meta(
    meta: &CompactionEntryAttachmentMeta,
) -> Option<CompactionEntryAttachmentMeta> {
    let typ = meta.typ.trim();
    if typ.is_empty() {
        return None;
    }
    let mut bounded = CompactionEntryAttachmentMeta {
        typ: typ.to_string(),
        path: None,
        name: None,
        text: None,
        tool_use_id: None,
    };
    if let Some(path) = meta.path.as_ref().filter(|value| !value.is_empty()) {
        bounded.path = Some(path.clone());
    }
    if let Some(name) = meta.name.as_ref().filter(|value| !value.is_empty()) {
        bounded.name = Some(name.clone());
    }
    if let Some(tool_use_id) = meta.tool_use_id.as_ref().filter(|value| !value.is_empty()) {
        bounded.tool_use_id = Some(tool_use_id.clone());
    }
    if let Some(text) = meta.text.as_ref().filter(|value| !value.is_empty()) {
        // 为 `text` 周围的 JSON key/引号预留空间。
        let room = MAX_ATTACHMENT_META_ITEM_BYTES
            .saturating_sub(serialized_bytes(&bounded))
            .saturating_sub(16);
        if room > 0 {
            bounded.text = Some(truncate_utf8(text, room));
        }
    }
    if serialized_bytes(&bounded) <= MAX_ATTACHMENT_META_ITEM_BYTES {
        return Some(bounded);
    }
    // 病态超长的 path/name 挤掉了所有空间：只保留 type。
    let only_type = CompactionEntryAttachmentMeta {
        typ: typ.to_string(),
        path: None,
        name: None,
        text: None,
        tool_use_id: None,
    };
    if serialized_bytes(&only_type) <= MAX_ATTACHMENT_META_ITEM_BYTES {
        Some(only_type)
    } else {
        None
    }
}

fn append_bounded(
    list: &[CompactionEntryAttachmentMeta],
    out: &mut Vec<CompactionEntryAttachmentMeta>,
    used: &mut usize,
) {
    for item in list {
        let Some(bounded) = bound_attachment_meta(item) else {
            continue;
        };
        // +2 为每条周围的 JSON 数组括号/逗号开销预留。
        let size = serialized_bytes(&bounded) + 2;
        if *used + size > MAX_ATTACHMENT_META_TOTAL_BYTES {
            break;
        }
        out.push(bounded);
        *used += size;
    }
}

/// TS `boundAttachmentMetaList(list)`：按顺序施加总预算，超预算即停。
pub fn bound_attachment_meta_list(
    list: &[CompactionEntryAttachmentMeta],
) -> Option<Vec<CompactionEntryAttachmentMeta>> {
    let mut out: Vec<CompactionEntryAttachmentMeta> = Vec::new();
    let mut used = 0usize;
    append_bounded(list, &mut out, &mut used);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 有界后的附件/hook 结果对（`None` = 该列表没有可用内容）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundedAttachmentMetaPair {
    pub attachments: Option<Vec<CompactionEntryAttachmentMeta>>,
    pub hook_results: Option<Vec<CompactionEntryAttachmentMeta>>,
}

/// TS `boundAttachmentMetaPair(attachments, hookResults)`。
///
/// attachments 与 hookResults 共享同一个 32KB 预算，顺序与 legacy
/// 压缩后布局一致（attachments 在前）。一个列表溢出后另一个列表没有
/// 剩余预算——即"在日志上限处截断"。
pub fn bound_attachment_meta_pair(
    attachments: &[CompactionEntryAttachmentMeta],
    hook_results: &[CompactionEntryAttachmentMeta],
) -> BoundedAttachmentMetaPair {
    let mut bounded_attachments: Vec<CompactionEntryAttachmentMeta> = Vec::new();
    let mut bounded_hook_results: Vec<CompactionEntryAttachmentMeta> = Vec::new();
    let mut used = 0usize;
    append_bounded(attachments, &mut bounded_attachments, &mut used);
    append_bounded(hook_results, &mut bounded_hook_results, &mut used);
    BoundedAttachmentMetaPair {
        attachments: if bounded_attachments.is_empty() {
            None
        } else {
            Some(bounded_attachments)
        },
        hook_results: if bounded_hook_results.is_empty() {
            None
        } else {
            Some(bounded_hook_results)
        },
    }
}

fn to_attachment_meta(value: &Value) -> Option<CompactionEntryAttachmentMeta> {
    let record = value.as_object()?;
    let typ = record.get("type").and_then(Value::as_str)?;
    if typ.is_empty() {
        return None;
    }
    Some(CompactionEntryAttachmentMeta {
        typ: typ.to_string(),
        path: record.get("path").and_then(Value::as_str).map(String::from),
        name: record.get("name").and_then(Value::as_str).map(String::from),
        text: record.get("text").and_then(Value::as_str).map(String::from),
        tool_use_id: record
            .get("toolUseId")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

fn to_attachment_meta_array(value: Option<&Value>) -> Vec<CompactionEntryAttachmentMeta> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(to_attachment_meta)
        .collect::<Vec<_>>()
}

/// TS `sanitizeAttachmentMetaList(value)`。
///
/// 读取侧护栏：丢掉损坏条目并重新施加预算，手改或截断的日志行就无法
/// 注入超大载荷。`None` 表示"没有可用内容"，调用方保持 legacy
/// 无附件行为。
pub fn sanitize_attachment_meta_list(
    value: Option<&Value>,
) -> Option<Vec<CompactionEntryAttachmentMeta>> {
    let items = to_attachment_meta_array(value);
    if items.is_empty() {
        None
    } else {
        bound_attachment_meta_list(&items)
    }
}

/// TS `sanitizeAttachmentMetaPair(attachments, hookResults)`。
pub fn sanitize_attachment_meta_pair(
    attachments: Option<&Value>,
    hook_results: Option<&Value>,
) -> BoundedAttachmentMetaPair {
    bound_attachment_meta_pair(
        &to_attachment_meta_array(attachments),
        &to_attachment_meta_array(hook_results),
    )
}

/// TS `CreateCompactionSessionEntryInput`。
///
/// 注：TS 还接受可选 `shortSummary`；仓内唯一的 Rust 调用方
/// （orchestrator 的 `persistCompactionEntry`）从不传它，此处不建模。
#[derive(Debug, Clone)]
pub struct CreateCompactionSessionEntryInput {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    pub method: CompactionMethodName,
    pub preserve_data: Option<Map<String, Value>>,
    pub attachments: Option<Vec<CompactionEntryAttachmentMeta>>,
    pub hook_results: Option<Vec<CompactionEntryAttachmentMeta>>,
    pub parent_uuid: Option<String>,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
}

/// TS `createCompactionSessionEntry(input)`。
pub fn create_compaction_session_entry(
    input: CreateCompactionSessionEntryInput,
) -> CompactionSessionEntry {
    CompactionSessionEntry {
        typ: COMPACTION_SESSION_ENTRY_TYPE.to_string(),
        uuid: input.uuid.unwrap_or_else(uuid_v4),
        parent_uuid: input.parent_uuid,
        summary: input.summary,
        short_summary: None,
        first_kept_entry_id: input.first_kept_entry_id,
        tokens_before: input.tokens_before,
        method: method_to_wire(input.method).to_string(),
        preserve_data: input.preserve_data,
        attachments: input.attachments,
        hook_results: input.hook_results,
        timestamp: input.timestamp.unwrap_or_else(now_ts),
    }
}

/// TS `appendCompactionEntry(entry)`。
///
/// 经 sessionStorage 既有写入队列追加，与其他 transcript 行共享同一套
/// 原子 append/flush/shutdown-seal 语义。`utils/sessionStorage.ts` 尚未
/// 转换：按仓内惯例 TODO 存根（调用方容错，不改变压缩结果）。
pub async fn append_compaction_entry(_entry: &CompactionSessionEntry) {
    // TODO(utils/sessionStorage.ts): appendSessionEntry(entry)。
}
