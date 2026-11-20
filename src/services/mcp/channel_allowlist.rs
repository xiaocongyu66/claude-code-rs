//! channel_allowlist.rs — 已批准 channel 插件 allowlist
//! （重写自 `channelAllowlist.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - allowlist 数据来自 GrowthBook feature `tengu_harbor_ledger`
//!   （`services/analytics/growthbook.ts` 未转换 → 占位返回空列表，
//!   等价于"无合法 channel"，fail-closed）。
//! - `is_channels_enabled()` 恒为 true（总是启用，GrowthBook 门控旁路）。
//! - `is_channel_allowlisted(plugin_source)`：解析 `plugin:name@marketplace`
//!   形式的 pluginSource，{marketplace, plugin} 匹配 allowlist；内建
//!   `weixin` 插件直接放行。
//!
//! 已知偏差：
//! - `utils/plugins/pluginIdentifier.ts` / `plugins/builtinPlugins.ts` 未转换：
//!   本地实现 `parse_plugin_identifier` 与 `BUILTIN_MARKETPLACE_NAME` 常量。

use serde::{Deserialize, Serialize};

/// 内建 marketplace 名称（对齐 TS `BUILTIN_MARKETPLACE_NAME`）。
pub const BUILTIN_MARKETPLACE_NAME: &str = "claude-code";

/// allowlist 条目（TS `ChannelAllowlistEntry`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAllowlistEntry {
    pub marketplace: String,
    pub plugin: String,
}

/// 解析插件标识 `name@marketplace`；无 `@` 时 marketplace 为 None。
pub fn parse_plugin_identifier(source: &str) -> (String, Option<String>) {
    match source.split_once('@') {
        Some((name, marketplace)) if !name.is_empty() && !marketplace.is_empty() => {
            (name.to_string(), Some(marketplace.to_string()))
        }
        _ => (source.to_string(), None),
    }
}

/// GrowthBook feature 占位：analytics 模块未转换，返回默认值（对齐
/// `getFeatureValue_CACHED_MAY_BE_STALE('tengu_harbor_ledger', [])` 在无
/// 网络时的行为）。接入 analytics 后替换为真实门控。
fn growthbook_value(_key: &str) -> Option<serde_json::Value> {
    None
}

/// 读取 channel allowlist（GrowthBook `tengu_harbor_ledger`）。
pub fn get_channel_allowlist() -> Vec<ChannelAllowlistEntry> {
    let Some(raw) = growthbook_value("tengu_harbor_ledger") else {
        return Vec::new();
    };
    let entries: Vec<ChannelAllowlistEntry> = match serde_json::from_value(raw) {
        Ok(parsed) => parsed,
        Err(_) => Vec::new(),
    };
    entries
        .into_iter()
        .filter(|e| !e.marketplace.is_empty() && !e.plugin.is_empty())
        .collect()
}

/// channel 总开关：恒为 true（TS 同名函数明确绕过 GrowthBook 门控）。
pub fn is_channels_enabled() -> bool {
    true
}

/// 纯 allowlist 检查：pluginSource 为 `plugin:name@marketplace` 形式时
/// 才可能命中；无 `@`（内建/内联）或 undefined 直接 false。
pub fn is_channel_allowlisted(plugin_source: Option<&str>) -> bool {
    let Some(plugin_source) = plugin_source else {
        return false;
    };
    let (name, marketplace) = parse_plugin_identifier(plugin_source);
    let Some(marketplace) = marketplace else {
        return false;
    };
    if marketplace == BUILTIN_MARKETPLACE_NAME && name == "weixin" {
        return true;
    }
    get_channel_allowlist()
        .iter()
        .any(|e| e.plugin == name && e.marketplace == marketplace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plugin_identifiers() {
        assert_eq!(
            parse_plugin_identifier("slack@anthropic"),
            ("slack".to_string(), Some("anthropic".to_string()))
        );
        assert_eq!(
            parse_plugin_identifier("builtin"),
            ("builtin".to_string(), None)
        );
    }

    #[test]
    fn rejects_non_plugin_sources() {
        assert!(!is_channel_allowlisted(None));
        assert!(!is_channel_allowlisted(Some("builtin")));
    }

    #[test]
    fn builtin_weixin_allowed() {
        assert!(is_channel_allowlisted(Some("weixin@claude-code")));
    }
}
