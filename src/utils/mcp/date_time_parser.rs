//! date_time_parser.rs — 自然语言日期时间解析（重写自 `src/utils/mcp/dateTimeParser.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `parse_natural_language_date_time`：用 query_haiku（small fast model）
//!   把 "tomorrow at 3pm" / "next Monday" 等自然语言解析成 ISO 8601；模型
//!   返回 "INVALID" 或非 `\d{4}` 开头时视为解析失败。
//! - `looks_like_iso8601`：正则 `/^\d{4}-\d{2}-\d{2}(T|$)/`，用于决定是否
//!   尝试 NL 解析。
//! - 时区上下文来自本地时区（TS `-getTimezoneOffset()`，即 UTC 偏移带正
//!   号），星期几取英文全名（对齐 `toLocaleDateString('en-US')`）。
//!
//! 已知偏差（依赖未就绪）：
//! - `query_haiku` 的 HTTP 层尚未接入（见 `services/api/claude.rs` 的偏差
//!   说明，当前返回 `HttpClientUnavailable`），因此解析当前永远走 catch
//!   分支返回统一错误消息；签名、提示词与参数构造完整保留，HTTP 落地后
//!   自动生效。
//! - `AbortSignal` → `utils::abort_controller::CancellationToken`；
//!   `query_haiku` Rust 端暂无 signal 参数，先按 `aborted()` 提前返回
//!   （等价 TS 中已取消的 signal 让请求立即 reject，随后被 catch 捕获）。
//! - `utils/messages.ts` 未转换：`extractTextContent` 在本模块内置最小等
//!   价实现（仅过滤 `type == "text"` 并拼接 `text` 字段），与 `claude.rs`
//!   的做法一致。

use chrono::{Local, Offset, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::services::api::claude::{query_haiku, AssistantMessage, MessageContent, Options};
use crate::utils::abort_controller::CancellationToken;
use crate::utils::errors::AbortError;
use crate::utils::log::log_error;
use crate::utils::system_prompt_type::as_system_prompt;

/// 解析结果（对齐 TS 的 `DateTimeParseResult` 联合类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateTimeParseResult {
    /// 解析成功，`value` 为 ISO 8601 字符串。
    Success { value: String },
    /// 解析失败，`error` 为面向用户的错误消息。
    Failure { error: String },
}

/// 目标格式（对齐 TS 的 `'date' | 'date-time'`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeParseFormat {
    /// `YYYY-MM-DD`（仅日期）。
    Date,
    /// 完整 ISO 8601 带时区。
    DateTime,
}

const MANUAL_FORMAT_ERROR: &str =
    "Unable to parse date/time. Please enter in ISO 8601 format manually.";
const UNPARSABLE_ERROR: &str = "Unable to parse date/time from input";

static YEAR_PREFIX_RE: Lazy<Option<Regex>> = Lazy::new(|| Regex::new(r"^\d{4}").ok());
static ISO8601_SHAPE_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}(T|$)").ok());

fn is_match(re: &Lazy<Option<Regex>>, input: &str) -> bool {
    match &**re {
        Some(regex) => regex.is_match(input),
        None => false,
    }
}

/// 解析自然语言日期时间（对齐 TS `parseNaturalLanguageDateTime`）。
///
/// 用 small fast model 转换自然语言为 ISO 8601；失败（含取消）时返回统一
/// 的错误消息，不向调用方暴露模型层细节。
pub async fn parse_natural_language_date_time(
    input: &str,
    format: DateTimeParseFormat,
    signal: &CancellationToken,
) -> DateTimeParseResult {
    // 已取消的 signal 会让 queryHaiku 立刻 reject；这里提前短路，
    // 走与 catch 分支相同的返回路径。
    if signal.aborted() {
        log_error(AbortError::new(None::<String>));
        return DateTimeParseResult::Failure {
            error: MANUAL_FORMAT_ERROR.to_string(),
        };
    }

    // 当前日期时间与本地时区上下文
    let now = Local::now();
    let current_date_time = now
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    // FixedOffset 的 Display 即 "+HH:MM" / "-HH:MM"，符号与 TS 的
    // -getTimezoneOffset() 一致
    let timezone = format!("{}", now.offset().fix());
    let day_of_week = now.format("%A").to_string();

    // 系统提示（逐条对齐 TS 的 systemPrompt 数组）
    let system_prompt = as_system_prompt(vec![
        "You are a date/time parser that converts natural language into ISO 8601 format.".to_string(),
        "You MUST respond with ONLY the ISO 8601 formatted string, with no explanation or additional text.".to_string(),
        "If the input is ambiguous, prefer future dates over past dates.".to_string(),
        "For times without dates, use today's date.".to_string(),
        "For dates without times, do not include a time component.".to_string(),
        "If the input is incomplete or you cannot confidently parse it into a valid date, respond with exactly \"INVALID\" (nothing else).".to_string(),
        "Examples of INVALID input: partial dates like \"2025-01-\", lone numbers like \"13\", gibberish.".to_string(),
        "Examples of valid natural language: \"tomorrow\", \"next Monday\", \"jan 1st 2025\", \"in 2 hours\", \"yesterday\".".to_string(),
    ]);

    // 用户提示（含上下文，逐行对齐 TS 模板）
    let format_description = match format {
        DateTimeParseFormat::Date => "YYYY-MM-DD (date only, no time)".to_string(),
        DateTimeParseFormat::DateTime => format!(
            "YYYY-MM-DDTHH:MM:SS{} (full date-time with timezone)",
            timezone
        ),
    };
    let user_prompt = format!(
        "Current context:\n- Current date and time: {} (UTC)\n- Local timezone: {}\n- Day of week: {}\n\nUser input: \"{}\"\n\nOutput format: {}\n\nParse the user's input into ISO 8601 format. Return ONLY the formatted string, or \"INVALID\" if the input is incomplete or unparseable.",
        current_date_time, timezone, day_of_week, input, format_description
    );

    // 对齐 TS 的 options：querySource='mcp_datetime_parse'，
    // agents/mcpTools 为空，非交互、无 appendSystemPrompt，无 prompt
    // caching。`model` 字段 query_haiku 内部固定使用 small fast model。
    let options = Options {
        model: String::new(),
        tool_choice: None,
        is_non_interactive_session: false,
        extra_tool_schemas: Vec::new(),
        max_output_tokens_override: None,
        fallback_model: None,
        query_source: "mcp_datetime_parse".to_string(),
        has_append_system_prompt: false,
        enable_prompt_caching: Some(false),
        skip_cache_write: false,
        temperature_override: None,
        effort_value: None,
        output_format: None,
        fast_mode: false,
        advisor_model: None,
        task_budget: None,
        agent_id: None,
        has_pending_mcp_servers: false,
    };

    let result = match query_haiku(&user_prompt, &system_prompt.0, None, &options).await {
        Ok(message) => message,
        Err(error) => {
            // 记录错误但不向用户暴露细节
            log_error(error);
            return DateTimeParseResult::Failure {
                error: MANUAL_FORMAT_ERROR.to_string(),
            };
        }
    };

    let parsed_text = extract_text_content(&result).trim().to_string();

    if parsed_text.is_empty() || parsed_text == "INVALID" {
        return DateTimeParseResult::Failure {
            error: UNPARSABLE_ERROR.to_string(),
        };
    }

    // 基本健全性检查：应以 4 位年份开头
    if !is_match(&YEAR_PREFIX_RE, &parsed_text) {
        return DateTimeParseResult::Failure {
            error: UNPARSABLE_ERROR.to_string(),
        };
    }

    DateTimeParseResult::Success { value: parsed_text }
}

/// 判断字符串是否“看起来像” ISO 8601 日期/时间（对齐 TS `looksLikeISO8601`）。
///
/// 正则 `/^\d{4}-\d{2}-\d{2}(T|$)/`（Rust `regex` 的 `$` 仅匹配文本末尾，
/// 不含 JS `$` 可匹配结尾换行的行为；对当前用途无影响）。
pub fn looks_like_iso8601(input: &str) -> bool {
    is_match(&ISO8601_SHAPE_RE, input.trim())
}

/// `extractTextContent` 的本地最小等价实现（`utils/messages.ts` 未转换）。
///
/// 对齐 TS：`Array.isArray(content) ? content : []`，过滤 `type === "text"`
/// 的块并按默认分隔符（空串）拼接 `text` 字段；非数组（字符串）内容返回
/// 空串。
fn extract_text_content(message: &AssistantMessage) -> String {
    match &message.message.content {
        MessageContent::Text(_) => String::new(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Value::Object(map) => {
                    if map.get("type").and_then(Value::as_str) == Some("text") {
                        map.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect::<Vec<String>>()
            .join(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_iso8601_accepts_date_only() {
        assert!(looks_like_iso8601("2024-03-15"));
        assert!(looks_like_iso8601("  2024-03-15  "));
    }

    #[test]
    fn looks_like_iso8601_accepts_datetime() {
        assert!(looks_like_iso8601("2024-03-15T14:30:00Z"));
        assert!(looks_like_iso8601("2024-03-15T14:30+08:00"));
    }

    #[test]
    fn looks_like_iso8601_rejects_non_iso() {
        assert!(!looks_like_iso8601("next Monday"));
        assert!(!looks_like_iso8601("13"));
        assert!(!looks_like_iso8601(""));
    }

    #[tokio::test]
    async fn parse_returns_failure_when_http_unavailable() {
        let token = CancellationToken::new();
        let result =
            parse_natural_language_date_time("next Monday", DateTimeParseFormat::Date, &token)
                .await;
        match result {
            DateTimeParseResult::Failure { error } => {
                assert_eq!(
                    error,
                    "Unable to parse date/time. Please enter in ISO 8601 format manually."
                );
            }
            DateTimeParseResult::Success { .. } => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn parse_returns_failure_when_aborted() {
        let token = CancellationToken::new();
        token.abort(Some("cancelled".to_string()));
        let result =
            parse_natural_language_date_time("next Monday", DateTimeParseFormat::Date, &token)
                .await;
        assert!(matches!(result, DateTimeParseResult::Failure { .. }));
    }
}
