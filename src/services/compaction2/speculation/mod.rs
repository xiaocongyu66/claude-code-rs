//! `services/compaction2/speculation/` — 异步投机压缩。
//!
//! 纯带区决策在 `triggers/speculation.rs`；本模块拥有围绕一次后台
//! 摘要运行的状态机（`manager.rs`）。

pub mod manager;

pub use manager::{
    create_speculation_manager, SpeculationConsumption, SpeculationContext, SpeculationManager,
    SpeculationManagerOptions, SpeculationResult, SpeculationRunner, SpeculationSnapshot,
    SpeculationState, DEFAULT_SPECULATION_TIMEOUT_MS,
};
