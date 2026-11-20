//! elicitation_validation.rs — elicitation 输入校验（重写自
//! `src/utils/mcp/elicitationValidation.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - TS 用 zod schema 做校验；Rust 无 zod，按 zod 的实际判定语义手写实现：
//!   - string：minLength/MaxLength（按 UTF-16 码元计数，对齐 JS `length`）
//!     + format（email / uri / date / date-time）校验，失败消息逐条以
//!     `"; "` 拼接（zod safeParse 的 issues 行为）。
//!   - date-time：zod >= 4.5 要求带 Z 或偏移时必须含秒；这里用 union
//!     兼容分钟精度（`2024-03-15T14:30+08:00`）与秒精度两种形式，与
//!     TS 的两分支 union 一致。
//!   - number/integer：`z.coerce.number()` 的 JS `Number()` 语义（空串
//!     归 0、解析失败为 NaN），int/min/max 校验；错误消息统一为单条
//!     `range_msg`（对齐 TS 的 `{ error: rangeMsg }` 用法）。
//!   - boolean：`z.coerce.boolean()` 的 JS `Boolean()` 语义——空串 false、
//!     非空 true（注意 "false" 也是 true，与 JS 一致）。
//!   - enum：单选（`enum` / `oneOf`）与多选（array + `items.enum` /
//!     `items.anyOf`）的值/标签取值函数。
//! - `getZodSchema` 对未知 schema 类型 `throw new Error`：Rust 端用
//!   `Result` 的 `UnsupportedSchema` 错误表达（签名上比 TS 多一个 Err
//!   分支，调用方需处理）。
//! - `@modelcontextprotocol/sdk/types.js` 的 schema 类型按 TS 实际用到的
//!   字段做 duck-type struct（统一扁平 struct + 可选字段，字段存在性等
//!   价 TS 的 `'x' in schema` 判定）。
//!
//! 已知偏差：
//! - `utils/messages.ts` 未转换：本模块不依赖它，无影响。
//! - `validate_elicitation_input_async` 依赖 `date_time_parser`，后者目前
//!   因 `query_haiku` HTTP 未接入而始终失败（异步回退路径形同 TS 测试中
//!   的 mock 行为）。

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::utils::mcp::date_time_parser::{
    looks_like_iso8601, parse_natural_language_date_time, DateTimeParseFormat, DateTimeParseResult,
};
use crate::utils::abort_controller::CancellationToken;
use crate::utils::slow_operations::json_stringify;
use crate::utils::string_utils::plural;

/// 校验结果（对齐 TS `ValidationResult`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationResult {
    /// 成功时为解析出的原始值（string / number / boolean）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<PrimitiveValue>,
    pub is_valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// elicitation 的原始值类型（对齐 TS 结果联合）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrimitiveValue {
    String(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Debug, Error)]
pub enum ElicitationValidationError {
    /// 对齐 TS `throw new Error('Unsupported schema: ...')`。
    #[error("Unsupported schema: {0}")]
    UnsupportedSchema(String),
}

/// enum 常量项（`oneOf` / `anyOf` 元素：`{ const, title? }`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnumOption {
    #[serde(default, rename = "const")]
    pub const_value: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// 多选 enum 的 `items`（`{ enum: string[] }` 或 `{ anyOf: EnumOption[] }`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemsSchema {
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default, rename = "anyOf")]
    pub any_of: Option<Vec<EnumOption>>,
}

/// MCP elicitation 的原始 schema 定义（duck-type 对齐
/// `@modelcontextprotocol/sdk` 的 `PrimitiveSchemaDefinition` 及其成员）。
///
/// 字段存在性（`Option::is_some`）等价 TS 的 `'x' in schema` 判定。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrimitiveSchemaDefinition {
    #[serde(rename = "type")]
    pub typ: String,
    pub format: Option<String>,
    #[serde(rename = "minLength")]
    pub min_length: Option<u64>,
    #[serde(rename = "maxLength")]
    pub max_length: Option<u64>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub description: Option<String>,
    #[serde(rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(rename = "enumNames")]
    pub enum_names: Option<Vec<String>>,
    #[serde(rename = "oneOf")]
    pub one_of: Option<Vec<EnumOption>>,
    pub items: Option<ItemsSchema>,
}

const STRING_FORMATS_EMAIL: (&str, &str) = ("email address", "user@example.com");
const STRING_FORMATS_URI: (&str, &str) = ("URI", "https://example.com");
const STRING_FORMATS_DATE: (&str, &str) = ("date", "2024-03-15");
const STRING_FORMATS_DATE_TIME: (&str, &str) = ("date-time", "2024-03-15T14:30:00Z");

static EMAIL_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    // rust regex 不支持 look-around：去掉 TS 正则的前瞻断言（开头非点 /
    // 无连续点），改由运行时两个前置检查补齐（见 email 校验入口）
    Regex::new(
        r"^([A-Za-z0-9_'+\-\.]*)[A-Za-z0-9_+-]@([A-Za-z0-9][A-Za-z0-9\-]*\.)+[A-Za-z]{2,}$",
    )
    .ok()
});

static URI_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z][A-Za-z0-9+\-.]*:(?://[^\s/@]+(:\d+)?([/?#]\S*)?|[^\s/]+)$").ok()
});

static DATE_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").ok());

/// zod datetime union（秒精度 | 分钟精度），offset: true，必须带 Z 或偏移。
static DATE_TIME_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?([zZ]|[+-]\d{2}:\d{2})$").ok()
});

fn is_match(re: &Lazy<Option<Regex>>, input: &str) -> bool {
    match &**re {
        Some(regex) => regex.is_match(input),
        None => false,
    }
}

/// 是否为单选 enum（对齐 TS `isEnumSchema`）。
pub fn is_enum_schema(schema: &PrimitiveSchemaDefinition) -> bool {
    schema.typ == "string" && (schema.enum_values.is_some() || schema.one_of.is_some())
}

/// 是否为多选 enum（对齐 TS `isMultiSelectEnumSchema`）。
pub fn is_multi_select_enum_schema(schema: &PrimitiveSchemaDefinition) -> bool {
    schema.typ == "array"
        && schema.items.is_some()
        && schema
            .items
            .as_ref()
            .map(|items| items.enum_values.is_some() || items.any_of.is_some())
            .unwrap_or(false)
}

/// 多选 enum 的候选值（对齐 TS `getMultiSelectValues`）。
pub fn get_multi_select_values(schema: &PrimitiveSchemaDefinition) -> Vec<String> {
    let Some(items) = &schema.items else {
        return Vec::new();
    };
    if let Some(any_of) = &items.any_of {
        return any_of
            .iter()
            .map(|item| item.const_value.clone().unwrap_or_default())
            .collect();
    }
    if let Some(enum_values) = &items.enum_values {
        return enum_values.clone();
    }
    Vec::new()
}

/// 多选 enum 的显示标签（对齐 TS `getMultiSelectLabels`）。
pub fn get_multi_select_labels(schema: &PrimitiveSchemaDefinition) -> Vec<Option<String>> {
    let Some(items) = &schema.items else {
        return Vec::new();
    };
    if let Some(any_of) = &items.any_of {
        return any_of.iter().map(|item| item.title.clone()).collect();
    }
    if let Some(enum_values) = &items.enum_values {
        return enum_values.iter().map(|v| Some(v.clone())).collect();
    }
    Vec::new()
}

/// 多选 enum 中某值对应的标签（对齐 TS `getMultiSelectLabel`）。
pub fn get_multi_select_label(schema: &PrimitiveSchemaDefinition, value: &str) -> String {
    let index = get_multi_select_values(schema)
        .iter()
        .position(|v| v == value);
    match index {
        Some(i) => match get_multi_select_labels(schema).get(i) {
            Some(Some(label)) => label.clone(),
            _ => value.to_string(),
        },
        None => value.to_string(),
    }
}

/// 单选 enum 的候选值（对齐 TS `getEnumValues`，兼容 `oneOf` 与 `enum`）。
pub fn get_enum_values(schema: &PrimitiveSchemaDefinition) -> Vec<String> {
    if let Some(one_of) = &schema.one_of {
        return one_of
            .iter()
            .map(|item| item.const_value.clone().unwrap_or_default())
            .collect();
    }
    if let Some(enum_values) = &schema.enum_values {
        return enum_values.clone();
    }
    Vec::new()
}

/// 单选 enum 的显示标签（对齐 TS `getEnumLabels`，`enumNames` 优先）。
pub fn get_enum_labels(schema: &PrimitiveSchemaDefinition) -> Vec<String> {
    if let Some(one_of) = &schema.one_of {
        return one_of
            .iter()
            .map(|item| item.title.clone().unwrap_or_default())
            .collect();
    }
    if let Some(enum_values) = &schema.enum_values {
        return schema
            .enum_names
            .clone()
            .unwrap_or_else(|| enum_values.clone());
    }
    Vec::new()
}

/// 单选 enum 中某值对应的标签（对齐 TS `getEnumLabel`）。
pub fn get_enum_label(schema: &PrimitiveSchemaDefinition, value: &str) -> String {
    let index = get_enum_values(schema).iter().position(|v| v == value);
    match index {
        Some(i) => get_enum_labels(schema)
            .get(i)
            .cloned()
            .unwrap_or_else(|| value.to_string()),
        None => value.to_string(),
    }
}

/// 格式化边界数值（对齐 TS 的 `formatNum`：number schema 的整数值补 ".0"）。
fn format_num(n: f64, is_integer: bool) -> String {
    if n.fract() == 0.0 && !is_integer {
        format!("{:.1}", n)
    } else {
        format!("{}", n)
    }
}

/// JS `Number()` 强制转换语义（空串/空白 → 0；解析失败 → None 表示 NaN）。
fn coerce_number(input: &str) -> Option<f64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    trimmed.parse::<f64>().ok()
}

fn ok_result(value: PrimitiveValue) -> ValidationResult {
    ValidationResult {
        value: Some(value),
        is_valid: true,
        error: None,
    }
}

fn error_result(messages: Vec<String>) -> ValidationResult {
    ValidationResult {
        value: None,
        is_valid: false,
        error: Some(messages.join("; ")),
    }
}

/// 同步校验（对齐 TS `validateElicitationInput`）。
///
/// 未知 schema 类型返回 `UnsupportedSchema`（TS 端为 `throw`）。
pub fn validate_elicitation_input(
    string_value: &str,
    schema: &PrimitiveSchemaDefinition,
) -> Result<ValidationResult, ElicitationValidationError> {
    if is_enum_schema(schema) {
        let values = get_enum_values(schema);
        if values.is_empty() {
            // z.never()
            return Ok(error_result(vec![
                "Invalid input: expected never".to_string()
            ]));
        }
        if values.iter().any(|v| v == string_value) {
            return Ok(ok_result(PrimitiveValue::String(string_value.to_string())));
        }
        let quoted: Vec<String> = values.iter().map(|v| format!("'{v}'")).collect();
        return Ok(error_result(vec![format!(
            "Invalid enum value. Expected {}, received '{}'",
            quoted.join(" | "),
            string_value
        )]));
    }

    if schema.typ == "string" {
        // 字符串长度按 UTF-16 码元计数（对齐 JS string.length）
        let len = string_value.encode_utf16().count();
        let mut errors: Vec<String> = Vec::new();
        if let Some(min) = schema.min_length {
            if (len as u64) < min {
                errors.push(format!(
                    "Must be at least {} {}",
                    min,
                    plural(min, "character", None)
                ));
            }
        }
        if let Some(max) = schema.max_length {
            if (len as u64) > max {
                errors.push(format!(
                    "Must be at most {} {}",
                    max,
                    plural(max, "character", None)
                ));
            }
        }
        match schema.format.as_deref() {
            Some("email") => {
                // 补齐 TS 前瞻断言语义：不得以点开头、不得含连续点
                if string_value.starts_with('.') || string_value.contains("..")
                    || !is_match(&EMAIL_RE, string_value)
                {
                    errors.push(
                        "Must be a valid email address, e.g. user@example.com".to_string(),
                    );
                }
            }
            Some("uri") => {
                if !is_match(&URI_RE, string_value) {
                    errors.push("Must be a valid URI, e.g. https://example.com".to_string());
                }
            }
            Some("date") => {
                if !is_match(&DATE_RE, string_value) {
                    errors.push(
                        "Must be a valid date, e.g. 2024-03-15, today, next Monday".to_string(),
                    );
                }
            }
            Some("date-time") => {
                if !is_match(&DATE_TIME_RE, string_value) {
                    errors.push(
                        "Must be a valid date-time, e.g. 2024-03-15T14:30:00Z, tomorrow at 3pm"
                            .to_string(),
                    );
                }
            }
            _ => {}
        }
        if errors.is_empty() {
            return Ok(ok_result(PrimitiveValue::String(string_value.to_string())));
        }
        return Ok(error_result(errors));
    }

    if schema.typ == "number" || schema.typ == "integer" {
        let is_integer = schema.typ == "integer";
        let type_label = if is_integer { "an integer" } else { "a number" };

        // 单条描述性范围消息（对齐 TS 的 rangeMsg）
        let range_msg = match (schema.minimum, schema.maximum) {
            (Some(min), Some(max)) => format!(
                "Must be {} between {} and {}",
                type_label,
                format_num(min, is_integer),
                format_num(max, is_integer)
            ),
            (Some(min), None) => {
                format!("Must be {} >= {}", type_label, format_num(min, is_integer))
            }
            (None, Some(max)) => {
                format!("Must be {} <= {}", type_label, format_num(max, is_integer))
            }
            (None, None) => format!("Must be {type_label}"),
        };

        let Some(value) = coerce_number(string_value) else {
            // coerce 失败（NaN）：zod 在此中止，仅一条 rangeMsg
            return Ok(error_result(vec![range_msg]));
        };

        let mut errors: Vec<String> = Vec::new();
        if is_integer && value.fract() != 0.0 {
            errors.push(range_msg.clone());
        }
        if let Some(min) = schema.minimum {
            if value < min {
                errors.push(range_msg.clone());
            }
        }
        if let Some(max) = schema.maximum {
            if value > max {
                errors.push(range_msg.clone());
            }
        }
        if errors.is_empty() {
            return Ok(ok_result(PrimitiveValue::Number(value)));
        }
        return Ok(error_result(errors));
    }

    if schema.typ == "boolean" {
        // z.coerce.boolean()：JS Boolean(str) —— 空串 false，非空 true
        return Ok(ok_result(PrimitiveValue::Boolean(!string_value.is_empty())));
    }

    Err(ElicitationValidationError::UnsupportedSchema(json_stringify(
        &serde_json::to_value(schema).unwrap_or(serde_json::Value::Null),
    )))
}

/// 是否为 date / date-time 格式（对齐 TS `isDateTimeSchema`）。
pub fn is_date_time_schema(schema: &PrimitiveSchemaDefinition) -> bool {
    schema.typ == "string"
        && matches!(schema.format.as_deref(), Some("date") | Some("date-time"))
}

/// 返回 format 对应的输入提示（对齐 TS `getFormatHint`）。
///
/// 注意：TS 对 STRING_FORMATS 未收录的 format 返回
/// "undefined, e.g. undefined"（模板字符串拼接 undefined），此处保留
/// 该行为。
pub fn get_format_hint(schema: &PrimitiveSchemaDefinition) -> Option<String> {
    if schema.typ == "string" {
        let format = schema.format.as_deref()?;
        let (description, example) = match format {
            "email" => STRING_FORMATS_EMAIL,
            "uri" => STRING_FORMATS_URI,
            "date" => STRING_FORMATS_DATE,
            "date-time" => STRING_FORMATS_DATE_TIME,
            _ => ("undefined", "undefined"),
        };
        return Some(format!("{description}, e.g. {example}"));
    }
    if schema.typ == "number" || schema.typ == "integer" {
        let is_integer = schema.typ == "integer";
        match (schema.minimum, schema.maximum) {
            (Some(min), Some(max)) => Some(format!(
                "({} between {} and {})",
                schema.typ,
                format_num(min, is_integer),
                format_num(max, is_integer)
            )),
            (Some(min), None) => Some(format!(
                "({} >= {})",
                schema.typ,
                format_num(min, is_integer)
            )),
            (None, Some(max)) => Some(format!(
                "({} <= {})",
                schema.typ,
                format_num(max, is_integer)
            )),
            (None, None) => {
                let example = if is_integer { "42" } else { "3.14" };
                Some(format!("({}, e.g. {example})", schema.typ))
            }
        }
    } else {
        None
    }
}

/// 异步校验（对齐 TS `validateElicitationInputAsync`）。
///
/// 同步校验通过则直接返回；否则对看起来不是 ISO 8601 的 date/date-time
/// 输入尝试 NL 解析（成功后重跑同步校验）。
pub async fn validate_elicitation_input_async(
    string_value: &str,
    schema: &PrimitiveSchemaDefinition,
    signal: &CancellationToken,
) -> Result<ValidationResult, ElicitationValidationError> {
    let sync_result = validate_elicitation_input(string_value, schema)?;
    if sync_result.is_valid {
        return Ok(sync_result);
    }

    if is_date_time_schema(schema) && !looks_like_iso8601(string_value) {
        let parse_format = match schema.format.as_deref() {
            Some("date") => DateTimeParseFormat::Date,
            _ => DateTimeParseFormat::DateTime,
        };
        let parse_result =
            parse_natural_language_date_time(string_value, parse_format, signal).await;
        if let DateTimeParseResult::Success { value } = parse_result {
            let validated_parsed = validate_elicitation_input(&value, schema)?;
            if validated_parsed.is_valid {
                return Ok(validated_parsed);
            }
        }
    }

    Ok(sync_result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date_time_schema() -> PrimitiveSchemaDefinition {
        PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("date-time".to_string()),
            ..Default::default()
        }
    }

    fn validate(input: &str, schema: &PrimitiveSchemaDefinition) -> ValidationResult {
        validate_elicitation_input(input, schema).expect("schema must be supported")
    }

    #[test]
    fn accepts_second_precision_offset() {
        let result = validate("2024-03-15T14:30:00+08:00", &date_time_schema());
        assert!(result.is_valid);
        assert_eq!(
            result.value,
            Some(PrimitiveValue::String(
                "2024-03-15T14:30:00+08:00".to_string()
            ))
        );
    }

    #[test]
    fn accepts_second_precision_utc() {
        let result = validate("2024-03-15T14:30:00Z", &date_time_schema());
        assert!(result.is_valid);
    }

    #[test]
    fn accepts_minute_precision_offset() {
        let result = validate("2024-03-15T14:30+08:00", &date_time_schema());
        assert!(result.is_valid);
        assert_eq!(
            result.value,
            Some(PrimitiveValue::String(
                "2024-03-15T14:30+08:00".to_string()
            ))
        );
    }

    #[test]
    fn accepts_minute_precision_utc() {
        let result = validate("2024-03-15T14:30Z", &date_time_schema());
        assert!(result.is_valid);
    }

    #[test]
    fn rejects_local_time_without_offset_or_z() {
        let result = validate("2024-03-15T14:30", &date_time_schema());
        assert!(!result.is_valid);
    }

    #[test]
    fn rejects_date_only_for_date_time_schema() {
        let result = validate("2024-03-15", &date_time_schema());
        assert!(!result.is_valid);
    }

    #[test]
    fn rejects_non_date_input() {
        let result = validate("not-a-date", &date_time_schema());
        assert!(!result.is_valid);
    }

    #[test]
    fn enforces_min_length_alongside_format() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("date-time".to_string()),
            min_length: Some(30),
            ..Default::default()
        };
        let result = validate("2024-03-15T14:30+08:00", &schema);
        assert!(!result.is_valid);
    }

    #[test]
    fn enforces_max_length_alongside_format() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("date-time".to_string()),
            max_length: Some(10),
            ..Default::default()
        };
        let result = validate("2024-03-15T14:30Z", &schema);
        assert!(!result.is_valid);
    }

    #[test]
    fn accepts_input_within_length_bounds() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("date-time".to_string()),
            min_length: Some(10),
            max_length: Some(30),
            ..Default::default()
        };
        let result = validate("2024-03-15T14:30+08:00", &schema);
        assert!(result.is_valid);
    }

    #[test]
    fn date_format_accepts_plain_dates() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("date".to_string()),
            ..Default::default()
        };
        assert!(validate("2024-03-15", &schema).is_valid);
        assert!(!validate("2024-3-15", &schema).is_valid);
    }

    #[test]
    fn email_format_validates() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some("email".to_string()),
            ..Default::default()
        };
        assert!(validate("user@example.com", &schema).is_valid);
        assert!(!validate("nope", &schema).is_valid);
    }

    #[test]
    fn number_enforces_range() {
        let schema = PrimitiveSchemaDefinition {
            typ: "number".to_string(),
            minimum: Some(0.0),
            maximum: Some(10.0),
            ..Default::default()
        };
        let ok = validate("3.14", &schema);
        assert!(ok.is_valid);
        // 不用 3.14 字面量：clippy approx_constant 拒绝级
        let expected: f64 = "3.14".parse().unwrap();
        assert_eq!(result_number(ok), expected);
        assert!(!validate("-1", &schema).is_valid);
        assert!(!validate("11", &schema).is_valid);
        // range_msg 固定消息
        let bad = validate("11", &schema);
        assert_eq!(
            bad.error.as_deref(),
            Some("Must be a number between 0.0 and 10.0")
        );
    }

    #[test]
    fn integer_rejects_fractional() {
        let schema = PrimitiveSchemaDefinition {
            typ: "integer".to_string(),
            ..Default::default()
        };
        assert!(validate("42", &schema).is_valid);
        assert!(!validate("3.14", &schema).is_valid);
    }

    #[test]
    fn boolean_coerces_js_truthiness() {
        let schema = PrimitiveSchemaDefinition {
            typ: "boolean".to_string(),
            ..Default::default()
        };
        let t = validate("anything", &schema);
        assert!(t.is_valid);
        assert_eq!(result_bool(t), true);
        let f = validate("", &schema);
        assert!(f.is_valid);
        assert_eq!(result_bool(f), false);
        // JS Boolean("false") === true（与 TS z.coerce.boolean() 一致）
        assert_eq!(result_bool(validate("false", &schema)), true);
    }

    #[test]
    fn enum_schema_validates_membership() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            enum_values: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        };
        assert!(validate("a", &schema).is_valid);
        assert!(!validate("c", &schema).is_valid);
        assert_eq!(
            get_enum_labels(&schema),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn one_of_enum_uses_titles() {
        let schema = PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            one_of: Some(vec![
                EnumOption {
                    const_value: Some("a".to_string()),
                    title: Some("Alpha".to_string()),
                },
                EnumOption {
                    const_value: Some("b".to_string()),
                    title: Some("Beta".to_string()),
                },
            ]),
            ..Default::default()
        };
        assert_eq!(get_enum_values(&schema), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            get_enum_labels(&schema),
            vec!["Alpha".to_string(), "Beta".to_string()]
        );
        assert_eq!(get_enum_label(&schema, "b"), "Beta");
        assert_eq!(get_enum_label(&schema, "zzz"), "zzz");
    }

    #[test]
    fn multi_select_enum_helpers() {
        let schema = PrimitiveSchemaDefinition {
            typ: "array".to_string(),
            items: Some(ItemsSchema {
                enum_values: Some(vec!["x".to_string(), "y".to_string()]),
                any_of: None,
            }),
            ..Default::default()
        };
        assert!(is_multi_select_enum_schema(&schema));
        assert_eq!(
            get_multi_select_values(&schema),
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(get_multi_select_label(&schema, "y"), "y");

        let any_of_schema = PrimitiveSchemaDefinition {
            typ: "array".to_string(),
            items: Some(ItemsSchema {
                enum_values: None,
                any_of: Some(vec![
                    EnumOption {
                        const_value: Some("x".to_string()),
                        title: Some("Ex".to_string()),
                    },
                    EnumOption {
                        const_value: Some("y".to_string()),
                        title: None,
                    },
                ]),
            }),
            ..Default::default()
        };
        assert_eq!(
            get_multi_select_values(&any_of_schema),
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(get_multi_select_label(&any_of_schema, "x"), "Ex");
        // title 缺失回退为值本身
        assert_eq!(get_multi_select_label(&any_of_schema, "y"), "y");
    }

    #[test]
    fn unsupported_schema_returns_error() {
        let schema = PrimitiveSchemaDefinition {
            typ: "object".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            validate_elicitation_input("x", &schema),
            Err(ElicitationValidationError::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn format_hints() {
        let string = |format: &str| PrimitiveSchemaDefinition {
            typ: "string".to_string(),
            format: Some(format.to_string()),
            ..Default::default()
        };
        assert_eq!(
            get_format_hint(&string("email")).as_deref(),
            Some("email address, e.g. user@example.com")
        );
        assert_eq!(
            get_format_hint(&string("uri")).as_deref(),
            Some("URI, e.g. https://example.com")
        );
        assert_eq!(
            get_format_hint(&string("date-time")).as_deref(),
            Some("date-time, e.g. 2024-03-15T14:30:00Z")
        );
        // 未收录 format：TS 模板拼接 undefined，保留该行为
        assert_eq!(
            get_format_hint(&string("password")).as_deref(),
            Some("undefined, e.g. undefined")
        );

        let integer = PrimitiveSchemaDefinition {
            typ: "integer".to_string(),
            ..Default::default()
        };
        assert_eq!(
            get_format_hint(&integer).as_deref(),
            Some("(integer, e.g. 42)")
        );

        let boolean = PrimitiveSchemaDefinition {
            typ: "boolean".to_string(),
            ..Default::default()
        };
        assert_eq!(get_format_hint(&boolean), None);
    }

    fn result_number(result: ValidationResult) -> f64 {
        match result.value {
            Some(PrimitiveValue::Number(n)) => n,
            _ => panic!("expected number value"),
        }
    }

    fn result_bool(result: ValidationResult) -> bool {
        match result.value {
            Some(PrimitiveValue::Boolean(b)) => b,
            _ => panic!("expected boolean value"),
        }
    }

    #[tokio::test]
    async fn async_passes_minute_precision_iso_through_sync_validation() {
        let token = CancellationToken::new();
        let result = validate_elicitation_input_async(
            "2024-03-15T14:30+08:00",
            &date_time_schema(),
            &token,
        )
        .await
        .expect("schema must be supported");
        assert!(result.is_valid);
    }

    #[tokio::test]
    async fn async_falls_back_to_nl_parsing_for_non_iso_input() {
        let token = CancellationToken::new();
        let result =
            validate_elicitation_input_async("next Monday", &date_time_schema(), &token)
                .await
                .expect("schema must be supported");
        // NL 解析当前不可用（query_haiku HTTP 未接入），回退到同步校验失败
        assert!(!result.is_valid);
    }
}
