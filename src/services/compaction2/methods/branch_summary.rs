//! `services/compaction2/methods/branchSummary.ts` — 分支摘要：纯预算/选
//! 择、提示词构造与文件操作跟踪。本模块从不写会话；调用方在导航目标
//! 处自行提交返回的摘要。
//!
//! 预算镜像参考实现：
//!   tokenBudget = (contextWindow || 128000) - reserveTokens（默认 16384）
//! 选择从新到旧遍历条目，让近期上下文存活。

use std::collections::BTreeSet;

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::contracts::EntryView;

/// TS `DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS`。
pub const DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS: i64 = 16_384;
/// TS `DEFAULT_BRANCH_SUMMARY_CONTEXT_WINDOW`。
pub const DEFAULT_BRANCH_SUMMARY_CONTEXT_WINDOW: i64 = 128_000;
/// TS `BRANCH_SUMMARY_MAX_OUTPUT_TOKENS`。
pub const BRANCH_SUMMARY_MAX_OUTPUT_TOKENS: i64 = 2_048;
/// TS `SUMMARIZATION_SYSTEM_PROMPT`（逐字节搬运）。
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You summarize conversation branches. Preserve \
user intent, decisions, open tasks, file paths, commands, and constraints. Return only the summary.";

/// TS `BRANCH_SUMMARY_PREAMBLE`（逐字节搬运）。
pub const BRANCH_SUMMARY_PREAMBLE: &str = "This is a summary of a conversation branch that was \
abandoned when the session tree was navigated. Use it as context for the new branch.";

/// TS `DEFAULT_BRANCH_SUMMARY_INSTRUCTIONS`（逐字节搬运）。
pub const DEFAULT_BRANCH_SUMMARY_INSTRUCTIONS: &str = "Summarize the branch. Preserve decisions, \
open tasks, file paths, commands, and constraints the user stated.";

/// TS `BranchSummaryEntry`。
#[derive(Debug, Clone)]
pub struct BranchSummaryEntry {
    pub id: String,
    pub kind: String,
    pub message: Option<Message>,
    pub tokens: Option<i64>,
    /// `compaction` / `branchSummary` 条目携带。
    pub summary: Option<String>,
    /// 更早分支摘要携带的累计文件明细。
    pub read_files: Option<Vec<String>>,
    pub modified_files: Option<Vec<String>>,
}

/// TS `FileOperations`。
#[derive(Debug, Clone, Default)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

/// TS `BranchPreparation`。
#[derive(Debug, Clone)]
pub struct BranchPreparation {
    pub messages: Vec<Message>,
    pub file_ops: FileOperations,
    pub total_tokens: i64,
}

/// TS `BranchSummaryResult`。
#[derive(Debug, Clone, Default)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub read_files: Option<Vec<String>>,
    pub modified_files: Option<Vec<String>>,
    pub aborted: Option<bool>,
    pub error: Option<String>,
    pub total_tokens: Option<i64>,
}

/// 一次注入的补全调用。
pub type CompleteFn = Box<
    dyn Fn(CompleteInput) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// TS `options.complete({ system, prompt, maxTokens })`。
#[derive(Debug, Clone)]
pub struct CompleteInput {
    pub system: String,
    pub prompt: String,
    pub max_tokens: i64,
}

fn text_of(content: Option<&Content>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str).map(String::from)
                } else {
                    None
                }
            })
            .collect(),
    }
}

/// TS `stripReadSelector(path)`：剥掉 `history://` 或 `path:1-200` 选择器。
pub fn strip_read_selector(path: &str) -> String {
    let trimmed = path.trim();
    let colon = trimmed.rfind(':');
    if let Some(colon) = colon {
        if colon > 0 {
            let tail = &trimmed[colon + 1..];
            let is_selector = tail
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
            if is_selector {
                return trimmed[..colon].to_string();
            }
        }
    }
    trimmed.to_string()
}

/// TS `truncateToolResultForSummary(text, maxChars?)`。
pub fn truncate_tool_result_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}… [truncated]")
}

/// TS `createFileOps()`。
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

fn path_from_input(input: Option<&Value>) -> Option<String> {
    let input = input?;
    let candidate = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("filePath"))
        .and_then(Value::as_str)?;
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// TS `extractFileOpsFromMessage(message, fileOps)`：从一条消息的
/// read/write/edit 工具调用跟踪文件操作。
pub fn extract_file_ops_from_message(message: &Message, file_ops: &mut FileOperations) {
    let Message::Assistant(assistant) = message else {
        return;
    };
    let Content::Blocks(blocks) = &assistant.message.content else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .map(|name| name.to_ascii_lowercase())
            .unwrap_or_default();
        let Some(path) = path_from_input(block.get("input")) else {
            continue;
        };
        match name.as_str() {
            "read" => {
                file_ops.read.insert(strip_read_selector(&path));
            }
            "write" | "edit" | "notebookedit" => {
                file_ops.edited.insert(path);
            }
            _ => {}
        }
    }
}

/// TS `resolveBranchSummaryBudget(contextWindow?, reserveTokens?)`。
pub fn resolve_branch_summary_budget(
    context_window: Option<i64>,
    reserve_tokens: Option<i64>,
) -> i64 {
    let reserve = reserve_tokens.unwrap_or(DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS);
    let window = match context_window {
        Some(window) if window > 0 => window,
        _ => DEFAULT_BRANCH_SUMMARY_CONTEXT_WINDOW,
    };
    std::cmp::max(0, window - reserve)
}

/// TS `estimateBranchMessageTokens(message, estimate?)`。
pub fn estimate_branch_message_tokens(
    message: &Message,
    estimate: Option<&dyn Fn(&Message) -> i64>,
) -> i64 {
    if let Some(estimate) = estimate {
        return std::cmp::max(0, estimate(message));
    }
    let content = message.content();
    let text = text_of(content);
    let tool_result = match content {
        Some(Content::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    Some(truncate_tool_result_for_summary(
                        &text_of(Some(&Content::Blocks(
                            block
                                .get("content")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default()
                                .into(),
                        ))),
                        2_000,
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    let body = if !tool_result.is_empty() { tool_result } else { text };
    if !body.is_empty() {
        return ((body.len() as f64) / 4.0).ceil() as i64;
    }
    let serialized = serde_json::to_string(message).unwrap_or_default();
    if serialized.is_empty() {
        0
    } else {
        ((serialized.len() as f64) / 4.0).ceil() as i64
    }
}

fn message_from_entry(entry: &BranchSummaryEntry) -> Option<Message> {
    if let Some(message) = &entry.message {
        return Some(message.clone());
    }
    let summary = entry.summary.as_ref().filter(|summary| !summary.is_empty())?;
    let label = if entry.kind == "compaction" {
        "Previous compaction summary"
    } else {
        "Previous branch summary"
    };
    Some(Message::User(crate::services::compact::UserMessage {
        uuid: entry.id.clone(),
        timestamp: crate::services::compact::now_ts(),
        message: crate::services::compact::UserMessageBody {
            role: "user".to_string(),
            content: Content::Blocks(vec![serde_json::json!({
                "type": "text",
                "text": format!("[{label}]\n{summary}")
            })]),
        },
        is_meta: None,
        is_compact_summary: None,
        is_visible_in_transcript_only: None,
        parent_uuid: None,
        tool_use_result: None,
        summarize_metadata: None,
    }))
}

/// TS `prepareBranchEntries(entries, tokenBudget, estimate?)`：选择摘要
/// 条目。第一遍收集全部条目（含更早分支摘要）的累计文件操作；第二遍
/// 从新到旧直到预算耗尽——compaction/branchSummary 在边缘略低处仍可
/// 放行，因为它携带连续性。
pub fn prepare_branch_entries(
    entries: &[BranchSummaryEntry],
    token_budget: i64,
    estimate: Option<&dyn Fn(&Message) -> i64>,
) -> BranchPreparation {
    let mut file_ops = create_file_ops();
    for entry in entries {
        if entry.kind == "branchSummary" {
            if let Some(read_files) = &entry.read_files {
                for path in read_files {
                    file_ops.read.insert(strip_read_selector(path));
                }
            }
            if let Some(modified_files) = &entry.modified_files {
                for path in modified_files {
                    file_ops.edited.insert(path.clone());
                }
            }
        }
        if let Some(message) = &entry.message {
            extract_file_ops_from_message(message, &mut file_ops);
        }
    }

    let mut messages: Vec<Message> = Vec::new();
    let mut total_tokens = 0i64;
    for entry in entries.iter().rev() {
        let Some(message) = message_from_entry(entry) else {
            continue;
        };
        let tokens = estimate_branch_message_tokens(&message, estimate);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            let is_summary =
                entry.kind == "compaction" || entry.kind == "branchSummary";
            if is_summary && (total_tokens as f64) < token_budget as f64 * 0.9 {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }
        messages.insert(0, message);
        total_tokens += tokens;
    }
    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

/// TS `FileLists`。
#[derive(Debug, Clone, Default)]
pub struct FileLists {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// TS `computeFileLists(fileOps)`：读列表剔出已编辑项，两者排序。
pub fn compute_file_lists(file_ops: &FileOperations) -> FileLists {
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|path| !file_ops.edited.contains(*path))
        .cloned()
        .collect();
    let modified_files: Vec<String> = file_ops.edited.iter().cloned().collect();
    FileLists {
        read_files,
        modified_files,
    }
}

/// TS `formatFileOperations(fileOps, maxFiles?)`：分组、前缀折叠的文件树。
pub fn format_file_operations(file_ops: &FileOperations, max_files: usize) -> String {
    let mut paths = BTreeSet::new();
    for path in &file_ops.read {
        paths.insert(path.clone());
    }
    for path in &file_ops.edited {
        paths.insert(path.clone());
    }
    if paths.is_empty() {
        return String::new();
    }
    let lines: Vec<(String, &'static str)> = paths
        .iter()
        .map(|path| {
            let read = file_ops.read.contains(path);
            let edited = file_ops.edited.contains(path);
            let marker = if read && edited {
                "(RW)"
            } else if edited {
                "(Write)"
            } else {
                "(Read)"
            };
            (path.clone(), marker)
        })
        .collect();
    let shown = &lines[..lines.len().min(max_files)];
    let elided = lines.len().saturating_sub(shown.len());

    let mut output: Vec<String> = vec!["<files>".to_string()];
    let mut current_dir: Option<String> = None;
    for (path, marker) in shown {
        let slash = path.rfind('/');
        let (dir, name) = match slash {
            Some(slash) => (path[..slash].to_string(), path[slash + 1..].to_string()),
            None => (".".to_string(), path.clone()),
        };
        if current_dir.as_deref() != Some(dir.as_str()) {
            output.push(format!("# {dir}/"));
            current_dir = Some(dir);
        }
        output.push(format!("{name} {marker}"));
    }
    if elided > 0 {
        output.push(format!("[…{elided} files elided…]"));
    }
    output.push("</files>".to_string());
    output.join("\n")
}

/// TS `upsertFileOperations(summary, readFiles, modifiedFiles, fileOps?)`：
/// 替换任何陈旧文件列表并附加当前列表。
pub fn upsert_file_operations(
    summary: &str,
    read_files: &[String],
    modified_files: &[String],
    file_ops: Option<&FileOperations>,
) -> String {
    let legacy = regex::Regex::new(
        r"(?s)<(?:files|read-files|modified-files)>.*?</(?:files|read-files|modified-files)>",
    );
    let cleaned = match legacy {
        Ok(legacy) => legacy.replace_all(summary, ""),
        Err(_) => summary.to_string(),
    }
    .trim_end()
    .to_string();
    let ops = match file_ops {
        Some(file_ops) => file_ops.clone(),
        None => FileOperations {
            read: read_files.iter().cloned().collect(),
            edited: modified_files.iter().cloned().collect(),
        },
    };
    let block = format_file_operations(&ops, 20);
    if block.is_empty() {
        return cleaned;
    }
    if cleaned.is_empty() {
        block
    } else {
        format!("{cleaned}\n\n{block}")
    }
}

/// TS `serializeConversationForSummary(messages)`：把选中消息序列化成
/// 文本，避免模型续写它们。
pub fn serialize_conversation_for_summary(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        let role = match message {
            Message::Assistant(_) => "Assistant",
            Message::User(_) => "User",
            _ => continue,
        };
        let Some(content) = message.content() else {
            continue;
        };
        match content {
            Content::Text(text) => {
                if !text.trim().is_empty() {
                    lines.push(format!("{role}: {text}"));
                }
            }
            Content::Blocks(blocks) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                lines.push(format!("{role}: {text}"));
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            lines.push(format!(
                                "{role} [tool {name}]: {}",
                                serde_json::to_string(&input).unwrap_or_default()
                            ));
                        }
                        Some("tool_result") => {
                            let text = text_of(Some(&Content::Blocks(
                                block
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .cloned()
                                    .unwrap_or_default()
                                    .into(),
                            )));
                            lines.push(format!(
                                "Tool result: {}",
                                truncate_tool_result_for_summary(&text, 2_000)
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    lines.join("\n\n")
}

/// TS `buildBranchSummaryPrompt(conversationText, instructions?)`。
pub fn build_branch_summary_prompt(
    conversation_text: &str,
    instructions: Option<&str>,
) -> String {
    let instructions = instructions.unwrap_or(DEFAULT_BRANCH_SUMMARY_INSTRUCTIONS);
    format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}")
}

/// TS `generateBranchSummary(entries, options?)`：经注入的补全生成分支
/// 摘要。从不写会话：调用方决定摘要附在哪。
pub async fn generate_branch_summary(
    entries: &[BranchSummaryEntry],
    context_window: Option<i64>,
    reserve_tokens: Option<i64>,
    instructions: Option<&str>,
    complete: Option<&CompleteFn>,
    estimate: Option<&dyn Fn(&Message) -> i64>,
) -> BranchSummaryResult {
    let token_budget = resolve_branch_summary_budget(context_window, reserve_tokens);
    let prepared = prepare_branch_entries(entries, token_budget, estimate);
    if prepared.messages.is_empty() {
        return BranchSummaryResult {
            summary: Some("No content to summarize".to_string()),
            total_tokens: Some(0),
            ..Default::default()
        };
    }

    let conversation_text = serialize_conversation_for_summary(&prepared.messages);
    let prompt = build_branch_summary_prompt(&conversation_text, instructions);
    let Some(complete) = complete else {
        return BranchSummaryResult {
            summary: None,
            total_tokens: Some(prepared.total_tokens),
            ..Default::default()
        };
    };

    let text = match (complete)(CompleteInput {
        system: SUMMARIZATION_SYSTEM_PROMPT.to_string(),
        prompt,
        max_tokens: BRANCH_SUMMARY_MAX_OUTPUT_TOKENS,
    })
    .await
    {
        Ok(text) => text,
        Err(error) => {
            return BranchSummaryResult {
                error: Some(error),
                total_tokens: Some(prepared.total_tokens),
                ..Default::default()
            };
        }
    };

    let FileLists {
        read_files,
        modified_files,
    } = compute_file_lists(&prepared.file_ops);
    let body = if text.trim().is_empty() {
        "No summary generated".to_string()
    } else {
        text.trim().to_string()
    };
    let summary = upsert_file_operations(
        &format!("{BRANCH_SUMMARY_PREAMBLE}\n\n{body}"),
        &read_files,
        &modified_files,
        Some(&prepared.file_ops),
    );
    BranchSummaryResult {
        summary: Some(summary),
        read_files: Some(read_files),
        modified_files: Some(modified_files),
        total_tokens: Some(prepared.total_tokens),
        ..Default::default()
    }
}

/// EntryView → BranchSummaryEntry 的便利转换（保留稳定字段）。
pub fn entry_to_branch_entry(entry: &EntryView) -> BranchSummaryEntry {
    BranchSummaryEntry {
        id: entry.id.clone(),
        kind: format!("{:?}", entry.kind),
        message: entry.message.clone(),
        tokens: entry.tokens,
        summary: None,
        read_files: None,
        modified_files: None,
    }
}
