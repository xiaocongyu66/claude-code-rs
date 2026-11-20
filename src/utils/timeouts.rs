//! timeouts.rs — bash 操作超时常量。
//!
//! 重写自 `src/utils/timeouts.ts`。从环境变量读取用户配置时采用与
//! `parseInt` 一致的前缀解析（"120s" → 120），非法或非正数时回落默认值。

use std::collections::HashMap;

const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 分钟
const MAX_TIMEOUT_MS: u64 = 600_000; // 10 分钟

/// 环境变量映射（`BASH_DEFAULT_TIMEOUT_MS` 等键）。
pub type Env = HashMap<String, String>;

/// JS `parseInt(string, 10)` 的前缀语义：跳过前导空白，允许一个正负号，
/// 然后取连续数字；无数字返回 None（对应 NaN）。
fn parse_int_prefix(input: &str) -> Option<i64> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() && is_js_whitespace(chars[i]) {
        i += 1;
    }
    let sign: i64 = match chars.get(i) {
        Some('+') => {
            i += 1;
            1
        }
        Some('-') => {
            i += 1;
            -1
        }
        _ => 1,
    };
    let start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let digits: String = chars[start..i].iter().collect();
    digits.parse::<i64>().ok().map(|v| sign * v)
}

/// ECMAScript `\s` 字符集（比 Rust `char::is_whitespace` 少 U+0085）。
fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn env_get(env: Option<&Env>, key: &str) -> Option<String> {
    match env {
        Some(map) => map.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

fn has_key(env: Option<&Env>, key: &str) -> bool {
    match env {
        Some(map) => map.contains_key(key),
        None => std::env::var_os(key).is_some(),
    }
}

/// bash 操作默认超时（毫秒）。
///
/// 读取 `BASH_DEFAULT_TIMEOUT_MS`；未设置或解析出的值不为正数时返回
/// 2 分钟默认值。`env` 为 `None` 时读进程环境（对齐 TS 的 `process.env`）。
pub fn get_default_bash_timeout_ms(env: Option<&Env>) -> u64 {
    if let Some(value) = env_get(env, "BASH_DEFAULT_TIMEOUT_MS").as_deref() {
        if let Some(parsed) = parse_int_prefix(value) {
            if parsed > 0 {
                return parsed as u64;
            }
        }
    }
    DEFAULT_TIMEOUT_MS
}

/// bash 操作最大超时（毫秒）。
///
/// 读取 `BASH_MAX_TIMEOUT_MS`；仅当用户显式配置了
/// `BASH_DEFAULT_TIMEOUT_MS` 时才把 max 抬到 default 之上——未配置
/// default 时必须原样尊重显式的低 ceiling（否则 `BASH_MAX_TIMEOUT_MS=1000`
/// 之类会被内置 2 分钟 default 静默覆盖）。
pub fn get_max_bash_timeout_ms(env: Option<&Env>) -> u64 {
    if let Some(value) = env_get(env, "BASH_MAX_TIMEOUT_MS").as_deref() {
        if let Some(parsed) = parse_int_prefix(value) {
            if parsed > 0 {
                if has_key(env, "BASH_DEFAULT_TIMEOUT_MS") {
                    return std::cmp::max(parsed as u64, get_default_bash_timeout_ms(env));
                }
                return parsed as u64;
            }
        }
    }
    // 始终保证 max 不小于 default
    std::cmp::max(MAX_TIMEOUT_MS, get_default_bash_timeout_ms(env))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults() {
        assert_eq!(get_default_bash_timeout_ms(Some(&env(&[]))), 120_000);
        assert_eq!(get_max_bash_timeout_ms(Some(&env(&[]))), 600_000);
    }

    #[test]
    fn parses_int_prefix_like_js() {
        let e = env(&[("BASH_DEFAULT_TIMEOUT_MS", "300s")]);
        assert_eq!(get_default_bash_timeout_ms(Some(&e)), 300);
    }

    #[test]
    fn ignores_non_positive_values() {
        for bad in ["0", "-5", "abc", ""] {
            let e = env(&[("BASH_DEFAULT_TIMEOUT_MS", bad)]);
            assert_eq!(get_default_bash_timeout_ms(Some(&e)), 120_000, "value: {bad}");
        }
    }

    #[test]
    fn respects_explicit_low_ceiling_without_default_override() {
        let e = env(&[("BASH_MAX_TIMEOUT_MS", "1000")]);
        assert_eq!(get_max_bash_timeout_ms(Some(&e)), 1000);
    }

    #[test]
    fn raises_ceiling_above_default_when_default_is_set() {
        let e = env(&[
            ("BASH_MAX_TIMEOUT_MS", "1000"),
            ("BASH_DEFAULT_TIMEOUT_MS", "5000"),
        ]);
        assert_eq!(get_max_bash_timeout_ms(Some(&e)), 5000);
    }

    #[test]
    fn default_above_builtin_max_stretches_max() {
        let e = env(&[("BASH_DEFAULT_TIMEOUT_MS", "900000")]);
        assert_eq!(get_default_bash_timeout_ms(Some(&e)), 900_000);
        assert_eq!(get_max_bash_timeout_ms(Some(&e)), 900_000);
    }
}
