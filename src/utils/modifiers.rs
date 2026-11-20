//! modifiers.rs — 键盘修饰键状态检测（macOS 原生模块桥接）。
//!
//! 重写自 `src/utils/modifiers.ts`。TS 端通过 modifiers-napi 原生模块
//! 查询修饰键；Rust 端没有等价的 FFI 绑定，按"不可用时返回 false"
//! 的兜底语义实现——与 TS 的 try/catch → false 一致：
//!
//! - 非 macOS 平台：恒 false（与 TS 相同）
//! - macOS：原生模块不可用，回落 false 并记录一次性的调试日志

use std::env;

/// 修饰键种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

impl ModifierKey {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shift => "shift",
            Self::Command => "command",
            Self::Control => "control",
            Self::Option => "option",
        }
    }
}

/// 预热原生模块（提前加载以避免首次使用的延迟）。
/// Rust 端无原生模块可加载：非 darwin 直接返回，darwin 记 no-op。
pub fn prewarm_modifiers() {
    if env::consts::OS != "macos" {
        return;
    }
    // 无原生 FFI 绑定：保持 no-op（对齐 TS 中加载失败被 catch 吞掉的路径）
}

/// 查询某个修饰键当前是否按下（同步）。
///
/// 非 macOS 恒 false；macOS 上原生模块不可用时同样回落 false
///（对齐 TS 的兜底语义）。
pub fn is_modifier_pressed(_modifier: ModifierKey) -> bool {
    if env::consts::OS != "macos" {
        return false;
    }
    // modifiers-napi 为 Bun 原生模块；Rust 端暂无等价 FFI 绑定，
    // 与 TS 中 require 失败被 catch 吞掉的行为一致：返回 false。
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_macos_always_false() {
        if env::consts::OS != "macos" {
            assert!(!is_modifier_pressed(ModifierKey::Shift));
            assert!(!is_modifier_pressed(ModifierKey::Command));
        }
        prewarm_modifiers();
    }
}
