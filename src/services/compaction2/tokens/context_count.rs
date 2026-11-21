//! `services/compaction2/tokens/context_count.ts` — 上下文窗口 token 记账。
//!
//! 镜像 oh-my-pi 的 `calculateContextTokens` 形状：最近一个 provider 上报
//! 的 usage 块是到该 API 响应为止一切的精确锚点，只有其后的消息本地
//! 估算。尾部优先用 `CCH_TOKENIZER_DIR` 的真实词表（W13）；无词表时降级
//! 到 cl100k 原生计数（未接线）再降到内容自适应粗估——与 `estimateTokens`
//! 同一降级链。
//!
//! 图片/文档按 provider 计价而非 base64 负载体积：Anthropic 的 28px
//! patch 公式（上限 4,784 token）、Gemini 的固定 1,120 媒体预算、
//! OpenAI 的 32px patch 面积估算。

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::tokens::registry::resolve_tokenizer;
use crate::services::compaction2::tokens::{content_adaptive_estimate, estimate_tokens};
use crate::services::compaction2::tokens::tiktoken::base64_decode_standard;

/// 返回总数的来源。
#[derive(Debug, Clone, PartialEq)]
pub enum ContextTokenSource {
    UsageAnchor,
    Tokenizer,
    Fallback,
}

/// TS `ContextTokenCount`。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextTokenCount {
    pub tokens: usize,
    pub source: ContextTokenSource,
    /// 锚定计数的消息下标。
    pub anchor_index: Option<usize>,
    /// 锚点之后本地估算的 token（无锚点时为全集）。
    pub tail_tokens: usize,
}

/// TS `CountContextTokensOptions`。
#[derive(Debug, Clone, Default)]
pub struct CountContextTokensOptions {
    /// 图片计价适用的 provider；默认 `getAPIProvider()`（懒解析）。
    pub provider: Option<String>,
    /// provider 侧编排 token：计费但从不重放进会话前缀，故从锚点排除。
    pub orchestration_tokens: Option<i64>,
    /// 测试缝：自定义单块图片/文档计价。
    pub image_tokens: Option<fn(&Value, &str) -> usize>,
}

/// TS `TokenizedMessageCount`。
#[derive(Debug, Clone, PartialEq)]
pub struct TokenizedMessageCount {
    pub tokens: usize,
    /// true 表示用了 tokenizer 数据目录的真实词表。
    pub exact: bool,
}

/// TS `CountMessagesWithTokenizerOptions`。
#[derive(Debug, Clone, Default)]
pub struct CountMessagesWithTokenizerOptions {
    pub provider: Option<String>,
    /// 测试缝：自定义单块图片/文档计价。
    pub image_tokens: Option<fn(&Value, &str) -> usize>,
}

#[derive(Debug, Clone, Copy)]
struct ImageSize {
    width: i64,
    height: i64,
}

// provider 规则取自 oh-my-pi 的目录（`image-tokenization.kdl`）。
const ANTHROPIC_PATCH_PX: i64 = 28;
const ANTHROPIC_MAX_EDGE: i64 = 2576;
const ANTHROPIC_MAX_TOKENS: i64 = 4784;
const GOOGLE_IMAGE_TOKENS: i64 = 1120;
const OPENAI_PATCH_PX: i64 = 32;
const OPENAI_PATCH_MULTIPLIER: f64 = 1.2;
// snapcompact 的帧估算（ceil(4784 * 1.05)）；OpenAI 图片尺寸读不出时
// 使用，保证未知尺寸的计费不低于真实值。
const OPENAI_UNKNOWN_IMAGE_TOKENS: i64 = 5024;

/// 合成模型标记（utils/messages.ts 未转换）：SYNTHETIC_MODEL 的 assistant
/// 消息不作为 usage 锚点候选。
const SYNTHETIC_MODEL: &str = "synthetic";

fn as_record(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn provider_kind(provider: &str) -> &'static str {
    match provider {
        "gemini" | "google" => "google",
        "openai" | "grok" => "openai",
        // firstParty / bedrock / vertex / foundry 都说 Anthropic wire。
        _ => "anthropic",
    }
}

/// 四舍五入到偶数（与 Anthropic API 的 resize 一致）。
fn round_ties_to_even(value: f64) -> i64 {
    let floor = value.floor();
    if (value - floor) != 0.5 {
        return value.round() as i64;
    }
    if floor % 2.0 == 0.0 {
        floor as i64
    } else {
        (floor + 1.0) as i64
    }
}

/// Anthropic 视觉 token：API resize 后 longest edge 二分搜索，取
/// 28px-padded 边适配 maxEdge 且 patch 数适配 maxTokens 的最大值。
fn anthropic_image_tokens(size: ImageSize) -> i64 {
    let long = size.width.max(size.height);
    let short_side = size.width.min(size.height);
    let aspect = long as f64 / short_side as f64;
    let patches = |l: i64, s: i64| {
        ((l as f64 / ANTHROPIC_PATCH_PX as f64).ceil() as i64)
            * ((s as f64 / ANTHROPIC_PATCH_PX as f64).ceil() as i64)
    };
    let fits = |l: i64, s: i64| {
        let pad = |edge: i64| ((edge as f64 / ANTHROPIC_PATCH_PX as f64).ceil() * ANTHROPIC_PATCH_PX as f64) as i64;
        pad(l) <= ANTHROPIC_MAX_EDGE
            && pad(s) <= ANTHROPIC_MAX_EDGE
            && patches(l, s) <= ANTHROPIC_MAX_TOKENS
    };
    let short = |l: i64| {
        let computed = round_ties_to_even(l as f64 / aspect);
        std::cmp::max(computed, 1)
    };
    if fits(long, short_side) {
        return patches(long, short_side);
    }
    let mut lo: i64 = 1; // 永远 fits
    let mut hi: i64 = long; // 永远不 fits
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if fits(mid, short(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    patches(lo, short(lo))
}

fn openai_image_tokens(size: ImageSize) -> i64 {
    let patches =
        ((size.width as f64 / OPENAI_PATCH_PX as f64).ceil() as i64)
            * ((size.height as f64 / OPENAI_PATCH_PX as f64).ceil() as i64);
    ((patches as f64 * OPENAI_PATCH_MULTIPLIER).ceil()) as i64
}

fn default_image_tokens(block: &Value, provider: &str) -> i64 {
    let size = image_size_from_block(block);
    match provider_kind(provider) {
        "google" => GOOGLE_IMAGE_TOKENS,
        "openai" => match size {
            Some(size) => openai_image_tokens(size),
            None => OPENAI_UNKNOWN_IMAGE_TOKENS,
        },
        _ => match size {
            Some(size) => anthropic_image_tokens(size),
            None => ANTHROPIC_MAX_TOKENS,
        },
    }
}

fn image_size_from_block(block: &Value) -> Option<ImageSize> {
    // 部分调用方直接附带尺寸（已解码或远程图片）。
    if let (Some(width), Some(height)) = (
        block.get("width").and_then(Value::as_i64),
        block.get("height").and_then(Value::as_i64),
    ) {
        if width > 0 && height > 0 {
            return Some(ImageSize { width, height });
        }
    }
    let source = block.get("source").and_then(as_record)?;
    if let (Some(width), Some(height)) = (
        source.get("width").and_then(Value::as_i64),
        source.get("height").and_then(Value::as_i64),
    ) {
        if width > 0 && height > 0 {
            return Some(ImageSize { width, height });
        }
    }
    if source.get("type").and_then(Value::as_str) == Some("base64") {
        if let Some(data) = source.get("data").and_then(Value::as_str) {
            return image_size_from_base64(data);
        }
    }
    if source.get("type").and_then(Value::as_str) == Some("url") {
        if let Some(url) = source.get("url").and_then(Value::as_str) {
            if url.starts_with("data:") {
                if let Some(comma) = url.find(',') {
                    return image_size_from_base64(&url[comma + 1..]);
                }
            }
        }
    }
    None
}

/// 64KiB base64 够覆盖 PNG/GIF/WebP 头与 JPEG SOF 标记（含典型
/// EXIF/ICC 段）；取 4 的倍数让切片干净解码。
fn image_size_from_base64(data: &str) -> Option<ImageSize> {
    if data.is_empty() {
        return None;
    }
    let end = std::cmp::min(data.len(), 65_536);
    let bytes = base64_decode_standard(&data[..end]).ok()?;
    image_size_from_buffer(&bytes)
}

fn image_size_from_buffer(buffer: &[u8]) -> Option<ImageSize> {
    // PNG：签名后 IHDR 的宽高在 16/20 偏移。
    if buffer.len() >= 24
        && buffer[0] == 0x89
        && buffer[1] == 0x50
        && buffer[2] == 0x4e
        && buffer[3] == 0x47
        && buffer[12] == 0x49
        && buffer[13] == 0x48
        && buffer[14] == 0x44
        && buffer[15] == 0x52
    {
        let width = read_u32_be(buffer, 16);
        let height = read_u32_be(buffer, 20);
        return positive_size(width, height);
    }

    // GIF："GIF87a"/"GIF89a"，小端尺寸在 6/8。
    if buffer.len() >= 10 && buffer[..3] == [b'G', b'I', b'F'] {
        let width = read_u16_le(buffer, 6) as i64;
        let height = read_u16_le(buffer, 8) as i64;
        return positive_size(width, height);
    }

    // JPEG：遍历 marker 直到 start-of-frame 段。
    if buffer.len() >= 4 && buffer[0] == 0xff && buffer[1] == 0xd8 {
        let mut offset = 2usize;
        while offset + 9 <= buffer.len() {
            if buffer[offset] != 0xff {
                offset += 1;
                continue;
            }
            let mut marker = buffer[offset + 1];
            while marker == 0xff {
                offset += 1;
                if offset + 1 >= buffer.len() {
                    return None;
                }
                marker = buffer[offset + 1];
            }
            if marker == 0xd8 || marker == 0xd9 {
                offset += 2;
                continue;
            }
            if (0xd0..=0xd7).contains(&marker) {
                offset += 2;
                continue;
            }
            let length = read_u16_be(buffer, offset + 2) as usize;
            if length < 2 {
                return None;
            }
            let is_sof = (0xc0..=0xcf).contains(&marker)
                && marker != 0xc4
                && marker != 0xc8
                && marker != 0xcc;
            if is_sof {
                let height = read_u16_be(buffer, offset + 5) as i64;
                let width = read_u16_be(buffer, offset + 7) as i64;
                return positive_size(width, height);
            }
            offset += 2 + length;
        }
        return None;
    }

    // WebP：RIFF 容器，VP8X/VP8/VP8L chunk 尺寸。
    if buffer.len() >= 30
        && &buffer[0..4] == b"RIFF"
        && &buffer[8..12] == b"WEBP"
    {
        let chunk = &buffer[12..16];
        if chunk == b"VP8X" {
            let width = 1
                + ((buffer[26] as i64) << 16 | (buffer[25] as i64) << 8 | buffer[24] as i64);
            let height =
                1 + ((buffer[29] as i64) << 16 | (buffer[28] as i64) << 8 | buffer[27] as i64);
            return positive_size(width, height);
        }
        if chunk == b"VP8 " && buffer[23] == 0x9d && buffer[24] == 0x01 && buffer[25] == 0x2a {
            let width = (read_u16_le(buffer, 26) as i64) & 0x3fff;
            let height = (read_u16_le(buffer, 28) as i64) & 0x3fff;
            return positive_size(width, height);
        }
        if chunk == b"VP8L" {
            let b0 = buffer[21] as i64;
            let b1 = buffer[22] as i64;
            let b2 = buffer[23] as i64;
            let b3 = buffer[24] as i64;
            let width = 1 + (((b1 & 0x3f) << 8) | b0);
            let height = 1 + (((b3 & 0x0f) << 10) | (b2 << 2) | ((b1 & 0xc0) >> 6));
            return positive_size(width, height);
        }
    }
    None
}

fn positive_size(width: i64, height: i64) -> Option<ImageSize> {
    if width > 0 && height > 0 {
        Some(ImageSize { width, height })
    } else {
        None
    }
}

fn read_u32_be(buffer: &[u8], offset: usize) -> i64 {
    ((buffer[offset] as i64) << 24)
        | ((buffer[offset + 1] as i64) << 16)
        | ((buffer[offset + 2] as i64) << 8)
        | (buffer[offset + 3] as i64)
}

fn read_u16_be(buffer: &[u8], offset: usize) -> u16 {
    ((buffer[offset] as u16) << 8) | buffer[offset + 1] as u16
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u16 {
    (buffer[offset] as u16) | ((buffer[offset + 1] as u16) << 8)
}

fn count_content(content: &Value, count_text: &dyn Fn(&str) -> usize, image_tokens: &dyn Fn(&Value) -> usize) -> usize {
    match content {
        Value::String(text) => count_text(text),
        Value::Array(items) => items
            .iter()
            .map(|item| count_block(item, count_text, image_tokens))
            .sum(),
        _ => 0,
    }
}

fn count_document(block: &Value, count_text: &dyn Fn(&str) -> usize, image_tokens: &dyn Fn(&Value) -> usize) -> usize {
    let Some(source) = block.get("source").and_then(as_record) else {
        return image_tokens(block);
    };
    match source.get("type").and_then(Value::as_str) {
        Some("text") => count_text(source.get("data").and_then(Value::as_str).unwrap_or("")),
        Some("content") => match source.get("content") {
            Some(content) => count_content(content, count_text, image_tokens),
            None => 0,
        },
        _ => image_tokens(block),
    }
}

fn count_block(item: &Value, count_text: &dyn Fn(&str) -> usize, image_tokens: &dyn Fn(&Value) -> usize) -> usize {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => count_text(item.get("text").and_then(Value::as_str).unwrap_or("")),
        Some("thinking") => count_text(item.get("thinking").and_then(Value::as_str).unwrap_or("")),
        Some("redacted_thinking") => count_text(item.get("data").and_then(Value::as_str).unwrap_or("")),
        Some("tool_use") => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let input_json = serde_json::to_string(item.get("input").unwrap_or(&Value::Null)).unwrap_or_default();
            count_text(&format!("{name}{input_json}"))
        }
        Some("tool_result") => item
            .get("content")
            .map(|content| count_content(content, count_text, image_tokens))
            .unwrap_or(0),
        Some("image") => image_tokens(item),
        Some("document") => count_document(item, count_text, image_tokens),
        _ => {
            // server_tool_use / web_search_tool_result …… 文本形负载。
            let serialized = serde_json::to_string(item).unwrap_or_default();
            count_text(&serialized)
        }
    }
}

fn count_one_message(message: &Message, count_text: &dyn Fn(&str) -> usize, image_tokens: &dyn Fn(&Value) -> usize) -> usize {
    match message {
        Message::User(user) => match &user.message.content {
            Content::Text(text) => count_text(text),
            Content::Blocks(blocks) => blocks
                .iter()
                .map(|item| count_block(item, count_text, image_tokens))
                .sum(),
        },
        Message::Assistant(assistant) => match &assistant.message.content {
            Content::Text(text) => count_text(text),
            Content::Blocks(blocks) => blocks
                .iter()
                .map(|item| count_block(item, count_text, image_tokens))
                .sum(),
        },
        // 附件归一化进 API user 消息；utils/messages.ts（normalizeAttachmentForAPI）
        // 未转换，附件消息当前计 0（与 messageCount.rs 一致）。
        Message::Attachment(_) | Message::System(_) => 0,
    }
}

/// TS `countMessagesWithTokenizer(messages, model, options?)`：有真实
/// 词表用之，否则 cl100k（未接线）再降级内容自适应。图片/文档单独计价。
pub fn count_messages_with_tokenizer(
    messages: &[Message],
    model: &str,
    options: &CountMessagesWithTokenizerOptions,
) -> TokenizedMessageCount {
    let tokenizer = resolve_tokenizer(model);
    // 懒解析 provider：仅在真的需要给图片计价时读取。
    // utils/model/providers.ts 未转换，默认 anthropic 计价规则。
    let provider = options.provider.clone().unwrap_or_else(|| "anthropic".to_string());
    let injected = options.image_tokens;
    let count_text = |text: &str| {
        if text.is_empty() {
            return 0usize;
        }
        match &tokenizer {
            Some(tokenizer) => tokenizer.count(text),
            None => estimate_tokens(text, model).tokens,
        }
    };
    let image_tokens = |block: &Value| {
        match injected {
            Some(injected) => injected(block, &provider),
            None => default_image_tokens(block, &provider) as usize,
        }
    };

    let tokens: usize = messages
        .iter()
        .map(|message| count_one_message(message, &count_text, &image_tokens))
        .sum();
    TokenizedMessageCount {
        tokens,
        exact: tokenizer.is_some(),
    }
}

fn get_assistant_message_id(message: &Message) -> Option<String> {
    let Message::Assistant(assistant) = message else {
        return None;
    };
    let id = &assistant.message.id;
    if let Some(id) = id {
        if assistant.message.model.as_deref() != Some(SYNTHETIC_MODEL) {
            return Some(id.clone());
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct UsageAnchor {
    index: usize,
    tokens: usize,
}

/// TS `findUsageAnchor(messages)`：找最近的真实 usage 锚点；回扫同一 API
/// 响应拆出的兄弟记录（同 message.id），让它们之间的 tool_result 算进
/// 尾部估算。合成与全零 usage 块跳过。
fn find_usage_anchor(messages: &[Message]) -> Option<UsageAnchor> {
    let mut i = messages.len() as isize - 1;
    while i >= 0 {
        let message = &messages[i as usize];
        if let Some(usage) = get_token_usage(message) {
            let tokens = get_token_count_from_usage(&usage);
            if tokens > 0 {
                if let Some(response_id) = get_assistant_message_id(message) {
                    let mut j = i - 1;
                    while j >= 0 {
                        let prior = &messages[j as usize];
                        let prior_id = get_assistant_message_id(prior);
                        if prior_id == Some(response_id.clone()) {
                            // 同一 API 响应的更早拆分——锚点改到这里。
                            i = j;
                        } else if prior_id.is_some() {
                            // 撞上另一个 API 响应——停止回扫。
                            break;
                        }
                        // prior_id 为 None：user/tool_result/attachment 消息，
                        // 可能穿插在拆分之间——继续回扫。
                        j -= 1;
                    }
                }
                return Some(UsageAnchor {
                    index: i as usize,
                    tokens,
                });
            }
        }
        i -= 1;
    }
    None
}

/// `utils/tokens.ts`（未转换）的本地等价：取消息的 usage 块。
fn get_token_usage(message: &Message) -> Option<Value> {
    let Message::Assistant(assistant) = message else {
        return None;
    };
    assistant.message.usage.clone()
}

/// `getTokenCountFromUsage(usage)`：input + cache_read + cache_creation +
/// output。
fn get_token_count_from_usage(usage: &Value) -> usize {
    let input = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input + cache_read + cache_creation + output).max(0) as usize
}

/// TS `countContextTokens(messages, model, options?)`。
///
/// 公式：`锚点(input + cache_read + cache_creation + output)
///        - orchestrationTokens
///        + tokenizerEstimate(锚点之后的消息)`
///
/// 无 usage 锚点时整段 transcript 用 tokenizer 计数；无 tokenizer 数据
/// 时尾部降级 cl100k 再降级粗估。
pub fn count_context_tokens(
    messages: &[Message],
    model: &str,
    options: &CountContextTokensOptions,
) -> ContextTokenCount {
    let anchor = find_usage_anchor(messages);
    let tail: Vec<Message> = match &anchor {
        Some(anchor) => messages[anchor.index + 1..].to_vec(),
        None => messages.to_vec(),
    };
    let counted = count_messages_with_tokenizer(
        &tail,
        model,
        &CountMessagesWithTokenizerOptions {
            provider: options.provider.clone(),
            image_tokens: options.image_tokens,
        },
    );
    let orchestration = std::cmp::max(0, options.orchestration_tokens.unwrap_or(0)) as usize;
    let anchor_tokens = anchor
        .map(|anchor| anchor.tokens.saturating_sub(orchestration))
        .unwrap_or(0);

    ContextTokenCount {
        tokens: anchor_tokens + counted.tokens,
        source: match &anchor {
            Some(_) => ContextTokenSource::UsageAnchor,
            None => {
                if counted.exact {
                    ContextTokenSource::Tokenizer
                } else {
                    ContextTokenSource::Fallback
                }
            }
        },
        anchor_index: anchor.map(|anchor| anchor.index),
        tail_tokens: counted.tokens,
    }
}

/// `contentAdaptiveEstimate` 转发（模块内尾部降级路径）。
pub fn fallback_adaptive(text: &str) -> usize {
    content_adaptive_estimate(text)
}
