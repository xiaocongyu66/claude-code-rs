//! session_url.rs — session resume 标识符解析。
//!
//! 重写自 `src/utils/sessionUrl.ts`。接受两种形态：
//! - 含 session ID 的 URL（如
//!   https://api.example.com/v1/session_ingress/session/<uuid>）
//! - 裸 session ID（UUID）
//!
//! JSONL 文件路径优先于 URL 判定：Windows 绝对路径
//! （如 C:\path\file.jsonl）会被 URL 解析器当作合法 URL（C: 作 scheme）。

use uuid::Uuid;

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSessionUrl {
    pub session_id: String,
    pub ingress_url: Option<String>,
    pub is_url: bool,
    pub jsonl_file: Option<String>,
    pub is_jsonl_file: bool,
}

/// 解析 session resume 标识符；非法输入返回 None。
pub fn parse_session_identifier(resume_identifier: &str) -> Option<ParsedSessionUrl> {
    // 1. JSONL 文件路径（大小写不敏感扩展名）
    if resume_identifier.to_ascii_lowercase().ends_with(".jsonl") {
        return Some(ParsedSessionUrl {
            session_id: new_session_id(),
            ingress_url: None,
            is_url: false,
            jsonl_file: Some(resume_identifier.to_string()),
            is_jsonl_file: true,
        });
    }

    // 2. 裸 UUID
    if crate::utils::uuid::validate_uuid(resume_identifier).is_some() {
        return Some(ParsedSessionUrl {
            session_id: resume_identifier.to_ascii_lowercase(),
            ingress_url: None,
            is_url: false,
            jsonl_file: None,
            is_jsonl_file: false,
        });
    }

    // 3. URL（手写最小 scheme 校验，替代 TS 的 new URL()）
    if let Some(href) = parse_url_like(resume_identifier) {
        return Some(ParsedSessionUrl {
            session_id: new_session_id(),
            ingress_url: Some(href),
            is_url: true,
            jsonl_file: None,
            is_jsonl_file: false,
        });
    }

    None
}

fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// 最小 URL 校验：`scheme:` 的 scheme 形如 `[a-zA-Z][a-zA-Z0-9+.-]*`。
///
/// 已知差异：WHATWG URL 解析还做 host/path 归一化（小写 host、
/// 补全尾斜杠等）；本实现把可解析的输入按原样作为 ingress_url 返回。
/// resume 场景的标识符形态可控，此处只拦明显非 URL 的输入。
fn parse_url_like(input: &str) -> Option<String> {
    let (scheme, rest) = input.split_once(':')?;
    if scheme.is_empty() {
        return None;
    }
    let bytes = scheme.as_bytes();
    if !(bytes[0].is_ascii_alphabetic()) {
        return None;
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'-' || *b == b'.')
    {
        return None;
    }
    if rest.is_empty() {
        return None;
    }
    Some(input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_uuid() {
        let parsed = parse_session_identifier("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(parsed.session_id, "550e8400-e29b-41d4-a716-446655440000");
        assert!(!parsed.is_url);
        assert!(!parsed.is_jsonl_file);
    }

    #[test]
    fn parses_jsonl_path() {
        let parsed = parse_session_identifier("C:\\path\\to\\SESSION.JSONL").unwrap();
        assert!(parsed.is_jsonl_file);
        assert_eq!(
            parsed.jsonl_file.as_deref(),
            Some("C:\\path\\to\\SESSION.JSONL")
        );
        assert!(!parsed.is_url);
    }

    #[test]
    fn parses_url() {
        let parsed = parse_session_identifier(
            "https://api.example.com/v1/session_ingress/session/550e8400-e29b-41d4-a716-446655440000",
        )
        .unwrap();
        assert!(parsed.is_url);
        assert_eq!(
            parsed.ingress_url.as_deref(),
            Some(
                "https://api.example.com/v1/session_ingress/session/550e8400-e29b-41d4-a716-446655440000"
            )
        );
        // URL 总是生成随机 session ID
        assert_ne!(parsed.session_id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_session_identifier("not-a-session-id").is_none());
        assert!(parse_session_identifier("").is_none());
    }
}
