//! intl.rs — Intl 对象的共享缓存与图元（grapheme）工具。
//!
//! 重写自 `src/utils/intl.ts`。Intl 构造昂贵，TS 端缓存共享实例；
//! Rust 端用 `OnceLock` 做等价的惰性初始化。
//!
//! 已知差异（Rust 无 ICU）：
//! - 图元簇：Rust 的 `char` 是 Unicode 标量值，`first_grapheme` /
//!   `last_grapheme` 用 chars() 实现。对常见 emoji 与 CJK 与 Intl
//!   Segmenter 一致；对含 ZWJ 的复合 emoji（如 👨‍👩‍👧）TS 按完整
//!   grapheme cluster 切分，本实现按标量值切分。
//! - 时区名：从 TZ 环境变量读取（回退 "UTC"），无法解析 OS IANA 名。
//! - 系统语言：从 POSIX 环境变量（LC_ALL > LC_TIME > LANG）推导。

use std::env;
use std::sync::OnceLock;

static CACHED_TIME_ZONE: OnceLock<String> = OnceLock::new();
static CACHED_SYSTEM_LOCALE_LANGUAGE: OnceLock<Option<String>> = OnceLock::new();

/// 提取字符串的第一个图元簇；空串返回空串。
pub fn first_grapheme(text: &str) -> &str {
    match text.chars().next() {
        Some(first) => &text[..first.len_utf8()],
        None => "",
    }
}

/// 提取字符串的最后一个图元簇；空串返回空串。
pub fn last_grapheme(text: &str) -> &str {
    match text.char_indices().next_back() {
        Some((idx, _)) => &text[idx..],
        None => "",
    }
}

/// 进程所在时区。TS 端读 `Intl.DateTimeFormat().resolvedOptions().timeZone`；
/// Rust 无 ICU，读 TZ 环境变量，回退 "UTC"。
pub fn get_time_zone() -> &'static str {
    CACHED_TIME_ZONE.get_or_init(|| env::var("TZ").unwrap_or_else(|_| "UTC".to_string()))
}

/// 系统语言子标签（如 "en"、"ja"）。从 POSIX 环境变量推导
///（LC_ALL > LC_TIME > LANG）；不可用时返回 None。
pub fn get_system_locale_language() -> Option<&'static str> {
    CACHED_SYSTEM_LOCALE_LANGUAGE
        .get_or_init(|| {
            for var in ["LC_ALL", "LC_TIME", "LANG"] {
                if let Some(raw) = env::var_os(var) {
                    let raw = raw.to_string_lossy();
                    if raw == "C" || raw == "POSIX" || raw.is_empty() {
                        continue;
                    }
                    let base = raw.split('.').next().unwrap_or(&raw);
                    let base = base.split('@').next().unwrap_or(base);
                    let lang = base.split('_').next().unwrap_or(base);
                    if !lang.is_empty() && lang.chars().all(|c| c.is_ascii_alphabetic()) {
                        return Some(lang.to_ascii_lowercase());
                    }
                }
            }
            None
        })
        .as_deref()
}

/// BCP 47 风格的 locale 标签（如 "en-GB"），从 POSIX 环境变量推导；
/// 未设置或非法时返回 None。
pub fn get_locale() -> Option<String> {
    let raw = env::var("LC_ALL")
        .or_else(|_| env::var("LC_TIME"))
        .or_else(|_| env::var("LANG"))
        .ok()?;
    if raw.is_empty() || raw == "C" || raw == "POSIX" {
        return None;
    }
    let base = raw.split('.').next().unwrap_or(&raw);
    let base = base.split('@').next().unwrap_or(base);
    if base.is_empty() {
        return None;
    }
    let tag = base.replace('_', "-");
    // 粗校验（替代 TS 端 `new Intl.DateTimeFormat(tag)` 的抛错校验）：
    // 至少含一个语言子标签，且所有子标签为 ASCII 字母/数字
    if tag
        .split('-')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        Some(tag)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphemes() {
        assert_eq!(first_grapheme(""), "");
        assert_eq!(first_grapheme("ab"), "a");
        assert_eq!(last_grapheme("ab"), "b");
        assert_eq!(last_grapheme("héllo"), "o");
    }

    #[test]
    fn locale_derivation() {
        // 函数为纯函数（不缓存 locale）；本进程环境因测试并行不可断言
        // 具体值，仅校验不 panic 与形状合法
        if let Some(tag) = get_locale() {
            assert!(tag.contains('-') || tag.len() <= 3);
        }
        let _ = get_time_zone();
        let _ = get_system_locale_language();
    }
}
