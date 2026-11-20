//! sink_killswitch.rs — per-sink analytics killswitch（重写自
//! `src/services/analytics/sinkKillswitch.ts`）。
//!
//! 职责（对齐 TS）：
//! - `isSinkKilled(sink)`：读取 GrowthBook 动态配置 `tengu_frond_boric`
//!   （shape `{ datadog?: boolean, firstParty?: boolean }`），对应键为
//!   `true` 时该 sink 全部分发停止。
//! - Fail-open：配置缺失/损坏时 sink 保持开启。
//!
//! 注意（对齐 TS 注释）：不得在 `is1PEventLoggingEnabled()` 内部调用——
//! `isGrowthBookEnabled()` 会调用它，会递归。应在 per-event 分发点调用。

use serde_json::Value;

use super::growthbook::get_dynamic_config_cached_may_be_stale;

/// Mangled name：per-sink analytics killswitch（TS 常量）。
const SINK_KILLSWITCH_CONFIG_NAME: &str = "tengu_frond_boric";

/// Sink 名称（TS `SinkName`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SinkName {
    Datadog,
    FirstParty,
}

impl SinkName {
    /// 配置 JSON 中的键名（对齐 TS `Partial<Record<SinkName, boolean>>`）。
    pub fn as_str(self) -> &'static str {
        match self {
            SinkName::Datadog => "datadog",
            SinkName::FirstParty => "firstParty",
        }
    }
}

/// 该 sink 是否已被 killswitch 关闭。
///
/// GrowthBook 的 `_CACHED_MAY_BE_STALE` 读取对 `undefined` 做了守卫，
/// 但缓存的 JSON `null` 会漏过而不是回落默认值——Rust 侧用
/// `Value::Null` 默认值表达同一情况，`null.as_bool()` 返回 `None`，
/// fail-open 保持开启。
pub fn is_sink_killed(sink: SinkName) -> bool {
    let config = get_dynamic_config_cached_may_be_stale(SINK_KILLSWITCH_CONFIG_NAME, Value::Null);
    config.get(sink.as_str()).and_then(|v| v.as_bool()) == Some(true)
}
