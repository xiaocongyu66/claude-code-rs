//! `services/compaction2/triggers/loop.ts` — 循环级触发选择。
//!
//! query.ts 每轮迭代一次 provider 请求：阈值轮在迭代顶部（请求前）
//! 跑，中途轮在工具迭代结束（tool result 已知后）跑。两者否则会为同一
//! 请求各触发一次，因此用一个一次性 skip 标志把中途决定带进下一次阈值
//! 检查。本模块全部纯函数，无需 query 循环即可测试选择/去重规则。

use crate::services::compaction2::contracts::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS,
};
use crate::services::compaction2::triggers::incomplete::evaluate_incomplete_trigger;
use crate::services::compaction2::triggers::mid_turn::evaluate_mid_turn_trigger;
use crate::services::compaction2::triggers::{ThresholdInput, TriggerDecision};

/// TS `LoopCompactionState`。
#[derive(Debug, Clone, Default)]
pub struct LoopCompactionState {
    /// 中途轮已经覆盖了下一个 provider 请求。
    pub mid_turn_skip_pending: bool,
    /// 本轮已完成的不完整输出恢复。
    pub incomplete_attempted: bool,
}

/// TS `createLoopCompactionState()`。
pub fn create_loop_compaction_state() -> LoopCompactionState {
    LoopCompactionState::default()
}

/// TS `consumeMidTurnSkip(state)`：在循环顶部阈值位点消费一次性中途
/// 标志。每次 `mark_mid_turn_consumed` 恰好返回一次 true。
pub fn consume_mid_turn_skip(state: &mut LoopCompactionState) -> bool {
    let skip = state.mid_turn_skip_pending;
    state.mid_turn_skip_pending = false;
    skip
}

/// TS `markMidTurnConsumed(state)`：记录中途维护已先于下一个 provider
/// 请求跑过。
pub fn mark_mid_turn_consumed(state: &mut LoopCompactionState) {
    state.mid_turn_skip_pending = true;
}

/// TS `MidTurnAttemptInput`。
pub struct MidTurnAttemptInput<'a> {
    /// COMPACTION_V2 被编译出去时为 false。
    pub enabled: bool,
    /// 本轮迭代的阈值轮已经跑过（并压缩了）。
    pub threshold_attempted_this_iteration: bool,
    pub token_count: i64,
    pub window_tokens: i64,
    pub settings: Option<&'a CompactionSettings>,
}

/// TS `shouldAttemptMidTurn(input)`：仅当阈值轮没处理本轮迭代、且上下文
/// 超过阈值时才跑中途维护。`midTurnEnabled` 由触发器自身评估。
pub fn should_attempt_mid_turn(input: &MidTurnAttemptInput<'_>) -> bool {
    if !input.enabled || input.threshold_attempted_this_iteration {
        return false;
    }
    let default_settings = DEFAULT_COMPACTION_SETTINGS.clone();
    let settings = input.settings.unwrap_or(&default_settings);
    evaluate_mid_turn_trigger(&ThresholdInput {
        token_count: input.token_count,
        window_tokens: input.window_tokens,
        settings: settings.clone(),
    })
    .should_compact
}

/// TS `OverflowRecovery`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowRecovery {
    V2,
    Reactive,
    Surface,
}

/// TS `OverflowRecoveryInput`。
pub struct OverflowRecoveryInput {
    /// COMPACTION_V2 被编译出去时为 false。
    pub v2_enabled: bool,
    pub prompt_too_long: bool,
    pub reactive_available: bool,
    /// 该错误已经跑过一次恢复（V2 或 reactive）。
    pub already_attempted: bool,
}

/// TS `selectOverflowRecovery(input)`：V2 与 reactive 互斥——V2 开启时
/// 它拥有 413 路径；V2 失败/已跑过时直接上浮错误，不重复跑 reactive。
/// flag 关闭时选择恰为 legacy reactive 路径。
pub fn select_overflow_recovery(input: &OverflowRecoveryInput) -> OverflowRecovery {
    if !input.prompt_too_long {
        return OverflowRecovery::Surface;
    }
    if input.v2_enabled {
        return if input.already_attempted {
            OverflowRecovery::Surface
        } else {
            OverflowRecovery::V2
        };
    }
    if input.reactive_available {
        OverflowRecovery::Reactive
    } else {
        OverflowRecovery::Surface
    }
}

/// TS `IncompleteRecoveryInput`。
pub struct IncompleteRecoveryInput<'a> {
    /// COMPACTION_V2 被编译出去时为 false。
    pub v2_enabled: bool,
    pub stop_reason: Option<&'a str>,
    pub token_count: i64,
    pub window_tokens: i64,
    pub settings: Option<&'a CompactionSettings>,
    /// 本轮已完成的不完整输出恢复。
    pub already_attempted: bool,
}

/// TS `shouldAttemptIncompleteRecovery(input)`：仅 length/max_tokens 停
/// 止原因、上下文超阈值（或窗口未知）且本轮未恢复过时触发。
pub fn should_attempt_incomplete_recovery(input: &IncompleteRecoveryInput<'_>) -> bool {
    if !input.v2_enabled || input.already_attempted {
        return false;
    }
    let default_settings = DEFAULT_COMPACTION_SETTINGS.clone();
    let settings = input.settings.unwrap_or(&default_settings);
    evaluate_incomplete_trigger(
        &ThresholdInput {
            token_count: input.token_count,
            window_tokens: input.window_tokens,
            settings: settings.clone(),
        },
        input.stop_reason,
    )
    .should_compact
}

/// 触发决策简写（避免每处写全路径）。
pub type LoopTriggerDecision = TriggerDecision;
