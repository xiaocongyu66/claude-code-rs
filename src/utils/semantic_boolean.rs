//! semantic_boolean.rs — 也接受 "true"/"false" 字面量的布尔语义。
//!
//! 重写自 `src/utils/semanticBoolean.ts`（TS 端为 z.preprocess 包装的
//! Zod schema）。工具入参来自模型生成的 JSON，模型偶尔会把布尔值
//! 引号化 —— `"replace_all":"false"` —— 而 z.boolean() 会以类型错误拒绝；
//! z.coerce.boolean() 又错在按 JS 真值转换（"false" → true）。
//!
//! Cargo.toml 无 zod 等价依赖，此移植提供等价的纯函数式归一化：
//! 字符串 "true"/"false" → 布尔；其余值原样返回，由调用方按
//! `Value::Bool` 校验。

use serde_json::Value;

/// 归一化语义布尔：`"true"` → true、`"false"` → false，其余原样返回。
pub fn normalize_semantic_boolean(value: &Value) -> Value {
    match value {
        Value::String(s) if s == "true" => Value::Bool(true),
        Value::String(s) if s == "false" => Value::Bool(false),
        other => other.clone(),
    }
}

/// 归一化后要求布尔类型；非布尔返回 None（对齐 z.boolean() 的拒绝）。
pub fn parse_semantic_boolean(value: &Value) -> Option<bool> {
    match normalize_semantic_boolean(value) {
        Value::Bool(b) => Some(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coerces_quoted_booleans() {
        assert_eq!(parse_semantic_boolean(&json!("true")), Some(true));
        assert_eq!(parse_semantic_boolean(&json!("false")), Some(false));
        assert_eq!(parse_semantic_boolean(&json!(true)), Some(true));
        assert_eq!(parse_semantic_boolean(&json!(false)), Some(false));
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(parse_semantic_boolean(&json!(1)), None);
        assert_eq!(parse_semantic_boolean(&json!(null)), None);
        assert_eq!(parse_semantic_boolean(&json!("yes")), None);
        assert_eq!(parse_semantic_boolean(&json!("")), None);
    }
}
