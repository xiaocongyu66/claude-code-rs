//! `services/compaction2/reducers/prune.rs` — 年龄裁剪与 supersede/useless
//! 工具输出修剪。
//!
//! oh-my-pi `compaction/pruning.ts`（`pruneToolOutputs`）的 CCH 端口：
//! 工具结果以 `tool_result` 块存于 user 消息，一条消息可携带多个。
//!
//! 纯实现：输入消息从不原地修改；重写的消息是浅拷贝，uuid/顺序/非文本
//! 块/tool_use-tool_result 配对保持不变。token 计数注入。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::reducers::tool_protection::{
    collect_tool_uses_by_id, get_tool_result_entries, get_tool_result_text, get_tool_name,
    is_protected_tool_result, is_skill_read_tool_result, replace_tool_result_text,
    ProtectedToolMatcher, ToolUseBlock,
};

/// 文本 token 估算器（如 len/4 或 tokenizer 适配器）。
pub type CountTokens<'a> = &'a dyn Fn(&str) -> usize;

/// TS `SupersedeKeyFn(toolName, args) -> string | undefined`。
pub type SupersedeKeyFn =
    Box<dyn Fn(&str, &HashMap<String, Value>) -> Option<String> + Send + Sync>;

/// TS `SupersedeCompleteFn(toolResult, message) -> boolean`。
pub type SupersedeCompleteFn =
    Box<dyn Fn(&Value, &Message) -> bool + Send + Sync>;

/// TS `PruneConfig`。
pub struct PruneConfig {
    /// 保留最近这么多 token 的上下文不动。
    pub protect_tokens: usize,
    /// 总估算节约低于该阈值时不修剪。
    pub minimum_savings: usize,
    /// 工具结果保护匹配器。
    pub protected_tools: Vec<ProtectedToolMatcher>,
    /// 可选的 supersede key 函数；启用陈旧重读修剪。
    pub supersede_key: Option<SupersedeKeyFn>,
    pub supersede_complete: Option<SupersedeCompleteFn>,
    /// useless 标记的结果绕过保护窗口。默认 true。
    pub prune_useless: Option<bool>,
    /// prompt-cache 护卫：全消息后缀超过该 token 的结果位于已发送的
    /// 热前缀，不动。
    pub cache_warm_suffix_tokens: Option<usize>,
    /// 可选的 prompt-cache 块位置回看上限。保守下界：位于该位置及以下
    /// 的结果不重写。undefined = 无回看限制。
    pub cache_lookback_positions: Option<usize>,
}

/// TS `DEFAULT_PRUNE_CONFIG`。
pub fn default_prune_config() -> PruneConfig {
    PruneConfig {
        protect_tokens: 40_000,
        minimum_savings: 20_000,
        protected_tools: vec![
            ProtectedToolMatcher::ToolName("skill".to_string()),
            ProtectedToolMatcher::Predicate(Box::new(|context| {
                is_skill_read_tool_result(context)
            })),
        ],
        supersede_key: Some(Box::new(read_tool_supersede_key)),
        supersede_complete: None,
        prune_useless: Some(true),
        cache_warm_suffix_tokens: None,
        cache_lookback_positions: None,
    }
}

/// TS `SUPERSEDED_NOTICE`。
pub const SUPERSEDED_NOTICE: &str = "[Superseded by a newer read of this file]";

/// TS `USELESS_NOTICE`。
pub const USELESS_NOTICE: &str = "[Uneventful result elided]";

/// 通用年龄裁剪下限：把不足该阈值的结果抹成
/// `[Output truncated - N tokens]` 收不回成本（占位符自身要花 token），
/// 只会让上下文变大。
pub const MIN_PRUNE_TOKENS: usize = 50;

/// 为下一个请求自己的输入保留的块位置数。
const RESERVED_LOOKBACK_POSITIONS: usize = 6;

/// TS `PrunedToolResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedToolResult {
    pub message_index: usize,
    pub block_index: usize,
    pub tool_use_id: String,
    pub notice: String,
    /// 该结果估算节约的 token。
    pub tokens_saved: usize,
}

/// TS `PruneResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct PruneResult {
    /// 新消息数组；未改动的条目按引用复用（Rust 侧 clone，原输入不变）。
    pub messages: Vec<Message>,
    pub pruned_count: usize,
    pub tokens_saved: usize,
    pub pruned: Vec<PrunedToolResult>,
}

#[derive(Debug, Clone)]
struct Candidate {
    message_index: usize,
    block_index: usize,
    tool_use_id: String,
    content: Value,
    tokens: usize,
    notice: String,
}

fn empty_result(messages: &[Message]) -> PruneResult {
    PruneResult {
        messages: messages.to_vec(),
        pruned_count: 0,
        tokens_saved: 0,
        pruned: Vec::new(),
    }
}

fn candidate_key(message_index: usize, block_index: usize) -> (usize, usize) {
    (message_index, block_index)
}

/// 拼接消息文本（仅用于 token 估算）。
fn message_text(message: &Message) -> String {
    let Some(content) = message.content() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    match content {
        Content::Text(text) => parts.push(text.clone()),
        Content::Blocks(blocks) => {
            for block in blocks {
                let typ = block.get("type").and_then(Value::as_str);
                if typ == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                } else if typ == Some("tool_result") {
                    if let Some(text) = tool_result_block_text(block) {
                        parts.push(text);
                    }
                } else if typ == Some("tool_use") {
                    if let Some(input) = block.get("input") {
                        parts.push(serde_json::to_string(input).unwrap_or_default());
                    }
                }
            }
        }
    }
    parts.join("\n")
}

fn tool_result_block_text(block: &Value) -> Option<String> {
    let content = block.get("content")?;
    match content {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(items) => {
            let fragments: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text") {
                        item.get("text").and_then(Value::as_str).map(String::from)
                    } else {
                        None
                    }
                })
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

/// TS `estimateMessageTokens(message, countTokens)`。
pub fn estimate_message_tokens(message: &Message, count_tokens: CountTokens<'_>) -> usize {
    count_tokens(&message_text(message))
}

fn create_pruned_notice(tokens: usize) -> String {
    format!("[Output truncated - {tokens} tokens]")
}

fn estimate_savings(tokens: usize, notice: &str, count_tokens: CountTokens<'_>) -> usize {
    tokens.saturating_sub(count_tokens(notice))
}

/// 在给定块位置回看下，下一个请求仍能命中 prompt cache 的最老可重写
/// 消息下标。保守：连续 `tool_use`/`tool_result` 块算一个位置。
/// 无界返回 0。
fn cache_lookback_floor(messages: &[Message], lookback: Option<usize>) -> usize {
    let Some(lookback) = lookback else {
        return 0;
    };
    let mut positions = RESERVED_LOOKBACK_POSITIONS;
    let mut later: Option<&str> = None;
    let mut floor = messages.len();
    for i in (0..messages.len()).rev() {
        let content = messages[i].content();
        match content {
            Some(Content::Text(text)) => {
                if !text.trim().is_empty() {
                    positions += 1;
                    later = None;
                }
            }
            Some(Content::Blocks(blocks)) => {
                for block in blocks.iter().rev() {
                    let typ = block.get("type").and_then(Value::as_str);
                    let kind = match typ {
                        Some("tool_use") => Some("tool_use"),
                        Some("tool_result") => Some("tool_result"),
                        _ => None,
                    };
                    if kind != later {
                        positions += 1;
                    }
                    later = kind;
                }
            }
            None => {}
        }
        if positions >= lookback {
            return floor;
        }
        floor = i;
    }
    0
}

/// 收集被更新成功读取代偿的结果。最新 → 最旧遍历，因为代偿只依赖
/// 更新的结果。
fn collect_superseded(
    messages: &[Message],
    tool_uses: &HashMap<String, ToolUseBlock>,
    config: &PruneConfig,
    count_tokens: CountTokens<'_>,
    pruned_positions: &HashSet<(usize, usize)>,
) -> Vec<Candidate> {
    let Some(supersede_key) = config.supersede_key.as_ref() else {
        return Vec::new();
    };
    let mut newer_by_key: HashMap<String, (bool, bool)> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    for i in (0..messages.len()).rev() {
        let entries = get_tool_result_entries(&messages[i]);
        for (block, index) in entries.into_iter().rev() {
            if pruned_positions.contains(&(i, index)) || block.pruned {
                continue;
            }
            let tool_call = tool_uses.get(&block.tool_use_id);
            if is_protected_tool_result(&block, tool_call, &config.protected_tools) {
                continue;
            }
            let Some(name) = get_tool_name(&block, tool_call) else {
                continue;
            };
            let key = supersede_key(&name, &tool_call.map(|c| c.input.clone()).unwrap_or_default());
            let Some(key) = key else {
                continue;
            };

            let prior = newer_by_key.get(&key).cloned();
            let is_error = block.is_error.unwrap_or(false);
            let superseded = if is_error {
                prior.is_some()
            } else {
                prior.map(|(success, _)| success).unwrap_or(false)
            };

            if !is_error {
                if !prior.map(|(success, _)| success).unwrap_or(false) {
                    let complete = match &config.supersede_complete {
                        Some(supersede_complete) => {
                            supersede_complete(&block.content.clone().unwrap_or(Value::Null), &messages[i])
                        }
                        None => true,
                    };
                    newer_by_key.insert(key, (true, complete));
                }
            } else if prior.is_none() {
                newer_by_key.insert(key, (false, false));
            }

            if !superseded {
                continue;
            }
            let tokens = get_tool_result_text(&block)
                .map(|text| count_tokens(&text))
                .unwrap_or(0);
            candidates.push(Candidate {
                message_index: i,
                block_index: index,
                tool_use_id: block.tool_use_id.clone(),
                content: block.content.clone().unwrap_or(Value::Null),
                tokens,
                notice: SUPERSEDED_NOTICE.to_string(),
            });
        }
    }
    candidates.reverse();
    candidates
}

/// 收集工具标记为"上下文无用"的结果。
fn collect_useless(
    messages: &[Message],
    tool_uses: &HashMap<String, ToolUseBlock>,
    config: &PruneConfig,
    count_tokens: CountTokens<'_>,
    exclude: &HashSet<(usize, usize)>,
) -> Vec<Candidate> {
    if config.prune_useless == Some(false) {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        for (block, index) in get_tool_result_entries(message) {
            if block.useless != Some(true) {
                continue;
            }
            if block.pruned || block.is_error == Some(true) {
                continue;
            }
            if exclude.contains(&(i, index)) {
                continue;
            }
            let tool_call = tool_uses.get(&block.tool_use_id);
            if is_protected_tool_result(&block, tool_call, &config.protected_tools) {
                continue;
            }
            let tokens = get_tool_result_text(&block)
                .map(|text| count_tokens(&text))
                .unwrap_or(0);
            if estimate_savings(tokens, USELESS_NOTICE, count_tokens) == 0 {
                continue;
            }
            candidates.push(Candidate {
                message_index: i,
                block_index: index,
                tool_use_id: block.tool_use_id.clone(),
                content: block.content.clone().unwrap_or(Value::Null),
                tokens,
                notice: USELESS_NOTICE.to_string(),
            });
        }
    }
    candidates
}

/// 收集年龄超保护窗口且体量值得压缩的结果；同时报告可选缓存护卫
/// 隐含的热缓存下界。
fn collect_aged(
    messages: &[Message],
    tool_uses: &HashMap<String, ToolUseBlock>,
    config: &PruneConfig,
    count_tokens: CountTokens<'_>,
) -> (Vec<Candidate>, usize) {
    let lookback_floor = cache_lookback_floor(messages, config.cache_lookback_positions);
    let mut candidates = Vec::new();
    let mut accumulated = 0usize;
    let mut warm_floor = 0usize;

    for i in (0..messages.len()).rev() {
        if let Some(cache_warm_suffix_tokens) = config.cache_warm_suffix_tokens {
            if accumulated > cache_warm_suffix_tokens || i < lookback_floor {
                warm_floor = i + 1;
                break;
            }
        }
        accumulated += estimate_message_tokens(&messages[i], count_tokens);

        for (block, index) in get_tool_result_entries(&messages[i]) {
            if block.pruned {
                continue;
            }
            if accumulated < config.protect_tokens {
                continue;
            }
            let tokens = get_tool_result_text(&block)
                .map(|text| count_tokens(&text))
                .unwrap_or(0);
            if tokens < MIN_PRUNE_TOKENS {
                continue;
            }
            let tool_call = tool_uses.get(&block.tool_use_id);
            if is_protected_tool_result(&block, tool_call, &config.protected_tools) {
                continue;
            }
            candidates.push(Candidate {
                message_index: i,
                block_index: index,
                tool_use_id: block.tool_use_id.clone(),
                content: block.content.clone().unwrap_or(Value::Null),
                tokens,
                notice: create_pruned_notice(tokens),
            });
        }
    }
    (candidates, warm_floor)
}

fn apply_candidates(
    messages: &[Message],
    candidates: &[Candidate],
    count_tokens: CountTokens<'_>,
) -> PruneResult {
    let mut by_message: HashMap<usize, HashMap<usize, (String, Value)>> = HashMap::new();
    let mut pruned: Vec<PrunedToolResult> = Vec::new();
    let mut tokens_saved = 0usize;

    for candidate in candidates {
        let dummy_block = crate::services::compaction2::reducers::tool_protection::ToolResultBlock {
            tool_use_id: candidate.tool_use_id.clone(),
            content: Some(candidate.content.clone()),
            is_error: None,
            useless: None,
            pruned: true,
            tool_name: None,
        };
        let replacement_content =
            replace_tool_result_text(&dummy_block, &candidate.notice, Some(&candidate.content));
        let blocks = by_message
            .entry(candidate.message_index)
            .or_default();
        blocks.insert(candidate.block_index, (candidate.notice.clone(), replacement_content));
        let saved = estimate_savings(candidate.tokens, &candidate.notice, count_tokens);
        tokens_saved += saved;
        pruned.push(PrunedToolResult {
            message_index: candidate.message_index,
            block_index: candidate.block_index,
            tool_use_id: candidate.tool_use_id.clone(),
            notice: candidate.notice.clone(),
            tokens_saved: saved,
        });
    }

    let next: Vec<Message> = messages
        .iter()
        .enumerate()
        .map(|(message_index, message)| {
            let Some(replacements) = by_message.get(&message_index) else {
                return message.clone();
            };
            let Message::User(user) = message else {
                return message.clone();
            };
            let Content::Blocks(blocks) = &user.message.content else {
                return message.clone();
            };
            let mut next_content: Vec<Value> = Vec::with_capacity(blocks.len());
            for (block_index, block) in blocks.iter().enumerate() {
                if let Some((_notice, replacement)) = replacements.get(&block_index) {
                    let mut replaced = block.clone();
                    replaced["content"] = replacement.clone();
                    // 带外"已清空"标记不落 wire（对齐 TS WeakSet 语义）：
                    // 已清空位置由 PruneResult.pruned 的 (message_index,
                    // block_index) 记录，后续修剪轮据此跳过。
                    next_content.push(replaced);
                } else {
                    next_content.push(block.clone());
                }
            }
            Message::User(crate::services::compact::UserMessage {
                message: crate::services::compact::UserMessageBody {
                    role: user.message.role.clone(),
                    content: Content::Blocks(next_content),
                },
                ..user.clone()
            })
        })
        .collect();

    PruneResult {
        messages: next,
        pruned_count: pruned.len(),
        tokens_saved,
        pruned,
    }
}

/// TS `pruneToolOutputs(messages, countTokens, config?)`。
///
/// 优先级：被代偿的重读（同一 path 更新的成功读取）与 useless 标记的
/// 结果不分年龄一律修剪；其余结果只在掉出 `protect_tokens` 窗口后才
/// 修剪。总节约低于 `minimum_savings` 时整轮跳过。
///
/// 被重写的 `tool_result` 块用带外标记 `_pruned` 记录（不会被消息
/// 转换器复制进 provider 请求——序列化时剥掉），非文本块保留。
pub fn prune_tool_outputs(
    messages: &[Message],
    count_tokens: CountTokens<'_>,
    config: Option<&PruneConfig>,
) -> PruneResult {
    let default_config;
    let config = match config {
        Some(config) => config,
        None => {
            default_config = default_prune_config();
            &default_config
        }
    };
    if messages.is_empty() {
        return empty_result(messages);
    }

    let tool_uses = collect_tool_uses_by_id(messages);
    let superseded = collect_superseded(messages, &tool_uses, config, count_tokens, &HashSet::new());
    let superseded_keys: HashSet<(usize, usize)> = supersended
        .iter()
        .map(|candidate| candidate_key(candidate.message_index, candidate.block_index))
        .collect();
    let useless = collect_useless(messages, &tool_uses, config, count_tokens, &superseded_keys);
    let (aged, warm_floor) = collect_aged(messages, &tool_uses, config, count_tokens);

    let mut merged: std::collections::BTreeMap<(usize, usize), Candidate> =
        std::collections::BTreeMap::new();
    // 缓存护卫作用于每一类受害者：更深的、仍热的结果留给
    // compaction/shake，它们反正会重建缓存。
    for candidate in superseded {
        if candidate.message_index >= warm_floor {
            merged.insert(
                candidate_key(candidate.message_index, candidate.block_index),
                candidate,
            );
        }
    }
    for candidate in useless {
        if candidate.message_index < warm_floor {
            continue;
        }
        merged
            .entry(candidate_key(candidate.message_index, candidate.block_index))
            .or_insert(candidate);
    }
    for candidate in aged {
        merged
            .entry(candidate_key(candidate.message_index, candidate.block_index))
            .or_insert(candidate);
    }

    let candidates: Vec<Candidate> = merged.into_values().collect();
    if candidates.is_empty() {
        return empty_result(messages);
    }

    let tokens_saved: usize = candidates
        .iter()
        .map(|candidate| estimate_savings(candidate.tokens, &candidate.notice, count_tokens))
        .sum();
    if tokens_saved < config.minimum_savings {
        return empty_result(messages);
    }

    apply_candidates(messages, &candidates, count_tokens)
}

/// TS `readToolSupersedeKey(toolName, args)`：CCH `Read` 工具的代偿键
/// ——目标路径（CCH 用 `file_path`）。内部/URL scheme 路径
/// （`skill://`、`artifact://`、`https://`）豁免。同 path 的后读代偿
/// 前读，无论 offset/limit——更新的视图权威。
pub fn read_tool_supersede_key(
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Option<String> {
    if tool_name.to_ascii_lowercase() != "read" {
        return None;
    }
    let path = args
        .get("file_path")
        .or_else(|| args.get("path"))
        .and_then(Value::as_str)?;
    if path.is_empty() || path.contains("://") {
        return None;
    }
    Some(path.to_string())
}
