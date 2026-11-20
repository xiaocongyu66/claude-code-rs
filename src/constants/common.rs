//! common.rs — 日期相关公共常量与函数。
//!
//! 重写自 `src/constants/common.ts`。TS 原文件用 lodash-es/memoize 缓存
//! 会话起始日期；此处用 `std::sync::LazyLock` 等价实现（首次调用求值，
//! 之后返回同一引用）。

use chrono::{Datelike, Local, NaiveDate};

/// This ensures you get the LOCAL date in ISO format.
/// Checks for ant-only date override first.
pub fn get_local_iso_date() -> String {
    // Check for ant-only date override
    if let Ok(override_date) = std::env::var("CLAUDE_CODE_OVERRIDE_DATE") {
        return override_date;
    }

    let now = Local::now();
    format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day())
}

/// Memoized for prompt-cache stability — captures the date once at session start.
/// The main interactive path gets this behavior via memoize(getUserContext) in
/// context.ts; simple mode (--bare) calls getSystemPrompt per-request and needs
/// an explicit memoized date to avoid busting the cached prefix at midnight.
/// When midnight rolls over, getDateChangeAttachments appends the new date at
/// the tail (though simple mode disables attachments, so the trade-off there is:
/// stale date after midnight vs. ~entire-conversation cache bust — stale wins).
pub fn get_session_start_date() -> &'static str {
    static DATE: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(get_local_iso_date);
    &DATE
}

/// Returns "Month YYYY" (e.g. "February 2026") in the user's local timezone.
/// Changes monthly, not daily — used in tool prompts to minimize cache busting.
pub fn get_local_month_year() -> String {
    let formatted = match std::env::var("CLAUDE_CODE_OVERRIDE_DATE") {
        Ok(override_date) => match NaiveDate::parse_from_str(&override_date, "%Y-%m-%d") {
            // JS 中 new Date(无法解析的字符串) 得到 Invalid Date，
            // toLocaleString 返回 "Invalid Date"；此处对齐该行为。
            Ok(date) => date.format("%B %Y").to_string(),
            Err(_) => "Invalid Date".to_string(),
        },
        Err(_) => Local::now().format("%B %Y").to_string(),
    };
    formatted
}
