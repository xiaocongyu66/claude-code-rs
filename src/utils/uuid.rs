//! uuid.rs — UUID 校验与 agent ID 生成。
//!
//! 重写自 `src/utils/uuid.ts`。正则改为手写十六进制串校验
//!（Cargo.toml 暂无 regex）；随机后缀用 uuid v4 的字节
//!（std 无 CSPRNG 直接 API，uuid crate 内部使用 getrandom）。

use crate::types::ids::AgentId;
use uuid::Uuid;

/// 校验是否为 UUID 格式（8-4-4-4-12 十六进制，大小写不敏感）。
/// 非字符串或格式不符返回 None。
pub fn validate_uuid(maybe_uuid: &str) -> Option<String> {
    if !is_uuid_format(maybe_uuid) {
        return None;
    }
    Some(maybe_uuid.to_ascii_lowercase())
}

fn is_uuid_format(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    // dash 位置：8-4-4-4-12
    let dash_positions = [8, 13, 18, 23];
    for (i, &b) in bytes.iter().enumerate() {
        if dash_positions.contains(&i) {
            if b != b'-' {
                return false;
            }
        } else if !(b as char).is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// 生成带前缀的新 agent ID（与 task ID 形态保持一致）。
/// 格式：a{label-}{16 位十六进制}
/// 示例：aa3f2c1b4d5e6f7a8、acompact-a3f2c1b4d5e6f7a8
pub fn create_agent_id(label: Option<&str>) -> AgentId {
    // randomBytes(8).toString('hex') —— uuid v4 的前 8 字节
    let uuid = Uuid::new_v4();
    let suffix: String = uuid.as_bytes()[..8]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    match label {
        Some(label) => AgentId(format!("a{label}-{suffix}")),
        None => AgentId(format!("a{suffix}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_uuids() {
        assert_eq!(
            validate_uuid("550E8400-E29B-41D4-A716-446655440000"),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert!(validate_uuid("not-a-uuid").is_none());
        assert!(validate_uuid("550e8400-e29b-41d4-a716-44665544000").is_none()); // 少一位
        assert!(validate_uuid("550e8400e29b41d4a716446655440000").is_none()); // 无 dash
    }

    #[test]
    fn creates_agent_id() {
        let id = create_agent_id(None);
        let s = &id.0;
        assert!(s.starts_with('a'));
        assert_eq!(s.len(), 17); // 'a' + 16 hex
        let labeled = create_agent_id(Some("compact"));
        assert!(labeled.0.starts_with("acompact-"));
        // 两次生成不同
        assert_ne!(create_agent_id(None).0, create_agent_id(None).0);
    }
}
