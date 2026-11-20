//! denial_tracking.rs — 权限分类器的拒绝计数基础设施。
//!
//! 重写自 `src/utils/permissions/denialTracking.ts`。追踪连续拒绝数与
//! 累计拒绝数，用于决定何时回退到提示用户。
//!
//! 语义对齐：TS 的 `recordSuccess` 在 `consecutiveDenials === 0` 时返回
//! 同一对象（无变更）；Rust 语义等价地直接返回自身克隆。

use serde::{Deserialize, Serialize};

/// 拒绝追踪状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialTrackingState {
    pub consecutive_denials: u32,
    pub total_denials: u32,
}

/// 触发回退提示的阈值（TS `DENIAL_LIMITS`，`as const` 对象）。
pub const DENIAL_LIMITS: DenialLimits = DenialLimits {
    max_consecutive: 3,
    max_total: 20,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenialLimits {
    pub max_consecutive: u32,
    pub max_total: u32,
}

pub fn create_denial_tracking_state() -> DenialTrackingState {
    DenialTrackingState {
        consecutive_denials: 0,
        total_denials: 0,
    }
}

pub fn record_denial(state: DenialTrackingState) -> DenialTrackingState {
    DenialTrackingState {
        consecutive_denials: state.consecutive_denials + 1,
        total_denials: state.total_denials + 1,
    }
}

pub fn record_success(state: DenialTrackingState) -> DenialTrackingState {
    if state.consecutive_denials == 0 {
        // No change needed
        return state;
    }
    DenialTrackingState {
        consecutive_denials: 0,
        total_denials: state.total_denials,
    }
}

pub fn should_fallback_to_prompting(state: DenialTrackingState) -> bool {
    state.consecutive_denials >= DENIAL_LIMITS.max_consecutive
        || state.total_denials >= DENIAL_LIMITS.max_total
}
