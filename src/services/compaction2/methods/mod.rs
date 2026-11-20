//! `services/compaction2/methods/` — 压缩方法链实现。
//!
//! 注：TS 的 `remote.ts` / `snapcompact.ts` 是重导出垫片，Rust 侧把
//! `remote/index.ts` 与 `snapcompact/index.ts` 的内容直接落在同名目录
//! 的 `mod.rs`（避免 `remote.rs` 与 `remote/mod.rs` 同时存在的 E0761）。

pub mod branch_summary;
pub mod handoff;
pub mod remote;
pub mod registry;
pub mod shake;
pub mod snapcompact;
pub mod soft;
