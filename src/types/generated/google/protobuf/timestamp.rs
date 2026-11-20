//! timestamp.rs — protobuf `google/protobuf/Timestamp` 的 Rust 映射。
//!
//! 重写自 `src/types/generated/google/protobuf/timestamp.ts`（protoc-gen-ts_proto
//! v2.6.1 生成代码）。语义对齐说明：
//! - TS 的 `MessageFns`（fromJSON/toJSON/create/fromPartial）由 serde +
//!   `Default` 覆盖：`create()` ⇄ `Timestamp::default()`，`fromJSON()` ⇄
//!   `serde_json::from_value`，`toJSON()` ⇄ `serde_json::to_value`；
//! - proto3 语义下缺失字段等于默认值（`seconds = 0, nanos = 0`），因此字段
//!   不用 `Option`（ts_proto 的 `createBase` 同样填默认值）；
//! - 附带与 `chrono::DateTime<Utc>` 的互转（对应 ts_proto 的
//!   `fromTimestamp` / RFC 3339 JSON 映射）。

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// UTC 时间戳：自 Unix 纪元（1970-01-01T00:00:00Z）起的秒数与纳秒小数部分。
///
/// 有效范围：秒数须在 `0001-01-01T00:00:00Z` 到 `9999-12-31T23:59:59Z` 之间；
/// 纳秒部分须为 `0..=999_999_999`。闰秒采用 24 小时线性抹平（smear）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Timestamp {
    /// 自 Unix 纪元起的 UTC 秒数。
    pub seconds: i64,
    /// 秒的非负小数部分（纳秒分辨率，`0..=999_999_999`）。
    pub nanos: i32,
}

impl Timestamp {
    /// 从秒数构造（纳秒为 0）。
    pub fn from_seconds(seconds: i64) -> Self {
        Self { seconds, nanos: 0 }
    }

    /// 转换为 `chrono::DateTime<Utc>`（对应 ts_proto 的 `fromTimestamp`）。
    pub fn to_date_time(&self) -> DateTime<Utc> {
        let secs = self.seconds;
        let nanos = self.nanos.clamp(0, 999_999_999) as u32;
        // 超出 chrono 表示范围（0001-01-01 ~ 9999-12-31）时回退到最近的有效
        // 边界，与 proto 文档声明的合法范围一致；ambiguous（闰秒抹平）情形
        // 取较早一侧。
        Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(|| {
            if secs < 0 {
                DateTime::<Utc>::MIN_UTC
            } else {
                DateTime::<Utc>::MAX_UTC
            }
        })
    }

    /// 从 `chrono::DateTime<Utc>` 构造。
    pub fn from_date_time(dt: &DateTime<Utc>) -> Self {
        Self {
            seconds: dt.timestamp(),
            nanos: dt.timestamp_subsec_nanos() as i32,
        }
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(dt: DateTime<Utc>) -> Self {
        Self::from_date_time(&dt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_create_base() {
        let ts = Timestamp::default();
        assert_eq!(ts.seconds, 0);
        assert_eq!(ts.nanos, 0);
    }

    #[test]
    fn json_roundtrip_keeps_fields() {
        let ts = Timestamp {
            seconds: 1_600_000_000,
            nanos: 123_456_789,
        };
        let json = serde_json::to_value(&ts).unwrap();
        assert_eq!(json["seconds"], 1_600_000_000);
        assert_eq!(json["nanos"], 123_456_789);
        let back: Timestamp = serde_json::from_value(json).unwrap();
        assert_eq!(back, ts);
    }

    #[test]
    fn missing_fields_default_to_zero() {
        let back: Timestamp = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(back, Timestamp::default());
    }

    #[test]
    fn date_time_roundtrip() {
        let dt = Utc.timestamp_opt(1_600_000_000, 500_000_000).unwrap();
        let ts = Timestamp::from_date_time(&dt);
        assert_eq!(ts.seconds, 1_600_000_000);
        assert_eq!(ts.nanos, 500_000_000);
        assert_eq!(ts.to_date_time(), dt);
    }
}
