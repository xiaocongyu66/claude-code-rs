//! permission_mode.rs — 权限模式的 schema、配置表与显示辅助函数。
//!
//! 重写自 `src/utils/permissions/PermissionMode.ts`。语义对齐说明：
//! - zod schema（`permissionModeSchema` / `externalPermissionModeSchema`）
//!   改为从 `serde_json::Value` 解析的函数；`PERMISSION_MODES` /
//!   `EXTERNAL_PERMISSION_MODES` 常量来自 `src/types/permissions.rs`；
//! - `PERMISSION_MODE_CONFIG` 在 TS 里是模块加载时求值的 `Partial<Record>`
//!   字面量，Rust 侧改为 `mode_config()` 的 match（`getModeConfig` 的
//!   `?? default` 兜底隐含在 match 的完整覆盖中——`bubble` 没有配置项，
//!   回落 default，与 TS 一致）；
//! - `isExternalPermissionMode` 读取 `process.env.USER_TYPE`（调用时求值），
//!   Rust 侧对齐为 `std::env::var("USER_TYPE")` 调用时读取；
//! - `PAUSE_ICON` 来自已转换的 `src/constants/figures.rs`。

use crate::constants::figures::PAUSE_ICON;
use crate::types::permissions::{ExternalPermissionMode, PermissionMode};

// 再出口（对齐 TS 的 `export { EXTERNAL_PERMISSION_MODES, PERMISSION_MODES, ... }`）
pub use crate::types::permissions::{EXTERNAL_PERMISSION_MODES, PERMISSION_MODES};

/// 模式在 UI 中使用的颜色键（TS `ModeColorKey`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeColorKey {
    Text,
    PlanMode,
    Permission,
    AutoAccept,
    Error,
    Warning,
}

/// 单个权限模式的显示配置（TS `PermissionModeConfig`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionModeConfig {
    pub title: &'static str,
    pub short_title: &'static str,
    pub symbol: &'static str,
    pub color: ModeColorKey,
    pub external: ExternalPermissionMode,
}

/// TS 的 `PERMISSION_MODE_CONFIG`；`bubble` 无配置，回落 default。
fn mode_config(mode: PermissionMode) -> PermissionModeConfig {
    match mode {
        PermissionMode::Default => PermissionModeConfig {
            title: "Default",
            short_title: "Default",
            symbol: "",
            color: ModeColorKey::Text,
            external: ExternalPermissionMode::Default,
        },
        PermissionMode::Plan => PermissionModeConfig {
            title: "Plan Mode",
            short_title: "Plan",
            symbol: PAUSE_ICON,
            color: ModeColorKey::PlanMode,
            external: ExternalPermissionMode::Plan,
        },
        PermissionMode::AcceptEdits => PermissionModeConfig {
            title: "Accept edits",
            short_title: "Accept",
            symbol: "⏵⏵",
            color: ModeColorKey::AutoAccept,
            external: ExternalPermissionMode::AcceptEdits,
        },
        PermissionMode::BypassPermissions => PermissionModeConfig {
            title: "Bypass",
            short_title: "Bypass",
            symbol: "⏵⏵",
            color: ModeColorKey::Error,
            external: ExternalPermissionMode::BypassPermissions,
        },
        PermissionMode::DontAsk => PermissionModeConfig {
            title: "Don't Ask",
            short_title: "DontAsk",
            symbol: "⏵⏵",
            color: ModeColorKey::Error,
            external: ExternalPermissionMode::DontAsk,
        },
        PermissionMode::Auto => PermissionModeConfig {
            title: "Auto",
            short_title: "Auto",
            symbol: "⏵⏵",
            // TS: 'warning' as ModeColorKey
            color: ModeColorKey::Warning,
            // TS: 'default' as ExternalPermissionMode
            external: ExternalPermissionMode::Default,
        },
        // TS: PERMISSION_MODE_CONFIG 取不到 bubble → 回落 default
        PermissionMode::Bubble => PermissionModeConfig {
            title: "Default",
            short_title: "Default",
            symbol: "",
            color: ModeColorKey::Text,
            external: ExternalPermissionMode::Default,
        },
    }
}

/// `permissionModeSchema`：`z.enum(PERMISSION_MODES)`。
pub fn permission_mode_schema(value: &serde_json::Value) -> Result<PermissionMode, String> {
    match value.as_str() {
        Some(s) => {
            if PERMISSION_MODES.contains(&s) {
                Ok(permission_mode_from_string(s))
            } else {
                Err(format!(
                    "Invalid enum value. Expected one of {}, received \"{s}\"",
                    PERMISSION_MODES
                        .iter()
                        .map(|m| format!("\"{m}\""))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ))
            }
        }
        None => Err("Expected string, received non-string".to_string()),
    }
}

/// `externalPermissionModeSchema`：`z.enum(EXTERNAL_PERMISSION_MODES)`。
pub fn external_permission_mode_schema(
    value: &serde_json::Value,
) -> Result<ExternalPermissionMode, String> {
    match value.as_str() {
        Some(s) => {
            if EXTERNAL_PERMISSION_MODES.contains(&s) {
                Ok(match s {
                    "acceptEdits" => ExternalPermissionMode::AcceptEdits,
                    "bypassPermissions" => ExternalPermissionMode::BypassPermissions,
                    "default" => ExternalPermissionMode::Default,
                    "dontAsk" => ExternalPermissionMode::DontAsk,
                    "plan" => ExternalPermissionMode::Plan,
                    _ => ExternalPermissionMode::Default,
                })
            } else {
                Err(format!(
                    "Invalid enum value. Expected one of {}, received \"{s}\"",
                    EXTERNAL_PERMISSION_MODES
                        .iter()
                        .map(|m| format!("\"{m}\""))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ))
            }
        }
        None => Err("Expected string, received non-string".to_string()),
    }
}

/// 类型守卫：该模式是否为外部可寻址模式。
/// `auto` 为 ant-only，排除在外；非 ant 用户恒真。
pub fn is_external_permission_mode(mode: PermissionMode) -> bool {
    // External users can't have auto, so always true for them
    if std::env::var("USER_TYPE").map(|v| v != "ant").unwrap_or(true) {
        return true;
    }
    !matches!(mode, PermissionMode::Auto | PermissionMode::Bubble)
}

pub fn to_external_permission_mode(mode: PermissionMode) -> ExternalPermissionMode {
    mode_config(mode).external
}

/// `permissionModeFromString`：不在 `PERMISSION_MODES` 内的字符串回落
/// `'default'`。注意 `PERMISSION_MODES` 不包含 `'bubble'`。
pub fn permission_mode_from_string(str: &str) -> PermissionMode {
    match str {
        "acceptEdits" => PermissionMode::AcceptEdits,
        "bypassPermissions" => PermissionMode::BypassPermissions,
        "default" => PermissionMode::Default,
        "dontAsk" => PermissionMode::DontAsk,
        "plan" => PermissionMode::Plan,
        "auto" => PermissionMode::Auto,
        _ => PermissionMode::Default,
    }
}

pub fn permission_mode_title(mode: PermissionMode) -> &'static str {
    mode_config(mode).title
}

/// `isDefaultMode`：`mode === 'default' || mode === undefined`。
pub fn is_default_mode(mode: Option<PermissionMode>) -> bool {
    matches!(mode, Some(PermissionMode::Default)) || mode.is_none()
}

pub fn permission_mode_short_title(mode: PermissionMode) -> &'static str {
    mode_config(mode).short_title
}

pub fn permission_mode_symbol(mode: PermissionMode) -> &'static str {
    mode_config(mode).symbol
}

pub fn get_mode_color(mode: PermissionMode) -> ModeColorKey {
    mode_config(mode).color
}
