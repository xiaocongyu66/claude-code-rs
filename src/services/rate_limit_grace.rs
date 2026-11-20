//! rate_limit_grace.rs — 官方临近上限收尾 / 宽限窗口状态机（重写自
//! `src/services/rateLimitGrace.ts`）。
//!
//! 纯逻辑搬运（对齐官方 2.1.284 的 `PDn`/`fve`/`Adt`/`_co`）：
//! - `parse_unified_grace_headers`：响应头 → 宽限观察（zone 选择、
//!   reset 容差门、extra-usage 覆盖判定）。
//! - `GraceWrapUpState`：none/owed/taken/placed 四态收尾机 + fence 防
//!   乱序观察。
//! - 临近上限提示（每 5h 窗口一次、per-tier 阈值、wall-active 抑制）。
//! - 注入构建（`build_main_turn_grace_notes` 主循环 +
//!   `build_subagent_grace_note` 子代理派发，后者 feature 门控）。
//! - 合成拒绝（`get_wrap_up_synthetic_rejection`）。
//!
//! 已知偏差：
//! - `installGraceLimitsListener` 的动态 import 依赖 claudeAiLimits（未
//!   转换为 Rust）：该函数为 no-op，观察输入改由
//!   `observe_grace_from_limits_snapshot` 供上层显式喂数据。
//! - `feature('LANTERN_WICK_BRAID')` → 环境变量
//!   `FEATURE_LANTERN_WICK_BRAID`（仓库统一约定，见 log.rs）。
//! - `UserMessage`（@ant/model-provider）未转换：注入消息以本模块结构
//!   体表达（字段与 TS 的 isMeta/turnCompanion/usageLimitNote 对齐）。
//! - TS 的 `Date.now()` 入参为 `now_ms`（u128 毫秒），测试确定性。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use chrono::Utc;
use once_cell::sync::Lazy;
use uuid::Uuid;

use crate::utils::env_utils::is_env_truthy;

use super::analytics::index::{log_event, LogEventMetadata, MetadataValue};

// ── 头名称（官方常量） ─────────────────────────────────────────────────

const UNIFIED_STATUS_HEADER: &str = "anthropic-ratelimit-unified-status";
const GRACE_5H_UTILIZATION_HEADER: &str = "anthropic-ratelimit-unified-grace-5h-utilization";
const GRACE_7D_UTILIZATION_HEADER: &str = "anthropic-ratelimit-unified-grace-7d-utilization";
const FIVE_HOUR_RESET_HEADER: &str = "anthropic-ratelimit-unified-5h-reset";
const SEVEN_DAY_RESET_HEADER: &str = "anthropic-ratelimit-unified-7d-reset";
const OVERAGE_STATUS_HEADER: &str = "anthropic-ratelimit-unified-overage-status";
const OVERAGE_IN_USE_HEADER: &str = "anthropic-ratelimit-unified-overage-in-use";

/// reset 头仅在恰好（勉强）在未来时有效：epoch 秒 × 1000 必须超过
/// now - 1000ms。
const RESET_VALIDITY_TOLERANCE_MS: u128 = 1_000;

/// 临近上限触发阈值，默认 tier（官方 QDn = 0.95）。
pub const NEAR_LIMIT_5H_UTILIZATION_THRESHOLD: f64 = 0.95;

/// 官方 JDn：按 tier 的临近上限 5h-utilization 阈值。
///
/// tier 来自 OAuth 账户的 `rateLimitTier`（CCH 等价物 auth.rs
/// `get_rate_limit_tier`）：
///   - `default_claude_max_5x`  → 0.99
///   - `default_claude_max_20x` → 0.9975
///   - 其他 / 未知 → QDn（NEAR_LIMIT_5H_UTILIZATION_THRESHOLD）
pub fn near_limit_threshold_for_rate_limit_tier(rate_limit_tier: Option<&str>) -> f64 {
    match rate_limit_tier {
        Some("default_claude_max_5x") => 0.99,
        Some("default_claude_max_20x") => 0.9975,
        _ => NEAR_LIMIT_5H_UTILIZATION_THRESHOLD,
    }
}

// ── 注入文案（官方字符串，逐字符） ─────────────────────────────────────

pub const USAGE_LIMIT_GRACE_WRAPUP_NOTE: &str =
    "[Usage limit reached — grace window active. Wrap up: finish or checkpoint; don't start subagents or long work.]";

pub const USAGE_LIMIT_NEAR_WRAPUP_NOTE: &str =
    "[Usage limit approaching. Checkpoint now: finish the current step, then list up to 3 short bullets of the most impactful remaining work. Don't start subagents or long-running work.]";

pub const USAGE_LIMIT_NEAR_WRAPUP_UI_NOTICE: &str =
    "Approaching your 5-hour usage limit — Claude will wrap up the current step.";

/// 发布说明文案。官方默认为空（完全抑制发布说明）。
pub const USAGE_LIMIT_GRACE_RELEASE_NOTE: &str = "";

// ── 类型 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceZoneType {
    FiveHour,
    SevenDay,
}

impl GraceZoneType {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            GraceZoneType::FiveHour => "five_hour",
            GraceZoneType::SevenDay => "seven_day",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraUsageStatus {
    Allowed,
    AllowedWarning,
}

impl ExtraUsageStatus {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "allowed" => Some(ExtraUsageStatus::Allowed),
            "allowed_warning" => Some(ExtraUsageStatus::AllowedWarning),
            _ => None,
        }
    }

    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            ExtraUsageStatus::Allowed => "allowed",
            ExtraUsageStatus::AllowedWarning => "allowed_warning",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraceZone {
    pub rate_limit_type: GraceZoneType,
    /// epoch 秒。
    pub resets_at: Option<f64>,
    pub extra_usage_status: Option<ExtraUsageStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceTurnNoteState {
    None,
    Owed,
    Taken,
    Placed,
}

/// 头读取抽象（TS `GraceHeadersLike`）。
pub trait GraceHeaders {
    fn get(&self, name: &str) -> Option<&str>;
}

impl GraceHeaders for HashMap<String, String> {
    fn get(&self, name: &str) -> Option<&str> {
        self.get(name).map(|s| s.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedGraceHeaders {
    /// unified-status 头缺失 → false（limiter 未运行）。
    pub limiter_ran: bool,
    pub zone: Option<GraceZone>,
    pub covered: bool,
    pub five_hour_utilization: f64,
    pub seven_day_utilization: f64,
    pub five_hour_reset: Option<f64>,
    pub seven_day_reset: Option<f64>,
}

/// 遥测事件名（官方临近上限收尾族）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceTelemetryEvent {
    UsageLimitGraceWrapup,
    UsageLimitGraceWrapupSubagent,
    UsageLimitGraceRelease,
    UsageLimitGraceReleaseSubagent,
    UsageLimitNearWrapup,
}

impl GraceTelemetryEvent {
    fn as_str(self) -> &'static str {
        match self {
            GraceTelemetryEvent::UsageLimitGraceWrapup => "usage_limit_grace_wrapup",
            GraceTelemetryEvent::UsageLimitGraceWrapupSubagent => "usage_limit_grace_wrapup_subagent",
            GraceTelemetryEvent::UsageLimitGraceRelease => "usage_limit_grace_release",
            GraceTelemetryEvent::UsageLimitGraceReleaseSubagent => "usage_limit_grace_release_subagent",
            GraceTelemetryEvent::UsageLimitNearWrapup => "usage_limit_near_wrapup",
        }
    }
}

/// claudeAiLimits 的原始每窗口利用率读数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraceWindowObservation {
    pub utilization: f64,
    /// epoch 秒。
    pub resets_at: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraceLimitsObservation {
    pub status: String,
    pub five_hour: Option<GraceWindowObservation>,
    pub seven_day: Option<GraceWindowObservation>,
    pub overage_status: Option<String>,
    pub is_using_overage: bool,
}

/// 合成拒绝形状（官方 dxt builder 输出；ClaudeAILimits 平价子集）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraceSyntheticRejection {
    pub status: String,
    pub resets_at: Option<f64>,
    pub rate_limit_type: Option<GraceZoneType>,
    pub unified_rate_limit_fallback_available: bool,
    pub is_using_overage: bool,
    pub rate_limit_grace_active: bool,
}

/// 注入的 meta 用户消息（TS `UserMessage` 的平价结构）。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageLimitNoteMessage {
    pub r#type: String,
    pub role: String,
    pub content: String,
    pub is_meta: bool,
    pub turn_companion: bool,
    pub usage_limit_note: UsageLimitNoteKind,
    pub uuid: String,
    /// ISO8601 时间戳。
    pub timestamp: String,
}

/// UI 通知（官方 Ut builder：system 消息，notice 级别）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraceUiNotice {
    pub r#type: String,
    pub subtype: String,
    pub content: String,
    pub level: String,
    pub is_meta: bool,
    pub timestamp: String,
    pub uuid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLimitNoteKind {
    WrapUp,
    NearLimit,
    Release,
}

impl UsageLimitNoteKind {
    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            UsageLimitNoteKind::WrapUp => "wrap_up",
            UsageLimitNoteKind::NearLimit => "near_limit",
            UsageLimitNoteKind::Release => "release",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "wrap_up" => Some(UsageLimitNoteKind::WrapUp),
            "near_limit" => Some(UsageLimitNoteKind::NearLimit),
            "release" => Some(UsageLimitNoteKind::Release),
            _ => None,
        }
    }
}

/// 消息载体（用于 `find_last_usage_limit_note` 的历史回溯）。
#[derive(Debug, Clone)]
pub struct UsageLimitNoteCarrier {
    pub r#type: Option<String>,
    pub usage_limit_note: Option<String>,
}

// ── 头解析（官方 PDn） ─────────────────────────────────────────────────

fn read_clamped_utilization(headers: &dyn GraceHeaders, name: &str) -> f64 {
    let value: f64 = headers.get(name).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn read_reset_seconds(headers: &dyn GraceHeaders, name: &str, now_ms: u128) -> Option<f64> {
    let value: f64 = headers.get(name)?.parse().ok()?;
    let is_valid = value.is_finite() && (value * 1000.0) > (now_ms - RESET_VALIDITY_TOLERANCE_MS) as f64;
    // 保留解析原值：容差门是唯一有效性判定（与 TS 一致：不四舍五入）
    if is_valid { Some(value) } else { None }
}

fn read_extra_usage_status(headers: &dyn GraceHeaders) -> Option<ExtraUsageStatus> {
    headers
        .get(OVERAGE_STATUS_HEADER)
        .and_then(ExtraUsageStatus::from_str)
}

/// 解析统一限流头为宽限观察（limiterRan / zone 选择 / 覆盖判定）。
pub fn parse_unified_grace_headers(headers: &dyn GraceHeaders, now_ms: u128) -> ParsedGraceHeaders {
    if headers.get(UNIFIED_STATUS_HEADER).is_none() {
        return ParsedGraceHeaders {
            limiter_ran: false,
            zone: None,
            covered: false,
            five_hour_utilization: 0.0,
            seven_day_utilization: 0.0,
            five_hour_reset: None,
            seven_day_reset: None,
        };
    }
    let five_hour_utilization = read_clamped_utilization(headers, GRACE_5H_UTILIZATION_HEADER);
    let seven_day_utilization = read_clamped_utilization(headers, GRACE_7D_UTILIZATION_HEADER);
    if five_hour_utilization == 0.0 && seven_day_utilization == 0.0 {
        return ParsedGraceHeaders {
            limiter_ran: true,
            zone: None,
            covered: false,
            five_hour_utilization: 0.0,
            seven_day_utilization: 0.0,
            five_hour_reset: None,
            seven_day_reset: None,
        };
    }
    let five_hour_reset = read_reset_seconds(headers, FIVE_HOUR_RESET_HEADER, now_ms);
    let seven_day_reset = read_reset_seconds(headers, SEVEN_DAY_RESET_HEADER, now_ms);
    let extra_usage_status = read_extra_usage_status(headers);
    let five_hour_resets_later = five_hour_utilization > 0.0
        && five_hour_reset.is_some()
        && (seven_day_reset.is_none() || five_hour_reset > seven_day_reset);
    let rate_limit_type = if seven_day_utilization > 0.0 && !five_hour_resets_later {
        GraceZoneType::SevenDay
    } else {
        GraceZoneType::FiveHour
    };
    let resets_at = if rate_limit_type == GraceZoneType::SevenDay {
        seven_day_reset
    } else {
        five_hour_reset
    };
    let zone = GraceZone {
        rate_limit_type,
        resets_at,
        extra_usage_status,
    };
    let covered = extra_usage_status.is_some()
        || headers.get(OVERAGE_IN_USE_HEADER) == Some("true");
    ParsedGraceHeaders {
        limiter_ran: true,
        zone: Some(zone),
        covered,
        five_hour_utilization,
        seven_day_utilization,
        five_hour_reset,
        seven_day_reset,
    }
}

// ── 收尾状态机（官方 fve） ─────────────────────────────────────────────

/// 宽限收尾状态机。非 Send 的单线程语义保持（模块级单例 + Mutex）。
#[derive(Debug, Default)]
pub struct GraceWrapUpState {
    pub zone: Option<GraceZone>,
    pub covered: bool,
    pub turn_note: GraceTurnNoteState,
    /// applyParsed 使用更老时间戳的调用被丢弃。
    pub fence_ms: u128,
}

impl GraceWrapUpState {
    fn default_state() -> Self {
        GraceWrapUpState {
            zone: None,
            covered: false,
            turn_note: GraceTurnNoteState::None,
            fence_ms: 0,
        }
    }

    /// 懒过期视图：reset 已过的 zone 读作 None。
    pub fn get_zone(&self, now_ms: u128) -> Option<&GraceZone> {
        if let Some(zone) = &self.zone {
            if let Some(resets_at) = zone.resets_at {
                if (now_ms as f64) / 1000.0 >= resets_at {
                    return None;
                }
            }
        }
        self.zone.as_ref()
    }

    pub fn apply_response(&mut self, headers: &dyn GraceHeaders, now_ms: u128) {
        let parsed = parse_unified_grace_headers(headers, now_ms);
        self.apply_parsed(&parsed, now_ms);
    }

    pub fn apply_parsed(&mut self, parsed: &ParsedGraceHeaders, now_ms: u128) {
        if !parsed.limiter_ran || now_ms < self.fence_ms {
            return;
        }
        self.fence_ms = now_ms;
        if parsed.zone.is_none() || parsed.zone.as_ref().and_then(|z| z.resets_at).is_some() {
            self.drop_ended_zone(now_ms);
        }
        self.zone = parsed.zone.clone();
        self.covered = parsed.zone.is_some() && parsed.covered;
        if parsed.zone.is_none() || parsed.covered {
            self.turn_note = GraceTurnNoteState::None;
        } else if self.turn_note == GraceTurnNoteState::None {
            self.turn_note = GraceTurnNoteState::Owed;
        }
    }

    fn drop_ended_zone(&mut self, now_ms: u128) {
        if self.zone.is_some() && self.get_zone(now_ms).is_none() {
            self.zone = None;
            self.covered = false;
            self.turn_note = GraceTurnNoteState::None;
        }
    }

    /// 真实配额拒绝清除 zone 的 extra-usage 覆盖。
    pub fn on_refused(&mut self) {
        let had_coverage = self.covered
            || self.zone.as_ref().and_then(|z| z.extra_usage_status).is_some();
        if let Some(zone) = &self.zone {
            if zone.extra_usage_status.is_some() {
                let mut cleared = zone.clone();
                cleared.extra_usage_status = None;
                self.zone = Some(cleared);
            }
        }
        self.covered = false;
        // 拒绝破坏了 extra-coverage 承诺：仍存活的未覆盖 zone 再次欠下
        // 收尾便条（applyParsed 只会把 none → owed，无此再臂）
        if had_coverage && self.zone.is_some() && self.turn_note == GraceTurnNoteState::None {
            self.turn_note = GraceTurnNoteState::Owed;
        }
    }

    /// 全清 + 升 fence（账户切换 / 登出发布路径）。
    pub fn leave(&mut self, now_ms: u128) {
        self.zone = None;
        self.covered = false;
        self.turn_note = GraceTurnNoteState::None;
        self.fence_ms = self.fence_ms.max(now_ms);
    }

    /// 官方 onMainTurnStart：主循环每个 depth-0 回合入口。
    pub fn on_main_turn_start(&mut self) {
        self.turn_note = GraceTurnNoteState::None;
    }

    /// owed → taken。每个宽限窗口仅一次。
    pub fn take_turn_note(&mut self, now_ms: u128) -> bool {
        self.drop_ended_zone(now_ms);
        if self.turn_note != GraceTurnNoteState::Owed {
            return false;
        }
        self.turn_note = GraceTurnNoteState::Taken;
        true
    }

    /// taken → placed（便条已进入 transcript）。
    pub fn mark_start_note_placed(&mut self) {
        if self.turn_note == GraceTurnNoteState::Taken {
            self.turn_note = GraceTurnNoteState::Placed;
        }
    }

    /// 便条已放置且 zone 存活且未覆盖：随后的 API 拒绝是预期内的——回
    /// 合在此终止。
    pub fn continue_after_wrap_up_at(&self, now_ms: u128) -> Option<(f64, GraceZoneType)> {
        let zone = self.get_zone(now_ms)?;
        if self.turn_note == GraceTurnNoteState::Placed && !self.covered {
            if let Some(resets_at) = zone.resets_at {
                return Some((resets_at, zone.rate_limit_type));
            }
        }
        None
    }

    /// zone 存活且未覆盖 → 便条到期（主循环或子代理）。
    pub fn subagent_note_due(&self, now_ms: u128) -> bool {
        self.get_zone(now_ms).is_some() && !self.covered
    }
}

// ── 模块级单例 ────────────────────────────────────────────────────────

static GRACE: Lazy<Mutex<GraceWrapUpState>> =
    Lazy::new(|| Mutex::new(GraceWrapUpState::default_state()));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 获取单例的守卫（主调用族使用）。
pub fn get_grace_wrap_up() -> std::sync::MutexGuard<'static, GraceWrapUpState> {
    lock(&GRACE)
}

// ── 临近上限提示（官方 Adt / pendingNearLimitWrapUpHint） ────────────────

static USAGE_LIMIT_WALL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 注入的 rateLimitTier 查询（官方 Xw()；默认 null tier）。set / lock /
/// reset 共用同一静态。
static TIER_ACCESSOR: Lazy<Mutex<Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>>>> =
    Lazy::new(|| Mutex::new(None));

fn lock_tier_accessor() -> Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>> {
    lock(&TIER_ACCESSOR).clone()
}

/// 官方 sL() 等价：wall 处于 active 阶段时为 true。
pub fn is_usage_limit_wall_active() -> bool {
    USAGE_LIMIT_WALL_ACTIVE.load(Ordering::SeqCst)
}

/// 喂入 wall 阶段（官方 `mr().state.phase==="active"`）。
pub fn set_usage_limit_wall_active(active: bool) {
    USAGE_LIMIT_WALL_ACTIVE.store(active, Ordering::SeqCst);
}

/// 注入实时 rateLimitTier 查询（官方 Xw()；默认 null tier）。
pub fn set_rate_limit_tier_accessor(accessor: impl Fn() -> Option<String> + Send + Sync + 'static) {
    *lock(&TIER_ACCESSOR) = Some(std::sync::Arc::new(accessor));
}

struct NearLimitState {
    pending_hint: bool,
    window_key: Option<f64>,
}

static NEAR_LIMIT: Lazy<Mutex<NearLimitState>> = Lazy::new(|| {
    Mutex::new(NearLimitState {
        pending_hint: false,
        window_key: None,
    })
});

fn expire_near_limit_window(now_ms: u128) {
    let mut state = lock(&NEAR_LIMIT);
    if let Some(key) = state.window_key {
        if (now_ms as f64) / 1000.0 > key {
            state.pending_hint = false;
            state.window_key = None;
        }
    }
}

fn update_near_limit_hint(parsed: &ParsedGraceHeaders, now_ms: u128, grace_covered: bool) {
    expire_near_limit_window(now_ms);
    if !parsed.limiter_ran {
        return;
    }
    // 每 5h 窗口一次的门键要求一个未来的 5h reset
    let Some(five_hour_reset) = parsed.five_hour_reset else {
        return;
    };
    if five_hour_reset * 1000.0 <= now_ms as f64 {
        return;
    }
    // 官方 ve=JDn(Xw())：per-tier 阈值
    let tier = lock_tier_accessor().and_then(|accessor| accessor());
    let threshold = near_limit_threshold_for_rate_limit_tier(tier.as_deref());
    if parsed.five_hour_utilization < threshold {
        return;
    }
    // 官方 !sL()：wall active 时完全抑制软提示
    if is_usage_limit_wall_active() {
        return;
    }
    // 已被 extra usage 覆盖：本窗口不需要收尾
    if grace_covered {
        return;
    }
    if parsed.zone.as_ref().and_then(|z| z.extra_usage_status).is_some() {
        let mut state = lock(&NEAR_LIMIT);
        state.pending_hint = false;
        return;
    }
    let mut state = lock(&NEAR_LIMIT);
    if state.window_key != Some(five_hour_reset) {
        state.pending_hint = true;
        state.window_key = Some(five_hour_reset);
    }
}

/// 消费挂起的临近上限提示（每 5h 窗口一次，未覆盖时）。
pub fn consume_pending_near_limit_wrap_up_hint(now_ms: u128) -> bool {
    expire_near_limit_window(now_ms);
    let was_pending = {
        let mut state = lock(&NEAR_LIMIT);
        if !state.pending_hint {
            return false;
        }
        state.pending_hint = false;
        true
    };
    was_pending && !lock(&GRACE).covered
}

// ── 观察喂入（claudeAiLimits 快照 → 纯解析器适配） ─────────────────────

/// 适配器：应用 claudeAiLimits 风格的限流观察。
pub fn observe_grace_from_limits_snapshot(observation: &GraceLimitsObservation, now_ms: u128) {
    // 观察绝不能破坏回合：所有失败路径吞掉（TS try/catch 等价）
    let mut headers: HashMap<String, String> = HashMap::new();
    headers.insert(
        UNIFIED_STATUS_HEADER.to_string(),
        observation.status.clone(),
    );
    if let Some(five_hour) = &observation.five_hour {
        headers.insert(
            GRACE_5H_UTILIZATION_HEADER.to_string(),
            format!("{}", five_hour.utilization),
        );
        headers.insert(
            FIVE_HOUR_RESET_HEADER.to_string(),
            format!("{}", five_hour.resets_at),
        );
    }
    if let Some(seven_day) = &observation.seven_day {
        headers.insert(
            GRACE_7D_UTILIZATION_HEADER.to_string(),
            format!("{}", seven_day.utilization),
        );
        headers.insert(
            SEVEN_DAY_RESET_HEADER.to_string(),
            format!("{}", seven_day.resets_at),
        );
    }
    if matches!(
        observation.overage_status.as_deref(),
        Some("allowed") | Some("allowed_warning")
    ) {
        headers.insert(
            OVERAGE_STATUS_HEADER.to_string(),
            observation.overage_status.clone().unwrap_or_default(),
        );
    }
    if observation.is_using_overage {
        headers.insert(OVERAGE_IN_USE_HEADER.to_string(), "true".to_string());
    }

    let fence_before = lock(&GRACE).fence_ms;
    let parsed = parse_unified_grace_headers(&headers, now_ms);
    let covered_after;
    {
        let mut grace = lock(&GRACE);
        grace.apply_parsed(&parsed, now_ms);
        covered_after = grace.covered;
    }
    // 乱序（更老）观察不更新提示
    if now_ms >= fence_before {
        update_near_limit_hint(&parsed, now_ms, covered_after);
    }
}

/// 订阅 claudeAiLimits 的状态发射，保持单例新鲜（幂等）。
///
/// **偏差（no-op）**：claudeAiLimits 尚未转换为 Rust（依赖
/// @anthropic-ai/sdk / api-client 等），无监听器可挂；上层改用
/// `observe_grace_from_limits_snapshot` 显式喂快照。待 claudeAiLimits
/// 落地后在此挂 `statusListeners`。
pub async fn install_grace_limits_listener() {
    // 有意留空：见函数文档偏差。
}

// ── 拒绝路径 ───────────────────────────────────────────────────────────

/// errors.ts 429 分支调用（异常路径绝不破坏调用方）。
pub fn grace_on_refused() {
    lock(&GRACE).on_refused();
}

/// 账户切换 / 登出发布路径：全清并清临近上限门键。
pub fn grace_leave(now_ms: u128) {
    {
        let mut grace = lock(&GRACE);
        grace.leave(now_ms);
    }
    let mut state = lock(&NEAR_LIMIT);
    state.pending_hint = false;
    state.window_key = None;
}

pub fn get_grace_zone(now_ms: u128) -> Option<GraceZone> {
    lock(&GRACE).get_zone(now_ms).cloned()
}

pub fn take_grace_turn_note(now_ms: u128) -> bool {
    lock(&GRACE).take_turn_note(now_ms)
}

pub fn mark_grace_start_note_placed() {
    lock(&GRACE).mark_start_note_placed();
}

pub fn grace_main_turn_start() {
    lock(&GRACE).on_main_turn_start();
}

pub fn grace_subagent_note_due(now_ms: u128) -> bool {
    lock(&GRACE).subagent_note_due(now_ms)
}

pub fn continue_after_wrap_up_at(now_ms: u128) -> Option<(f64, GraceZoneType)> {
    lock(&GRACE).continue_after_wrap_up_at(now_ms)
}

/// 官方 dxt builder：已放置的收尾便条 + 存活未覆盖 zone 把下一次 API
/// 拒绝变成合成回合终止拒绝。fallback 可用性恒 false。
pub fn get_wrap_up_synthetic_rejection(now_ms: u128) -> Option<GraceSyntheticRejection> {
    let (resets_at, rate_limit_type) = continue_after_wrap_up_at(now_ms)?;
    Some(GraceSyntheticRejection {
        status: "rejected".to_string(),
        resets_at: Some(resets_at),
        rate_limit_type: Some(rate_limit_type),
        unified_rate_limit_fallback_available: false,
        is_using_overage: false,
        rate_limit_grace_active: true,
    })
}

// ── 便条消息（官方 Re/Ut builders） ─────────────────────────────────────

fn default_note_text(kind: UsageLimitNoteKind) -> String {
    match kind {
        UsageLimitNoteKind::WrapUp => USAGE_LIMIT_GRACE_WRAPUP_NOTE.to_string(),
        UsageLimitNoteKind::NearLimit => USAGE_LIMIT_NEAR_WRAPUP_NOTE.to_string(),
        UsageLimitNoteKind::Release => USAGE_LIMIT_GRACE_RELEASE_NOTE.to_string(),
    }
}

fn default_note_message(
    kind: UsageLimitNoteKind,
    content: Option<&str>,
    now_ms: u128,
    uuid: Option<&str>,
) -> UsageLimitNoteMessage {
    UsageLimitNoteMessage {
        r#type: "user".to_string(),
        role: "user".to_string(),
        content: content
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_note_text(kind)),
        is_meta: true,
        turn_companion: true,
        usage_limit_note: kind,
        uuid: uuid
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        timestamp: ms_to_iso(now_ms),
    }
}

fn ms_to_iso(now_ms: u128) -> String {
    Utc
        .timestamp_opt((now_ms / 1000) as i64, 0)
        .single()
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_default()
}

pub fn create_usage_limit_note_message(
    kind: UsageLimitNoteKind,
    content: Option<&str>,
    now_ms: u128,
    uuid: Option<&str>,
) -> UsageLimitNoteMessage {
    default_note_message(kind, content, now_ms, uuid)
}

pub fn create_grace_ui_notice(now_ms: u128) -> GraceUiNotice {
    GraceUiNotice {
        r#type: "system".to_string(),
        subtype: "informational".to_string(),
        content: USAGE_LIMIT_NEAR_WRAPUP_UI_NOTICE.to_string(),
        level: "notice".to_string(),
        is_meta: false,
        timestamp: ms_to_iso(now_ms),
        uuid: Uuid::new_v4().to_string(),
    }
}

/// 官方 u()：反向回溯到最近的 usageLimitNote 用户消息。
pub fn find_last_usage_limit_note(messages: &[UsageLimitNoteCarrier]) -> Option<UsageLimitNoteKind> {
    messages
        .iter()
        .rev()
        .find_map(|message| {
            if message.r#type.as_deref() == Some("user") {
                message
                    .usage_limit_note
                    .as_deref()
                    .and_then(UsageLimitNoteKind::from_str)
            } else {
                None
            }
        })
}

// ── 遥测（官方事件族） ─────────────────────────────────────────────────

fn log_grace_event(event_name: GraceTelemetryEvent, metadata: &[(String, Option<MetadataValue>)]) {
    // analytics 不可用时 TS 用 try/catch 兜底；Rust 的 log_event 不抛
    let mut meta: LogEventMetadata = LogEventMetadata::new();
    for (key, value) in metadata {
        meta.insert(key.clone(), value.to_owned());
    }
    log_event(event_name.as_str(), &meta);
}

// ── 注入构建（官方 _co：主循环 + 子代理拆分） ──────────────────────────

/// 主循环回合开始注入的产物。
#[derive(Debug, Default)]
pub struct GraceTurnNote {
    pub notes: Vec<UsageLimitNoteMessage>,
    pub notices: Vec<GraceUiNotice>,
}

/// 主循环 depth-0 新问题入口：onMainTurnStart → takeTurnNote → 注入
/// 收尾便条 → markStartNotePlaced，然后每窗口一次的临近上限提示（UI
/// 通知 + 便条；本回合已注入收尾便条时跳过 UI），以及默认惰性的发布
/// 检测。
pub fn build_main_turn_grace_notes(
    messages: &[UsageLimitNoteCarrier],
    is_new_depth_0_question: bool,
    observation: Option<&GraceLimitsObservation>,
    now_ms: u128,
) -> GraceTurnNote {
    let mut notes: Vec<UsageLimitNoteMessage> = Vec::new();
    let mut notices: Vec<GraceUiNotice> = Vec::new();

    if let Some(observation) = observation {
        observe_grace_from_limits_snapshot(observation, now_ms);
    }

    // 官方：历史中已有的收尾/临近便条抑制本窗口所有（再）注入
    let prior_note = if is_new_depth_0_question {
        find_last_usage_limit_note(messages)
    } else {
        None
    };
    let history_has_grace_note = matches!(
        prior_note,
        Some(UsageLimitNoteKind::WrapUp) | Some(UsageLimitNoteKind::NearLimit)
    );

    let due = if is_new_depth_0_question && !history_has_grace_note {
        grace_subagent_note_due(now_ms)
    } else {
        false
    };
    if is_new_depth_0_question && !due {
        grace_main_turn_start();
    }
    let mut taken = false;
    if is_new_depth_0_question && due {
        taken = take_grace_turn_note(now_ms);
        if due && !taken {
            grace_main_turn_start();
        }
    }

    let mut wrapped_up = false;
    if taken {
        notes.push(create_usage_limit_note_message(
            UsageLimitNoteKind::WrapUp,
            None,
            now_ms,
            None,
        ));
        log_grace_event(
            GraceTelemetryEvent::UsageLimitGraceWrapup,
            &[
                ("next_steps".to_string(), Some(MetadataValue::Bool(false))),
                ("custom".to_string(), Some(MetadataValue::Bool(false))),
            ],
        );
        mark_grace_start_note_placed();
        wrapped_up = true;
    }

    // 发布检测：历史有收尾/临近便条且 zone 不再是「存活且无 extra
    // usage」。官方默认发布文案为空 → 不推便条（对齐官方默认）。
    if is_new_depth_0_question {
        let zone = get_grace_zone(now_ms);
        let zone_alive_without_extra_usage =
            zone.is_some() && zone.as_ref().and_then(|z| z.extra_usage_status).is_none();
        if !zone_alive_without_extra_usage
            && matches!(
                prior_note,
                Some(UsageLimitNoteKind::WrapUp) | Some(UsageLimitNoteKind::NearLimit)
            )
        {
            let release_text = default_note_text(UsageLimitNoteKind::Release);
            if !release_text.is_empty() {
                notes.push(create_usage_limit_note_message(
                    UsageLimitNoteKind::Release,
                    Some(&release_text),
                    now_ms,
                    None,
                ));
                log_grace_event(GraceTelemetryEvent::UsageLimitGraceRelease, &[]);
            }
        }
    }

    // 临近上限：每 5h 窗口一次，未覆盖。即便本回合注入了收尾便条也消费
    // 提示（收尾优先于软提示），但 UI 通知 + 便条仅在未注入收尾便条时
    // 发射。历史抑制跳过整个门键。
    if is_new_depth_0_question
        && !history_has_grace_note
        && consume_pending_near_limit_wrap_up_hint(now_ms)
    {
        if !wrapped_up {
            notices.push(create_grace_ui_notice(now_ms));
            notes.push(create_usage_limit_note_message(
                UsageLimitNoteKind::NearLimit,
                None,
                now_ms,
                None,
            ));
        }
        log_grace_event(GraceTelemetryEvent::UsageLimitNearWrapup, &[]);
    }

    GraceTurnNote { notes, notices }
}

/// 子代理派发注入（Task/Agent 工具）。feature `LANTERN_WICK_BRAID`
/// 门控（默认关）；门关 → 零注入。
pub fn build_subagent_grace_note(
    messages: &[UsageLimitNoteCarrier],
    is_subagent_dispatch: bool,
    now_ms: u128,
) -> Option<UsageLimitNoteMessage> {
    if !is_env_truthy(std::env::var("FEATURE_LANTERN_WICK_BRAID").ok().as_deref()) {
        return None;
    }
    if !is_subagent_dispatch {
        return None;
    }
    if !grace_subagent_note_due(now_ms) {
        return None;
    }
    if find_last_usage_limit_note(messages) == Some(UsageLimitNoteKind::WrapUp) {
        return None;
    }
    let note = create_usage_limit_note_message(UsageLimitNoteKind::WrapUp, None, now_ms, None);
    log_grace_event(
        GraceTelemetryEvent::UsageLimitGraceWrapupSubagent,
        &[
            ("next_steps".to_string(), Some(MetadataValue::Bool(false))),
            ("custom".to_string(), Some(MetadataValue::Bool(false))),
        ],
    );
    Some(note)
}

/// 测试专用：重置模块单例。
pub fn reset_grace_state_for_tests() {
    {
        let mut grace = lock(&GRACE);
        grace.zone = None;
        grace.covered = false;
        grace.turn_note = GraceTurnNoteState::None;
        grace.fence_ms = 0;
    }
    {
        let mut state = lock(&NEAR_LIMIT);
        state.pending_hint = false;
        state.window_key = None;
    }
    set_usage_limit_wall_active(false);
    *lock(&TIER_ACCESSOR) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u128 = 1_700_000_000_000;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn near_limit_thresholds_per_tier() {
        assert_eq!(
            near_limit_threshold_for_rate_limit_tier(None),
            NEAR_LIMIT_5H_UTILIZATION_THRESHOLD
        );
        assert_eq!(
            near_limit_threshold_for_rate_limit_tier(Some("default_claude_max_5x")),
            0.99
        );
        assert_eq!(
            near_limit_threshold_for_rate_limit_tier(Some("default_claude_max_20x")),
            0.9975
        );
    }

    #[test]
    fn missing_status_header_means_limiter_did_not_run() {
        let headers = headers(&[]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert!(!parsed.limiter_ran);
        assert!(parsed.zone.is_none());
    }

    #[test]
    fn zero_utilizations_mean_no_zone() {
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0"),
            (GRACE_7D_UTILIZATION_HEADER, "0"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert!(parsed.limiter_ran);
        assert!(parsed.zone.is_none());
    }

    #[test]
    fn five_hour_zone_selected_when_utilization_present() {
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.9"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        let zone = parsed.zone.unwrap();
        assert_eq!(zone.rate_limit_type, GraceZoneType::FiveHour);
        assert_eq!(zone.resets_at, Some(1_800_000_000.0));
    }

    #[test]
    fn seven_day_zone_when_5h_resets_earlier() {
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.5"),
            (FIVE_HOUR_RESET_HEADER, "1700000100"),
            (GRACE_7D_UTILIZATION_HEADER, "0.9"),
            (SEVEN_DAY_RESET_HEADER, "1700100000"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert_eq!(parsed.zone.unwrap().rate_limit_type, GraceZoneType::SevenDay);
    }

    #[test]
    fn extra_usage_status_and_in_use_cover_zone() {
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.9"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
            (OVERAGE_STATUS_HEADER, "allowed"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert!(parsed.covered);

        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.9"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
            (OVERAGE_IN_USE_HEADER, "true"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert!(parsed.covered);
    }

    #[test]
    fn stale_resets_are_dropped() {
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.9"),
            (FIVE_HOUR_RESET_HEADER, "1699999999.5"), // now-0.5s：容差门内仍有效
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert_eq!(parsed.five_hour_reset, Some(1_699_999_999.5));

        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "allowed"),
            (GRACE_5H_UTILIZATION_HEADER, "0.9"),
            (FIVE_HOUR_RESET_HEADER, "1699999900"),
        ]);
        let parsed = parse_unified_grace_headers(&headers, NOW_MS);
        assert!(parsed.five_hour_reset.is_none());
    }

    #[test]
    fn owed_note_transitions_once_per_window() {
        reset_grace_state_for_tests();
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "rejected"),
            (GRACE_5H_UTILIZATION_HEADER, "0.99"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
        ]);
        {
            let mut grace = lock(&GRACE);
            grace.apply_response(&headers, NOW_MS);
        }
        assert!(take_grace_turn_note(NOW_MS));
        mark_grace_start_note_placed();
        // 已 taken/placed：再次 take 不成功
        assert!(!take_grace_turn_note(NOW_MS));
        reset_grace_state_for_tests();
    }

    #[test]
    fn refusal_after_coverage_re_arms_note() {
        reset_grace_state_for_tests();
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "rejected"),
            (GRACE_5H_UTILIZATION_HEADER, "0.99"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
            (OVERAGE_STATUS_HEADER, "allowed"),
        ]);
        {
            let mut grace = lock(&GRACE);
            grace.apply_response(&headers, NOW_MS);
        }
        assert!(lock(&GRACE).covered);
        grace_on_refused();
        assert!(!lock(&GRACE).covered);
        // 覆盖被撕后仍存活的 zone 重新欠下便条
        assert!(take_grace_turn_note(NOW_MS));
        reset_grace_state_for_tests();
    }

    #[test]
    fn synthetic_rejection_only_after_placed_note() {
        reset_grace_state_for_tests();
        assert!(get_wrap_up_synthetic_rejection(NOW_MS).is_none());
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "rejected"),
            (GRACE_5H_UTILIZATION_HEADER, "0.99"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
        ]);
        {
            let mut grace = lock(&GRACE);
            grace.apply_response(&headers, NOW_MS);
        }
        take_grace_turn_note(NOW_MS);
        assert!(get_wrap_up_synthetic_rejection(NOW_MS).is_none());
        mark_grace_start_note_placed();
        let rejection = get_wrap_up_synthetic_rejection(NOW_MS).unwrap();
        assert!(rejection.rate_limit_grace_active);
        assert!(!rejection.unified_rate_limit_fallback_available);
        reset_grace_state_for_tests();
    }

    #[test]
    fn find_last_usage_limit_note_walks_backward() {
        let messages = vec![
            UsageLimitNoteCarrier {
                r#type: Some("user".to_string()),
                usage_limit_note: Some("near_limit".to_string()),
            },
            UsageLimitNoteCarrier {
                r#type: Some("user".to_string()),
                usage_limit_note: Some("wrap_up".to_string()),
            },
        ];
        assert_eq!(
            find_last_usage_limit_note(&messages),
            Some(UsageLimitNoteKind::WrapUp)
        );
        assert_eq!(find_last_usage_limit_note(&[]), None);
    }

    #[test]
    fn covered_zone_does_not_emit_subagent_note() {
        reset_grace_state_for_tests();
        let headers = headers(&[
            (UNIFIED_STATUS_HEADER, "rejected"),
            (GRACE_5H_UTILIZATION_HEADER, "0.99"),
            (FIVE_HOUR_RESET_HEADER, "1800000000"),
            (OVERAGE_STATUS_HEADER, "allowed"),
        ]);
        {
            let mut grace = lock(&GRACE);
            grace.apply_response(&headers, NOW_MS);
        }
        assert!(!grace_subagent_note_due(NOW_MS));
        reset_grace_state_for_tests();
    }
}
