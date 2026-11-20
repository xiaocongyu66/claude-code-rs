//! growthbook.rs — GrowthBook 特性门控（重写自
//! `src/services/analytics/growthbook.ts`）。
//!
//! 职责（对齐 TS）：
//! - 用户属性（`GrowthBookUserAttributes`）与 base URL host 解析。
//! - 配置来源优先级链：env 覆盖（`CLAUDE_INTERNAL_FC_OVERRIDES`，ant）
//!   > `/config` Gates 覆盖（`growthBookOverrides`）> 本地默认门
//!   （`LOCAL_GATE_DEFAULTS`，`CLAUDE_CODE_DISABLE_LOCAL_GATES=1` 绕过）
//!   > 内存 remoteEval 值 > 磁盘缓存（`cachedGrowthBookFeatures`）
//!   > 调用方默认值。
//! - `_CACHED_MAY_BE_STALE` 同步读取族（`getFeatureValue_*`、
//!   `checkStatsigFeatureGate_*`、`checkSecurityRestrictionGate`、
//!   `checkGate_CACHED_OR_BLOCKING`）与 `_BLOCKS_ON_INIT` 异步族。
//! - 刷新信号（`onGrowthBookRefresh`，供 firstPartyEventLogger 等长生命周期
//!   对象在配置变化时重建）。
//! - 动态配置包装（`getDynamicConfig_*`：Statsig API 平价——GrowthBook
//!   里 dynamic config 就是对象值的 feature）。
//!
//! 已知偏差（依赖未转换模块 / 无 SDK）：
//! - **无 GrowthBook SDK**：`@growthbook/growthbook` 无 Rust 等价物。客户端
//!   为本地占位结构，`init`/`refreshFeatures` 的 HTTP 远端拉取未接入，
//!   故 `remoteEvalFeatureValues` 永不填充、实验曝光数据
//!   （`experimentDataByFeature`）永不落库、`syncRemoteEvalToDisk` 的
//!   磁盘回写永不触发——读取链自然回落到磁盘缓存与本地默认。
//! - **无后台定时器**：TS 的 6h/20m 定期刷新（`setInterval`）无 Rust 等价
//!   宿主调度；`refreshGrowthBookFeatures` 保留为显式异步方法，
//!   `setupPeriodicGrowthBookRefresh` no-op（记录偏差）。因此
//!   `refreshGrowthBookAfterAuthChange` 的重建只重建占位客户端。
//! - `utils/config.ts` 未转换：`~/.claude.json` 的读取/写入在本模块内
//!   duck-type（同 auth.rs 的做法），含进程内写穿透缓存。
//! - `utils/user.ts`（`getUserForGrowthBook`）未转换：用户属性改由
//!   `~/.claude.json`（`userID`/`oauthAccount`）+ bootstrap state +
//!   `utils/auth`（subscriptionType/rateLimitTier）组装。
//! - `utils/http.ts` 的 `getAuthHeaders` 未转换：hasTrust 分支保留
//!   （`checkHasTrustDialogAccepted` 已就绪），auth header 本身不采集
//!   （占位客户端不发起请求）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bootstrap::state::{get_is_non_interactive_session, get_session_id, get_session_trust_accepted};
use crate::constants::keys::get_growthbook_client_key;
use crate::utils::auth::{check_has_trust_dialog_accepted, get_rate_limit_tier, get_subscription_type};
use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::log::log_error;
use crate::utils::platform::{get_platform, Platform};
use crate::utils::signal::{Signal, Subscription};

use super::first_party_event_logger::log_growthbook_experiment_to_1p;

/// 用户属性（TS `GrowthBookUserAttributes`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GrowthBookUserAttributes {
    pub id: String,
    pub session_id: String,
    #[serde(rename = "deviceID")]
    pub device_id: String,
    pub platform: String,
    pub api_base_url_host: Option<String>,
    pub organization_uuid: Option<String>,
    pub account_uuid: Option<String>,
    pub user_type: Option<String>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub first_token_time: Option<f64>,
    pub email: Option<String>,
    pub app_version: Option<String>,
    pub github: Option<GitHubActionsMetadata>,
}

/// GitHub Actions 元数据（TS `GitHubActionsMetadata`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitHubActionsMetadata {
    pub actor_id: Option<String>,
    pub repository_id: Option<String>,
    pub repository_owner_id: Option<String>,
}

/// 远端实验解析结果（TS `StoredExperimentData`）。
#[derive(Debug, Clone, Default)]
struct StoredExperimentData {
    experiment_id: String,
    variation_id: i64,
}

/// GrowthBook 客户端占位（无 SDK：仅保存创建期配置）。
pub struct GrowthBookClient {
    #[allow(dead_code)]
    base_url: String,
    #[allow(dead_code)]
    client_key: String,
    #[allow(dead_code)]
    remote_eval: bool,
}

struct State {
    client: Option<GrowthBookClient>,
    client_created_with_auth: bool,
    remote_eval_feature_values: HashMap<String, Value>,
    experiment_data_by_feature: HashMap<String, StoredExperimentData>,
    pending_exposures: HashSet<String>,
    logged_exposures: HashSet<String>,
}

static STATE: Lazy<Mutex<State>> = Lazy::new(|| {
    Mutex::new(State {
        client: None,
        client_created_with_auth: false,
        remote_eval_feature_values: HashMap::new(),
        experiment_data_by_feature: HashMap::new(),
        pending_exposures: HashSet::new(),
        logged_exposures: HashSet::new(),
    })
});

static REINITIALIZING: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 刷新信号（TS `refreshed = createSignal()`）。
static REFRESHED: Lazy<Signal<()>> = Lazy::new(Signal::new);

fn call_safe(listener: &dyn Fn()) {
    // TS 用 Promise.resolve().catch() 兜底；Rust 侧 listener 无失败路径
    // （均为同步纯函数），仍保留统一入口以便未来切换。
    listener();
}

/// 订阅 GrowthBook 特性刷新。返回的 `Subscription` drop 时取消订阅。
///
/// 若注册时 remoteEval 值已填充（init 已完成），立即补播一次（TS 的
/// queueMicrotask catch-up；Rust 无宿主微任务，改为同步补播，并保持
/// 「注册与补播之间取消订阅则不补播」的语义）。
pub fn on_growth_book_refresh(listener: impl Fn() + Send + Sync + 'static) -> Subscription<()> {
    let has_values = !lock(&STATE).remote_eval_feature_values.is_empty();
    let listener: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(listener);
    let listening = std::sync::Arc::new(AtomicBool::new(true));
    let listening_clone = listening.clone();
    let listener_for_closure = listener.clone();
    let subscription = REFRESHED.subscribe(move |_| {
        if listening_clone.load(Ordering::SeqCst) {
            call_safe(&*listener_for_closure);
        }
    });
    if has_values {
        // 补播前再查：listener 可能已取消，或 reset 已清空 map
        let still_listening = listening.load(Ordering::SeqCst);
        let still_has_values = !lock(&STATE).remote_eval_feature_values.is_empty();
        if still_listening && still_has_values {
            call_safe(&*listener);
        }
    }
    // listening flag 随 subscription 生命周期：drop subscription 取消订阅
    // 后旧监听器不再被调用（Signal 已摘除），flag 仅作补播窗口的可见性
    let _ = listening;
    subscription
}

// ---------------------------------------------------------------------------
// 环境变量覆盖（eval harness 用；仅 ant）
// ---------------------------------------------------------------------------

static ENV_OVERRIDES: OnceLock<Option<HashMap<String, Value>>> = OnceLock::new();

/// `CLAUDE_INTERNAL_FC_OVERRIDES` 的 JSON 覆盖（TS `getEnvOverrides`）。
fn get_env_overrides() -> Option<&'static HashMap<String, Value>> {
    ENV_OVERRIDES.get_or_init(|| {
        if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
            return None;
        }
        let raw = std::env::var("CLAUDE_INTERNAL_FC_OVERRIDES").ok()?;
        match serde_json::from_str::<HashMap<String, Value>>(&raw) {
            Ok(overrides) => {
                log_for_debugging(
                    &format!(
                        "GrowthBook: Using env var overrides for {} features",
                        overrides.len()
                    ),
                    DebugLogLevel::Debug,
                );
                Some(overrides)
            }
            Err(_) => {
                log_error(ClaudeCodeError::new(format!(
                    "GrowthBook: Failed to parse CLAUDE_INTERNAL_FC_OVERRIDES: {raw}"
                )));
                None
            }
        }
    })
    .as_ref()
}

/// 该 feature 是否有 env 覆盖（TS `hasGrowthBookEnvOverride`）。
pub fn has_growth_book_env_override(feature: &str) -> bool {
    get_env_overrides()
        .map(|overrides| overrides.contains_key(feature))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 全局配置 duck-type（utils/config.ts 未转换）
// ---------------------------------------------------------------------------

static GLOBAL_CONFIG_CACHE: Mutex<Option<Value>> = Mutex::new(None);

fn get_global_claude_file() -> PathBuf {
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let config_home = crate::utils::env_utils::get_claude_config_home_dir();
            let legacy = PathBuf::from(&config_home).join(".config.json");
            if legacy.exists() {
                return legacy;
            }
            // 与 utils/config.ts / auth.rs 一致的 oauth 文件名后缀
            let suffix = crate::constants::oauth::file_suffix_for_oauth_config();
            let filename = format!(".claude{suffix}.json");
            match std::env::var("CLAUDE_CONFIG_DIR") {
                Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(filename),
                _ => PathBuf::from(crate::utils::system_directories::home_dir()).join(filename),
            }
        })
        .clone()
}

fn read_global_config() -> Value {
    if let Some(config) = lock(&GLOBAL_CONFIG_CACHE).as_ref() {
        return config.clone();
    }
    let config = std::fs::read(get_global_claude_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Value::Object(Default::default()));
    *lock(&GLOBAL_CONFIG_CACHE) = Some(config.clone());
    config
}

fn save_global_config(updater: &dyn Fn(&Value) -> Value) {
    let current = read_global_config();
    let updated = updater(&current);
    let path = get_global_claude_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_vec_pretty(&updated) {
        Ok(bytes) => {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                *lock(&GLOBAL_CONFIG_CACHE) = Some(updated);
            }
        }
        Err(e) => log_error(e),
    }
}

/// 简单错误类型（log_error 需要 Into<Box<dyn Error>>）。
struct ClaudeCodeError {
    message: String,
}
impl ClaudeCodeError {
    fn new(message: String) -> Self {
        Self { message }
    }
}
impl std::fmt::Display for ClaudeCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::fmt::Debug for ClaudeCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ClaudeCodeError {}

/// `/config` Gates 覆盖（TS `getConfigOverrides`；仅 ant，非 memoize）。
fn get_config_overrides() -> Option<HashMap<String, Value>> {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        return None;
    }
    // getGlobalConfig 在 configReadingAllowed 未设置前会抛错（TS 注释）；
    // Rust 侧读取失败时同样降级为 None
    let config = read_global_config();
    config
        .get("growthBookOverrides")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// 枚举全部已知特性及当前解析值（不含覆盖；`/config` Gates 用）。
pub fn get_all_growth_book_features() -> HashMap<String, Value> {
    let state = lock(&STATE);
    if !state.remote_eval_feature_values.is_empty() {
        return state.remote_eval_feature_values.clone();
    }
    drop(state);
    read_global_config()
        .get("cachedGrowthBookFeatures")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// 当前 `/config` Gates 覆盖。
pub fn get_growth_book_config_overrides() -> HashMap<String, Value> {
    get_config_overrides().unwrap_or_default()
}

/// 设置/清除单个配置覆盖（undefined 清除）。写后触发刷新信号。
pub fn set_growth_book_config_override(feature: &str, value: Option<&Value>) {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        return;
    }
    save_global_config(&|current| {
        let mut next = current.clone();
        let overrides = next
            .get_mut("growthBookOverrides")
            .and_then(Value::as_object_mut);
        match (value, overrides) {
            (Some(value), Some(map)) => {
                map.insert(feature.to_string(), value.clone());
            }
            (Some(value), None) => {
                let mut map = serde_json::Map::new();
                map.insert(feature.to_string(), value.clone());
                if let Value::Object(obj) = &mut next {
                    obj.insert("growthBookOverrides".to_string(), Value::Object(map));
                }
            }
            (None, Some(map)) => {
                map.remove(feature);
                if map.is_empty() {
                    if let Value::Object(obj) = &mut next {
                        obj.remove("growthBookOverrides");
                    }
                }
            }
            (None, None) => {}
        }
        next
    });
    // 订阅者各自做变更检测（见 onGrowthBookRefresh 文档），no-op 写也触发
    REFRESHED.emit(&());
}

/// 清除全部配置覆盖。
pub fn clear_growth_book_config_overrides() {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        return;
    }
    save_global_config(&|current| {
        let mut next = current.clone();
        if next.get("growthBookOverrides").is_some() {
            if let Value::Object(obj) = &mut next {
                obj.remove("growthBookOverrides");
            }
        }
        next
    });
    REFRESHED.emit(&());
}

// ---------------------------------------------------------------------------
// 本地门默认（fork/自托管部署有意启用的门）
// ---------------------------------------------------------------------------

/// `LOCAL_GATE_DEFAULTS`（TS 同名常量，逐条搬运）。
static LOCAL_GATE_DEFAULTS: Lazy<HashMap<&'static str, Value>> = Lazy::new(|| {
    let mut map = HashMap::new();
    // P0：纯本地特性
    for key in [
        "tengu_keybinding_customization_release",
        "tengu_streaming_tool_execution2",
        "tengu_kairos_cron",
        "tengu_amber_json_tools",
        "tengu_immediate_model_command",
        "tengu_basalt_3kr",
        "tengu_pebble_leaf_prune",
        "tengu_chair_sermon",
        "tengu_lodestone_enabled",
        "tengu_auto_background_agents",
        "tengu_fgts",
    ] {
        map.insert(key, Value::Bool(true));
    }
    // P1：依赖 Claude API
    for key in [
        "tengu_session_memory",
        "tengu_passport_quail",
        "tengu_moth_copse",
        "tengu_coral_fern",
        "tengu_chomp_inflection",
        "tengu_hive_evidence",
        "tengu_kairos_brief",
        "tengu_sedge_lantern",
    ] {
        map.insert(key, Value::Bool(true));
    }
    map.insert(
        "tengu_kairos_brief_config",
        serde_json::json!({ "enable_slash_command": true }),
    );
    map.insert("tengu_onyx_plover", serde_json::json!({ "enabled": true }));
    map.insert("tengu_willow_mode", Value::String("dialog".to_string()));
    // Kill switches（保持 true，防远端关闭）
    for key in [
        "tengu_turtle_carbon",
        "tengu_amber_stoat",
        "tengu_amber_flint",
        "tengu_slim_subagent_claudemd",
        "tengu_birch_trellis",
        "tengu_collage_kaleidoscope",
        "tengu_compact_cache_prefix",
        "tengu_kairos_assistant",
        "tengu_kairos_cron_durable",
        "tengu_attribution_header",
        "tengu_slate_prism",
    ] {
        map.insert(key, Value::Bool(true));
    }
    // Ultrareview（云端 code review）
    map.insert("tengu_review_bughunter_config", serde_json::json!({ "enabled": true }));
    map.insert("tengu_ccr_bundle_seed_enabled", Value::Bool(true));
    map
});

/// 本地门默认查询（`CLAUDE_CODE_DISABLE_LOCAL_GATES` 绕过）。
fn get_local_gate_default(feature: &str) -> Option<Value> {
    if std::env::var("CLAUDE_CODE_DISABLE_LOCAL_GATES").is_ok() {
        return None;
    }
    LOCAL_GATE_DEFAULTS.get(feature).cloned()
}

/// GrowthBook 是否应启用（适配器模式或 1P 事件日志开启时）。
pub fn is_growth_book_enabled() -> bool {
    if std::env::var("CLAUDE_GB_ADAPTER_URL").is_ok()
        && std::env::var("CLAUDE_GB_ADAPTER_KEY").is_ok()
    {
        return true;
    }
    super::first_party_event_logger::is_1p_event_logging_enabled()
}

/// `ANTHROPIC_BASE_URL` 指向非 Anthropic 代理时的 host（企业代理部署
/// targeting 用）。默认/解析失败/api.anthropic.com 返回 None。
pub fn get_api_base_url_host() -> Option<String> {
    let base_url = std::env::var("ANTHROPIC_BASE_URL").ok()?;
    let host = url_host(&base_url)?;
    if host == "api.anthropic.com" {
        return None;
    }
    Some(host)
}

/// 极简 URL host 提取（`scheme://host[:port]`，忽略 userinfo/path/query）。
fn url_host(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(idx) => &url[idx + 3..],
        None => return None, // TS new URL() 无 scheme 会抛错 → None
    };
    let authority = after_scheme.split('/').next().unwrap_or("");
    // 去掉 userinfo
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    Some(authority.to_lowercase())
}

/// 组装用户属性（偏差：`getUserForGrowthBook` 未转换，改由本地来源组装）。
fn get_user_attributes() -> GrowthBookUserAttributes {
    let config = read_global_config();
    let user_id = config
        .get("userID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let email = config
        .get("oauthAccount")
        .and_then(|a| a.get("emailAddress"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let platform = match get_platform() {
        Platform::Windows => "win32",
        Platform::Macos => "darwin",
        _ => "linux",
    };
    GrowthBookUserAttributes {
        id: user_id.clone(),
        session_id: get_session_id().to_string(),
        device_id: user_id,
        platform: platform.to_string(),
        api_base_url_host: get_api_base_url_host(),
        organization_uuid: None, // utils/user.ts 未转换（偏差）
        account_uuid: None,
        user_type: std::env::var("USER_TYPE").ok(),
        subscription_type: get_subscription_type().map(|t| t.as_str().to_string()),
        rate_limit_tier: get_rate_limit_tier(),
        first_token_time: None,
        email,
        app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        github: None,
    }
}

// ---------------------------------------------------------------------------
// 客户端生命周期（no-op 网络层）
// ---------------------------------------------------------------------------

/// 创建 GrowthBook 客户端（TS `getGrowthBookClient`，memoized）。
///
/// 偏差：无 SDK，返回保存配置的占位结构；不发起 remoteEval 请求。
fn create_growth_book_client() -> Option<GrowthBookClient> {
    if !is_growth_book_enabled() {
        return None;
    }
    let is_adapter_mode = std::env::var("CLAUDE_GB_ADAPTER_URL").is_ok()
        && std::env::var("CLAUDE_GB_ADAPTER_KEY").is_ok();
    // 跳过 trust 尚未建立的请求（对齐 TS：非交互会话隐含 trust）
    let has_trust = check_has_trust_dialog_accepted()
        || get_session_trust_accepted()
        || get_is_non_interactive_session();
    let has_auth = is_adapter_mode || has_trust;
    {
        let mut state = lock(&STATE);
        state.client_created_with_auth = has_auth;
    }
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        log_for_debugging(
            &format!(
                "GrowthBook: Creating client (clientKey={}, adapter={}, auth={})",
                get_growthbook_client_key(),
                is_adapter_mode,
                has_auth
            ),
            DebugLogLevel::Debug,
        );
    }
    Some(GrowthBookClient {
        base_url: std::env::var("CLAUDE_GB_ADAPTER_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com/".to_string()),
        client_key: get_growthbook_client_key(),
        remote_eval: !is_adapter_mode,
    })
}

/// 初始化 GrowthBook 客户端（阻塞直到就绪；TS memoized）。
///
/// 偏差：无 SDK → 无网络 init；保留 auth 变更后重建逻辑（TS 的
/// `clientCreatedWithAuth` 守卫）。
pub async fn initialize_growth_book() -> bool {
    let mut client_present = lock(&STATE).client.is_some();
    if !client_present {
        let client = create_growth_book_client();
        if client.is_some() {
            lock(&STATE).client = client;
            client_present = true;
        }
    }
    if !client_present {
        return false;
    }

    // auth 变得可用 → 重建客户端（对齐 TS 的重建守卫）
    let created_with_auth = lock(&STATE).client_created_with_auth;
    if !created_with_auth {
        let has_trust = check_has_trust_dialog_accepted()
            || get_session_trust_accepted()
            || get_is_non_interactive_session();
        if has_trust {
            reset_growth_book();
            let client = create_growth_book_client();
            if client.is_none() {
                return false;
            }
            lock(&STATE).client = client;
        }
    }

    setup_periodic_growth_book_refresh();
    true
}

/// 实验曝光记录（同会话去重）。
fn log_exposure_for_feature(feature: &str) {
    let exp_data = {
        let mut state = lock(&STATE);
        if state.logged_exposures.contains(feature) {
            return;
        }
        state.logged_exposures.insert(feature.to_string());
        state.experiment_data_by_feature.get(feature).cloned()
    };
    if let Some(exp_data) = exp_data {
        let user_attributes = get_user_attributes();
        let mut experiment_metadata = serde_json::Map::new();
        experiment_metadata.insert("feature_id".into(), Value::String(feature.to_string()));
        log_growthbook_experiment_to_1p(&super::first_party_event_logger::GrowthBookExperimentData {
            experiment_id: exp_data.experiment_id,
            variation_id: exp_data.variation_id,
            user_attributes: Some(user_attributes),
            experiment_metadata: Some(Value::Object(experiment_metadata)),
        });
    }
}

// ---------------------------------------------------------------------------
// 特性读取族
// ---------------------------------------------------------------------------

/// 内部：阻塞式读取（TS `getFeatureValueInternal`）。
async fn get_feature_value_internal(feature: &str, default_value: &Value, log_exposure: bool) -> Value {
    // 1) env 覆盖（eval harness）
    if let Some(overrides) = get_env_overrides() {
        if let Some(value) = overrides.get(feature) {
            return value.clone();
        }
    }
    // 2) /config Gates 覆盖
    if let Some(overrides) = get_config_overrides() {
        if let Some(value) = overrides.get(feature) {
            return value.clone();
        }
    }
    // 3) 未启用 → 本地默认 > 调用方默认
    if !is_growth_book_enabled() {
        return get_local_gate_default(feature).unwrap_or_else(|| default_value.clone());
    }
    // 4) 阻塞 init
    if initialize_growth_book().await {
        // 内存 remoteEval 值（无网络层时恒空；保留读取保持 API 平价）
        let in_memory = {
            let state = lock(&STATE);
            state.remote_eval_feature_values.get(feature).cloned()
        };
        if let Some(value) = in_memory {
            return value;
        }
        if let Some(local) = get_local_gate_default(feature) {
            return local;
        }
        if let Some(cached) = read_disk_cache(feature) {
            return cached;
        }
    } else if let Some(local) = get_local_gate_default(feature) {
        return local;
    }
    if log_exposure {
        log_exposure_for_feature(feature);
    }
    default_value.clone()
}

/// 从磁盘缓存读单个特性（`cachedGrowthBookFeatures`）。
fn read_disk_cache(feature: &str) -> Option<Value> {
    let config = read_global_config();
    config
        .get("cachedGrowthBookFeatures")
        .and_then(|f| f.get(feature))
        .cloned()
}

/// @deprecated 阻塞式读取（TS `getFeatureValue_DEPRECATED`）。
pub async fn get_feature_value_deprecated(feature: &str, default_value: Value) -> Value {
    get_feature_value_internal(feature, &default_value, true).await
}

/// 磁盘缓存即时读取（TS `getFeatureValue_CACHED_MAY_BE_STALE`）。
///
/// 优先级：env 覆盖 > config 覆盖 > 未启用→本地默认 > 本地门默认
/// （自托管防远端关停）> 内存 remoteEval 值 > 磁盘缓存 > 调用方默认。
pub fn get_feature_value_cached_may_be_stale(feature: &str, default_value: Value) -> Value {
    if let Some(overrides) = get_env_overrides() {
        if let Some(value) = overrides.get(feature) {
            return value.clone();
        }
    }
    if let Some(overrides) = get_config_overrides() {
        if let Some(value) = overrides.get(feature) {
            return value.clone();
        }
    }

    if !is_growth_book_enabled() {
        return get_local_gate_default(feature).unwrap_or(default_value);
    }

    // 自托管/fork 部署中远端可能把这些门推 false：本地默认优先于远端值
    // 与磁盘缓存（env/config 覆盖是显式用户意图，仍最高）
    if let Some(local_default) = get_local_gate_default(feature) {
        return local_default;
    }

    // 实验曝光：有数据即记录，否则延迟到 init 后
    let exp_data_present = lock(&STATE).experiment_data_by_feature.contains_key(feature);
    if exp_data_present {
        log_exposure_for_feature(feature);
    } else {
        lock(&STATE).pending_exposures.insert(feature.to_string());
    }

    let in_memory = {
        let state = lock(&STATE);
        state.remote_eval_feature_values.get(feature).cloned()
    };
    if let Some(value) = in_memory {
        return value;
    }
    read_disk_cache(feature).unwrap_or(default_value)
}

/// @deprecated 磁盘缓存 + TTL 刷新（TS：TTL 从不拉新，直接转发）。
pub fn get_feature_value_cached_with_refresh(
    feature: &str,
    default_value: Value,
    _refresh_interval_ms: u64,
) -> Value {
    get_feature_value_cached_may_be_stale(feature, default_value)
}

/// Statsig 门迁移辅助（TS `checkStatsigFeatureGate_CACHED_MAY_BE_STALE`）。
///
/// 布尔化读取；缓存优先 GrowthBook 磁盘缓存，回落 Statsig
/// `cachedStatsigGates`，再回落本地门默认。
pub fn check_statsig_feature_gate_cached_may_be_stale(gate: &str) -> bool {
    if let Some(overrides) = get_env_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }
    if let Some(overrides) = get_config_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }

    if !is_growth_book_enabled() {
        return get_local_gate_default(gate)
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false);
    }

    let exp_data_present = lock(&STATE).experiment_data_by_feature.contains_key(gate);
    if exp_data_present {
        log_exposure_for_feature(gate);
    } else {
        lock(&STATE).pending_exposures.insert(gate.to_string());
    }

    let config = read_global_config();
    if let Some(gb_cached) = config.get("cachedGrowthBookFeatures").and_then(|f| f.get(gate)) {
        return gb_cached.as_bool().unwrap_or(false);
    }
    if let Some(statsig_cached) = config.get("cachedStatsigGates").and_then(|f| f.get(gate)) {
        return statsig_cached.as_bool().unwrap_or(false);
    }
    get_local_gate_default(gate)
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false)
}

/// 安全限制门（TS `checkSecurityRestrictionGate`）：重初始化进行中时
/// 等待完成，再按 Statsig 缓存 → GrowthBook 缓存 → false。
pub async fn check_security_restriction_gate(gate: &str) -> bool {
    if let Some(overrides) = get_env_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }
    if let Some(overrides) = get_config_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }

    if !is_growth_book_enabled() {
        return false;
    }

    // 等待重初始化完成（偏差：init 为本地占位，等待无实际挂起）
    while REINITIALIZING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let config = read_global_config();
    if let Some(statsig_cached) = config.get("cachedStatsigGates").and_then(|f| f.get(gate)) {
        return statsig_cached.as_bool().unwrap_or(false);
    }
    if let Some(gb_cached) = config.get("cachedGrowthBookFeatures").and_then(|f| f.get(gate)) {
        return gb_cached.as_bool().unwrap_or(false);
    }
    false
}

/// 布尔特权门（TS `checkGate_CACHED_OR_BLOCKING`）：磁盘缓存说 true 直
/// 接返回；否则阻塞拉取远端值。
pub async fn check_gate_cached_or_blocking(gate: &str) -> bool {
    if let Some(overrides) = get_env_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }
    if let Some(overrides) = get_config_overrides() {
        if let Some(value) = overrides.get(gate) {
            return value.as_bool().unwrap_or(false);
        }
    }

    if !is_growth_book_enabled() {
        return get_local_gate_default(gate)
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false);
    }

    // 快速路径：磁盘缓存已说 true
    if let Some(cached) = read_disk_cache(gate) {
        if cached.as_bool() == Some(true) {
            let exp_data_present = lock(&STATE).experiment_data_by_feature.contains_key(gate);
            if exp_data_present {
                log_exposure_for_feature(gate);
            } else {
                lock(&STATE).pending_exposures.insert(gate.to_string());
            }
            return true;
        }
    }

    // 慢路径：可能过期 → 阻塞拉取
    let value = get_feature_value_internal(gate, &Value::Bool(false), true).await;
    value.as_bool().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 重置 / 刷新
// ---------------------------------------------------------------------------

/// auth 变更后重建客户端（TS `refreshGrowthBookAfterAuthChange`）。
pub fn refresh_growth_book_after_auth_change() {
    if !is_growth_book_enabled() {
        return;
    }
    reset_growth_book();
    // reset 清空了 remoteEval 值；重 init 超时/短路时 init 回调不会触发，
    // 此处立即通知订阅者回落磁盘缓存（对齐 TS 注释）
    REFRESHED.emit(&());
    REINITIALIZING.store(true, Ordering::SeqCst);
    // 偏差：init 为本地占位（即时完成）；无 promise 可等
    REINITIALIZING.store(false, Ordering::SeqCst);
}

/// 重置客户端状态（主要供测试）。
pub fn reset_growth_book() {
    stop_periodic_growth_book_refresh();
    let mut state = lock(&STATE);
    state.client = None;
    state.client_created_with_auth = false;
    state.experiment_data_by_feature.clear();
    state.pending_exposures.clear();
    state.logged_exposures.clear();
    state.remote_eval_feature_values.clear();
    // envOverrides / memoize 缓存：OnceLock 无法清空——与 TS 的差异，
    // 通过进程级 env 覆盖语义不受运行期 reset 影响（同 TS 行为：env 覆
    // 盖解析一次，reset 只清 TS 的 memoize 缓存）
}

/// 轻量刷新（TS `refreshGrowthBookFeatures`）。
///
/// 偏差：无 SDK 的 `refreshFeatures()`；无远端 payload → hadFeatures 恒
/// false → 不触发磁盘回写与订阅者通知（对齐 TS 的 hadFeatures 门控）。
pub async fn refresh_growth_book_features() {
    if !is_growth_book_enabled() {
        return;
    }
    if !initialize_growth_book().await {
        return;
    }
    // processRemoteEvalPayload 需要 SDK payload（偏差：永不发生）
}

/// 周期性刷新调度（TS 6h / ant 20min）。
///
/// 偏差：无 setInterval 宿主调度，此处 no-op；刷新由显式调用
/// `refresh_growth_book_features` 驱动。
pub fn setup_periodic_growth_book_refresh() {
    if !is_growth_book_enabled() {
        return;
    }
    let _ = growthbook_refresh_interval_ms(); // 常量保留（供未来定时器接入）
}

/// 停止周期性刷新。
pub fn stop_periodic_growth_book_refresh() {
    // 偏差：无定时器需清理（setup 为 no-op）
}

fn growthbook_refresh_interval_ms() -> u64 {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        6 * 60 * 60 * 1000 // 6 小时
    } else {
        20 * 60 * 1000 // ant：20 分钟
    }
}

// ---------------------------------------------------------------------------
// 动态配置（Statsig API 平价）
// ---------------------------------------------------------------------------

/// 动态配置读取（阻塞 init；TS `getDynamicConfig_BLOCKS_ON_INIT`）。
pub async fn get_dynamic_config_blocks_on_init(config_name: &str, default_value: Value) -> Value {
    get_feature_value_deprecated(config_name, default_value).await
}

/// 动态配置读取（磁盘缓存即时；TS
/// `getDynamicConfig_CACHED_MAY_BE_STALE`）。
pub fn get_dynamic_config_cached_may_be_stale(config_name: &str, default_value: Value) -> Value {
    get_feature_value_cached_may_be_stale(config_name, default_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_host_extracts_authority() {
        assert_eq!(url_host("https://api.anthropic.com/v1/x"), Some("api.anthropic.com".into()));
        assert_eq!(url_host("https://proxy.example.com").as_deref(), Some("proxy.example.com"));
        assert!(url_host("not-a-url").is_none());
    }

    #[test]
    fn api_base_url_host_ignores_anthropic() {
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
        assert_eq!(get_api_base_url_host(), None);
        std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.com/api");
        assert_eq!(get_api_base_url_host().as_deref(), Some("proxy.example.com"));
        std::env::remove_var("ANTHROPIC_BASE_URL");
    }

    #[test]
    fn local_gate_defaults_cover_known_gates() {
        assert_eq!(get_local_gate_default("tengu_lodestone_enabled"), Some(Value::Bool(true)));
        assert_eq!(
            get_local_gate_default("tengu_willow_mode"),
            Some(Value::String("dialog".into()))
        );
        assert_eq!(get_local_gate_default("tengu_unknown_gate"), None);
        std::env::set_var("CLAUDE_CODE_DISABLE_LOCAL_GATES", "1");
        assert_eq!(get_local_gate_default("tengu_lodestone_enabled"), None);
        std::env::remove_var("CLAUDE_CODE_DISABLE_LOCAL_GATES");
    }

    #[test]
    fn refresh_interval_matches_statsig_cadence() {
        assert_eq!(growthbook_refresh_interval_ms(), 6 * 60 * 60 * 1000);
    }
}
