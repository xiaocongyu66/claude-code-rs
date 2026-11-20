//! semantic_number.rs — 也接受数字字符串字面量的数字语义。
//!
//! 重写自 `src/utils/semanticNumber.ts`（TS 端为 z.preprocess 包装的
//! Zod schema）。模型偶尔把数字引用号化 —— `"head_limit":"30"` ——
//! 而 z.number() 以类型错误拒绝；z.coerce.number() 又错在接受 ""、null
//! 之类的 JS Number() 转换，掩盖 bug。
//!
//! 只有匹配 /^-?\d+(\.\d+)?$/ 的合法十进制字面量才被归一化为数字，
//! 其余值原样返回（由调用方按 `Value::Number` 校验）。

use serde_json::Value;

/// 归一化语义数字：合法十进制字符串 → f64 数字；其余原样返回。
pub fn normalize_semantic_number(value: &Value) -> Value {
    if let Value::String(s) = value {
        if is_decimal_literal(s) {
            if let Ok(n) = s.parse::<f64>() {
                if n.is_finite() {
                    return Value::from(n);
                }
            }
        }
    }
    value.clone()
}

/// 归一化后要求数字类型；非数字返回 None（对齐 z.number() 的拒绝）。
pub fn parse_semantic_number(value: &Value) -> Option<f64> {
    match normalize_semantic_number(value) {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn is_decimal_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let start = if bytes[0] == b'-' { 1 } else { 0 };
    if start >= bytes.len() {
        return false; // 只有 "-"
    }
    let mut dot_seen = false;
    let mut int_digits = 0;
    for &b in &bytes[start..] {
        if b.is_ascii_digit() {
            if !dot_seen {
                int_digits += 1;
            }
            continue;
        }
        if b == b'.' && !dot_seen {
            dot_seen = true;
            continue;
        }
        return false;
    }
    // "." 或 "3." 之类不合法：小数点后至少一位（对齐 (\.\d+)?）
    if dot_seen {
        // ".5" 之类不合法：小数点前至少一位（对齐 ^-?\d+ 的整数部分）
        if int_digits == 0 {
            return false;
        }
        if let Some(dot_pos) = bytes[start..].iter().position(|&b| b == b'.') {
            let frac_len = bytes.len() - start - dot_pos - 1;
            return frac_len >= 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coerces_numeric_strings() {
        assert_eq!(parse_semantic_number(&json!("30")), Some(30.0));
        assert_eq!(parse_semantic_number(&json!("-5")), Some(-5.0));
        assert_eq!(parse_semantic_number(&json!("3.15")), Some(3.15));
        assert_eq!(parse_semantic_number(&json!(42)), Some(42.0));
    }

    #[test]
    fn rejects_non_numeric_strings_and_junk() {
        assert_eq!(parse_semantic_number(&json!("")), None);
        assert_eq!(parse_semantic_number(&json!("3.")), None);
        assert_eq!(parse_semantic_number(&json!(".5")), None);
        assert_eq!(parse_semantic_number(&json!("1e3")), None);
        assert_eq!(parse_semantic_number(&json!(null)), None);
        assert_eq!(parse_semantic_number(&json!("abc")), None);
    }
}
