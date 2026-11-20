//! growthbook_experiment_event.rs — protobuf
//! `events_mono/growthbook/v1/growthbook_experiment_event.proto` 的 Rust 映射。
//!
//! 重写自 `src/types/generated/events_mono/growthbook/v1/`
//! `growthbook_experiment_event.ts`（protoc-gen-ts_proto v2.6.1 生成代码）。
//! 语义对齐说明：
//! - TS 的 `MessageFns` 由 serde + `Default` 覆盖；
//! - proto3 缺失字段等于默认值，标量字段不用 `Option`；
//! - `timestamp` 在 TS 侧为 `Date`：JSON 映射为 RFC 3339 字符串（`toISOString()`
//!   / `fromJsonTimestamp`），映射为 `Option<DateTime<Utc>>`；
//! - 嵌套消息（`auth`）用 `Option` 区分缺失（ts_proto 的 `undefined`）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::generated::events_mono::common::v1::auth::PublicApiAuth;

/// GrowthBook 实验分桶事件：用户被曝光到某个实验变体时记录。
/// 字段与 GrowthBook BigQuery schema 对应，详见
/// <https://docs.growthbook.io/guide/bigquery>。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GrowthbookExperimentEvent {
    /// 事件唯一标识（用于去重）。
    #[serde(default)]
    pub event_id: String,
    /// 用户曝光到实验的时间（对应 GrowthBook 的 timestamp 列）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// 实验追踪 key（对应 GrowthBook 的 experiment_id 列）。
    #[serde(default)]
    pub experiment_id: String,
    /// 变体序号：0=对照组，1+=实验变体（对应 variation_id 列）。
    #[serde(default)]
    pub variation_id: i64,
    /// 分桶发生的环境。
    #[serde(default)]
    pub environment: String,
    /// 分桶时刻的用户属性。
    #[serde(default)]
    pub user_attributes: String,
    /// 实验元数据。
    #[serde(default)]
    pub experiment_metadata: String,
    /// 客户端设备标识。
    #[serde(default)]
    pub device_id: String,
    /// API 端自动注入的鉴权上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<PublicApiAuth>,
    /// 会话标识（用于跟踪用户会话）。
    #[serde(default)]
    pub session_id: String,
    /// 未登录用户的匿名标识。
    #[serde(default)]
    pub anonymous_id: String,
    /// 事件元数据变量（由 internal-tools-common event_logging 库自动填充）。
    #[serde(default)]
    pub event_metadata_vars: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_create_base() {
        let e = GrowthbookExperimentEvent::default();
        assert_eq!(e.event_id, "");
        assert_eq!(e.timestamp, None);
        assert_eq!(e.experiment_id, "");
        assert_eq!(e.variation_id, 0);
        assert_eq!(e.environment, "");
        assert_eq!(e.user_attributes, "");
        assert_eq!(e.experiment_metadata, "");
        assert_eq!(e.device_id, "");
        assert_eq!(e.auth, None);
        assert_eq!(e.session_id, "");
        assert_eq!(e.anonymous_id, "");
        assert_eq!(e.event_metadata_vars, "");
    }

    #[test]
    fn json_roundtrip_with_timestamp_and_auth() {
        let event = GrowthbookExperimentEvent {
            event_id: "evt-1".into(),
            timestamp: Some(
                "2024-01-15T01:30:15.01Z".parse::<DateTime<Utc>>().unwrap(),
            ),
            experiment_id: "exp-1".into(),
            variation_id: 1,
            environment: "production".into(),
            auth: Some(PublicApiAuth {
                account_id: 7,
                organization_uuid: "org".into(),
                account_uuid: "acct".into(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&event).unwrap();
        let ts_str = json["timestamp"].as_str().unwrap();
        assert_eq!(
            ts_str.parse::<DateTime<Utc>>().unwrap(),
            event.timestamp.unwrap()
        );
        assert_eq!(json["auth"]["account_id"], 7);
        let back: GrowthbookExperimentEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let back: GrowthbookExperimentEvent =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(back.event_id, "");
        assert_eq!(back.timestamp, None);
        assert_eq!(back.variation_id, 0);
        assert_eq!(back.auth, None);
    }
}
