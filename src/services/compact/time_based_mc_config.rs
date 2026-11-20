//! `services/compact/timeBasedMCConfig.ts` — 时间触发 microcompact 的
//! GrowthBook 配置。
//!
//! 语义对齐：`tengu_slate_heron` 未命中时使用默认值（enabled=false /
//! gap=60min / keepRecent=5）。analytics 层未转换，`get_feature_value`
//! 返回 None → 默认值。

use serde::{Deserialize, Serialize};

use crate::services::compact::get_feature_value_cached_may_be_stale;

/// TS `TimeBasedMCConfig`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeBasedMcConfig {
    /// 主开关；false 时时间触发 microcompact 为 no-op。
    pub enabled: bool,
    /// (now − 最后一条 assistant 时间戳) 超过该分钟数则触发。
    /// 60 是安全值：服务端 1h 缓存 TTL 保证已过期。
    pub gap_threshold_minutes: f64,
    /// 保留最近 N 个可压缩 tool 结果；设置后优先于任何默认值。
    pub keep_recent: usize,
}

/// TS `TIME_BASED_MC_CONFIG_DEFAULTS`。
const TIME_BASED_MC_CONFIG_DEFAULTS: TimeBasedMcConfig = TimeBasedMcConfig {
    enabled: false,
    gap_threshold_minutes: 60.0,
    keep_recent: 5,
};

/// TS `getTimeBasedMCConfig()`：GrowthBook 读取代默认值。
pub fn get_time_based_mc_config() -> TimeBasedMcConfig {
    // 与 TS 一致：把 GB 读取提前到每条评估路径（exposure 记账），未命中
    // 或 analytics 层不可用时回落默认值。
    match get_feature_value_cached_may_be_stale("tengu_slate_heron") {
        Some(value) => serde_json::from_value(value).unwrap_or(TIME_BASED_MC_CONFIG_DEFAULTS),
        None => TIME_BASED_MC_CONFIG_DEFAULTS,
    }
}
