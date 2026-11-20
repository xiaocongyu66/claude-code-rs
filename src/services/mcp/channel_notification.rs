//! channel_notification.rs — channel 通知协议与门控
//! （重写自 `channelNotification.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 通知方法名常量（`notifications/claude/channel`、
//!   `.../permission`、`.../permission_request`）与 zod 校验语义
//!   （Rust 用结构体 + serde 校验）。
//! - `wrap_channel_message`：把服务器推送内容包成 `<channel>` XML，
//!   meta key 只接受标识符形态（正则 `^[a-zA-Z_][a-zA-Z0-9_]*$`），
//!   属性值经 `escapeXmlAttr` 转义。
//! - `get_effective_channel_allowlist`：team/enterprise org 的
//!   `allowedChannelPlugins` 托管设置替换 GrowthBook ledger。
//! - `find_channel_entry`：服务器名按 `server`/`plugin:X:Y` 两种形态匹配
//!   会话 `--channels` 条目。
//! - `gate_channel_server`：门控顺序 capability → 会话 --channels →
//!   plugin 条目的 marketplace 校验。
//!
//! 已知偏差：
//! - `utils/plugins/pluginIdentifier.ts` 未转换：本地
//!   `parse_plugin_identifier`（与 channel_allowlist.rs 同实现）。
//! - TS 原文的 org policy（channelsEnabled 托管设置）与 subscription
//!   读取依赖未转换模块；`get_effective_channel_allowlist` 保留签名，
//!   sub 由调用方传入（`utils::auth::get_subscription_type`）。

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::bootstrap::state::{get_allowed_channels, ChannelEntry};
use crate::constants::xml::CHANNEL_TAG;
use crate::services::mcp::channel_allowlist::{
    get_channel_allowlist, ChannelAllowlistEntry,
};
use crate::services::mcp::types::ServerCapabilities;
use crate::utils::xml::escape_xml_attr;

/// 入站 channel 消息通知方法名。
pub const CHANNEL_MESSAGE_METHOD: &str = "notifications/claude/channel";

/// 入站 channel 权限回复通知方法名。
pub const CHANNEL_PERMISSION_METHOD: &str = "notifications/claude/channel/permission";

/// 出站权限请求通知方法名。
pub const CHANNEL_PERMISSION_REQUEST_METHOD: &str =
    "notifications/claude/channel/permission_request";

/// channel 消息通知参数（zod schema 等价）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelMessageNotificationParams {
    pub content: String,
    /// 不透明透传（thread_id、user 等），渲染为 `<channel>` 属性。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<std::collections::HashMap<String, String>>,
}

/// channel 权限通知参数（zod schema 等价）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelPermissionBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPermissionNotificationParams {
    pub request_id: String,
    pub behavior: ChannelPermissionBehavior,
}

/// 出站权限请求参数（TS `ChannelPermissionRequestParams`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPermissionRequestParams {
    pub request_id: String,
    pub tool_name: String,
    pub description: String,
    /// JSON 字符串化、截断 200 字的工具输入预览。
    pub input_preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_context: Option<ChannelContext>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

/// 解析 `name@marketplace`（`utils/plugins/pluginIdentifier.ts` 未转换）。
fn parse_plugin_identifier(source: &str) -> (String, Option<String>) {
    match source.split_once('@') {
        Some((name, marketplace)) if !name.is_empty() && !marketplace.is_empty() => {
            (name.to_string(), Some(marketplace.to_string()))
        }
        _ => (source.to_string(), None),
    }
}

/// meta key 安全过滤器：仅接受 `^[a-zA-Z_][a-zA-Z0-9_]*$`
/// （TS `SAFE_META_KEY`）。
static SAFE_META_KEY: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").ok());

/// 把 channel 消息包成 `<channel source="..." key="value">…</channel>`。
pub fn wrap_channel_message(
    server_name: &str,
    content: &str,
    meta: Option<&std::collections::HashMap<String, String>>,
) -> String {
    let mut attrs = String::new();
    if let Some(meta) = meta {
        if let Some(rx) = SAFE_META_KEY.as_ref() {
            let mut keys: Vec<&String> = meta.keys().collect();
            keys.sort();
            for key in keys {
                if rx.is_match(key) {
                    attrs.push_str(&format!(
                        " {key}=\"{}\"",
                        escape_xml_attr(&meta[key])
                    ));
                }
            }
        }
    }
    format!(
        "<{tag} source=\"{}\"{attrs}>\n{content}\n</{tag}>",
        escape_xml_attr(server_name),
        tag = CHANNEL_TAG
    )
}

/// org policy 来源标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistSource {
    Org,
    Ledger,
}

/// 托管 org allowlist 优先于 GrowthBook ledger；未托管用户回退 ledger。
pub fn get_effective_channel_allowlist(
    sub: Option<&crate::utils::auth::SubscriptionType>,
    org_list: Option<&[ChannelAllowlistEntry]>,
) -> (Vec<ChannelAllowlistEntry>, AllowlistSource) {
    let is_org = matches!(
        sub,
        Some(crate::utils::auth::SubscriptionType::Team)
            | Some(crate::utils::auth::SubscriptionType::Enterprise)
    );
    if is_org {
        if let Some(org_list) = org_list {
            return (org_list.to_vec(), AllowlistSource::Org);
        }
    }
    (get_channel_allowlist(), AllowlistSource::Ledger)
}

/// 门控结果（TS `ChannelGateResult`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelGateResult {
    Register,
    Skip {
        kind: ChannelSkipKind,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSkipKind {
    Capability,
    Disabled,
    Auth,
    Policy,
    Session,
    Marketplace,
    Allowlist,
}

/// 服务器名（`server` 或 `plugin:X:Y` 形态）是否命中会话 --channels 条目。
pub fn find_channel_entry<'a>(
    server_name: &str,
    channels: &'a [ChannelEntry],
) -> Option<&'a ChannelEntry> {
    let parts: Vec<&str> = server_name.split(':').collect();
    channels.iter().find(|c| match c {
        ChannelEntry::Server { name, .. } => name == server_name,
        ChannelEntry::Plugin { name, .. } => {
            parts.first() == Some(&"plugin") && parts.get(1) == Some(&name.as_str())
        }
    })
}

/// channel 通知路径门控（TS `gateChannelServer`）。
///
/// 门控顺序：能力声明 `experimental['claude/channel']` → 会话 --channels
/// 条目 → plugin 条目的 marketplace 校验（条目 tag 与已安装来源必须一致）。
pub fn gate_channel_server(
    server_name: &str,
    capabilities: Option<&ServerCapabilities>,
    plugin_source: Option<&str>,
) -> ChannelGateResult {
    if !capabilities.map(|c| c.declares_channel()).unwrap_or(false) {
        return ChannelGateResult::Skip {
            kind: ChannelSkipKind::Capability,
            reason: "server did not declare claude/channel capability".to_string(),
        };
    }

    let channels = get_allowed_channels();
    let Some(entry) = find_channel_entry(server_name, &channels) else {
        return ChannelGateResult::Skip {
            kind: ChannelSkipKind::Session,
            reason: format!(
                "server {server_name} not in --channels list for this session"
            ),
        };
    };

    if let ChannelEntry::Plugin {
        name,
        marketplace,
        dev: _,
    } = entry
    {
        let actual = plugin_source.map(|s| parse_plugin_identifier(s).1).unwrap_or(None);
        if actual.as_deref() != Some(marketplace.as_str()) {
            return ChannelGateResult::Skip {
                kind: ChannelSkipKind::Marketplace,
                reason: format!(
                    "you asked for plugin:{name}@{marketplace} but the installed {name} plugin is from {}",
                    actual.as_deref().unwrap_or("an unknown source")
                ),
            };
        }
    }

    ChannelGateResult::Register
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn wraps_message_with_safe_meta() {
        let mut meta = HashMap::new();
        meta.insert("chat_id".to_string(), "a<b".to_string());
        meta.insert("bad key!".to_string(), "dropped".to_string());
        let wrapped = wrap_channel_message("tg", "hello", Some(&meta));
        assert!(wrapped.contains(r#"source="tg""#));
        assert!(wrapped.contains(r#"chat_id="a&lt;b""#));
        assert!(!wrapped.contains("bad key"));
        assert!(wrapped.starts_with("<channel "));
    }

    #[test]
    fn finds_server_and_plugin_entries() {
        let channels = vec![
            ChannelEntry::Server { name: "slack".to_string(), dev: false },
            ChannelEntry::Plugin {
                name: "telegram".to_string(),
                marketplace: "m".to_string(),
                dev: false,
            },
        ];
        assert!(find_channel_entry("slack", &channels).is_some());
        assert!(find_channel_entry("plugin:telegram:tg", &channels).is_some());
        assert!(find_channel_entry("plugin:other:tg", &channels).is_none());
    }

    #[test]
    fn gates_without_capability() {
        let caps = ServerCapabilities::default();
        assert!(matches!(
            gate_channel_server("x", Some(&caps), None),
            ChannelGateResult::Skip { kind: ChannelSkipKind::Capability, .. }
        ));
    }
}
