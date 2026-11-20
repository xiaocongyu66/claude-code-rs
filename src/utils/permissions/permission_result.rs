//! permission_result.rs — 权限结果类型与行为描述。
//!
//! 重写自 `src/utils/permissions/PermissionResult.ts`。该 TS 文件本身
//! 只是 `src/types/permissions.ts`（已重写为 `src/types/permissions.rs`）
//! 的再出口加一个描述函数；Rust 侧以 `pub use` 对齐再出口语义。
//!
//! 语义对齐说明：
//! - `getRuleBehaviorDescription` 接收 `PermissionResult['behavior']`
//!   （"allow" | "deny" | "ask" | "passthrough"），Rust 侧以 `&str` 入参，
//!   逐分支对齐 switch（passthrough 落入 default 分支）。

pub use crate::types::permissions::{
    PermissionAllowDecision, PermissionAskDecision, PermissionDecision,
    PermissionDecisionReason, PermissionDenyDecision, PermissionMetadata, PermissionResult,
};

/// 返回规则行为的散文描述，用于 hook 日志等面向用户的文案。
///
/// - `"allow"` → `"allowed"`
/// - `"deny"` → `"denied"`
/// - 其它（`"ask"` / `"passthrough"`）→ `"asked for confirmation for"`
pub fn get_rule_behavior_description(behavior: &str) -> &'static str {
    match behavior {
        "allow" => "allowed",
        "deny" => "denied",
        _ => "asked for confirmation for",
    }
}
