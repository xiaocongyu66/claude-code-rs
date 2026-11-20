//! `services/compaction2/triggers/index.ts` — 模块根。

pub mod idle;
pub mod incomplete;
pub mod r#loop;
pub mod manual;
pub mod mid_turn;
pub mod overflow;
pub mod promotion;
pub mod speculation;
pub mod threshold;
pub mod types;

pub use idle::*;
pub use incomplete::*;
pub use r#loop::*;
pub use manual::*;
pub use mid_turn::*;
pub use overflow::*;
pub use promotion::*;
pub use speculation::*;
pub use threshold::*;
pub use types::*;
