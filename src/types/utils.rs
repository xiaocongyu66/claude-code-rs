//! utils.rs — 通用类型工具占位。
//!
//! 重写自 `src/types/utils.ts`（Auto-generated stub）。`DeepImmutable<T>` 与
//! `Permutations<T>` 都是类型层面的 stub：Rust 侧分别映射为 identity 别名与
//! `Vec<T>`。

/// 递归不可变类型的占位别名（TS 源码中为 `DeepImmutable<T> = T`）。
pub type DeepImmutable<T> = T;

/// 排列类型的占位别名（TS 源码中为 `Permutations<T> = T[]`）。
pub type Permutations<T> = Vec<T>;
