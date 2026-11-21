//! `services/compaction2/reducers/shake.rs` — 外科式本地压缩（"shake"）。
//!
//! oh-my-pi `compaction/shake.ts` 的 CCH 端口：把重型工具结果文本与大
//! 围栏/XML 块替换为短占位符，保留非文本 tool_result 内容（图片/文档）。
//! artifact 卸载由调用方通过 `offload(region) => String` 注入；本模块
//! 从不碰磁盘或网络。纯实现：输入消息不原地修改。

use std::collections::HashMap;

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::reducers::prune::{estimate_message_tokens, CountTokens};
use crate::services::compaction2::reducers::tool_protection::{
    collect_tool_uses_by_id, get_tool_name, get_tool_result_entries, get_tool_result_text,
    is_artifact_recovery_tool_result, is_skill_read_tool_result, is_protected_tool_result,
    replace_tool_result_text, ToolResultBlock,
};

/// TS `ShakeConfig`。
pub struct ShakeConfig {
    /// 保留最近这么多 token 的上下文不动。
    pub protect_tokens: usize,
    /// 估算总节约低于该阈值时不 shake。
    pub min_savings: usize,
    /// 工具结果保护匹配器。
    pub protected_tools: Vec<crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher>,
    /// 围栏/XML 块参与裁剪的最小 token 体量。
    pub fence_min_tokens: usize,
}

/// 自动 shake 配置：保护活跃尾部、保守阈值。
pub fn default_shake_config() -> ShakeConfig {
    ShakeConfig {
        protect_tokens: 16_000,
        min_savings: 4_000,
        protected_tools: vec![
            crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::ToolName(
                "skill".to_string(),
            ),
            crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::Predicate(
                Box::new(|context| is_skill_read_tool_result(context)),
            ),
            crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::Predicate(
                Box::new(|context| is_artifact_recovery_tool_result(context)),
            ),
        ],
        fence_min_tokens: 400,
    }
}

/// 手动 `/shake`：激进——无节约阈值、横跨历史扫 Eligible 区域、包含
/// artifact 恢复读取。
pub fn aggressive_shake_config() -> ShakeConfig {
    ShakeConfig {
        protect_tokens: 4_000,
        min_savings: 0,
        protected_tools: vec![
            crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::ToolName(
                "skill".to_string(),
            ),
            crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::Predicate(
                Box::new(|context| is_skill_read_tool_result(context)),
            ),
        ],
        fence_min_tokens: 400,
    }
}

/// 压缩死路救援：激进取舍面，但保留 artifact 恢复读取。保护窗口归零，
/// 连最新的超大结果也能被抹掉。
pub fn rescue_shake_config() -> ShakeConfig {
    let mut config = aggressive_shake_config();
    config.protect_tokens = 0;
    config.protected_tools.push(
        crate::services::compaction2::reducers::tool_protection::ProtectedToolMatcher::Predicate(
            Box::new(|context| is_artifact_recovery_tool_result(context)),
        ),
    );
    config
}

/// 占位符行的粗略 token 成本；仅用于节约阈值。
const PLACEHOLDER_TOKEN_ESTIMATE: usize = 16;

/// TS `ToolResultShakeRegion`。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultShakeRegion {
    pub message_index: usize,
    pub block_index: usize,
    pub tool_use_id: String,
    /// 可移除文本的估算 token；保留的图片不计。
    pub tokens: usize,
    pub original_text: String,
    /// offload 文档的可读标签（工具名）。
    pub label: String,
}

/// TS `BlockShakeRegion`。
#[derive(Debug, Clone, PartialEq)]
pub struct BlockShakeRegion {
    pub message_index: usize,
    /// 内容数组下标；字符串内容为 usize::MAX。
    pub block_index: isize,
    /// 目标文本的字符区间 [start, end)。
    pub start: usize,
    pub end: usize,
    pub tokens: usize,
    pub original_text: String,
    /// offload 文档的可读标签（消息角色）。
    pub label: String,
}

/// TS `ShakeRegion`。
#[derive(Debug, Clone, PartialEq)]
pub enum ShakeRegion {
    ToolResult(ToolResultShakeRegion),
    Block(BlockShakeRegion),
}

/// TS `AppliedShakeRegion`。
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedShakeRegion {
    pub region: ShakeRegion,
    pub replacement: String,
    pub replacement_tokens: usize,
}

/// TS `ShakeResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct ShakeResult {
    /// 新消息数组；未触及条目按引用复用（Rust 侧 clone）。
    pub messages: Vec<Message>,
    /// 被替换的全部区域，按应用顺序。
    pub regions: Vec<AppliedShakeRegion>,
    pub tokens_saved: usize,
}

// 镜像 prompt 渲染：只认小写标签名——刻意保守（大写/混合大小写标签忽略）。
const OPENING_XML: &str = r"^<([a-z_-]+)(?:\s+[^>]*)?>$";
const CLOSING_XML: &str = r"^</([a-z_-]+)>$";

/// TS `scanTextForBlockRanges(text)`：定位围栏代码块与顶层 XML 元素区间。
/// 返回字符区间 [start, end)，覆盖完整块（含围栏/标签行，不含尾换行）。
/// 未闭合围栏/标签不出区间；围栏内抑制 XML 检测。
fn scan_text_for_block_ranges(text: &str) -> Vec<(usize, usize)> {
    let opening = regex::Regex::new(OPENING_XML).unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    let closing = regex::Regex::new(CLOSING_XML).unwrap_or_else(|_| regex::Regex::new("$^").unwrap());

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut in_fence = false;
    let mut fence_start: usize = 0;
    let mut tag_stack: Vec<String> = Vec::new();
    let mut xml_start: Option<usize> = None;

    let mut line_start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i <= bytes.len() {
        if i != bytes.len() && bytes[i] != b'\n' {
            i += 1;
            continue;
        }
        let line = &text[line_start..i];
        let line_end = i;
        let trimmed_start = line.trim_start();
        let leading = line.len() - trimmed_start.len();

        let is_fence_line =
            trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~");
        if is_fence_line {
            if !in_fence {
                in_fence = true;
                fence_start = line_start;
            } else {
                in_fence = false;
                ranges.push((fence_start, line_end));
                fence_start = 0;
            }
            line_start = i + 1;
            i += 1;
            continue;
        }

        if !in_fence {
            let is_opening_xml = leading == 0 && opening.is_match(trimmed_start);
            if is_opening_xml {
                if let Some(captures) = opening.captures(trimmed_start) {
                    if tag_stack.is_empty() {
                        xml_start = Some(line_start);
                    }
                    tag_stack.push(captures.get(1).map(|m| m.as_str().to_string()).unwrap_or_default());
                }
            } else if let Some(captures) = closing.captures(trimmed_start) {
                let tag = captures.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
                if tag_stack.last().map(|t| t == &tag).unwrap_or(false) {
                    tag_stack.pop();
                    if tag_stack.is_empty() {
                        if let Some(start) = xml_start {
                            ranges.push((start, line_end));
                            xml_start = None;
                        }
                    }
                }
            }
        }

        line_start = i + 1;
        i += 1;
    }

    merge_ranges(ranges)
}

/// 升序排序并丢弃与已保留区间重叠的区间。围栏/XML 区间正确嵌套，
/// 重叠即包含，保留起点更早的区间即保留最外层。
fn merge_ranges(ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.len() <= 1 {
        return ranges;
    }
    let mut sorted = ranges;
    sorted.sort_by_key(|range| range.0);
    let mut kept: Vec<(usize, usize)> = Vec::new();
    let mut last_end: usize = 0;
    for range in sorted {
        if range.0 < last_end {
            continue;
        }
        kept.push(range);
        last_end = range.1;
    }
    kept
}

fn push_block_regions(
    regions: &mut Vec<ShakeRegion>,
    message_index: usize,
    block_index: isize,
    text: &str,
    count_tokens: CountTokens<'_>,
    config: &ShakeConfig,
    label: &str,
) {
    for (start, end) in scan_text_for_block_ranges(text) {
        let slice = &text[start..end];
        if slice.is_empty() {
            continue;
        }
        let tokens = count_tokens(slice);
        if tokens < config.fence_min_tokens {
            continue;
        }
        regions.push(ShakeRegion::Block(BlockShakeRegion {
            message_index,
            block_index,
            start,
            end,
            tokens,
            original_text: slice.to_string(),
            label: label.to_string(),
        }));
    }
}

/// TS `collectShakeRegions(messages, countTokens, config)`：纯检测。
///
/// 走保护最近窗口（最近 `protect_tokens` 上下文不动），收集工具结果文本
/// （遵守 `protected_tools`、跳过已修剪结果）与 user/assistant 文本中的
/// 大围栏/XML 块。useless 标记的工具结果绕过保护窗口。
///
/// `tool_use` 块永不触及（配对保持），区域从不跨越消息边界。合并估算
/// 节约低于 `min_savings` 时返回空。
pub fn collect_shake_regions(
    messages: &[Message],
    count_tokens: CountTokens<'_>,
    config: &ShakeConfig,
) -> Vec<ShakeRegion> {
    let n = messages.len();
    if n == 0 {
        return Vec::new();
    }

    // 严格晚于下标 i 的全部消息 token。
    let mut accumulated_after = vec![0usize; n];
    let mut accumulated = 0usize;
    for i in (0..n).rev() {
        accumulated_after[i] = accumulated;
        accumulated += estimate_message_tokens(&messages[i], count_tokens);
    }

    let tool_uses = collect_tool_uses_by_id(messages);
    let mut regions: Vec<ShakeRegion> = Vec::new();

    for i in 0..n {
        let message = &messages[i];
        let results = get_result_entries(message);
        // useless 标记结果消费后无信息；即使在保护窗口内也可裁。
        let has_useless = results.iter().any(|(block, _)| {
            block.useless == Some(true) && block.is_error != Some(true)
        });
        if !has_useless && accumulated_after[i] < config.protect_tokens {
            continue;
        }

        if !results.is_empty() {
            for (block, index) in results {
                if block.pruned {
                    continue;
                }
                let tool_call = tool_uses.get(&block.tool_use_id);
                if is_protected_tool_result(&block, tool_call, &config.protected_tools) {
                    continue;
                }
                let Some(text) = get_tool_result_text(&block) else {
                    continue;
                };
                regions.push(ShakeRegion::ToolResult(ToolResultShakeRegion {
                    message_index: i,
                    block_index: index,
                    tool_use_id: block.tool_use_id.clone(),
                    tokens: count_tokens(&text),
                    original_text: text,
                    label: get_tool_name(&block, tool_call)
                        .unwrap_or_else(|| "tool".to_string()),
                }));
            }
            // 工具结果消息不再扫围栏块（结果文本整体已是一个区域）。
            continue;
        }

        let typ = message.typ();
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let Some(content) = message.content() else {
            continue;
        };
        match content {
            Content::Text(text) => {
                push_block_regions(&mut regions, i, -1, text, count_tokens, config, typ);
            }
            Content::Blocks(blocks) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            push_block_regions(
                                &mut regions,
                                i,
                                block_index as isize,
                                text,
                                count_tokens,
                                config,
                                typ,
                            );
                        }
                    }
                }
            }
        }
    }

    let savings: usize = regions
        .iter()
        .map(|region| {
            let tokens = match region {
                ShakeRegion::ToolResult(region) => region.tokens,
                ShakeRegion::Block(region) => region.tokens,
            };
            tokens.saturating_sub(PLACEHOLDER_TOKEN_ESTIMATE)
        })
        .sum();
    if savings < config.min_savings {
        return Vec::new();
    }
    regions
}

fn get_result_entries(message: &Message) -> Vec<(ToolResultBlock, usize)> {
    get_tool_result_entries(message)
}

/// 确保工作副本存在（保持 TS 浅拷贝语义：消息整体克隆，原输入不动）。
fn ensure_work_copy(
    working: &mut HashMap<usize, Message>,
    messages: &[Message],
    index: usize,
) -> bool {
    if working.contains_key(&index) {
        return true;
    }
    match messages.get(index) {
        Some(message) => {
            working.insert(index, message.clone());
            true
        }
        None => false,
    }
}

fn splice_text(text: &str, region: &BlockShakeRegion, replacement: &str) -> String {
    let start = region.start.min(text.len());
    let end = region.end.min(text.len());
    let mut next = String::with_capacity(text.len());
    next.push_str(&text[..start]);
    next.push_str(replacement);
    next.push_str(&text[end..]);
    next
}

fn apply_one(
    working: &mut HashMap<usize, Message>,
    messages: &[Message],
    item: &AppliedShakeRegion,
) {
    let index = match &item.region {
        ShakeRegion::ToolResult(region) => region.message_index,
        ShakeRegion::Block(region) => region.message_index,
    };
    if !ensure_work_copy(working, messages, index) {
        return;
    }
    let Some(message) = working.get_mut(&index) else {
        return;
    };

    match &item.region {
        ShakeRegion::ToolResult(region) => {
            let Message::User(user) = message else {
                return;
            };
            let Content::Blocks(blocks) = &mut user.message.content else {
                return;
            };
            let Some(block) = blocks.get_mut(region.block_index) else {
                return;
            };
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return;
            }
            let view = ToolResultBlock {
                tool_use_id: region.tool_use_id.clone(),
                content: block.get("content").cloned(),
                is_error: None,
                useless: None,
                pruned: false,
                tool_name: None,
            };
            let replaced = replace_tool_result_text(&view, &item.replacement, block.get("content"));
            block["content"] = replaced;
        }
        ShakeRegion::Block(region) => {
            let content = match message {
                Message::User(user) => &mut user.message.content,
                Message::Assistant(assistant) => &mut assistant.message.content,
                _ => return,
            };
            if region.block_index < 0 {
                let Content::Text(text) = content else {
                    return;
                };
                *text = splice_text(text, region, &item.replacement);
                return;
            }
            let Content::Blocks(blocks) = content else {
                return;
            };
            let Some(block) = blocks.get_mut(region.block_index as usize) else {
                return;
            };
            let Some(text) = block.get("text").and_then(Value::as_str).map(String::from) else {
                return;
            };
            block["text"] = Value::String(splice_text(&text, region, &item.replacement));
        }
    }
}

/// TS `applyShakeRegions(messages, items)`：一次性应用多个区域。
/// 块区域按 start 最高优先应用，同文本块内拼接不会错位；工具结果区域
/// 相互独立。输入不原地修改。
pub fn apply_shake_regions(
    messages: &[Message],
    items: &[AppliedShakeRegion],
) -> Vec<Message> {
    if items.is_empty() {
        return messages.to_vec();
    }
    let mut working: HashMap<usize, Message> = HashMap::new();
    let mut ordered: Vec<&AppliedShakeRegion> = items.iter().collect();
    ordered.sort_by(|a, b| {
        let a_start = match &a.region {
            ShakeRegion::ToolResult(_) => -1isize,
            ShakeRegion::Block(region) => region.start as isize,
        };
        let b_start = match &b.region {
            ShakeRegion::ToolResult(_) => -1isize,
            ShakeRegion::Block(region) => region.start as isize,
        };
        b_start.cmp(&a_start)
    });
    for item in ordered {
        apply_one(&mut working, messages, item);
    }
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            working.remove(&index).unwrap_or_else(|| message.clone())
        })
        .collect()
}

/// TS `shakeMessages(messages, countTokens, config, offload)`：检测、
/// 卸载并替换 eligible 区域。
///
/// `offload` 收到每个区域并返回替换文本（例如调用方铸造的
/// `artifact://` 引用）。此处不做任何持久化。返回重写后的消息、已应用
/// 区域与真实节约 token（`region.tokens - replacement tokens`，下界 0）。
pub async fn shake_messages(
    messages: &[Message],
    count_tokens: impl Fn(&str) -> usize,
    config: &ShakeConfig,
    offload: impl Fn(&ShakeRegion) -> Option<String>,
) -> ShakeResult {
    let regions = collect_shake_regions(messages, &count_tokens, config);
    if regions.is_empty() {
        return ShakeResult {
            messages: messages.to_vec(),
            regions: Vec::new(),
            tokens_saved: 0,
        };
    }

    let mut applied: Vec<AppliedShakeRegion> = Vec::new();
    let mut tokens_saved = 0usize;
    for region in regions {
        // 卸载失败：保留原区域。用无目标的标记替换会丢数据并虚报节约。
        let Some(replacement) = offload(&region) else {
            continue;
        };
        let replacement_tokens = count_tokens(&replacement);
        let region_tokens = match &region {
            ShakeRegion::ToolResult(region) => region.tokens,
            ShakeRegion::Block(region) => region.tokens,
        };
        tokens_saved += region_tokens.saturating_sub(replacement_tokens);
        applied.push(AppliedShakeRegion {
            region,
            replacement,
            replacement_tokens,
        });
    }

    ShakeResult {
        messages: apply_shake_regions(messages, &applied),
        regions: applied,
        tokens_saved,
    }
}
