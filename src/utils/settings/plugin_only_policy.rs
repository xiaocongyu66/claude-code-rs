//! plugin_only_policy.rs — `strictPluginOnlyCustomization` 策略判定。
//!
//! 重写自 `src/utils/settings/pluginOnlyPolicy.ts`。零文件依赖：只读取
//! `policySettings` 来源的 [`types::SettingsJson::strict_plugin_only_customization`]
//! 字段。
//!
//! 语义对齐：`true` 锁全部四个定制面；数组形式只锁列出的面；undefined/
//! 其它 → 不锁定。managed（policySettings）与 plugin 来源永远可信——
//! managed 来源由管理员控制，plugin 由 `strictKnownMarketplaces` 单独把关。

use crate::utils::settings::settings::get_settings_for_source;
use crate::utils::settings::types::{CUSTOMIZATION_SURFACES, StrictPluginOnlyCustomization};
use crate::bootstrap::state::SettingSource;

/// 定制面（skills / agents / hooks / mcp）。
pub type CustomizationSurface = &'static str;

/// 锁定的定制面集合（对齐 TS 的 CUSTOMIZATION_SURFACES）。
pub const CUSTOMIZATION_SURFACE_VALUES: &[&str] = CUSTOMIZATION_SURFACES;

/// 某定制面是否被 `strictPluginOnlyCustomization` 策略锁定为仅插件来源。
///
/// 「锁定」= 跳过用户级（~/.claude/*）与项目级（.claude/*）来源；managed
/// 与 plugin 来源始终加载。
pub fn is_restricted_to_plugin_only(surface: &str) -> bool {
    let Some(policy) = get_settings_for_source(SettingSource::PolicySettings) else {
        return false;
    };
    match policy.strict_plugin_only_customization {
        Some(StrictPluginOnlyCustomization::All(all)) => all,
        Some(StrictPluginOnlyCustomization::Surfaces(list)) => list.iter().any(|s| s == surface),
        None => false,
    }
}

/// 绕过 strictPluginOnlyCustomization 的来源（管理员可信来源）：
/// plugin（由 strictKnownMarketplaces 单独把关）、policySettings（managed
/// settings，管理员控制）、built-in / builtin / bundled（随 CLI 发行）。
/// 用户可控来源（userSettings / projectSettings / localSettings /
/// flagSettings / mcp / 未定义）在对应面被锁定时拦截。
const ADMIN_TRUSTED_SOURCES: &[&str] = &["plugin", "policySettings", "built-in", "builtin", "bundled"];

/// 定制项的来源是否为管理员可信来源（对齐 `isSourceAdminTrusted`）。
pub fn is_source_admin_trusted(source: Option<&str>) -> bool {
    match source {
        Some(s) => ADMIN_TRUSTED_SOURCES.contains(&s),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_trusted_sources() {
        assert!(is_source_admin_trusted(Some("plugin")));
        assert!(is_source_admin_trusted(Some("policySettings")));
        assert!(is_source_admin_trusted(Some("built-in")));
        assert!(is_source_admin_trusted(Some("builtin")));
        assert!(is_source_admin_trusted(Some("bundled")));
        assert!(!is_source_admin_trusted(Some("userSettings")));
        assert!(!is_source_admin_trusted(Some("mcp")));
        assert!(!is_source_admin_trusted(None));
    }

    #[test]
    fn no_policy_means_unlocked() {
        // 无 managed settings 的环境下，任何面都不锁定
        for surface in CUSTOMIZATION_SURFACE_VALUES {
            assert!(!is_restricted_to_plugin_only(surface));
        }
    }
}
