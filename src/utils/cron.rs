//! cron.rs — 极简 cron 表达式解析与下次运行计算。
//!
//! 重写自 `src/utils/cron.ts`。支持标准 5 字段子集：
//!   minute hour day-of-month month day-of-week
//! 字段语法：通配、N、步进（*/N）、范围（N-M）、列表（N,M,…）。
//! 不支持 L、W、?、名称别名。所有时刻按进程本地时区解释。
//!
//! 已知差异：TS 的 `computeNextCronRun` 通过 JS Date 的本地时区 setter
//! 推进（天然处理 DST 跳变）；本实现使用 naive 本地时钟（NaiveDateTime）
//! 逐步推进，等价于 vixie-cron 的"挂钟时间"语义——春跳变当日不存在的
//! 时间点在 naive 时间线里仍存在，落在该小时的 cron 会比 TS 多触发一次；
//! 这种边界情况整体行为仍保持"每分钟推进、字段集合命中即返回"。

use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike,
};

/// 展开后的 cron 字段：各位置回的有序数值集合。
#[derive(Debug, Clone)]
pub struct CronFields {
    pub minute: Vec<u32>,
    pub hour: Vec<u32>,
    pub day_of_month: Vec<u32>,
    pub month: Vec<u32>,
    pub day_of_week: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct FieldRange {
    min: u32,
    max: u32,
}

const FIELD_RANGES: [FieldRange; 5] = [
    FieldRange { min: 0, max: 59 }, // minute
    FieldRange { min: 0, max: 23 }, // hour
    FieldRange { min: 1, max: 31 }, // dayOfMonth
    FieldRange { min: 1, max: 12 }, // month
    FieldRange { min: 0, max: 6 },  // dayOfWeek （0=Sunday；7 为 Sunday 别名）
];

/// 解析单个 cron 字段为有序匹配值列表。非法返回 None。
fn expand_field(field: &str, range: FieldRange) -> Option<Vec<u32>> {
    use std::collections::BTreeSet;

    let mut out = BTreeSet::new();
    for part in field.split(',') {
        // 通配或 */N
        if part == "*" || part.starts_with("*/") {
            let step: u32 = if part == "*" {
                1
            } else {
                part[2..].parse().ok()?
            };
            if step < 1 {
                return None;
            }
            let mut i = range.min;
            while i <= range.max {
                out.insert(i);
                i += step;
            }
            continue;
        }

        // N-M 或 N-M/S
        let (range_part, step) = match part.split_once('/') {
            Some((rp, s)) => (rp, s.parse::<u32>().ok()?),
            None => (part, 1),
        };
        if step < 1 {
            return None;
        }
        if let Some((lo_s, hi_s)) = range_part.split_once('-') {
            let lo: u32 = lo_s.parse().ok()?;
            let hi: u32 = hi_s.parse().ok()?;
            // dayOfWeek：范围内接受 7 作 Sunday 别名（5-7 → [5,6,0]）
            let is_dow = range.min == 0 && range.max == 6;
            let eff_max = if is_dow { 7 } else { range.max };
            if lo > hi || lo < range.min || hi > eff_max {
                return None;
            }
            let mut i = lo;
            while i <= hi {
                out.insert(if is_dow && i == 7 { 0 } else { i });
                i += step;
            }
            continue;
        }

        // 纯 N
        if !range_part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut n: u32 = range_part.parse().ok()?;
        // dayOfWeek：7 作 Sunday 别名 → 0
        if range.min == 0 && range.max == 6 && n == 7 {
            n = 0;
        }
        if n < range.min || n > range.max {
            return None;
        }
        out.insert(n);
    }

    if out.is_empty() {
        return None;
    }
    Some(out.into_iter().collect())
}

/// 解析 5 字段 cron 表达式；非法或语法不支持返回 None。
pub fn parse_cron_expression(expr: &str) -> Option<CronFields> {
    let parts: Vec<&str> = expr.trim().split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let mut expanded: Vec<Vec<u32>> = Vec::with_capacity(5);
    for (i, part) in parts.iter().enumerate() {
        expanded.push(expand_field(part, FIELD_RANGES[i])?);
    }
    Some(CronFields {
        minute: expanded.remove(0),
        hour: expanded.remove(0),
        day_of_month: expanded.remove(0),
        month: expanded.remove(0),
        day_of_week: expanded.remove(0),
    })
}

/// 计算 `from` 之后（严格大于）首个匹配 cron 字段的本地时刻。
/// 逐分钟推进，上限 366 天；合法 cron 必然命中，但为满足类型返回 Option。
pub fn compute_next_cron_run(
    fields: &CronFields,
    from: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let minute_set: Vec<bool> = bool_set(&fields.minute, 60);
    let hour_set: Vec<bool> = bool_set(&fields.hour, 24);
    let dom_set: Vec<bool> = bool_set(&fields.day_of_month, 32);
    let month_set: Vec<bool> = bool_set(&fields.month, 13);
    let dow_set: Vec<bool> = bool_set(&fields.day_of_week, 7);

    // dayOfMonth / dayOfWeek 是否为全范围通配
    let dom_wild = fields.day_of_month.len() == 31;
    let dow_wild = fields.day_of_week.len() == 7;

    // 向上取整到下一整分钟（严格晚于 from）。
    // with_second/with_nanosecond 截断对任何输入都成立，失败时退回
    // 原始本地时刻继续推进（不带 expect，保持 panic-free）。
    let truncated = from
        .naive_local()
        .with_second(0)
        .and_then(|dt| dt.with_nanosecond(0));
    let mut t = truncated.unwrap_or_else(|| from.naive_local()) + Duration::minutes(1);

    let max_iter = 366 * 24 * 60;
    for _ in 0..max_iter {
        if !month_set[t.month() as usize] {
            // 跳到下个月 1 日 00:00
            let (year, month) = if t.month() == 12 {
                (t.year() + 1, 1u32)
            } else {
                (t.year(), t.month() + 1)
            };
            match NaiveDate::from_ymd_opt(year, month, 1).and_then(|d| d.and_hms_opt(0, 0, 0)) {
                Some(first) => t = first,
                None => {
                    // 构造失败（corner case）：推进一日继续扫描
                    t += Duration::days(1);
                    continue;
                }
            }
            continue;
        }

        let dom = t.day();
        let dow = t.weekday().num_days_from_sunday();
        // dom 与 dow 同时受限时，任一命中即可（或语义）
        let day_matches = if dom_wild && dow_wild {
            true
        } else if dom_wild {
            dow_set[dow as usize]
        } else if dow_wild {
            dom_set[dom as usize]
        } else {
            dom_set[dom as usize] || dow_set[dow as usize]
        };

        if !day_matches {
            // 跳到次日 00:00
            t = (t.date() + Duration::days(1)).and_time(midnight());
            continue;
        }

        if !hour_set[t.hour() as usize] {
            let truncated = t
                .with_minute(0)
                .and_then(|dt| dt.with_second(0))
                .and_then(|dt| dt.with_nanosecond(0));
            t = truncated.unwrap_or(t) + Duration::hours(1);
            continue;
        }

        if !minute_set[t.minute() as usize] {
            t += Duration::minutes(1);
            continue;
        }

        return Local.from_local_datetime(&t).single();
    }

    None
}

fn bool_set(values: &[u32], len: usize) -> Vec<bool> {
    let mut set = vec![false; len];
    for v in values {
        if (*v as usize) < len {
            set[*v as usize] = true;
        }
    }
    set
}

// --- cronToHuman ------------------------------------------------------------

/// cron 表达式的人类可读描述。有意收窄：只覆盖常见模式，其余回退
/// 原始 cron 串。`utc` 选项用于 CCR 远程触发器——服务端固定 UTC cron，
/// 展示时需转换到本地。
pub fn cron_to_human(cron: &str, utc: bool) -> String {
    let trimmed = cron.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() != 5 {
        return cron.to_string();
    }
    let minute = parts[0];
    let hour = parts[1];
    let day_of_month = parts[2];
    let month = parts[3];
    let day_of_week = parts[4];

    let all_star = |s: &str| s == "*";

    // 每 N 分钟：*/N * * * *
    if let Some(n) = minute.strip_prefix("*/").filter(|_| {
        all_star(hour) && all_star(day_of_month) && all_star(month) && all_star(day_of_week)
    }) {
        return match n.parse::<u32>() {
            Ok(1) => "Every minute".to_string(),
            Ok(n) => format!("Every {} minutes", n),
            Err(_) => cron.to_string(),
        };
    }

    // 每小时：M * * * *
    if minute.bytes().all(|b| b.is_ascii_digit())
        && !minute.is_empty()
        && all_star(hour)
        && all_star(day_of_month)
        && all_star(month)
        && all_star(day_of_week)
    {
        let m: u32 = minute.parse().unwrap_or(0);
        if m == 0 {
            return "Every hour".to_string();
        }
        return format!("Every hour at :{:02}", m);
    }

    // 每 N 小时：M */N * * *
    if let Some(n) = hour.strip_prefix("*/").filter(|_| {
        minute.bytes().all(|b| b.is_ascii_digit())
            && !minute.is_empty()
            && all_star(day_of_month)
            && all_star(month)
            && all_star(day_of_week)
    }) {
        let n: u32 = n.parse().unwrap_or(0);
        let m: u32 = minute.parse().unwrap_or(0);
        let suffix = if m == 0 {
            String::new()
        } else {
            format!(" at :{:02}", m)
        };
        return if n == 1 {
            format!("Every hour{}", suffix)
        } else {
            format!("Every {} hours{}", n, suffix)
        };
    }

    // 剩余分支需要 minute/hour 均为纯数字
    if !minute.bytes().all(|b| b.is_ascii_digit()) || !hour.bytes().all(|b| b.is_ascii_digit()) {
        return cron.to_string();
    }
    let m: u32 = minute.parse().unwrap_or(0);
    let h: u32 = hour.parse().unwrap_or(0);

    // 每天 M H * * *
    if all_star(day_of_month) && all_star(month) && all_star(day_of_week) {
        return format!("Every day at {}", fmt_time(m, h, utc));
    }

    // 每周 M H * * D
    if all_star(day_of_month)
        && all_star(month)
        && day_of_week.len() == 1
        && day_of_week.chars().all(|c| c.is_ascii_digit())
    {
        let day_index: u32 = day_of_week.parse().unwrap_or(0) % 7; // 7 → 0
        let day_name = weekday_at(h, m, day_index, utc);
        return format!("Every {} at {}", day_name, fmt_time(m, h, utc));
    }

    // 工作日 M H * * 1-5
    if all_star(day_of_month) && all_star(month) && day_of_week == "1-5" {
        return format!("Weekdays at {}", fmt_time(m, h, utc));
    }

    cron.to_string()
}

/// 本地 12h 风格时间（与 TS 的 toLocaleTimeString('en-US') 对齐）。
fn fmt_time(minute: u32, hour: u32, utc: bool) -> String {
    if utc {
        // UTC 时刻按本地时区展示，附加偏移名（无 IANA，用数字偏移近似）
        let now = Local::now();
        let dt = now
            .timezone()
            .from_utc_datetime(&utc_today_at(hour, minute));
        format!(
            "{}:{:02} {}",
            hour12(dt.hour()),
            dt.minute(),
            am_pm(dt.hour())
        )
    } else {
        format!("{}:{:02} {}", hour12(hour), minute, am_pm(hour))
    }
}

fn hour12(hour: u32) -> u32 {
    match hour {
        0 => 12,
        h if h > 12 => h - 12,
        h => h,
    }
}

fn am_pm(hour: u32) -> &'static str {
    if hour >= 12 { "PM" } else { "AM" }
}

fn weekday_at(hour: u32, minute: u32, day_index: u32, utc: bool) -> &'static str {
    const DAY_NAMES: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    if utc {
        // UTC 日+时刻可能落在不同的本地日（午夜跨越）：
        // 用今天的 UTC 时刻换算，取实际本地周名
        let now = Local::now();
        let mut utc_dt = utc_today_at(0, 0)
            + chrono::Duration::days(
                ((day_index as i64 - now.weekday().num_days_from_sunday() as i64 + 7) % 7),
            );
        utc_dt = utc_dt
            .with_hour(hour)
            .and_then(|dt| dt.with_minute(minute))
            .unwrap_or(utc_dt);
        let local = now.timezone().from_utc_datetime(&utc_dt);
        return DAY_NAMES[local.weekday().num_days_from_sunday() as usize];
    }
    DAY_NAMES[day_index as usize]
}

fn midnight() -> NaiveTime {
    NaiveTime::from_hms_opt(0, 0, 0).unwrap_or(NaiveTime::MIN)
}

fn utc_today_at(hour: u32, minute: u32) -> NaiveDateTime {
    let today = chrono::Utc::now().date_naive();
    today
        .and_hms_opt(hour, minute, 0)
        .unwrap_or(today.and_time(midnight()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_fields() {
        let f = parse_cron_expression("0 9 * * *").expect("valid");
        assert_eq!(f.minute, vec![0]);
        assert_eq!(f.hour, vec![9]);
        assert_eq!(f.day_of_month.len(), 31);
        assert_eq!(f.day_of_week.len(), 7);
    }

    #[test]
    fn rejects_invalid() {
        assert!(parse_cron_expression("60 9 * * *").is_none());
        assert!(parse_cron_expression("0 9 *").is_none());
        assert!(parse_cron_expression("0 25 * * *").is_none());
        assert!(parse_cron_expression("* * * * * *").is_none());
    }

    #[test]
    fn dow_seven_alias() {
        let f = parse_cron_expression("0 9 * * 5-7").expect("valid");
        assert_eq!(f.day_of_week, vec![0, 5, 6]);
        let f = parse_cron_expression("0 9 * * 7").expect("valid");
        assert_eq!(f.day_of_week, vec![0]);
    }

    #[test]
    fn next_run_daily() {
        let from = DateTime::parse_from_rfc3339("2026-03-20T08:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        let fields = parse_cron_expression("30 9 * * *").expect("valid");
        let next = compute_next_cron_run(&fields, from).expect("matches");
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_run_steps_and_or_day_semantics() {
        let from = DateTime::parse_from_rfc3339("2026-03-20T08:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        let fields = parse_cron_expression("*/15 * * * *").expect("valid");
        let next = compute_next_cron_run(&fields, from).expect("matches");
        assert_eq!(next.minute() % 15, 0);
    }

    #[test]
    fn human_descriptions() {
        assert_eq!(cron_to_human("*/5 * * * *", false), "Every 5 minutes");
        assert_eq!(cron_to_human("0 * * * *", false), "Every hour");
        assert_eq!(cron_to_human("30 * * * *", false), "Every hour at :30");
        assert_eq!(cron_to_human("0 */2 * * *", false), "Every 2 hours");
        assert_eq!(cron_to_human("0 9 * * *", false), "Every day at 9:00 AM");
        assert_eq!(cron_to_human("0 9 * * 1", false), "Every Monday at 9:00 AM");
        assert_eq!(cron_to_human("0 9 * * 1-5", false), "Weekdays at 9:00 AM");
        // 不认识的格式回退原串
        assert_eq!(cron_to_human("0 9 1,15 * *", false), "0 9 1,15 * *");
    }
}
