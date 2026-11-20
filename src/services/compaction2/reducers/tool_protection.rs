//! `services/compaction2/reducers/tool_protection.rs` — 本地压缩 reducer
//! 的工具结果防护助手（CCH 适配 oh-my-pi 的 tool-protection）。
//!
//! CCH 把工具调用放在 assistant 消息的 `tool_use` 块、结果放在 user
//! 消息的 `tool_result` 块，配对经 `tool_use_id` 解析。纯函数：无 I/O、
//! 无 settings、无 provider 访问。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::services::compact::{Content, Message};

/// TS `ToolUseBlock`（结构化视图，块本身仍是 `serde_json::Value`）。
#[derive(Debug, Clone)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: HashMap<String, Value>,
}

/// TS `ToolResultBlock`：tool_result 块的结构化视图。reducer 通过
/// `mark_pruned` 在**副本**上做带外"已清空"标记（原块不落请求 wire）。
#[derive(Debug, Clone)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    /// 内容副本（字符串或块数组）。
    pub content: Option<Value>,
    pub is_error: Option<bool>,
    /// 工具标记的"无信息"结果。
    pub useless: Option<bool>,
    /// reducer 清空文本后设置的带外标记。
    pub pruned: bool,
    /// 可选的工具名（CCH 通常从配对的 tool_use 解析）。
    pub tool_name: Option<String>,
}

/// TS `TextBlockLike` 的结构化视图。
pub struct TextBlock {
    pub text: String,
}

/// TS `ProtectedToolContext`。
pub struct ProtectedToolContext<'a> {
    pub tool_result: &'a ToolResultBlock,
    pub tool_call: Option<&'a ToolUseBlock>,
    pub message: Option<&'a Message>,
}

/// TS `ProtectedToolMatcher`：字符串（工具名，大小写不敏感）或谓词。
pub enum ProtectedToolMatcher {
    ToolName(String),
    Predicate(Box<dyn Fn(&ProtectedToolContext<'_>) -> bool + Send + Sync>),
}

const SKILL_INTERNAL_URL_PREFIX: &str = "skill://";
const ARTIFACT_INTERNAL_URL_PREFIX: &str = "artifact://";

pub fn is_record(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

pub fn is_text_block(value: &Value) -> Option<TextBlock> {
    if value.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .map(|text| TextBlock { text: text.to_string() })
}

pub fn as_tool_use_block(value: &Value) -> Option<ToolUseBlock> {
    if value.get("type").and_then(Value::as_str) != Some("tool_use") {
        return None;
    }
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let name = value.get("name").and_then(Value::as_str)?.to_string();
    let input = match value.get("input") {
        Some(Value::Object(map)) => map.clone().into_iter().collect(),
        _ => HashMap::new(),
    };
    Some(ToolUseBlock { id, name, input })
}

pub fn as_tool_result_block(value: &Value) -> Option<ToolResultBlock> {
    if value.get("type").and_then(Value::as_str) != Some("tool_result") {
        return None;
    }
    let tool_use_id = value.get("tool_use_id").and_then(Value::as_str)?.to_string();
    Some(ToolResultBlock {
        tool_use_id,
        content: value.get("content").cloned(),
        is_error: value.get("is_error").and_then(Value::as_bool),
        useless: value.get("useless").and_then(Value::as_bool),
        pruned: false,
        tool_name: value.get("tool_name").and_then(Value::as_str).map(String::from),
    })
}

/// 消息的内容块数组（字符串内容返回 None）。
pub fn get_content_array(message: &Message) -> Option<&Vec<Value>> {
    message.content().and_then(|content| content.blocks())
}

/// TS `collectToolUsesById(messages)`：id → tool_use 映射。
pub fn collect_tool_uses_by_id(messages: &[Message]) -> HashMap<String, ToolUseBlock> {
    let mut tool_calls = HashMap::new();
    for message in messages {
        let Some(blocks) = get_content_array(message) else {
            continue;
        };
        for block in blocks {
            if let Some(tool_use) = as_tool_use_block(block) {
                tool_calls.insert(tool_use.id.clone(), tool_use);
            }
        }
    }
    tool_calls
}

/// 消息中的全部 `tool_result` 块视图，按内容数组顺序。
pub fn get_tool_result_entries(message: &Message) -> Vec<(ToolResultBlock, usize)> {
    let Some(blocks) = get_content_array(message) else {
        return Vec::new();
    };
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            as_tool_result_block(block).map(|block| (block, index))
        })
        .collect()
}

/// TS `getToolResultText(block)`：字符串形式，或数组形式中全部文本块按
/// 换行拼接。非文本块（图片/文档）忽略。
pub fn get_tool_result_text(block: &ToolResultBlock) -> Option<String> {
    match &block.content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Some(Value::Array(items)) => {
            let fragments: Vec<String> = items
                .iter()
                .filter_map(|item| is_text_block(item).map(|text| text.text))
                .filter(|text| !text.is_empty())
                .collect();
            if fragments.is_empty() {
                None
            } else {
                Some(fragments.join("\n"))
            }
        }
        _ => None,
    }
}

/// TS `getToolName(toolResult, toolCall?)`：优先配对调用，其次块覆盖。
pub fn get_tool_name(
    tool_result: &ToolResultBlock,
    tool_call: Option<&ToolUseBlock>,
) -> Option<String> {
    if let Some(tool_call) = tool_call {
        return Some(tool_call.name.clone());
    }
    tool_result.tool_name.clone()
}

/// 与结果配对的 `Read`/`read` 调用的目标路径。CCH 的 Read 用
/// `file_path`；oh-my-pi 用 `path`——两者都接受。
pub fn get_read_tool_path(context: &ProtectedToolContext<'_>) -> Option<String> {
    if get_tool_name(context.tool_result, context.tool_call)
        .map(|name| name.to_ascii_lowercase())
        .as_deref()
        != Some("read")
    {
        return None;
    }
    let tool_call = context.tool_call?;
    let path = tool_call
        .input
        .get("file_path")
        .or_else(|| tool_call.input.get("path"))
        .and_then(Value::as_str)?;
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// `read` 结果目标是 `skill://` 内部路径时，所有 reducer 都保护之。
pub fn is_skill_read_tool_result(context: &ProtectedToolContext<'_>) -> bool {
    get_read_tool_path(context)
        .map(|path| path.starts_with(SKILL_INTERNAL_URL_PREFIX))
        .unwrap_or(false)
}

/// 会话 artifact（`artifact://`）的恢复读取：清掉只会再铸一个
/// artifact，无限循环。
pub fn is_artifact_recovery_tool_result(context: &ProtectedToolContext<'_>) -> bool {
    get_read_tool_path(context)
        .map(|path| path.starts_with(ARTIFACT_INTERNAL_URL_PREFIX))
        .unwrap_or(false)
}

/// TS `isProtectedToolResult(toolResult, toolCall?, matchers)`。
pub fn is_protected_tool_result(
    tool_result: &ToolResultBlock,
    tool_call: Option<&ToolUseBlock>,
    matchers: &[ProtectedToolMatcher],
) -> bool {
    for matcher in matchers {
        match matcher {
            ProtectedToolMatcher::ToolName(expected) => {
                let name = get_tool_name(tool_result, tool_call);
                if name
                    .map(|name| name.to_ascii_lowercase() == expected.to_ascii_lowercase())
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            ProtectedToolMatcher::Predicate(predicate) => {
                let context = ProtectedToolContext {
                    tool_result,
                    tool_call,
                    message: None,
                };
                if predicate(&context) {
                    return true;
                }
            }
        }
    }
    false
}

/// TS `replaceToolResultText(block, replacement)`：返回文本被替换、保留
/// 全部非文本块（图片/文档）原序的副本。首个文本块就地替换，其余文本
/// 块丢弃；原本无文本块时前置替换文本。
pub fn replace_tool_result_text(
    block: &ToolResultBlock,
    replacement: &str,
    original: Option<&Value>,
) -> Value {
    let Some(original) = original else {
        return Value::String(replacement.to_string());
    };
    match original {
        Value::String(_) => Value::String(replacement.to_string()),
        Value::Array(items) => {
            let mut kept: Vec<Value> = Vec::with_capacity(items.len());
            let mut replaced = false;
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text")
                    && item.get("text").and_then(Value::as_str).is_some()
                {
                    if !replaced {
                        kept.push(serde_json::json!({ "type": "text", "text": replacement }));
                        replaced = true;
                    }
                    continue;
                }
                kept.push(item.clone());
            }
            if !replaced {
                kept.insert(0, serde_json::json!({ "type": "text", "text": replacement }));
            }
            Value::Array(kept)
        }
        _ => Value::Array(vec![serde_json::json!({
            "type": "text",
            "text": replacement
        })]),
    }
}

/// 空保护集合（reducer 组装默认配置时使用）。
pub fn no_protected_matchers() -> Vec<ProtectedToolMatcher> {
    Vec::new()
}

/// 保持上层模块类型一致性的简写。
pub type BlockIndex = usize;

/// 用于 shake 的文本块判定（`isTextBlock` 的布尔形式）。
pub fn block_is_text(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("text")
        && value.get("text").and_then(Value::as_str).is_some()
}

/// 内容块迭代辅助：user/assistant 消息的块列表。
pub fn message_blocks(message: &Message) -> Vec<&Value> {
    match message.content() {
        Some(Content::Blocks(blocks)) => blocks.iter().collect(),
        _ => Vec::new(),
    }
}

/// 无用 tool_use id 集合辅助类型。
pub type ToolUseIdSet = HashSet<String>;
