//! error_ids.rs — 生产环境错误来源追踪 ID。
//!
//! 重写自 `src/constants/errorIds.ts`。
//!
//! These IDs are obfuscated identifiers that help us trace
//! which logError() call generated an error.
//!
//! These errors are represented as individual const exports for optimal
//! dead code elimination (external build will only see the numbers).
//!
//! ADDING A NEW ERROR TYPE:
//! 1. Add a const based on Next ID.
//! 2. Increment Next ID.
/// Next ID: 346
pub const E_TOOL_USE_SUMMARY_GENERATION_FAILED: u32 = 344;
