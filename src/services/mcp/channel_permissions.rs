//! channel_permissions.rs — channel 权限回复中继
//! （重写自 `channelPermissions.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - GrowthBook 门控 `tengu_harbor_permissions` 默认 true（analytics 未转换 →
//!   占位返回默认值 true）。
//! - `PERMISSION_REPLY_RE`：`^\s*(y|yes|n|no)\s+([a-km-z]{5})\s*$`（导出供
//!   插件复用）。
//! - `short_request_id`：toolUseID 经 FNV-1a 哈希后按 25 字母表（去掉
//!   易混的 'l'）取 5 位；命中脏词子串时加盐重哈希（≤10 次）。
//! - `truncate_for_preview`：JSON 序列化后 200 字符截断。
//! - `filter_permission_relay_clients`：connected + 在 allowlist + 同时声明
//!   claude/channel 与 claude/channel/permission 能力。
//! - `create_channel_permission_callbacks`：pending Map（小写键）+
//!   onResponse/resolve，闭包持有而非 AppState/全局。
//!
//! 已知偏差：无（本模块为纯逻辑，依赖均在本目录内）。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::services::mcp::channel_notification::CHANNEL_PERMISSION_METHOD;
use crate::services::mcp::types::ServerCapabilities;

/// GrowthBook 门控键（默认开）。
pub const CHANNEL_PERMISSION_GATE_KEY: &str = "tengu_harbor_permissions";

/// 用户回复格式规范（导出供 channel 服务器实现对齐）。
pub const PERMISSION_REPLY_RE_STR: &str = r"^\s*(y|yes|n|no)\s+([a-km-z]{5})\s*$";

/// 已编译的回复格式正则（等价 TS `PERMISSION_REPLY_RE`）。
pub static PERMISSION_REPLY_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(PERMISSION_REPLY_RE_STR).ok());

/// 25 字母表：a-z 去掉 'l'（与 1/I 易混）。25^5 ≈ 9.8M 空间。
const ID_ALPHABET: &str = "abcdefghijkmnopqrstuvwxyz";

/// 脏词子串黑名单（5 个随机字母可能拼出不宜发到手机的词）。
const ID_AVOID_SUBSTRINGS: &[&str] = &[
    "fuck", "shit", "cunt", "cock", "dick", "twat", "piss", "crap", "bitch", "whore", "ass", "tit",
    "cum", "fag", "dyke", "nig", "kike", "rape", "nazi", "damn", "poo", "pee", "wank", "anus",
];

/// 权限回复（TS `ChannelPermissionResponse`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPermissionResponse {
    pub behavior: ChannelPermissionBehavior,
    /// 回复来源服务器（如 "plugin:telegram:tg"）。
    pub from_server: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelPermissionBehavior {
    Allow,
    Deny,
}

/// FNV-1a → u32 → base-25 取 5 位。
fn hash_to_id(input: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for byte in input.as_bytes() {
        h ^= *byte as u32;
        h = h.wrapping_mul(0x01000193);
    }
    let mut s = String::new();
    let alphabet = ID_ALPHABET.as_bytes();
    for _ in 0..5 {
        let idx = (h % 25) as usize;
        s.push(alphabet[idx] as char);
        h /= 25;
    }
    s
}

/// 由 toolUseID 生成 5 字母短 ID。
pub fn short_request_id(tool_use_id: &str) -> String {
    let mut candidate = hash_to_id(tool_use_id);
    for salt in 0..10 {
        if !ID_AVOID_SUBSTRINGS.iter().any(|bad| candidate.contains(bad)) {
            return candidate;
        }
        candidate = hash_to_id(&format!("{tool_use_id}:{salt}"));
    }
    candidate
}

/// 把工具输入截成手机屏大小的 JSON 预览（200 字符）。
pub fn truncate_for_preview(input: &Value) -> String {
    match serde_json::to_string(input) {
        Ok(s) => {
            if s.chars().count() > 200 {
                let truncated: String = s.chars().take(200).collect();
                format!("{truncated}…")
            } else {
                s
            }
        }
        Err(_) => "(unserializable)".to_string(),
    }
}

/// 客户端最小视图（connected 判定 + 能力声明），供泛型过滤。
pub trait ChannelClient {
    fn name(&self) -> &str;
    fn is_connected(&self) -> bool;
    fn declares_channel(&self) -> bool;
    fn declares_channel_permission(&self) -> bool;
}

/// 过滤可中继权限提示的客户端：connected + 在 allowlist + 同时声明两项
/// 能力。`is_in_allowlist` 由调用方提供（channel 门控结果）。
pub fn filter_permission_relay_clients<C: ChannelClient>(
    clients: &[C],
    is_in_allowlist: &dyn Fn(&str) -> bool,
) -> Vec<&C> {
    clients
        .iter()
        .filter(|c| {
            c.is_connected()
                && is_in_allowlist(c.name())
                && c.declares_channel()
                && c.declares_channel_permission()
        })
        .collect()
}

/// 从 MCPServerConnection 视图提取能力声明（供 filter_permission_relay_clients
/// 的调用方转接）。
pub fn client_declares(
    type_str: &str,
    capabilities: Option<&ServerCapabilities>,
    method: &str,
) -> bool {
    type_str == "connected"
        && capabilities
            .map(|c| match method {
                CHANNEL_PERMISSION_METHOD => c.declares_channel_permission(),
                _ => c.declares_channel(),
            })
            .unwrap_or(false)
}

/// 权限回调句柄（TS `ChannelPermissionCallbacks`）。pending Map 以
/// `Arc<Mutex<...>>` 持有：构造一次、稳定引用多处共享，注册者持克隆 ——
/// 不放 AppState（函数不能进状态）、不放模块级全局。
pub struct ChannelPermissionCallbacks {
    pending: std::sync::Arc<
        std::sync::Mutex<HashMap<String, Box<dyn Fn(ChannelPermissionResponse) + Send + Sync>>>,
    >,
}

/// 创建回调句柄（TS `createChannelPermissionCallbacks`）。
pub fn create_channel_permission_callbacks() -> ChannelPermissionCallbacks {
    ChannelPermissionCallbacks {
        pending: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    }
}

impl ChannelPermissionCallbacks {
    /// 注册 request ID 的 resolver；返回取消注册函数。
    pub fn on_response(
        &self,
        request_id: &str,
        handler: Box<dyn Fn(ChannelPermissionResponse) + Send + Sync>,
    ) -> impl Fn() + Send + Sync + 'static {
        let key = request_id.to_lowercase();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(key.clone(), handler);
        }
        let pending = self.pending.clone();
        move || {
            let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.remove(&key);
        }
    }

    /// 由结构化通知解决一个 pending 请求；返回是否命中。
    pub fn resolve(
        &self,
        request_id: &str,
        behavior: ChannelPermissionBehavior,
        from_server: &str,
    ) -> bool {
        let key = request_id.to_lowercase();
        let resolver = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            // 先删再调：resolver 抛错或重入时条目已清，重复事件也直接落空
            pending.remove(&key)
        };
        match resolver {
            Some(resolver) => {
                resolver(ChannelPermissionResponse {
                    behavior,
                    from_server: from_server.to_string(),
                });
                true
            }
            None => false,
        }
    }
}

/// 权限中继 GrowthBook 门控（analytics 未转换：占位返回默认 true）。
pub fn is_channel_permission_relay_enabled() -> bool {
    // TS: getFeatureValue_CACHED_MAY_BE_STALE('tengu_harbor_permissions', true)
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_ids_are_5_letters_no_l() {
        let id = short_request_id("toolu_abc123");
        assert_eq!(id.len(), 5);
        assert!(!id.contains('l'));
        // 稳定
        assert_eq!(short_request_id("toolu_abc123"), id);
    }

    #[test]
    fn avoids_blocklist() {
        for candidate in [
            "toolu_fUcKx", "toolu_shIt1", "toolu_nAzi9", "toolu_wAnK2",
        ] {
            let id = short_request_id(candidate);
            for bad in ID_AVOID_SUBSTRINGS {
                assert!(!id.to_lowercase().contains(bad), "{id} contains {bad}");
            }
        }
    }

    #[test]
    fn truncates_previews() {
        let short = json!({"a": 1});
        assert_eq!(truncate_for_preview(&short), "{\"a\":1}");
        let long = json!({"big": "x".repeat(300)});
        let preview = truncate_for_preview(&long);
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 201);
    }

    #[test]
    fn callbacks_resolve_and_unregister() {
        let cb = create_channel_permission_callbacks();
        let hit = std::sync::Arc::new(std::sync::Mutex::new(false));
        let hit_clone = hit.clone();
        let unregister = cb.on_response(
            "ABCDE",
            Box::new(move |_r| {
                *hit_clone.lock().unwrap() = true;
            }),
        );
        // 小写键也能命中
        assert!(cb.resolve("abcde", ChannelPermissionBehavior::Allow, "tg"));
        assert!(*hit.lock().unwrap());
        // 第二次（重复事件）落空
        assert!(!cb.resolve("ABCDE", ChannelPermissionBehavior::Allow, "tg"));
        drop(unregister);
    }
}
