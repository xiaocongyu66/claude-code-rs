//! env_validation.rs — 带上下界的环境变量整型校验（重写自
//! `src/utils/envValidation.ts`）。
//!
//! 语义对齐说明：TS `logForDebugging(message)` 默认级别 `debug`，
//! Rust 侧显式传 `DebugLogLevel::Debug`。

use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// 环境变量校验结果（对齐 TS `EnvVarValidationResult`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVarValidationResult {
    /// 实际生效的值。
    pub effective: i64,
    /// valid=原样生效；capped=被上限截断；invalid=非法回退默认。
    pub status: &'static str,
    /// 给用户看的说明信息（仅 capped/invalid 有）。
    pub message: Option<String>,
}

/// 校验「正整数 ≤ upperLimit」的环境变量。
///
/// - 未设置（None/空串）→ valid + 默认值
/// - 非数字或 ≤ 0 → invalid + 默认值（记调试日志）
/// - 超过上限 → capped + 上限（记调试日志）
pub fn validate_bounded_int_env_var(
    name: &str,
    value: Option<&str>,
    default_value: i64,
    upper_limit: i64,
) -> EnvVarValidationResult {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return EnvVarValidationResult {
            effective: default_value,
            status: "valid",
            message: None,
        };
    };

    let Ok(parsed) = value.parse::<i64>() else {
        let message = format!("Invalid value \"{}\" (using default: {})", value, default_value);
        log_for_debugging(&format!("{name} {message}"), DebugLogLevel::Debug);
        return EnvVarValidationResult {
            effective: default_value,
            status: "invalid",
            message: Some(message),
        };
    };

    if parsed <= 0 {
        let message = format!("Invalid value \"{}\" (using default: {})", value, default_value);
        log_for_debugging(&format!("{name} {message}"), DebugLogLevel::Debug);
        return EnvVarValidationResult {
            effective: default_value,
            status: "invalid",
            message: Some(message),
        };
    }

    if parsed > upper_limit {
        let message = format!("Capped from {} to {}", parsed, upper_limit);
        log_for_debugging(&format!("{name} {message}"), DebugLogLevel::Debug);
        return EnvVarValidationResult {
            effective: upper_limit,
            status: "capped",
            message: Some(message),
        };
    }

    EnvVarValidationResult {
        effective: parsed,
        status: "valid",
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_value_uses_default() {
        let r = validate_bounded_int_env_var("X", None, 10, 100);
        assert_eq!(r, EnvVarValidationResult { effective: 10, status: "valid", message: None });
    }

    #[test]
    fn invalid_value_uses_default() {
        let r = validate_bounded_int_env_var("X", Some("abc"), 10, 100);
        assert_eq!(r.status, "invalid");
        assert_eq!(r.effective, 10);
        let r2 = validate_bounded_int_env_var("X", Some("0"), 10, 100);
        assert_eq!(r2.status, "invalid");
        let r3 = validate_bounded_int_env_var("X", Some("-5"), 10, 100);
        assert_eq!(r3.status, "invalid");
    }

    #[test]
    fn over_limit_is_capped() {
        let r = validate_bounded_int_env_var("X", Some("500"), 10, 100);
        assert_eq!(r.effective, 100);
        assert_eq!(r.status, "capped");
        assert_eq!(r.message.as_deref(), Some("Capped from 500 to 100"));
    }

    #[test]
    fn in_range_is_valid() {
        let r = validate_bounded_int_env_var("X", Some("42"), 10, 100);
        assert_eq!(r.effective, 42);
        assert_eq!(r.status, "valid");
        assert_eq!(r.message, None);
    }
}
