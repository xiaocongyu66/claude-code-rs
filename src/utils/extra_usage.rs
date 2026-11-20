//! extra_usage.rs — 判断某模型/模式是否按额外用量计费。
//!
//! 重写自 `src/utils/extraUsage.ts`。语义对齐：
//!
//! - 仅 Claude AI 订阅者（`isClaudeAISubscriber()`）适用。
//! - fast mode 永远按额外用量计费。
//! - 否则只有 1M 上下文模型中的 opus-4-6 / opus-4-7 / sonnet-4-6 适用；
//!   opus-4-6 在 `isOpus1mMerged` 合并计划下不再计费。

use crate::utils::auth::is_claude_ai_subscriber;
use crate::utils::context::has_1m_context;

/// 判断给定模型/模式是否按额外用量计费。
///
/// - `model` 为 None 时按非 1M 上下文处理。
/// - `model` 形如 `opus[1m]`/`sonnet-4-6[1m]`（可带 `[1m]` 后缀），匹配前
///   先转小写并去掉该后缀，再与 opus-4-6/opus-4-7/sonnet-4-6 系列比较。
pub fn is_billed_as_extra_usage(
    model: Option<&str>,
    is_fast_mode: bool,
    is_opus_1m_merged: bool,
) -> bool {
    if !is_claude_ai_subscriber() {
        return false;
    }
    if is_fast_mode {
        return true;
    }
    let model = match model {
        Some(m) if has_1m_context(m) => m,
        _ => return false,
    };

    let m = model.to_lowercase();
    let m = m.strip_suffix("[1m]").unwrap_or(&m).trim();
    let is_opus46 = m == "opus" || m.contains("opus-4-6") || m.contains("opus-4-7");
    let is_sonnet46 = m == "sonnet" || m.contains("sonnet-4-6");

    if is_opus46 && is_opus_1m_merged {
        return false;
    }

    is_opus46 || is_sonnet46
}
