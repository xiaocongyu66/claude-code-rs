//! permission_update_schema.rs — 权限更新（PermissionUpdate）的 schema。
//!
//! 重写自 `src/utils/permissions/PermissionUpdateSchema.ts`。语义对齐说明：
//! - TS 侧是刻意保持最小依赖的 zod schema（供 `src/types/hooks.ts` 打破
//!   循环依赖）；Rust 侧同样只依赖 `types::permissions` 与同目录的
//!   `permission_mode` / `permission_rule` 两个解析函数；
//! - `z.discriminatedUnion('type', ...)` 改为按 `type` 字段分派的解析
//!   函数 `permission_update_schema`；
//! - `z.enum([...])` 的 destination 改为 `permission_update_destination_schema`。
//!
//! 类型本体（`PermissionUpdate` / `PermissionUpdateDestination`）提取自
//! `src/types/permissions.ts`（已重写为 `src/types/permissions.rs`）。

use crate::types::permissions::{
    PermissionUpdate, PermissionUpdateDestination, PermissionRuleValue,
};
use crate::utils::permissions::permission_mode::external_permission_mode_schema;
use crate::utils::permissions::permission_rule::{permission_behavior_schema, permission_rule_value_schema};

// 再出口（对齐 TS 的 `export type { PermissionUpdate, PermissionUpdateDestination }`）
pub use crate::types::permissions::{PermissionUpdate, PermissionUpdateDestination};

/// `permissionUpdateDestinationSchema`：解析权限更新的持久化目标。
///
/// - `userSettings`：用户设置（全局）
/// - `projectSettings`：项目设置（共享）
/// - `localSettings`：本地设置（gitignored）
/// - `session`：仅当前会话内存
/// - `cliArg`：来自命令行参数
pub fn permission_update_destination_schema(
    value: &serde_json::Value,
) -> Result<PermissionUpdateDestination, String> {
    match value.as_str() {
        Some("userSettings") => Ok(PermissionUpdateDestination::UserSettings),
        Some("projectSettings") => Ok(PermissionUpdateDestination::ProjectSettings),
        Some("localSettings") => Ok(PermissionUpdateDestination::LocalSettings),
        Some("session") => Ok(PermissionUpdateDestination::Session),
        Some("cliArg") => Ok(PermissionUpdateDestination::CliArg),
        Some(_) => Err(
            "Invalid enum value. Expected 'userSettings' | 'projectSettings' | 'localSettings' | 'session' | 'cliArg'"
                .to_string(),
        ),
        None => Err("Expected string, received non-string".to_string()),
    }
}

/// `permissionUpdateSchema`：解析 `z.discriminatedUnion('type', [...])`。
///
/// 三个规则变体（addRules / replaceRules / removeRules）共享
/// `{ rules, behavior, destination }` 形状；setMode 使用外部模式枚举；
/// addDirectories / removeDirectories 为字符串数组。
pub fn permission_update_schema(value: &serde_json::Value) -> Result<PermissionUpdate, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "Expected object, received non-object".to_string())?;

    let type_ = obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Discriminator field 'type' missing or not a string".to_string())?;

    let destination = permission_update_destination_schema(
        obj.get("destination")
            .ok_or_else(|| "Required field 'destination' missing".to_string())?,
    )?;

    match type_ {
        "addRules" | "replaceRules" | "removeRules" => {
            let behavior = permission_behavior_schema(
                obj.get("behavior")
                    .ok_or_else(|| "Required field 'behavior' missing".to_string())?,
            )?;
            let rules_v = obj
                .get("rules")
                .ok_or_else(|| "Required field 'rules' missing".to_string())?;
            let arr = rules_v
                .as_array()
                .ok_or_else(|| "Field 'rules' must be an array".to_string())?;
            let mut rules: Vec<PermissionRuleValue> = Vec::with_capacity(arr.len());
            for item in arr {
                rules.push(permission_rule_value_schema(item)?);
            }
            Ok(match type_ {
                "addRules" => PermissionUpdate::AddRules {
                    destination,
                    rules,
                    behavior,
                },
                "replaceRules" => PermissionUpdate::ReplaceRules {
                    destination,
                    rules,
                    behavior,
                },
                _ => PermissionUpdate::RemoveRules {
                    destination,
                    rules,
                    behavior,
                },
            })
        }
        "setMode" => {
            let mode = external_permission_mode_schema(
                obj.get("mode")
                    .ok_or_else(|| "Required field 'mode' missing".to_string())?,
            )?;
            Ok(PermissionUpdate::SetMode { destination, mode })
        }
        "addDirectories" | "removeDirectories" => {
            let dirs_v = obj
                .get("directories")
                .ok_or_else(|| "Required field 'directories' missing".to_string())?;
            let arr = dirs_v
                .as_array()
                .ok_or_else(|| "Field 'directories' must be an array".to_string())?;
            let mut directories: Vec<String> = Vec::with_capacity(arr.len());
            for item in arr {
                directories.push(
                    item.as_str()
                        .ok_or_else(|| "Directory entries must be strings".to_string())?
                        .to_string(),
                );
            }
            Ok(if type_ == "addDirectories" {
                PermissionUpdate::AddDirectories {
                    destination,
                    directories,
                }
            } else {
                PermissionUpdate::RemoveDirectories {
                    destination,
                    directories,
                }
            })
        }
        _ => Err(format!(
            "Invalid discriminator value for 'type': \"{type_}\""
        )),
    }
}
