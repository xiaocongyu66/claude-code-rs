//! `services/compact/cachedMicrocompact.ts` — 缓存编辑型 microcompact 状态机。
//!
//! 语义对齐：保留 TS 的全部阈值常量与状态流转（注册 → 达到 trigger
//! threshold → 生成 cache_edits 块 → pin 到 user message 位置）。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// TS `CachedMCState`。
#[derive(Debug, Clone, Default)]
pub struct CachedMcState {
    pub registered_tools: HashSet<String>,
    pub tool_order: Vec<String>,
    pub deleted_refs: HashSet<String>,
    pub pinned_edits: Vec<PinnedCacheEdits>,
    pub tools_sent_to_api: bool,
}

/// TS `CacheEditsBlock`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEditsBlock {
    #[serde(rename = "type")]
    pub typ: String,
    pub edits: Vec<CacheEdit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEdit {
    #[serde(rename = "type")]
    pub typ: String,
    pub tool_use_id: String,
}

/// TS `PinnedCacheEdits`。
#[derive(Debug, Clone, PartialEq)]
pub struct PinnedCacheEdits {
    pub user_message_index: usize,
    pub block: CacheEditsBlock,
}

/// TS `TRIGGER_THRESHOLD = 10`。
pub const TRIGGER_THRESHOLD: usize = 10;
/// TS `KEEP_RECENT = 5`。
pub const KEEP_RECENT: usize = 5;

/// TS `isCachedMicrocompactEnabled()`：`CLAUDE_CACHED_MICROCOMPACT === '1'`。
pub fn is_cached_microcompact_enabled() -> bool {
    std::env::var("CLAUDE_CACHED_MICROCOMPACT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// TS `isModelSupportedForCacheEditing(model)`：`/claude-[a-z]+-4[-\d]/`。
pub fn is_model_supported_for_cache_editing(model: &str) -> bool {
    regex::Regex::new(r"claude-[a-z]+-4[-\d]")
        .map(|re| re.is_match(model))
        .unwrap_or(false)
}

/// TS `getCachedMCConfig()`。
pub fn get_cached_mc_config() -> (usize, usize) {
    (TRIGGER_THRESHOLD, KEEP_RECENT)
}

/// TS `createCachedMCState()`。
pub fn create_cached_mc_state() -> CachedMcState {
    CachedMcState::default()
}

/// TS `markToolsSentToAPI(state)`。
pub fn mark_tools_sent_to_api(state: &mut CachedMcState) {
    state.tools_sent_to_api = true;
}

/// TS `resetCachedMCState(state)`。
pub fn reset_cached_mc_state(state: &mut CachedMcState) {
    state.registered_tools.clear();
    state.tool_order.clear();
    state.deleted_refs.clear();
    state.pinned_edits.clear();
    state.tools_sent_to_api = false;
}

/// TS `registerToolResult(state, toolId)`：幂等注册并记录顺序。
pub fn register_tool_result(state: &mut CachedMcState, tool_id: &str) {
    if !state.registered_tools.contains(tool_id) {
        state.registered_tools.insert(tool_id.to_string());
        state.tool_order.push(tool_id.to_string());
    }
}

/// TS `registerToolMessage(state, groupIds)`。
pub fn register_tool_message(state: &mut CachedMcState, group_ids: &[String]) {
    for id in group_ids {
        register_tool_result(state, id);
    }
}

/// TS `getToolResultsToDelete(state)`： oldest-first 地返回需要删除的工具
/// id，使活跃数降到阈值以下（排除已删除的、保留最近 keepRecent 个）。
pub fn get_tool_results_to_delete(state: &CachedMcState) -> Vec<String> {
    let (trigger_threshold, keep_recent) = get_cached_mc_config();
    let active: Vec<&String> = state
        .tool_order
        .iter()
        .filter(|id| !state.deleted_refs.contains(*id))
        .collect();
    if active.len() <= trigger_threshold {
        return Vec::new();
    }
    let delete_count = active.len().saturating_sub(keep_recent);
    active[..delete_count].iter().map(|s| s.to_string()).collect()
}

/// TS `createCacheEditsBlock(state, toolIds)`：空列表返回 None。
pub fn create_cache_edits_block(_state: &CachedMcState, tool_ids: &[String]) -> Option<CacheEditsBlock> {
    if tool_ids.is_empty() {
        return None;
    }
    Some(CacheEditsBlock {
        typ: "cache_edits".to_string(),
        edits: tool_ids
            .iter()
            .map(|id| CacheEdit {
                typ: "delete_tool_result".to_string(),
                tool_use_id: id.clone(),
            })
            .collect(),
    })
}
