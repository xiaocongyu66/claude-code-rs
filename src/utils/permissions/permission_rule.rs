//! permission_rule.rs — 权限规则的 Zod schema 对应物。
//!
//! 重写自 `src/utils/permissions/PermissionRule.ts`。TS 侧用 `lazySchema`
//! 包装 zod schema；Rust 无 zod，schema 语义改为「从 `serde_json::Value`
//! 解析并校验」的函数：
//! - `permission_behavior_schema` 对应 `z.enum(['allow', 'deny', 'ask'])`
//! - `permission_rule_value_schema` 对应
//!   `z.object({ toolName: z.string(), ruleContent: z.string().optional() })`
//!
//! 类型本体（`PermissionBehavior` / `PermissionRule` /
//! `PermissionRuleSource` / `PermissionRuleValue`）提取自
//! `src/types/permissions.ts`（已重写为 `src/types/permissions.rs`），
//! 此处 `pub use` 保持再出口兼容。
//!
//! 校验行为对齐：
//! - zod enum 只接受精确字符串，非字符串或不匹配返回错误；
//! - zod object 剥离未知键（Rust 侧直接忽略），缺 `toolName` 报错，
//!   `ruleContent` 缺省时为 `None`。

// 再出口（对齐 TS 的 `export type { ... } from '../../types/permissions.js'`）
pub use crate::types::permissions::{
    PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
};

/// `permissionBehaviorSchema`：解析 `'allow' | 'deny' | 'ask'`。
pub fn permission_behavior_schema(value: &serde_json::Value) -> Result<PermissionBehavior, String> {
    match value.as_str() {
        Some("allow") => Ok(PermissionBehavior::Allow),
        Some("deny") => Ok(PermissionBehavior::Deny),
        Some("ask") => Ok(PermissionBehavior::Ask),
        Some(_) => Err(
            "Invalid enum value. Expected 'allow' | 'deny' | 'ask', received another string"
                .to_string(),
        ),
        None => Err("Expected string, received non-string".to_string()),
    }
}

/// `permissionRuleValueSchema`：解析
/// `{ toolName: string, ruleContent?: string }`。
pub fn permission_rule_value_schema(
    value: &serde_json::Value,
) -> Result<PermissionRuleValue, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "Expected object, received non-object".to_string())?;

    let tool_name = obj
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Required field 'toolName' missing or not a string".to_string())?;

    let rule_content = match obj.get("ruleContent") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| "Field 'ruleContent' must be a string".to_string())?
                .to_string(),
        ),
    };

    Ok(PermissionRuleValue {
        tool_name: tool_name.to_string(),
        rule_content,
    })
}
