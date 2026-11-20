//! local_validate.rs — /local-memory 与 /local-vault 输入键名校验。
//!
//! 重写自 `src/utils/localValidate.ts`。两处工具需要一致的、路径安全、
//! 跨 OS 可移植的键名方案。
//!
//! 允许：字母、数字、点、下划线、连字符；长度 1..=128。
//! 拒绝：空 / 超长；任何 [A-Za-z0-9._-] 之外的字符；前导点
//!（隐藏文件形态，如 ".gitconfig"）；Windows 保留设备名
//!（NUL、CON、COM1 等——在 Windows 上无论如何扩展名都会静默写到设备）。

use thiserror::Error;

const MAX_KEY_LENGTH: usize = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("Empty key")]
    Empty,
    #[error("Key too long (max {0})")]
    TooLong(usize),
    #[error("Invalid key chars: {0:?}")]
    InvalidChars(String),
    #[error("Leading dot forbidden")]
    LeadingDot,
    #[error("Windows reserved name: {0}")]
    WindowsReserved(String),
}

fn is_valid_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
}

fn is_windows_reserved_basename(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    let bytes = upper.as_bytes();
    if bytes.len() >= 4 {
        let prefix = &upper[..3];
        let digits = &upper[3..];
        let is_digits = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        if is_digits {
            if (prefix == "COM" || prefix == "LPT") && digits.len() == 1 {
                let n = digits.parse::<u32>().unwrap_or(0);
                return (1..=9).contains(&n);
            }
        }
    }
    false
}

/// 校验键名；非法时返回错误（对齐 TS 的 throw 语义）。
pub fn validate_key(key: &str) -> Result<(), KeyError> {
    if key.is_empty() {
        return Err(KeyError::Empty);
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(KeyError::TooLong(MAX_KEY_LENGTH));
    }
    if !key.chars().all(is_valid_key_char) {
        return Err(KeyError::InvalidChars(key.to_string()));
    }
    if key.starts_with('.') {
        return Err(KeyError::LeadingDot);
    }
    // M6 fix：匹配 basename（首个点之前的部分），使 NUL.txt / CON.foo
    // 也被拒绝——Windows 上这些仍会别名到设备文件而静默丢数据。
    let basename_component = key.split('.').next().unwrap_or(key);
    if is_windows_reserved_basename(basename_component) {
        return Err(KeyError::WindowsReserved(key.to_string()));
    }
    Ok(())
}

/// validate_key 的布尔形式（守卫场景用）。
pub fn is_valid_key(key: &str) -> bool {
    validate_key(key).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_simple_keys() {
        assert!(is_valid_key("memory"));
        assert!(is_valid_key("my_key.v2-1"));
        assert!(validate_key("memory").is_ok());
    }

    #[test]
    fn rejects_empty_and_long() {
        assert_eq!(validate_key("").unwrap_err(), KeyError::Empty);
        let long = "a".repeat(129);
        assert!(matches!(validate_key(&long), Err(KeyError::TooLong(128))));
    }

    #[test]
    fn rejects_bad_chars_and_leading_dot() {
        assert!(matches!(
            validate_key("a b"),
            Err(KeyError::InvalidChars(_))
        ));
        assert!(matches!(
            validate_key("a/b"),
            Err(KeyError::InvalidChars(_))
        ));
        assert_eq!(
            validate_key(".gitconfig").unwrap_err(),
            KeyError::LeadingDot
        );
    }

    #[test]
    fn rejects_windows_reserved_names() {
        assert!(matches!(
            validate_key("NUL"),
            Err(KeyError::WindowsReserved(_))
        ));
        assert!(matches!(
            validate_key("NUL.txt"),
            Err(KeyError::WindowsReserved(_))
        ));
        assert!(matches!(
            validate_key("CON.foo"),
            Err(KeyError::WindowsReserved(_))
        ));
        assert!(matches!(
            validate_key("COM1"),
            Err(KeyError::WindowsReserved(_))
        ));
        assert!(matches!(
            validate_key("LPT9.bak"),
            Err(KeyError::WindowsReserved(_))
        ));
        assert!(is_valid_key("COM10")); // COM10 不是保留名
        assert!(is_valid_key("NULL")); // NULL 不等于 NUL
    }
}
