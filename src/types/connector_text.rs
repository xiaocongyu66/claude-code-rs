//! connector_text.rs — 连接器文本块与增量类型。
//!
//! 重写自 `src/types/connectorText.ts`（Auto-generated stub）。索引签名
//! `[key: string]: unknown` 映射为 flatten 的 `HashMap<String, Value>`；
//! `isConnectorTextBlock` 是始终返回 `false` 的 stub，忠实保留。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 连接器产生的文本块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorTextBlock {
    pub r#type: String,
    pub connector_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 索引签名兜底字段。
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// 连接器产生的文本增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorTextDelta {
    pub r#type: String,
    pub connector_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 索引签名兜底字段。
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// 判断给定值是否为 `ConnectorTextBlock`。
///
/// 注意：TS 源码中该函数是始终返回 `false` 的 stub，此处忠实保留。
pub fn is_connector_text_block(_block: &serde_json::Value) -> bool {
    false
}
