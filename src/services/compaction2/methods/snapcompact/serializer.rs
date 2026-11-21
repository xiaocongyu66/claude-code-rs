//! `services/compaction2/methods/snapcompact/serializer.ts` — snapcompact
//! 帧的会话序列化。
//!
//! 从 oh-my-pi 的 `serializeConversation` 移植，适配 CCH 的 Anthropic
//! 形态消息（`msg.message.content` 含 text/tool_use/tool_result/thinking/
//! image 块）。
//!
//! 预算与上游契约一致：
//! - tool 结果：2,000 字符，头尾按 0.6 切分；
//! - 每个工具参数值：500 字符；
//! - 一次工具调用的整个参数列表：2,000 字符；
//! - tool 结果体默认dims（零宽 ink 开关）。

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::services::compact::{Content, Message};

/// TS `TOOL_RESULT_MAX_CHARS`：每个 tool 结果的默认字符上限。
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;
/// TS `TOOL_ARG_MAX_CHARS`：每个参数值的默认字符上限。
pub const TOOL_ARG_MAX_CHARS: usize = 500;
/// TS `TOOL_CALL_MAX_CHARS`：每次调用的整个参数列表默认上限。
pub const TOOL_CALL_MAX_CHARS: usize = 2000;
/// TS `TRUNCATE_HEAD_RATIO`：截断预算的头部占比；其余留给尾部。
pub const TRUNCATE_HEAD_RATIO: f64 = 0.6;

/// TS `DIM_ON`：渲染器识别的零宽 dim-ink 开。
pub const DIM_ON: char = '\u{000e}';
/// TS `DIM_OFF`：渲染器识别的零宽 dim-ink 关。
pub const DIM_OFF: char = '\u{000f}';
/// TS `NEWLINE_GLYPH`：换行使用的可见单元（对齐上游的全块字符）。
pub const NEWLINE_GLYPH: char = '\u{2588}';

/// TS `SerializeOptions`。
#[derive(Debug, Clone)]
pub struct SerializeOptions {
    /// 每个 tool 结果的上限；默认 TOOL_RESULT_MAX_CHARS。
    pub tool_result_max_chars: usize,
    /// 每个参数值的上限；默认 TOOL_ARG_MAX_CHARS。
    pub tool_arg_max_chars: usize,
    /// 每次调用整个参数列表的上限；默认 TOOL_CALL_MAX_CHARS。
    pub tool_call_max_chars: usize,
    /// 头部占比，钳制到 [0, 1]；默认 TRUNCATE_HEAD_RATIO。
    pub truncate_head_ratio: f64,
    /// tool 结果体用 dim ink 打印；默认 true。
    pub dim_tool_results: bool,
    /// assistant 推理序列化为 `¶think:` 段；默认 true。
    pub include_thinking: bool,
}

impl Default for SerializeOptions {
    fn default() -> Self {
        SerializeOptions {
            tool_result_max_chars: TOOL_RESULT_MAX_CHARS,
            tool_arg_max_chars: TOOL_ARG_MAX_CHARS,
            tool_call_max_chars: TOOL_CALL_MAX_CHARS,
            truncate_head_ratio: TRUNCATE_HEAD_RATIO,
            dim_tool_results: true,
            include_thinking: true,
        }
    }
}

// biome-ignore lint/suspicious/noControlCharactersInRegex: ink 开关是有意的零宽控制字符
static DIM_MARKERS: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new("[\u{000e}\u{000f}]").ok());
// biome-ignore lint/suspicious/noControlCharactersInRegex: ANSI escape 引导序列
static ANSI_ESCAPE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new("\\x1b\\[[0-9;?]*[ -/]*[@-~]").ok());
static DATA_URL: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(concat!(
        r"data:[A-Za-z][\w.+-]*/[\w.+-]+",
        r"(?:[\w!#$%&'*+.^|~\\-]+=[\w!#$%&'*+.^|~\\-]+)*",
        r";base64,[A-Za-z0-9+/=]+"
    ))
    .ok()
});

/// TS `truncateForSummary(text, maxChars, headRatio)`。
///
/// 保留 `text` 的头尾，中段超过 `maxChars` 时省略。
pub fn truncate_for_summary(text: &str, max_chars: usize, head_ratio: f64) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let ratio = head_ratio.clamp(0.0, 1.0);
    let head_chars = (max_chars as f64 * ratio).round() as usize;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let elided = text.chars().count() - max_chars;
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars.iter().take(head_chars).collect();
    let tail: String = if tail_chars > 0 {
        chars[chars.len().saturating_sub(tail_chars)..].iter().collect()
    } else {
        String::new()
    };
    format!("{head} […{elided}ch elided…] {tail}")
}

/// TS `stripDimMarkers(text)`：剥掉混入的 ink 开关，原始内容无法伪造 dim 段。
pub fn strip_dim_markers(text: &str) -> String {
    DIM_MARKERS
        .as_ref()
        .map(|regex| regex.replace_all(text, "").to_string())
        .unwrap_or_else(|| text.to_string())
}

/// TS `elideDataUrls(text)`：截断前把内联 base64 data URL 换成短标记。
fn elide_data_urls(text: &str) -> String {
    DATA_URL
        .as_ref()
        .map(|regex| regex.replace_all(text, "[data-url]").to_string())
        .unwrap_or_else(|| text.to_string())
}

/// `AnyBlock`：内容块的已拥有投影（字符串内容也等价为 text 块）。
type AnyBlock = serde_json::Map<String, Value>;

fn blocks_of(message: &Message) -> Vec<AnyBlock> {
    let Some(content) = message.content() else {
        return Vec::new();
    };
    match content {
        Content::Text(text) => {
            // 字符串内容等价为单个 text 块。
            let mut map = serde_json::Map::new();
            map.insert("type".to_string(), Value::String("text".to_string()));
            map.insert("text".to_string(), Value::String(text.clone()));
            vec![map]
        }
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| block.as_object().cloned())
            .collect(),
    }
}

/// TS `toolResultText(block)`：把 tool_result 体（字符串或内容块数组）拍平为文本。
fn tool_result_text(block: &AnyBlock) -> String {
    let Some(content) = block.get("content") else {
        return String::new();
    };
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                let Some(record) = item.as_object() else {
                    continue;
                };
                if record.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(Value::String(text)) = record.get("text") {
                        parts.push(text.clone());
                    }
                } else if matches!(
                    record.get("type").and_then(Value::as_str),
                    Some("image") | Some("document")
                ) {
                    parts.push(format!("[{}]", record["type"].as_str().unwrap_or("block")));
                } else if let Some(Value::String(text)) = record.get("text") {
                    parts.push(text.clone());
                }
            }
            parts.join("")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn arg_value(value: &Value) -> String {
    match value {
        Value::Null => "undefined".to_string(),
        other => other.to_string(),
    }
}

/// 同前缀相邻段合并：`¶user:a` + `¶user:b` → 同一段内换行连接。
fn push_part<'a>(
    parts: &mut Vec<String>,
    last_prefix: &mut Option<&'a str>,
    prefix: &'a str,
    content: &str,
) {
    if let Some(last_index) = parts.len().checked_sub(1) {
        if last_prefix.as_deref() == Some(prefix) {
            let previous = parts[last_index].clone();
            let sep = if previous.ends_with('\n') || content.starts_with('\n') {
                ""
            } else {
                "\n"
            };
            parts[last_index] = format!("{previous}{sep}{content}");
            return;
        }
    }
    parts.push(format!("{prefix}{content}"));
    *last_prefix = Some(prefix);
}

/// TS `serializeConversation(messages, options?)`。
///
/// 把丢弃的会话序列化为帧就绪文本。调用在窗口内时，tool 结果并入其来源
/// `¶call:` 作用域；孤立结果独立渲染。
pub fn serialize_conversation(messages: &[Message], options: Option<&SerializeOptions>) -> String {
    let options = options.cloned().unwrap_or_default();

    let mut parts: Vec<String> = Vec::new();
    let mut last_prefix: Option<&str> = None;

    // 按调用 id 索引存活的 tool 结果。CCH 消息不带 oh-my-pi 的
    // `useless` 标志，但宿主设置时予以尊重。
    let useless_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result_text_by_call_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for message in messages {
        if !matches!(message, Message::User(_)) {
            continue;
        }
        for block in blocks_of(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(Value::String(call_id)) = block.get("tool_use_id") else {
                continue;
            };
            // CCH 的 Message 不带 oh-my-pi 的 `useless` 标志；宿主设置时
            // 由调用方先改写消息再进入序列化。
            let text = tool_result_text(&block);
            if !text.is_empty() {
                result_text_by_call_id.insert(call_id.clone(), text);
            }
        }
    }

    let render_result_block = |raw_text: &str| -> String {
        let body = truncate_for_summary(
            &elide_data_urls(&strip_dim_markers(raw_text)),
            options.tool_result_max_chars,
            options.truncate_head_ratio,
        );
        if options.dim_tool_results {
            format!("<out>\n{DIM_ON}{body}{DIM_OFF}\n</out>")
        } else {
            format!("<out>\n{body}\n</out>")
        }
    };

    let mut merged_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for message in messages {
        if let Message::User(_) = message {
            for block in blocks_of(message) {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(Value::String(text)) = block.get("text") {
                            let text = strip_dim_markers(text);
                            if !text.is_empty() {
                                push_part(&mut parts, &mut last_prefix, "¶user:", &text);
                            }
                        }
                    }
                    Some("image") => {
                        push_part(&mut parts, &mut last_prefix, "¶user:", "[image]");
                    }
                    _ => {}
                }
            }
            continue;
        }
        if !matches!(message, Message::Assistant(_)) {
            continue;
        }

        let mut pending_thinking: Vec<String> = Vec::new();
        let mut pending_text: Vec<String> = Vec::new();
        let flush_assistant = |parts: &mut Vec<String>,
                               last_prefix: &mut Option<&str>,
                               pending_thinking: &mut Vec<String>,
                               pending_text: &mut Vec<String>| {
            if !pending_thinking.is_empty() {
                push_part(parts, last_prefix, "¶think:", &pending_thinking.join("\n"));
                pending_thinking.clear();
            }
            if !pending_text.is_empty() {
                push_part(parts, last_prefix, "¶ai:", &pending_text.join("\n"));
                pending_text.clear();
            }
        };

        for block in blocks_of(message) {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(Value::String(text)) = block.get("text") {
                        let text = strip_dim_markers(text);
                        if !text.trim().is_empty() {
                            pending_text.push(text);
                        }
                    }
                }
                Some("thinking") => {
                    if !options.include_thinking {
                        continue;
                    }
                    let thinking = strip_dim_markers(
                        block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                    if !thinking.trim().is_empty() {
                        pending_thinking.push(thinking);
                    }
                }
                Some("tool_use") => {
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !call_id.is_empty() && useless_call_ids.contains(call_id) {
                        continue;
                    }
                    flush_assistant(
                        &mut parts,
                        &mut last_prefix,
                        &mut pending_thinking,
                        &mut pending_text,
                    );
                    let empty: serde_json::Map<String, Value> = serde_json::Map::new();
                    let args: &serde_json::Map<String, Value> = match block.get("input") {
                        Some(Value::Object(map)) => map,
                        Some(Value::Null) | None => match block.get("arguments") {
                            Some(Value::Object(map)) => map,
                            _ => &empty,
                        },
                        _ => &empty,
                    };
                    let raw_intent = block
                        .get("intent")
                        .and_then(Value::as_str)
                        .or_else(|| args.get("intent").and_then(Value::as_str))
                        .unwrap_or("");
                    let intent = strip_dim_markers(raw_intent)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let mut arg_parts: Vec<String> = Vec::new();
                    for (key, value) in args
                        .iter()
                        .filter(|(key, _)| key.as_str() != "intent")
                    {
                        arg_parts.push(format!(
                            "{}={}",
                            key,
                            truncate_for_summary(
                                &elide_data_urls(&arg_value(value)),
                                options.tool_arg_max_chars,
                                options.truncate_head_ratio,
                            )
                        ));
                    }
                    let args_str = truncate_for_summary(
                        &arg_parts.join(", "),
                        options.tool_call_max_chars,
                        options.truncate_head_ratio,
                    );
                    let block_name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let mut first_line = format!("{block_name}({args_str})");
                    if !intent.is_empty() {
                        first_line.push_str(&format!("//{intent}"));
                    }
                    let mut lines = vec![first_line];
                    if !call_id.is_empty() {
                        if let Some(result_text) = result_text_by_call_id.get(call_id) {
                            merged_call_ids.insert(call_id.to_string());
                            lines.push(render_result_block(result_text));
                        }
                    }
                    push_part(&mut parts, &mut last_prefix, "¶call:", &lines.join("\n"));
                }
                _ => {}
            }
        }
        flush_assistant(&mut parts, &mut last_prefix, &mut pending_thinking, &mut pending_text);
    }

    // 孤立 tool 结果（其调用在本窗口之外）独立渲染。
    for message in messages {
        if !matches!(message, Message::User(_)) {
            continue;
        }
        for block in blocks_of(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(Value::String(call_id)) = block.get("tool_use_id") else {
                continue;
            };
            if useless_call_ids.contains(call_id) || merged_call_ids.contains(call_id) {
                continue;
            }
            if let Some(result_text) = result_text_by_call_id.get(call_id) {
                push_part(
                    &mut parts,
                    &mut last_prefix,
                    "¶call:",
                    &format!("\n{}", render_result_block(result_text)),
                );
            }
        }
    }

    parts.join("\n\n")
}

/// 常见 Unicode 标点折叠为 ASCII，供位图字体使用。
fn char_fold(character: char) -> Option<&'static str> {
    match character {
        '\u{a0}' => Some(" "),
        '—' | '–' | '−' | '─' | '━' => Some("-"),
        '“' | '”' | '„' => Some("\""),
        '‘' | '’' | '‚' => Some("'"),
        '…' => Some("..."),
        '•' | '·' => Some("*"),
        '→' => Some("->"),
        '←' => Some("<-"),
        '¶' => Some("#"),
        '│' | '┃' => Some("|"),
        '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' => Some("+"),
        '✓' | '✔' => Some("[OK]"),
        '✗' | '✘' => Some("[X]"),
        '⚠' => Some("[!]"),
        _ => None,
    }
}

/// ANSI 转义序列剥离后的空白折叠（三步，对齐 TS 的三个正则替换）。
fn collapse_whitespace(text: &str) -> String {
    static SPACE_RUN: Lazy<Option<Regex>> = Lazy::new(|| Regex::new("[\\t\\f\\v ]+").ok());
    static SPACE_AROUND_NEWLINE: Lazy<Option<Regex>> = Lazy::new(|| Regex::new(" *\\n *").ok());
    static NEWLINE_RUN: Lazy<Option<Regex>> = Lazy::new(|| Regex::new("\\n{3,}").ok());
    let collapsed = match SPACE_RUN.as_ref() {
        Some(regex) => regex.replace_all(text, " ").to_string(),
        None => text.to_string(),
    };
    let collapsed = match SPACE_AROUND_NEWLINE.as_ref() {
        Some(regex) => regex.replace_all(&collapsed, "\n").to_string(),
        None => collapsed,
    };
    let collapsed = match NEWLINE_RUN.as_ref() {
        Some(regex) => regex.replace_all(&collapsed, "\n\n").to_string(),
        None => collapsed,
    };
    collapsed.trim().to_string()
}

/// TS `normalizeForFrame(text)`。
///
/// 把序列化文本归一化为位图帧：剥 ANSI、折叠空白、把换行折叠为
/// NEWLINE_GLYPH，并把不可渲染码点音译为 ASCII（无折叠时 `?`）。
/// ink 开关作为零宽标记保留。
pub fn normalize_for_frame(text: &str) -> String {
    let stripped = ANSI_ESCAPE
        .as_ref()
        .map(|regex| regex.replace_all(text, "").to_string())
        .unwrap_or_else(|| text.to_string());
    let stripped = stripped.replace("\r\n", "\n").replace('\r', "\n");
    let collapsed = collapse_whitespace(&stripped);

    let mut out = String::with_capacity(collapsed.len());
    for character in collapsed.chars() {
        if character == '\n' {
            out.push(NEWLINE_GLYPH);
        } else if character == DIM_ON || character == DIM_OFF {
            out.push(character);
        } else if (character as u32) >= 0x20 && (character as u32) <= 0x7e {
            out.push(character);
        } else {
            out.push_str(char_fold(character).unwrap_or("?"));
        }
    }
    out
}
