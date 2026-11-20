//! `services/compaction2/reducers/index.ts` — 模块根。

pub mod prune;
pub mod shake;
pub mod shake_reducer;
pub mod tool_protection;

pub use prune::*;
pub use shake::*;
pub use tool_protection::*;
