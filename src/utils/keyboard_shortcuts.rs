//! keyboard_shortcuts.rs — macOS Option 组合键特殊字符映射。
//!
//! 重写自 `src/utils/keyboardShortcuts.ts`。用于在未启用
//! "Option as Meta" 的 macOS 终端上识别 Option+key 快捷键。

use std::collections::HashMap;
use std::sync::OnceLock;

/// macOS Option+key 产生的特殊字符 → 快捷键名。
fn table() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("†", "alt+t"); // Option+T -> thinking toggle
        m.insert("π", "alt+p"); // Option+P -> model picker
        m.insert("ø", "alt+o"); // Option+O -> fast mode
        m
    })
}

/// 字符是否为 macOS Option 组合键产生的特殊字符。
pub fn is_macos_option_char(char: &str) -> bool {
    table().contains_key(char)
}

/// 查询 Option 特殊字符对应的快捷键名；非 option 字符返回 None。
pub fn macos_option_shortcut(char: &str) -> Option<&'static str> {
    table().get(char).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_option_chars() {
        assert!(is_macos_option_char("†"));
        assert!(is_macos_option_char("π"));
        assert!(is_macos_option_char("ø"));
        assert!(!is_macos_option_char("a"));
        assert_eq!(macos_option_shortcut("π"), Some("alt+p"));
        assert_eq!(macos_option_shortcut("x"), None);
    }
}
