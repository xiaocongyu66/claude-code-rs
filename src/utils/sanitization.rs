//! sanitization.rs — 针对隐藏字符攻击的 Unicode 清洗。
//!
//! 重写自 `src/utils/sanitization.ts`。防御 ASCII 走私与隐藏提示注入
//!（HackerOne #3086545）：用不可见 Unicode 字符（Tag 字符、格式控制、
//! 私用区、非字符）藏入对用户不可见、但模型会执行的指令。
//!
//! 与 TS 的差异（Rust 无 ICU）：TS 的第一步 NFKC 归一化在 Rust 端
//! 无对应实现（Cargo.toml 无 unicode-normalization 依赖）。TS 的
//! 清洗在纯 ASCII 输入上一次收敛，且非 ASCII 输入下类别剥离本身也
//! 消除绝大部分归一化差异来源；本实现保留迭代收敛的骨架与
//! MAX_ITERATIONS 保护，但每趟只做类别剥离。
//!
//! 类别剥离对齐 TS 的两条路径：
//! 1. `\p{Cf}|\p{Co}|\p{Cn}` —— 以硬编码范围表实现（Cf 含关键的
//!    U+E0000-E007F Tag 字符，即原始漏洞向量）
//! 2. 显式危险范围（零宽、方向控制、隔离符、BOM、BMP 私用区）

/// 最大迭代次数；达到仍不收敛视为编程错误或恶意深层嵌套输入。
const MAX_ITERATIONS: usize = 10;

/// 剥离危险 Unicode 字符（`partiallySanitizeUnicode`）。
///
/// 迭代清洗直至无变化或达到 MAX_ITERATIONS 次；达到上限时返回 Err
///（对齐 TS 的 throw：只应在 bug 或刻意构造的深层嵌套输入下发生）。
pub fn partially_sanitize_unicode(input: &str) -> Result<String, String> {
    let mut current = input.to_string();
    let mut previous = String::new();
    let mut iterations = 0;

    while current != previous && iterations < MAX_ITERATIONS {
        previous = current;
        // TS：current.normalize('NFKC') —— Rust 端无 NFKC（无 ICU），
        // 直接进入类别剥离（差异见模块注释）。
        current = strip_dangerous(&previous);
        iterations += 1;
    }

    if iterations >= MAX_ITERATIONS {
        let preview: String = current.chars().take(100).collect();
        return Err(format!(
            "Unicode sanitization reached maximum iterations ({MAX_ITERATIONS}) for input: {preview}"
        ));
    }

    Ok(current)
}

/// `recursivelySanitizeUnicode`：递归清洗 serde_json::Value。
pub fn recursively_sanitize_unicode(value: serde_json::Value) -> Result<serde_json::Value, String> {
    match value {
        serde_json::Value::String(s) => {
            Ok(serde_json::Value::String(partially_sanitize_unicode(&s)?))
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(recursively_sanitize_unicode(item)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, val) in map {
                let key = partially_sanitize_unicode(&key)?;
                out.insert(key, recursively_sanitize_unicode(val)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        // 其它原语（数字、布尔、null）原样返回
        other => Ok(other),
    }
}

/// 剥离 Cf / Co / Cn 与 TS 方法 2 的显式危险范围。
fn strip_dangerous(s: &str) -> String {
    s.chars().filter(|c| !is_dangerous(*c)).collect()
}

/// 字符是否属于应剥离的类别/范围。
fn is_dangerous(c: char) -> bool {
    let cp = c as u32;

    // --- 路径 1：\p{Cf}（格式字符，硬编码范围表） ---
    // 零宽与不可见操作符（含数学不可见符号）
    if cp == 0x00AD                                     // SOFT HYPHEN
        || (0x0600..=0x0605).contains(&cp)              // 阿拉伯字母标记
        || cp == 0x061C                                 // ARABIC LETTER MARK
        || cp == 0x06DD                                 // 阿拉伯空格标记
        || cp == 0x070F                                 // 叙利亚缩写标记
        || (0x180B..=0x180E).contains(&cp)              // 蒙古语元音分隔符
        || cp == 0x1885 || cp == 0x1886
        || (0x200B..=0x200F).contains(&cp)              // 零宽空格 / LTR·RTL 标记
        || (0x202A..=0x202E).contains(&cp)              // 方向格式化字符
        || (0x2060..=0x2064).contains(&cp)              // 字连接符等
        || (0x2066..=0x206F).contains(&cp)              // 方向隔离符
        || cp == 0xFEFF                                // BOM
        || (0xFFF9..=0xFFFB).contains(&cp)              // 内联注释隔离符
        || cp == 0x110BD                               // 卡罗须提字母标记
        || cp == 0x110CD
        || cp == 0x13430 || cp == 0x13431 || cp == 0x13432 || cp == 0x13433 || cp == 0x13434 || cp == 0x13435
        || cp == 0x13436 || cp == 0x13437 || cp == 0x13438
        || cp == 0x1BCA0 || cp == 0x1BCA1 || cp == 0x1BCA2 || cp == 0x1BCA3
        || cp == 0x1D173 || cp == 0x1D174 || cp == 0x1D175 || cp == 0x1D176
        || cp == 0x1D177 || cp == 0x1D178 || cp == 0x1D179 || cp == 0x1D17A
        || (0xE0000..=0xE0FFF).contains(&cp)           // 语言 Tag 字符（攻击向量）/ 变体选择符
        || cp == 0xE0001
        || (0xE0020..=0xE007F).contains(&cp)
        || cp == 0xE0100 || (0xE0101..=0xE01EF).contains(&cp)
    {
        return true;
    }

    // --- 路径 1：\p{Co}（私用区） ---
    if (0xE000..=0xF8FF).contains(&cp)                  // BMP 私用区
        || (0xF0000..=0xFFFFD).contains(&cp)            // 补充私用区 A
        || (0x100000..=0x10FFFD).contains(&cp)
    // 补充私用区 B
    {
        return true;
    }

    // --- 路径 1：\p{Cn}（未分配 / 代理 / 保留）：非字符与代理对 ---
    if (0xD800..=0xDFFF).contains(&cp)                  // 代理对（Rust char 不会出现，保持防御）
        || (0xFDD0..=0xFDEF).contains(&cp)              // 非字符
        || matches!(cp, 0xFFFE | 0xFFFF | 0x1FFFE | 0x1FFFF | 0x2FFFE | 0x2FFFF
            | 0x3FFFE | 0x3FFFF | 0x4FFFE | 0x4FFFF | 0x5FFFE | 0x5FFFF
            | 0x6FFFE | 0x6FFFF | 0x7FFFE | 0x7FFFF | 0x8FFFE | 0x8FFFF
            | 0x9FFFE | 0x9FFFF | 0xAFFFE | 0xAFFFF | 0xBFFFE | 0xBFFFF
            | 0xCFFFE | 0xCFFFF | 0xDFFFE | 0xDFFFF | 0xEFFFE | 0xEFFFF
            | 0xFFFFE | 0xFFFFF | 0x10FFFE | 0x10FFFF)
    {
        return true;
    }

    // --- 路径 2：TS 的显式回退范围（与上面去重后总有覆盖） ---
    // 0x200B-0x200F / 0x202A-0x202E / 0x2066-0x2069 / U+FEFF / U+E000-F8FF
    // 已在上面的 Cf/Co 剥离中处理；为忠实保留原逻辑，这里显式再列剩余
    // 方向隔离符（0x2066-0x2069）与零宽断言 —— 已包含，不再重复。
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_normal_text() {
        let s = "Hello, 世界 🌍 — plain English text!";
        assert_eq!(partially_sanitize_unicode(s).unwrap(), s);
    }

    #[test]
    fn strips_zero_width_and_directional_marks() {
        let s = "hi\u{200B}\u{200E}\u{202E}dd\u{2066}x\u{2069}";
        assert_eq!(partially_sanitize_unicode(s).unwrap(), "hiddx");
    }

    #[test]
    fn strips_bom_and_private_use() {
        assert_eq!(partially_sanitize_unicode("\u{FEFF}ok").unwrap(), "ok");
        assert_eq!(partially_sanitize_unicode("a\u{E000}b").unwrap(), "ab");
    }

    #[test]
    fn strips_unicode_tag_characters() {
        // 原始攻击向量：U+E0000-E007F Tag 字符隐藏指令
        let s = "tags:\u{E0020}secret\u{E0001}";
        let cleaned = partially_sanitize_unicode(s).unwrap();
        assert_eq!(cleaned, "tags:secret");
        assert!(
            !cleaned
                .chars()
                .any(|c| (0xE0000..=0xE007F).contains(&(c as u32)))
        );
    }

    #[test]
    fn recursively_sanitizes_json() {
        let value = json!({
            "key\u{200B}": ["a\u{202E}", 42, null, true]
        });
        let cleaned = recursively_sanitize_unicode(value).unwrap();
        assert_eq!(cleaned, json!({ "key": ["a", 42, null, true] }));
    }

    #[test]
    fn normal_input_converges_below_max_iterations() {
        // 幂等输入一次收敛，iterations 为 1，远低于上限
        let s = "plain";
        assert_eq!(partially_sanitize_unicode(s).unwrap(), s);
    }
}
