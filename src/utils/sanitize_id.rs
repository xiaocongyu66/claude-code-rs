//! sanitize_id.rs — 错误消息中的 ID 脱敏。
//!
//! 重写自 `src/utils/sanitizeId.ts`。安全不变量：完整 ID（vault_id、
//! credential_id、agent_id 等）不得出现在错误消息里，以免泄漏到日志、
//! bug 报告或用户可见文本中；只暴露前 8 个字符。

/// 将 ID 脱敏为前 8 个字符（不足 8 个字符则原样返回）。
///
/// 对齐 TS：`id.length <= 8` 以 UTF-16 码元计数，此处按字符计数
/// （对 ASCII ID 等价）。
pub fn sanitize_id(id: &str) -> String {
    if id.chars().take(8).count() < 8 {
        return id.to_string();
    }
    let prefix: String = id.chars().take(8).collect();
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::sanitize_id;

    #[test]
    fn short_ids_are_returned_verbatim() {
        assert_eq!(sanitize_id("abc"), "abc");
        assert_eq!(sanitize_id("12345678"), "12345678");
    }

    #[test]
    fn long_ids_are_truncated_to_eight_chars() {
        assert_eq!(sanitize_id("123456789abcdef"), "12345678…");
    }
}
