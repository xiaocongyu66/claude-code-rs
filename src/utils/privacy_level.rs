//! privacy_level.rs — 隐私级别控制。
//!
//! 重写自 `src/utils/privacyLevel.ts`。控制 Claude Code 产生多少
//! 非必要网络流量与遥测。
//!
//! 级别按严格程度排序：default < no-telemetry < essential-traffic。
//! 取所有信号中最严格的一级：
//!   CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC → essential-traffic
//!   DISABLE_TELEMETRY                        → no-telemetry

use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyLevel {
    /// 全部启用。
    Default,
    /// 分析/遥测关闭（Datadog、1P 事件、反馈问卷）。
    NoTelemetry,
    /// 所有非必要网络流量关闭（遥测 + 自动更新、grove、release notes、
    /// model capabilities 等）。
    EssentialTraffic,
}

pub fn get_privacy_level() -> PrivacyLevel {
    if env::var_os("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").is_some() {
        return PrivacyLevel::EssentialTraffic;
    }
    if env::var_os("DISABLE_TELEMETRY").is_some() {
        return PrivacyLevel::NoTelemetry;
    }
    PrivacyLevel::Default
}

/// 是否应压制全部非必要网络流量（等价旧的
/// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` 检查）。
pub fn is_essential_traffic_only() -> bool {
    matches!(get_privacy_level(), PrivacyLevel::EssentialTraffic)
}

/// 是否应压制遥测/分析（no-telemetry 与 essential-traffic 均为 true）。
pub fn is_telemetry_disabled() -> bool {
    !matches!(get_privacy_level(), PrivacyLevel::Default)
}

/// 导致当前 essential-traffic 限制的环境变量名；未受限时返回 None。
/// 用于面向用户的"取消设置 X 以重新启用"提示。
pub fn get_essential_traffic_only_reason() -> Option<&'static str> {
    if env::var_os("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").is_some() {
        return Some("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_level_unrestricted() {
        // CI/测试环境通常不带这些变量；带了则此用例跳过语义检查
        if env::var_os("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").is_some()
            || env::var_os("DISABLE_TELEMETRY").is_some()
        {
            return;
        }
        assert_eq!(get_privacy_level(), PrivacyLevel::Default);
        assert!(!is_essential_traffic_only());
        assert!(!is_telemetry_disabled());
        assert!(get_essential_traffic_only_reason().is_none());
    }
}
