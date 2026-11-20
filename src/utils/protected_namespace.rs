//! protected_namespace.rs — 受保护命名空间检查桩。
//!
//! 重写自 `src/utils/protectedNamespace.ts`（TS 端即为自动生成的桩）。
//! 生产实现仅在 USER_TYPE === 'ant' 的内部构建中加载；此处保持恒 false。

/// 桩实现：恒为 false。
pub fn check_protected_namespace() -> bool {
    false
}
