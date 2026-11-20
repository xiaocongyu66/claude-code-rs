//! crypto.rs — crypto 模块的间接入口（pkg browser 字段适配点）。
//!
//! 重写自 `src/utils/crypto.ts`。TS 版只是把 Node `crypto` 的
//! `randomUUID` 具名重导出（显式 import-then-export 以避免 bun
//! bytecode 链接问题）；Rust 侧转发到 `uuid` crate 的 v4 生成。

pub use crate::utils::random_uuid_string as random_uuid;

use uuid::Uuid;

/// 生成随机 UUID v4 字符串（对齐 TS `randomUUID()`）。
pub fn random_uuid_string() -> String {
    Uuid::new_v4().to_string()
}
