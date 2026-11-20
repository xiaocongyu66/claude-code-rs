//! auth.rs — protobuf `events_mono/common/v1/auth.proto` 的 Rust 映射。
//!
//! 重写自 `src/types/generated/events_mono/common/v1/auth.ts`（protoc-gen-ts_proto
//! v2.6.1 生成代码）。语义对齐说明：
//! - TS 的 `MessageFns` 由 serde + `Default` 覆盖（`create()` ⇄
//!   `PublicApiAuth::default()`，`fromJSON`/`toJSON` ⇄ serde）；
//! - proto3 缺失字段等于默认值，字段不用 `Option`；
//! - `account_id` 在 TS 侧为 `number`（`toJSON` 做 `Math.round`），映射为
//!   `i64` 以容纳 proto 的 64 位整型。

use serde::{Deserialize, Serialize};

/// API 端自动注入的鉴权上下文。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PublicApiAuth {
    /// 账户 ID（数字标识）。
    pub account_id: i64,
    /// 组织 UUID。
    pub organization_uuid: String,
    /// 账户 UUID。
    pub account_uuid: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_create_base() {
        let auth = PublicApiAuth::default();
        assert_eq!(auth.account_id, 0);
        assert_eq!(auth.organization_uuid, "");
        assert_eq!(auth.account_uuid, "");
    }

    #[test]
    fn json_roundtrip() {
        let auth = PublicApiAuth {
            account_id: 123,
            organization_uuid: "org-uuid".into(),
            account_uuid: "acct-uuid".into(),
        };
        let json = serde_json::to_value(&auth).unwrap();
        assert_eq!(json["account_id"], 123);
        let back: PublicApiAuth = serde_json::from_value(json).unwrap();
        assert_eq!(back, auth);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let back: PublicApiAuth = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(back, PublicApiAuth::default());
    }
}
