//! claude-code-rs — Claude Code, file-by-file rewritten in Rust.
//!
//! 目录与 TS 源树一一对应：`src/foo.ts` → `src/foo.rs`，每转换一个目录
//! 就在这里声明它的模块。入口快速路径见 `src/entrypoints/cli.rs`。

pub mod bootstrap;
pub mod constants;
pub mod entrypoints;
pub mod i18n;
pub mod types;
pub mod utils;
