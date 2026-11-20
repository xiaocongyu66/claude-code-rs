//! `services/compaction2/index.ts` — 模块根，重导出 contracts。
//!
//! 附加声明：子模块树（guards / methods / reducers / session /
//! speculation / tokens / triggers）在此声明，便于未来在
//! `services/mod.rs` 解除 `pub mod compaction2;` 挂载。

pub mod contracts;

pub use contracts::*;
