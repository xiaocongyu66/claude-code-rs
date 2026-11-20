//! format_brief_timestamp.rs — 聊天消息标签行的简短时间戳。
//!
//! 重写自 `src/utils/formatBriefTimestamp.ts`。按消息年龄缩放显示：
//!   - 当天：      "1:30 PM" / "13:30"（依 locale 12/24h）
//!   - 6 天内：    "Sunday, 4:15 PM"
//!   - 更早：      "Sunday, Feb 20, 4:30 PM"
//!
//! 已知差异（Rust 无 ICU）：
//! - 周名/月名固定英文（chrono 默认），TS 端跟随系统 locale 渲染。
//! - 12/24h 由 POSIX locale（LC_ALL > LC_TIME > LANG，含 "en_US" 类
//!   tag 时强制 12h）与 LC_TIME/LANG 推导，规则与 TS 对齐：
//!   `en-US` 风格 locale 用 12h，其余用 24h。
//! - 效期秒数等数值语义与 chrono `Local` 对齐（时区由 TZ 决定）。

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};

/// 把 ISO 8601 时间戳格式化为简短标签；非法日期返回空串。
pub fn format_brief_timestamp(iso_string: &str) -> String {
    format_brief_timestamp_at(iso_string, Local::now())
}

/// 可注入"当前时刻"的变体（测试用，对齐 TS 的 `now` 参数）。
pub fn format_brief_timestamp_at<Tz: TimeZone>(
    iso_string: &str,
    now: chrono::DateTime<Tz>,
) -> String {
    let datetime = match parse_iso_local(iso_string) {
        Some(dt) => dt,
        None => return String::new(),
    };

    let twelve_hour = use_twelve_hour();
    let now_date = now.date_naive();
    let days_ago = now_date
        .signed_duration_since(datetime.date())
        .num_days();

    if days_goes_to_zero(now_date, datetime.date()) {
        return format_time(&datetime, twelve_hour, false);
    }

    if days_ago > 0 && days_ago < 7 {
        return format!(
            "{}, {}",
            weekday_name(datetime.weekday()),
            format_time(&datetime, twelve_hour, false)
        );
    }

    format!(
        "{}, {} {} {}",
        weekday_name(datetime.weekday()),
        month_name(datetime.month()),
        datetime.day(),
        format_time(&datetime, twelve_hour, false)
    )
}

/// TS 计算 daysAgo = round((startOfDay(now) - startOfDay(d)) / 86400000)：
/// startOfDay 相同即 0。这里直接按本地日期比较天数差。
fn days_goes_to_zero(now_date: NaiveDate, date: NaiveDate) -> bool {
    now_date == date
}

/// 解析 ISO 串为本地 naive 时刻（本地时区渲染用）。
fn parse_iso_local(iso_string: &str) -> Option<NaiveDateTime> {
    chrono::DateTime::parse_from_rfc3339(iso_string)
        .ok()
        .map(|dt| dt.with_timezone(&Local).naive_local())
        .or_else(|| {
            // 允许无时区的 naive 串（TS 的 new Date(iso) 宽容）
            NaiveDateTime::parse_from_str(iso_string, "%Y-%m-%dT%H:%M:%S%.f")
                .or_else(|_| NaiveDateTime::parse_from_str(iso_string, "%Y-%m-%d %H:%M:%S"))
                .or_else(|_| {
                    NaiveDate::parse_from_str(iso_string, "%Y-%m-%d").map(|d| {
                        d.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap_or(NaiveTime::MIN))
                    })
                })
                .ok()
        })
}

fn weekday_name(weekday: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match weekday {
        Sun => "Sunday",
        Mon => "Monday",
        Tue => "Tuesday",
        Wed => "Wednesday",
        Thu => "Thursday",
        Fri => "Friday",
        Sat => "Saturday",
    }
}

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

/// format_time：`hour:numeric minute:2-digit`，12h 模式带 AM/PM。
fn format_time(dt: &NaiveDateTime, twelve_hour: bool, _with_tz: bool) -> String {
    if twelve_hour {
        let hour12 = match dt.hour() {
            0 => 12,
            h if h > 12 => h - 12,
            h => h,
        };
        format!(
            "{}:{:02} {}",
            hour12,
            dt.minute(),
            if dt.hour() >= 12 { "PM" } else { "AM" }
        )
    } else {
        format!("{}:{:02}", dt.hour(), dt.minute())
    }
}

/// 是否使用 12 小时制。TS 端由 `toLocaleString(locale, ...)` 决定；
/// 这里近似为：locale 主语言为 `en` 且按 POSIX env 环境变量可得，
/// 否则 24h。（en-US → "1:30 PM"；ja-JP → "13:30"）
fn use_twelve_hour() -> bool {
    super::intl::get_locale()
        .map(|tag| {
            let primary = tag.split('-').next().unwrap_or("");
            primary.eq_ignore_ascii_case("en")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> chrono::DateTime<Local> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn invalid_returns_empty() {
        assert_eq!(format_brief_timestamp("not-a-date"), "");
    }

    #[test]
    fn same_day_shows_time() {
        let now = parse("2026-03-20T10:00:00Z");
        let s = format_brief_timestamp_at("2026-03-20T08:30:00Z", now);
        // 时区相关：只校验形状（含冒号、无周名）
        assert!(s.contains(':'));
        assert!(!s.contains("Sunday"));
    }

    #[test]
    fn within_week_shows_weekday() {
        let now = parse("2026-03-20T10:00:00Z");
        let s = format_brief_timestamp_at("2026-03-15T08:30:00Z", now);
        assert!(s.contains("day") || s.contains("Day")); // Sunday..Saturday
    }

    #[test]
    fn older_shows_weekday_month_day() {
        let now = parse("2026-03-20T10:00:00Z");
        let s = format_brief_timestamp_at("2026-02-20T08:30:00Z", now);
        assert!(s.contains("Feb"));
    }
}
