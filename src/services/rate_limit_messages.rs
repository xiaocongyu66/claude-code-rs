//! rate_limit_messages.rs — 限流消息文案中枢（重写自
//! `src/services/rateLimitMessages.ts`）。
//!
//! 职责（对齐 TS）：
//! - `RATE_LIMIT_ERROR_PREFIXES` / `isRateLimitErrorMessage`：UI 组件不做
//!   脆弱字符串匹配。
//! - `getRateLimitMessage(limits, model)`：按状态机产出 error/warning 文
//!   案（overage → rejected → allowed_warning 三级）。
//! - `getRateLimitErrorMessage` / `getRateLimitWarning`：error 与 warning
//!   的分流包装。
//! - `getUsingOverageText`：进入 overage 模式的瞬态通知文案。
//!
//! 已知偏差：
//! - `utils/format.ts` 未转换：`formatResetTime` 在本模块内置最小等价实
//!   现（en-US 风格：>24h 显示「月 日, 时段」，<=24h 仅时段；am/pm 小
//!   写无空格；时区用 `intl::get_time_zone()`——无 ICU 时是 TZ 名而非
//!   PST 形式缩写）。
//! - `utils/billing.ts` 未转换：`hasClaudeAiBillingAccess` 在本模块内置
//!   最小等价实现（mock 覆盖 > 订阅判定 > max/pro 直接 true >
//!   team/enterprise 看 `~/.claude.json` 的
//!   `oauthAccount.organizationRole` 是否为 admin/billing/owner/
//!   primary_owner）。
//! - `claudeAiLimits.ts` 未转换：`ClaudeAILimits` 以本模块结构体定义
//!   （字段一一对应）。
//! - `~/.claude.json` 的读取为本地 duck-type（与 auth.rs 同做法）。

use std::path::PathBuf;
use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};

use crate::utils::auth::{get_subscription_type, is_claude_ai_subscriber, is_overage_provisioning_allowed};
use crate::utils::intl::get_time_zone;

const FEEDBACK_CHANNEL_ANT: &str = "#briarpatch-cc";

/// 所有限流错误消息前缀（UI 依赖本常量判定，不做字符串匹配）。
pub const RATE_LIMIT_ERROR_PREFIXES: &[&str] = &[
    "You've hit your",
    "You've used",
    "You're now using extra usage",
    "You're close to",
    "You're out of extra usage",
];

/// 消息是否为限流错误（前缀匹配）。
pub fn is_rate_limit_error_message(text: &str) -> bool {
    RATE_LIMIT_ERROR_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitMessage {
    pub message: String,
    pub severity: RateLimitSeverity,
}

/// 配额状态（对齐 claudeAiLimits 的 `QuotaStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

impl QuotaStatus {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "allowed" => Some(QuotaStatus::Allowed),
            "allowed_warning" => Some(QuotaStatus::AllowedWarning),
            "rejected" => Some(QuotaStatus::Rejected),
            _ => None,
        }
    }

    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            QuotaStatus::Allowed => "allowed",
            QuotaStatus::AllowedWarning => "allowed_warning",
            QuotaStatus::Rejected => "rejected",
        }
    }
}

/// 限流类型（对齐 claudeAiLimits 的 `RateLimitType`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitType {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    Overage,
}

impl RateLimitType {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "five_hour" => Some(RateLimitType::FiveHour),
            "seven_day" => Some(RateLimitType::SevenDay),
            "seven_day_opus" => Some(RateLimitType::SevenDayOpus),
            "seven_day_sonnet" => Some(RateLimitType::SevenDaySonnet),
            "overage" => Some(RateLimitType::Overage),
            _ => None,
        }
    }
}

/// overage 禁用原因（对齐 claudeAiLimits 的 `OverageDisabledReason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverageDisabledReason {
    OverageNotProvisioned,
    OrgLevelDisabled,
    OrgLevelDisabledUntil,
    OutOfCredits,
    SeatTierLevelDisabled,
    MemberLevelDisabled,
    SeatTierZeroCreditLimit,
    GroupZeroCreditLimit,
    MemberZeroCreditLimit,
    OrgServiceLevelDisabled,
    OrgServiceZeroCreditLimit,
    NoLimitsConfigured,
    Unknown,
}

impl OverageDisabledReason {
    fn from_str(value: &str) -> Self {
        match value {
            "overage_not_provisioned" => OverageDisabledReason::OverageNotProvisioned,
            "org_level_disabled" => OverageDisabledReason::OrgLevelDisabled,
            "org_level_disabled_until" => OverageDisabledReason::OrgLevelDisabledUntil,
            "out_of_credits" => OverageDisabledReason::OutOfCredits,
            "seat_tier_level_disabled" => OverageDisabledReason::SeatTierLevelDisabled,
            "member_level_disabled" => OverageDisabledReason::MemberLevelDisabled,
            "seat_tier_zero_credit_limit" => OverageDisabledReason::SeatTierZeroCreditLimit,
            "group_zero_credit_limit" => OverageDisabledReason::GroupZeroCreditLimit,
            "member_zero_credit_limit" => OverageDisabledReason::MemberZeroCreditLimit,
            "org_service_level_disabled" => OverageDisabledReason::OrgServiceLevelDisabled,
            "org_service_zero_credit_limit" => OverageDisabledReason::OrgServiceZeroCreditLimit,
            "no_limits_configured" => OverageDisabledReason::NoLimitsConfigured,
            _ => OverageDisabledReason::Unknown,
        }
    }

    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            OverageDisabledReason::OverageNotProvisioned => "overage_not_provisioned",
            OverageDisabledReason::OrgLevelDisabled => "org_level_disabled",
            OverageDisabledReason::OrgLevelDisabledUntil => "org_level_disabled_until",
            OverageDisabledReason::OutOfCredits => "out_of_credits",
            OverageDisabledReason::SeatTierLevelDisabled => "seat_tier_level_disabled",
            OverageDisabledReason::MemberLevelDisabled => "member_level_disabled",
            OverageDisabledReason::SeatTierZeroCreditLimit => "seat_tier_zero_credit_limit",
            OverageDisabledReason::GroupZeroCreditLimit => "group_zero_credit_limit",
            OverageDisabledReason::MemberZeroCreditLimit => "member_zero_credit_limit",
            OverageDisabledReason::OrgServiceLevelDisabled => "org_service_level_disabled",
            OverageDisabledReason::OrgServiceZeroCreditLimit => "org_service_zero_credit_limit",
            OverageDisabledReason::NoLimitsConfigured => "no_limits_configured",
            OverageDisabledReason::Unknown => "unknown",
        }
    }
}

/// Claude.ai 限流快照（对齐 claudeAiLimits 的 `ClaudeAILimits`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeAiLimits {
    pub status: Option<QuotaStatus>,
    /// unifiedRateLimitFallbackAvailable：仅用于在 Opus 配额将尽时警示
    /// 切了 Opus 的用户；不改变实际使用的模型。
    pub unified_rate_limit_fallback_available: bool,
    pub resets_at: Option<f64>,
    pub rate_limit_type: Option<RateLimitType>,
    pub utilization: Option<f64>,
    pub overage_status: Option<QuotaStatus>,
    pub overage_resets_at: Option<f64>,
    pub overage_disabled_reason: Option<OverageDisabledReason>,
    pub is_using_overage: bool,
    pub surpassed_threshold: Option<f64>,
}

impl ClaudeAiLimits {
    /// 从 JSON map 构造（头解析 / 跨模块搬运用；键名对齐 TS snake_case）。
    pub fn from_value(value: &serde_json::Value) -> Self {
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(QuotaStatus::from_str);
        let defaults = ClaudeAiLimits {
            status,
            unified_rate_limit_fallback_available: value
                .get("unifiedRateLimitFallbackAvailable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            is_using_overage: value
                .get("isUsingOverage")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            ..Default::default()
        };
        ClaudeAiLimits {
            resets_at: value.get("resetsAt").and_then(serde_json::Value::as_f64),
            rate_limit_type: value
                .get("rateLimitType")
                .and_then(serde_json::Value::as_str)
                .and_then(RateLimitType::from_str),
            utilization: value.get("utilization").and_then(serde_json::Value::as_f64),
            overage_status: value
                .get("overageStatus")
                .and_then(serde_json::Value::as_str)
                .and_then(QuotaStatus::from_str),
            overage_resets_at: value.get("overageResetsAt").and_then(serde_json::Value::as_f64),
            overage_disabled_reason: value
                .get("overageDisabledReason")
                .and_then(serde_json::Value::as_str)
                .map(OverageDisabledReason::from_str),
            surpassed_threshold: value
                .get("surpassedThreshold")
                .and_then(serde_json::Value::as_f64),
            ..defaults
        }
    }
}

/// 按限流状态产出消息；无需展示时返回 None。
pub fn get_rate_limit_message(limits: &ClaudeAiLimits, model: &str) -> Option<RateLimitMessage> {
    // overage 场景优先（订阅被拒但 overage 可用）；
    // getUsingOverageText 与 warning 分开渲染
    if limits.is_using_overage {
        // 接近 overage 消费上限时给 warning
        if limits.overage_status == Some(QuotaStatus::AllowedWarning) {
            return Some(RateLimitMessage {
                message: "You're close to your extra usage spending limit".to_string(),
                severity: RateLimitSeverity::Warning,
            });
        }
        return None;
    }

    // ERROR：limits 被拒
    if limits.status == Some(QuotaStatus::Rejected) {
        return Some(RateLimitMessage {
            message: get_limit_reached_text(limits, model),
            severity: RateLimitSeverity::Error,
        });
    }

    // WARNING：early warning
    if limits.status == Some(QuotaStatus::AllowedWarning) {
        // 仅利用率高于阈值（70%）时警告：防周重置后 API 发低利用率的
        // allowed_warning 假警报
        const WARNING_THRESHOLD: f64 = 0.7;
        if limits.utilization.map(|u| u < WARNING_THRESHOLD).unwrap_or(false) {
            return None;
        }

        // 非账单管理员的 Team/Enterprise 用户：overage 已启用时静默滚入
        // overage，不打扰
        let subscription_type = get_subscription_type();
        let is_team_or_enterprise = matches!(
            subscription_type.as_ref().map(|t| t.as_str()),
            Some("team") | Some("enterprise")
        );
        let has_extra_usage_enabled = has_extra_usage_enabled();

        if is_team_or_enterprise && has_extra_usage_enabled && !has_claude_ai_billing_access() {
            return None;
        }

        if let Some(text) = get_early_warning_text(limits) {
            return Some(RateLimitMessage {
                message: text,
                severity: RateLimitSeverity::Warning,
            });
        }
    }

    None
}

/// API 错误消息（errors.ts 用）：仅返回 error 级别文案。
pub fn get_rate_limit_error_message(limits: &ClaudeAiLimits, model: &str) -> Option<String> {
    let message = get_rate_limit_message(limits, model)?;
    if message.severity == RateLimitSeverity::Error {
        Some(message.message)
    } else {
        None
    }
}

/// UI footer 的 warning 文案（error 在 AssistantTextMessages 展示）。
pub fn get_rate_limit_warning(limits: &ClaudeAiLimits, model: &str) -> Option<String> {
    let message = get_rate_limit_message(limits, model)?;
    if message.severity == RateLimitSeverity::Warning {
        Some(message.message)
    } else {
        None
    }
}

fn get_limit_reached_text(limits: &ClaudeAiLimits, _model: &str) -> String {
    let resets_at = limits.resets_at;
    let reset_time = resets_at.and_then(|t| format_reset_time(t, true, true));
    let overage_reset_time = limits.overage_resets_at.and_then(|t| format_reset_time(t, true, true));
    let reset_message = reset_time
        .as_deref()
        .map(|t| format!(" · resets {t}"))
        .unwrap_or_default();

    // 订阅与 overage 双双耗尽：展示最早的 reset 时间
    if limits.overage_status == Some(QuotaStatus::Rejected) {
        let mut overage_reset_message = String::new();
        if let (Some(resets_at), Some(overage_resets_at)) = (resets_at, limits.overage_resets_at) {
            if resets_at < overage_resets_at {
                overage_reset_message = format!(" · resets {}", reset_time.unwrap_or_default());
            } else {
                overage_reset_message = format!(" · resets {}", overage_reset_time.unwrap_or_default());
            }
        } else if let Some(reset_time) = &reset_time {
            overage_reset_message = format!(" · resets {reset_time}");
        } else if let Some(overage_time) = &overage_reset_time {
            overage_reset_message = format!(" · resets {overage_time}");
        }

        if limits.overage_disabled_reason == Some(OverageDisabledReason::OutOfCredits) {
            return format!("You're out of extra usage{overage_reset_message}");
        }

        return format_limit_reached_text("limit", &overage_reset_message);
    }

    match limits.rate_limit_type {
        Some(RateLimitType::SevenDaySonnet) => {
            // pro/enterprise 的 Sonnet 限额等同 weekly 限额
            let subscription_type = get_subscription_type();
            let is_pro_or_enterprise = matches!(
                subscription_type.as_ref().map(|t| t.as_str()),
                Some("pro") | Some("enterprise")
            );
            let limit = if is_pro_or_enterprise { "weekly limit" } else { "Sonnet limit" };
            format_limit_reached_text(limit, &reset_message)
        }
        Some(RateLimitType::SevenDayOpus) => format_limit_reached_text("Opus limit", &reset_message),
        Some(RateLimitType::SevenDay) => format_limit_reached_text("weekly limit", &reset_message),
        Some(RateLimitType::FiveHour) => format_limit_reached_text("session limit", &reset_message),
        _ => format_limit_reached_text("usage limit", &reset_message),
    }
}

fn get_early_warning_text(limits: &ClaudeAiLimits) -> Option<String> {
    let limit_name: Option<&str> = match limits.rate_limit_type {
        Some(RateLimitType::SevenDay) => Some("weekly limit"),
        Some(RateLimitType::FiveHour) => Some("session limit"),
        Some(RateLimitType::SevenDayOpus) => Some("Opus limit"),
        Some(RateLimitType::SevenDaySonnet) => Some("Sonnet limit"),
        Some(RateLimitType::Overage) => Some("extra usage"),
        None => return None,
    };

    // early warning 基于利用率与 reset 计算，二者应同时存在
    let used = limits.utilization.map(|u| (u * 100.0).floor());
    let reset_time = limits.resets_at.and_then(|t| format_reset_time(t, true, true));

    let upsell = get_warning_upsell_text(limits.rate_limit_type);
    let with_upsell = |base: String| match upsell {
        Some(upsell) => format!("{base} · {upsell}"),
        None => base,
    };

    if let (Some(used), Some(reset_time)) = (used, reset_time.as_deref()) {
        return Some(with_upsell(format!(
            "You've used {used}% of your {} · resets {reset_time}",
            limit_name.unwrap_or("usage limit")
        )));
    }
    if let Some(used) = used {
        return Some(with_upsell(format!(
            "You've used {used}% of your {}",
            limit_name.unwrap_or("usage limit")
        )));
    }

    let limit_name = if limits.rate_limit_type == Some(RateLimitType::Overage) {
        // 对「Approaching <x>」措辞，「extra usage limit」更顺
        "extra usage limit"
    } else {
        limit_name?
    };

    if let Some(reset_time) = reset_time.as_deref() {
        return Some(with_upsell(format!(
            "Approaching {limit_name} · resets {reset_time}"
        )));
    }
    Some(with_upsell(format!("Approaching {limit_name}")))
}

/// warning 的追加指令文案。仅在 warning 展示（真实限流命中走交互菜单）。
fn get_warning_upsell_text(rate_limit_type: Option<RateLimitType>) -> Option<String> {
    let subscription_type = get_subscription_type();
    let has_extra_usage_enabled = has_extra_usage_enabled();

    // 5 小时会话限额 warning
    if rate_limit_type == Some(RateLimitType::FiveHour) {
        // Teams/Enterprise 未开 overage：提示申请（AWS marketplace 等不
        // 支持 overage 的组织类型不提示）
        if matches!(
            subscription_type.as_ref().map(|t| t.as_str()),
            Some("team") | Some("enterprise")
        ) {
            if !has_extra_usage_enabled && is_overage_provisioning_allowed() {
                return Some("/extra-usage to request more".to_string());
            }
            // 已开 overage 或 billing 类型不支持：无追加
            return None;
        }
        // Pro/Max：提示升级
        if matches!(
            subscription_type.as_ref().map(|t| t.as_str()),
            Some("pro") | Some("max")
        ) {
            return Some("/upgrade to keep using Claude Code".to_string());
        }
    }

    // Overage warning（接近消费上限）
    if rate_limit_type == Some(RateLimitType::Overage) {
        if matches!(
            subscription_type.as_ref().map(|t| t.as_str()),
            Some("team") | Some("enterprise")
        ) && !has_extra_usage_enabled
            && is_overage_provisioning_allowed()
        {
            return Some("/extra-usage to request more".to_string());
        }
    }

    // 周限额 warning 按规格不展示追加指令
    None
}

/// overage 模式切换的瞬态通知文案（进入 overage 时）。
pub fn get_using_overage_text(limits: &ClaudeAiLimits) -> String {
    let reset_time = limits
        .resets_at
        .and_then(|t| format_reset_time(t, true, true))
        .unwrap_or_default();

    let limit_name: Option<&str> = match limits.rate_limit_type {
        Some(RateLimitType::FiveHour) => Some("session limit"),
        Some(RateLimitType::SevenDay) => Some("weekly limit"),
        Some(RateLimitType::SevenDayOpus) => Some("Opus limit"),
        Some(RateLimitType::SevenDaySonnet) => {
            // pro/enterprise 的 Sonnet 限额等同 weekly 限额
            let subscription_type = get_subscription_type();
            let is_pro_or_enterprise = matches!(
                subscription_type.as_ref().map(|t| t.as_str()),
                Some("pro") | Some("enterprise")
            );
            if is_pro_or_enterprise { Some("weekly limit") } else { Some("Sonnet limit") }
        }
        _ => None,
    };

    let Some(limit_name) = limit_name else {
        return "Now using extra usage".to_string();
    };

    let reset_message = if reset_time.is_empty() {
        String::new()
    } else {
        format!(" · Your {limit_name} resets {reset_time}")
    };
    format!("You're now using extra usage{reset_message}")
}

fn format_limit_reached_text(limit: &str, reset_message: &str) -> String {
    // ant 用户的增强文案
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        return format!(
            "You've hit your {limit}{reset_message}. If you have feedback about this limit, post in {FEEDBACK_CHANNEL_ANT}. You can reset your limits with /reset-limits"
        );
    }
    format!("You've hit your {limit}{reset_message}")
}

// ---------------------------------------------------------------------------
// 内部等价实现（format.ts / billing.ts 未转换）
// ---------------------------------------------------------------------------

/// `formatResetTime` 最小等价：>24h 展示「月 日, 时段」，<=24h 仅时段；
/// am/pm 小写无空格；可选时区后缀。输入为 epoch 秒。
pub(crate) fn format_reset_time(
    timestamp_in_seconds: f64,
    show_timezone: bool,
    show_time: bool,
) -> Option<String> {
    if timestamp_in_seconds <= 0.0 {
        return None;
    }
    let timestamp_ms = (timestamp_in_seconds * 1000.0) as i64;
    let date = Local.timestamp_opt(timestamp_ms, 0).single()?;
    let now = Local::now();
    let minutes = date.minute();

    let hours_until_reset = (date.timestamp() - now.timestamp()) as f64 / (1000.0 * 60.0 * 60.0);

    // TS：minute === 0 时不带分钟；am/pm 小写无前导空格
    let time_fragment = |date: DateTime<Local>| {
        let (is_pm, hour_12) = date.hour12();
        let am_pm = if is_pm { "pm" } else { "am" };
        if minutes == 0 {
            format!("{hour_12}{am_pm}")
        } else {
            format!("{hour_12}:{minutes:02}{am_pm}")
        }
    };

    if hours_until_reset > 24.0 {
        // 月 + 日（+ 年），可选时间
        let month = month_short(date.month());
        let day = date.day();
        let mut formatted = if show_time {
            format!("{month} {day}, {}", time_fragment(date))
        } else {
            format!("{month} {day}")
        };
        if date.year() != now.year() {
            formatted = format!("{}, {}", formatted, date.year());
        }
        if show_timezone {
            formatted.push_str(&format!(" ({})", get_time_zone()));
        }
        return Some(formatted);
    }

    // 24 小时内：仅时间
    let mut formatted = time_fragment(date);
    if show_timezone {
        formatted.push_str(&format!(" ({})", get_time_zone()));
    }
    Some(formatted)
}

fn month_short(month: u32) -> &'static str {
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

/// `hasExtraUsageEnabled`：oauthAccount.hasExtraUsageEnabled。
fn has_extra_usage_enabled() -> bool {
    read_global_config()
        .get("oauthAccount")
        .and_then(|a| a.get("hasExtraUsageEnabled"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// `hasClaudeAiBillingAccess` 最小等价（utils/billing.ts 未转换）。
fn has_claude_ai_billing_access() -> bool {
    if !is_claude_ai_subscriber() {
        return false;
    }
    let subscription_type = get_subscription_type();
    // 消费版（Max/Pro）：个人用户始终有账单权限
    if matches!(
        subscription_type.as_ref().map(|t| t.as_str()),
        Some("max") | Some("pro")
    ) {
        return true;
    }
    // Team/Enterprise：检查 admin / billing 角色
    let global_config = read_global_config();
    let org_role = global_config
        .get("oauthAccount")
        .and_then(|a| a.get("organizationRole"))
        .and_then(serde_json::Value::as_str);
    matches!(
        org_role,
        Some("admin") | Some("billing") | Some("owner") | Some("primary_owner")
    )
}

// --- 全局配置 duck-type（仅读）---

fn read_global_config() -> serde_json::Value {
    static CACHE: OnceLock<Option<serde_json::Value>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = global_claude_file();
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        })
        .clone()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

fn global_claude_file() -> PathBuf {
    let config_home = crate::utils::env_utils::get_claude_config_home_dir();
    let legacy = PathBuf::from(&config_home).join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    let suffix = crate::constants::oauth::file_suffix_for_oauth_config();
    let filename = format!(".claude{suffix}.json");
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(filename),
        _ => PathBuf::from(crate::utils::system_directories::home_dir()).join(filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(status: &str) -> ClaudeAiLimits {
        ClaudeAiLimits {
            status: QuotaStatus::from_str(status),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_detection() {
        assert!(is_rate_limit_error_message("You've hit your weekly limit"));
        assert!(!is_rate_limit_error_message("Something else"));
    }

    #[test]
    fn rejected_produces_error() {
        let message = get_rate_limit_message(&limits("rejected"), "claude-sonnet").unwrap();
        assert_eq!(message.severity, RateLimitSeverity::Error);
        assert!(message.message.starts_with("You've hit your usage limit"));
    }

    #[test]
    fn rejected_five_hour_uses_session_limit() {
        let mut limits = limits("rejected");
        limits.rate_limit_type = Some(RateLimitType::FiveHour);
        limits.resets_at = Some((Utc::now().timestamp() + 3600) as f64);
        let message = get_rate_limit_message(&limits, "claude-sonnet").unwrap();
        assert!(message.message.contains("session limit"));
        assert!(message.message.contains("resets"));
    }

    #[test]
    fn allowed_warning_below_threshold_is_silent() {
        let mut limits = limits("allowed_warning");
        limits.utilization = Some(0.5);
        assert!(get_rate_limit_message(&limits, "claude-sonnet").is_none());
    }

    #[test]
    fn allowed_warning_with_high_utilization_warns() {
        let mut limits = limits("allowed_warning");
        limits.utilization = Some(0.9);
        limits.rate_limit_type = Some(RateLimitType::SevenDay);
        limits.resets_at = Some((Utc::now().timestamp() + 7 * 86400) as f64);
        let message = get_rate_limit_message(&limits, "claude-sonnet").unwrap();
        assert_eq!(message.severity, RateLimitSeverity::Warning);
        assert!(message.message.contains("You've used 90% of your weekly limit"));
    }

    #[test]
    fn overage_in_use_near_limit_warns() {
        let mut limits = ClaudeAiLimits {
            is_using_overage: true,
            ..Default::default()
        };
        assert!(get_rate_limit_message(&limits, "m").is_none());
        limits.overage_status = Some(QuotaStatus::AllowedWarning);
        let message = get_rate_limit_message(&limits, "m").unwrap();
        assert_eq!(message.severity, RateLimitSeverity::Warning);
        assert!(message.message.contains("extra usage spending limit"));
    }

    #[test]
    fn using_overage_text_names_limit() {
        let mut limits = ClaudeAiLimits {
            is_using_overage: true,
            ..Default::default()
        };
        assert_eq!(get_using_overage_text(&limits), "Now using extra usage");
        limits.rate_limit_type = Some(RateLimitType::FiveHour);
        limits.resets_at = Some((Utc::now().timestamp() + 3600) as f64);
        let text = get_using_overage_text(&limits);
        assert!(text.starts_with("You're now using extra usage"));
        assert!(text.contains("session limit resets"));
    }

    #[test]
    fn format_reset_time_shapes() {
        let now = Utc::now().timestamp();
        let within_day = format_reset_time((now + 3600) as f64, false, true).unwrap();
        // 24 小时内：仅时间（am/pm 小写无空格）
        assert!(within_day.ends_with("am") || within_day.ends_with("pm"));
        assert!(!within_day.contains("  "));

        let beyond_day = format_reset_time((now + 7 * 86400) as f64, false, true).unwrap();
        // 超过 24 小时：含月日
        assert!(beyond_day.contains(' ') || beyond_day.contains(','));
    }
}
