//! `services/contextCollapse/index.ts` — 上下文折叠（CONTEXT_COLLAPSE）入口。
//!
//! 语义对齐说明：
//! - CCH 中 CONTEXT_COLLAPSE 是**故意只实现一半的存根**：消息级折叠存储
//!   （persist/drain/commit）从未实现，因此使能该 feature 会压制 auto
//!   compact 并撑爆上下文窗口（见 docs/reverse-284 + scripts/defines.ts）。
//! - 安全策略与 TS 一致：当 `FEATURE_CONTEXT_COLLAPSE` 打开时，每个公开
//!   入口 **panic 代替静默 no-op**，让 flag 永远不可能"意外开着"。
//!   （对齐 TS `assertContextCollapseNotEnabled()` 抛错语义；Rust 侧按
//!   crate 纪律用 `panic!` 表达"fail loudly"，因为调用方无法恢复。）
//! - 订阅/统计等返回值全部为零值存根，与 TS 逐字段对齐。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::services::compact::Message;

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// TS `ContextCollapseHealth`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextCollapseHealth {
    pub total_spawns: u64,
    pub total_errors: u64,
    pub last_error: Option<String>,
    pub empty_spawn_warning_emitted: bool,
    pub total_empty_spawns: u64,
}

/// TS `ContextCollapseStats`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextCollapseStats {
    pub collapsed_spans: u64,
    pub collapsed_messages: u64,
    pub staged_spans: u64,
    pub health: ContextCollapseHealth,
}

/// TS `CollapseResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct CollapseResult {
    pub messages: Vec<Message>,
}

/// TS `DrainResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct DrainResult {
    pub committed: u64,
    pub messages: Vec<Message>,
}

/// TS `getStats()` — 零值存根。
pub fn get_stats() -> ContextCollapseStats {
    ContextCollapseStats::default()
}

static ENABLED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// TS `isContextCollapseEnabled()`。
pub fn is_context_collapse_enabled() -> bool {
    *ENABLED.lock().unwrap_or(&false)
}

/// TS `subscribe()` — 返回取消订阅句柄（空实现：无存储层可订阅）。
pub fn subscribe(_callback: Arc<dyn Fn() + Send + Sync>) -> impl Fn() {
    // 无状态可通知；保持签名以便调用方在 feature 关闭时照常解绑。
    || {}
}

/// TS `assertContextCollapseNotEnabled()` — flag 开启时 fail loudly。
pub fn assert_context_collapse_not_enabled() {
    if feature("CONTEXT_COLLAPSE") {
        panic!(
            "CONTEXT_COLLAPSE is not implemented in this build: message collapse storage \
             (persist/drain/commit) is absent, so enabling the feature would suppress auto \
             compact and strand the context window. Remove FEATURE_CONTEXT_COLLAPSE=1 \
             (see scripts/defines.ts) or implement the storage layer first."
        );
    }
}

/// TS `applyCollapsesIfNeeded()`。
pub async fn apply_collapses_if_needed(
    messages: Vec<Message>,
    _tool_use_context: &crate::tool::ToolUseContext,
    _query_source: &crate::constants::query_source::QuerySource,
) -> CollapseResult {
    assert_context_collapse_not_enabled();
    CollapseResult { messages }
}

/// TS `isWithheldPromptTooLong()` — 恒 false 存根。
pub fn is_withheld_prompt_too_long(
    _message: &Message,
    _is_prompt_too_long_message: &(dyn Fn(&Message) -> bool + Sync),
    _query_source: &crate::constants::query_source::QuerySource,
) -> bool {
    false
}

/// TS `recoverFromOverflow()`。
pub fn recover_from_overflow(
    messages: Vec<Message>,
    _query_source: &crate::constants::query_source::QuerySource,
) -> DrainResult {
    DrainResult {
        committed: 0,
        messages,
    }
}

/// TS `resetContextCollapse()`。
pub fn reset_context_collapse() {
    if let Ok(mut guard) = ENABLED.lock() {
        *guard = false;
    }
}

/// TS `initContextCollapse()`。
pub fn init_context_collapse() {
    if let Ok(mut guard) = ENABLED.lock() {
        *guard = true;
    }
}
