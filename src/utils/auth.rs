//! `src/utils/auth.rs` —— 认证枢纽（OAuth token、API key、订阅等级判定）。
//!
//! 重写自 `src/utils/auth.ts`（TS 快照共 2012 行）。
//!
//! ## 语义对齐要点
//! - token 源判定优先级与 TS `getAuthTokenSource` 完全一致；
//! - OAuth token 读取顺序：env `CLAUDE_CODE_OAUTH_TOKEN` > CCR 文件描述符 >
//!   `.credentials.json`（SecureStorage 明文后端）；
//! - 订阅判定（`is_max_subscriber` 等 5 个 + `has_opus_access`、
//!   `is_consumer_subscriber`、`is_1p_api_customer` 等）为纯逻辑实现；
//! - `apiKeyHelper` / `awsAuthRefresh` / `gcpAuthRefresh` /
//!   `awsCredentialExport` / `otelHeadersHelper` 均为「shell 命令执行 +
//!   结果缓存」模型，用 tokio/std process 实现；
//! - macOS keychain 通过 `security` CLI 子进程实现（对齐 TS subprocess 调用）；
//! - 401 处理与 token 刷新流程（跨进程锁、in-flight 去重、重试退避）完整落地，
//!   仅真正发起 HTTP 的一步留占位。
//!
//! ## 需要 reqwest 的函数（以 `AuthError::HttpClientUnavailable` 占位）
//! - [`refresh_oauth_token`]：POST `${token_url}`（grant_type=refresh_token）
//! - [`get_oauth_profile_from_oauth_token`]：GET `${base_api_url}/api/oauth/profile`
//! - [`check_sts_caller_identity`]：AWS SDK STS 调用
//!
//! ## 已知偏差（依赖模块尚未转 Rust）
//! - `services/analytics`（logEvent）未移植，本地为 no-op 占位；
//! - SecureStorage 只实现 `.credentials.json` 明文后端，keychain 后端
//!   （macOS 双写/首选项）未接入：token 互斥读写靠本模块缓存 + mtime 失效；
//! - `lockfile` 未移植：跨进程刷新锁用 `<configHome>/.auth-refresh.lock`
//!   独占文件近似（TS 为 OSS lockfile，锁目录本身）；
//! - `services/oauth/client.ts` 未移植：`is_oauth_token_expired`、
//!   `should_use_claude_ai_auth` 两个纯函数在本模块内实现；
//! - `services/mockRateLimits.ts` 未移植：mock 订阅状态用本模块全局
//!   `Mutex` 近似（setter 仅供测试）；
//! - `utils/config.ts` 未移植：`~/.claude.json` 读写在本模块内 duck-type；
//! - `otelHeadersHelper` 的 30s 超时未强制（std::process 无 deadline）；
//! - `otelHeadersHelper` 返回 `Vec<(String, String)>` 保序（TS 为 Record）；
//! - `getOauthConfig()` 失败时（如自定义 OAuth URL 非白名单）回退默认值，
//!   不像 TS 那样抛出——auth 层调用点全部是尽力读取。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::bootstrap::state::{
    get_api_key_from_fd, get_is_non_interactive_session, get_oauth_token_from_fd,
    prefer_third_party_authentication, set_api_key_from_fd, set_oauth_token_from_fd, SettingSource,
};
use crate::constants::oauth::{
    file_suffix_for_oauth_config, get_oauth_config, CLAUDE_AI_INFERENCE_SCOPE, CLAUDE_AI_PROFILE_SCOPE,
};
use crate::utils::aws_auth_status_manager::AwsAuthStatusManager;
use crate::utils::debug::{log_ant_error, log_for_debugging, DebugLogLevel};
use crate::utils::env_utils::{
    get_claude_config_home_dir, is_bare_mode, is_env_truthy, is_running_on_homespace,
};
use crate::utils::errors::error_message;
use crate::utils::log::log_error;
use crate::utils::memoize::{memoize_with_ttl_async, TtlMemoizedAsync};
use crate::utils::model::provider_classification::{is_third_party_api_provider, ApiProvider};
use crate::utils::platform::{get_platform, Platform};
use crate::utils::settings::settings::{get_initial_settings, get_settings_for_source};
use crate::utils::settings::types::{ModelTypeSetting, SettingsJson};
use crate::utils::slow_operations::json_parse;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// apiKeyHelper 缓存默认 TTL：5 分钟。
const DEFAULT_API_KEY_HELPER_TTL_MS: u64 = 5 * 60 * 1000;

/// AWS STS 凭据默认 TTL：1 小时（手动失效，不用太精确）。
const DEFAULT_AWS_STS_TTL_MS: u64 = 60 * 60 * 1000;

/// GCP 凭据默认 TTL：1 小时（对齐典型 ADC token 生命周期）。
const DEFAULT_GCP_CREDENTIAL_TTL_MS: u64 = 60 * 60 * 1000;

/// AWS / GCP auth refresh 命令超时：3 分钟。
const PROVIDER_AUTH_REFRESH_TIMEOUT_MS: u64 = 3 * 60 * 1000;

/// apiKeyHelper 命令超时：10 分钟。
const API_KEY_HELPER_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// OAuth 过期判定的提前量：5 分钟。
const OAUTH_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;

/// otelHeadersHelper 防抖默认值：29 分钟。
const DEFAULT_OTEL_HEADERS_DEBOUNCE_MS: u64 = 29 * 60 * 1000;

/// CCR（claude code remote）token 文件位置。
const CCR_TOKEN_DIR: &str = "/home/claude/.claude/remote";
const CCR_OAUTH_TOKEN_PATH: &str = "/home/claude/.claude/remote/.oauth_token";
const CCR_API_KEY_PATH: &str = "/home/claude/.claude/remote/.api_key";

/// 锁文件最大重试次数（ELOCKED 时退避重试）。
const MAX_LOCK_RETRIES: u32 = 5;

/// TS `MACRO.FEEDBACK_CHANNEL`（构建期 define，当前为空串）。
const FEEDBACK_CHANNEL: &str = "";

/// TS 安全提示前缀（TS 为模板拼接，此处提取为常量保持文案一致）。
const SECURITY_PREFIX: &str = "Security:";

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// auth 层错误。`Clone` 用于 in-flight 去重（watch channel 广播结果）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AuthError {
    /// CI 环境缺少必需的认证环境变量。
    #[error("ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN env var is required")]
    MissingApiKeyOrOAuthToken,
    /// API key 格式非法。
    #[error("Invalid API key format. API key must contain only alphanumeric characters, dashes, and underscores.")]
    InvalidApiKeyFormat,
    /// apiKeyHelper 执行失败。`detail` 对齐 TS 抛出的 `timed out` / `exited N: stderr`。
    #[error("apiKeyHelper failed: {detail}")]
    ApiKeyHelperFailed { detail: String },
    /// apiKeyHelper 未输出任何内容。
    #[error("did not return a value")]
    ApiKeyHelperEmpty,
    /// otelHeadersHelper 输出非法。
    #[error("{0}")]
    OtelHeadersInvalid(String),
    /// awsCredentialExport 输出非法。
    #[error("awsCredentialExport did not return valid AWS STS output structure")]
    InvalidAwsStsOutput,
    /// 需要 HTTP 客户端（reqwest 尚未加入 Cargo.toml）。
    #[error("HTTP client unavailable: {0} requires reqwest (not yet in Cargo.toml)")]
    HttpClientUnavailable(&'static str),
    /// 文件锁被其他进程持有（对齐 TS 的 ELOCKED）。
    #[error("lock busy")]
    LockBusy,
    /// IO 错误。
    #[error("{0}")]
    Io(String),
    /// JSON 解析错误。
    #[error("{0}")]
    Json(String),
}

impl From<std::io::Error> for AuthError {
    fn from(e: std::io::Error) -> Self {
        AuthError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for AuthError {
    fn from(e: serde_json::Error) -> Self {
        AuthError::Json(e.to_string())
    }
}

pub type AuthResult<T> = Result<T, AuthError>;

/// TS `saveOAuthTokensIfNeeded` 的返回值：`{ success, warning? }`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SaveStatus {
    pub success: bool,
    pub warning: Option<String>,
}

// ---------------------------------------------------------------------------
// 类型（对齐 oauth/types.ts + auth.ts 局部类型）
// ---------------------------------------------------------------------------

/// `SubscriptionType`：TS 侧实际为 `any`，取值见 oauth/client.ts 的判定分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionType {
    Free,
    Pro,
    Max,
    Team,
    Enterprise,
}

impl SubscriptionType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "free" => Some(Self::Free),
            "pro" => Some(Self::Pro),
            "max" => Some(Self::Max),
            "team" => Some(Self::Team),
            "enterprise" => Some(Self::Enterprise),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Pro => "pro",
            Self::Max => "max",
            Self::Team => "team",
            Self::Enterprise => "enterprise",
        }
    }
}

/// `OAuthTokens`（TS services/oauth/types.ts 的实际载荷）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthTokens {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken", default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix 毫秒。
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(rename = "subscriptionType", default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<SubscriptionType>,
    #[serde(rename = "rateLimitTier", default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
}

impl OAuthTokens {
    /// 构造 env-var/FD 来源的 inference-only token（无刷新、无过期信息）。
    fn inference_only(access_token: String) -> Self {
        Self {
            access_token,
            refresh_token: None,
            expires_at: None,
            scopes: vec![CLAUDE_AI_INFERENCE_SCOPE.to_string()],
            subscription_type: None,
            rate_limit_tier: None,
        }
    }
}

/// `ApiKeySource`（TS auth.ts）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    AnthropicApiKey,
    ApiKeyHelper,
    LoginManagedKey,
    None,
}

impl ApiKeySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AnthropicApiKey => "ANTHROPIC_API_KEY",
            Self::ApiKeyHelper => "apiKeyHelper",
            Self::LoginManagedKey => "/login managed key",
            Self::None => "none",
        }
    }
}

/// `getAuthTokenSource()` 的返回标签（TS 字面量联合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthTokenSource {
    None,
    AnthropicAuthToken,
    ClaudeCodeOauthToken,
    ClaudeCodeOauthTokenFileDescriptor,
    CcrOauthTokenFile,
    ApiKeyHelper,
    ClaudeAi,
}

impl AuthTokenSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AnthropicAuthToken => "ANTHROPIC_AUTH_TOKEN",
            Self::ClaudeCodeOauthToken => "CLAUDE_CODE_OAUTH_TOKEN",
            Self::ClaudeCodeOauthTokenFileDescriptor => "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
            Self::CcrOauthTokenFile => "CCR_OAUTH_TOKEN_FILE",
            Self::ApiKeyHelper => "apiKeyHelper",
            Self::ClaudeAi => "claude.ai",
        }
    }
}

/// `AccountInfo`（TS config.ts 的 oauthAccount 子集）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OauthAccountInfo {
    #[serde(rename = "billingType", default, skip_serializing_if = "Option::is_none")]
    pub billing_type: Option<String>,
    #[serde(rename = "organizationName", default, skip_serializing_if = "Option::is_none")]
    pub organization_name: Option<String>,
    #[serde(rename = "emailAddress", default, skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,
}

/// `UserAccountInfo`（TS auth.ts）。
#[derive(Debug, Clone, Default)]
pub struct UserAccountInfo {
    pub subscription: Option<String>,
    pub token_source: Option<AuthTokenSource>,
    pub api_key_source: Option<ApiKeySource>,
    pub organization: Option<String>,
    pub email: Option<String>,
}

/// `OrgValidationResult`（TS auth.ts）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgValidationResult {
    Valid,
    Invalid { message: String },
}

/// AWS STS 凭据（`awsCredentialExport` 输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
}

// ---------------------------------------------------------------------------
// 通用薄封装
// ---------------------------------------------------------------------------

/// `process.env[name]`（空串视为未设置，对齐 TS 真值判断）。
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `Date.now()`——Unix 毫秒。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 毒锁自愈：poisoned 时取回内部数据而不是 panic（替代 `.unwrap()`）。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// TS `logEvent`（services/analytics）未移植——no-op 占位。
/// **TODO(analytics)**：接入 `crate::services::analytics` 后替换。
fn log_event(_name: &str) {}

/// 判断当前是否 CCR / Claude Desktop 托管上下文。
///
/// CCR 和 Claude Desktop 带着 OAuth 启动 CLI，绝不能回退到用户
/// `~/.claude/settings.json` 的 API key 配置。
pub fn is_managed_oauth_context() -> bool {
    is_env_truthy(env("CLAUDE_CODE_REMOTE").as_deref())
        || env("CLAUDE_CODE_ENTRYPOINT").as_deref() == Some("claude-desktop")
}

/// 用户主目录（对齐 TS `os.homedir()`）。
fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

// ---------------------------------------------------------------------------
// 全局配置（~/.claude.json duck-type；config.ts 尚未移植）
// ---------------------------------------------------------------------------

static GLOBAL_CONFIG_CACHE: Mutex<Option<Value>> = Mutex::new(None);

/// TS `env.ts` 的 `getGlobalClaudeFile`：legacy `.config.json` 回退，
/// 否则 `$CLAUDE_CONFIG_DIR|homedir/.claude{oauth文件后缀}.json`。
fn get_global_claude_file() -> PathBuf {
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let config_home = get_claude_config_home_dir();
            let legacy = PathBuf::from(&config_home).join(".config.json");
            if legacy.exists() {
                return legacy;
            }
            let suffix = file_suffix_for_oauth_config();
            let filename = format!(".claude{suffix}.json");
            match std::env::var("CLAUDE_CONFIG_DIR") {
                Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(filename),
                _ => home_dir().join(filename),
            }
        })
        .clone()
}

/// 读 `~/.claude.json`（不存在/损坏时返回空对象）。带进程内写穿透缓存。
fn get_global_config() -> Value {
    {
        let cache = lock(&GLOBAL_CONFIG_CACHE);
        if let Some(config) = cache.as_ref() {
            return config.clone();
        }
    }
    let config = match std::fs::read(get_global_claude_file()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::Object(Default::default())),
        Err(_) => Value::Object(Default::default()),
    };
    *lock(&GLOBAL_CONFIG_CACHE) = Some(config.clone());
    config
}

/// `saveGlobalConfig(updater)`：读-改-写（原子重命名），并同步缓存。
/// **偏差**：TS 用 proper-lockfile 串行化 + 备份恢复，此处为简单覆写。
fn save_global_config(updater: &dyn Fn(&Value) -> Value) {
    let current = get_global_config();
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

/// config 中 `customApiKeyResponses.approved` 列表。
fn config_approved_api_keys(config: &Value) -> Vec<String> {
    config
        .get("customApiKeyResponses")
        .and_then(|r| r.get("approved"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 信任对话框（config.ts 尚未移植，按 TS `checkHasTrustDialogAccepted` 近似）
// ---------------------------------------------------------------------------

static TRUST_LATCH: AtomicBool = AtomicBool::new(false);
static SESSION_TRUST: AtomicBool = AtomicBool::new(false);

/// 会话级信任（TS `setSessionTrustAccepted` 的等价物，内存态）。
pub fn set_session_trust_accepted(value: bool) {
    SESSION_TRUST.store(value, Ordering::SeqCst);
}

fn session_trust_accepted() -> bool {
    SESSION_TRUST.load(Ordering::SeqCst)
}

/// 信任只能 false→true，true 之后锁存；否则每次现读磁盘。
pub fn check_has_trust_dialog_accepted() -> bool {
    if TRUST_LATCH.load(Ordering::SeqCst) {
        return true;
    }
    let accepted = compute_trust_dialog_accepted();
    if accepted {
        TRUST_LATCH.store(true, Ordering::SeqCst);
    }
    accepted
}

fn compute_trust_dialog_accepted() -> bool {
    // 会话级信任（home 目录场景：信任只存内存）。
    if session_trust_accepted() {
        return true;
    }
    let config = get_global_config();
    let projects = config.get("projects").and_then(|p| p.as_object());
    let mut current = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .into_owned();
    loop {
        if let Some(accepted) = projects
            .and_then(|p| p.get(&current))
            .and_then(|c| c.get("hasTrustDialogAccepted"))
            .and_then(|v| v.as_bool())
        {
            if accepted {
                return true;
            }
        }
        let parent = Path::new(&current)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if parent == current {
            break;
        }
        current = parent;
    }
    false
}

// ---------------------------------------------------------------------------
// 文件描述符凭据（CCR；authFileDescriptor.ts 尚未移植）
// ---------------------------------------------------------------------------

/// 从 `/dev/fd/{n}`（macOS）或 `/proc/self/fd/{n}`（Linux）读取凭据。
fn read_fd(fd: u32) -> Option<String> {
    let path = if get_platform() == Platform::Macos {
        format!("/dev/fd/{fd}")
    } else {
        format!("/proc/self/fd/{fd}")
    };
    match std::fs::read_to_string(&path) {
        Ok(token) => {
            let token = token.trim();
            if token.is_empty() {
                log_for_debugging(
                    &format!("File descriptor {fd} contained empty credential"),
                    DebugLogLevel::Error,
                );
                None
            } else {
                Some(token.to_string())
            }
        }
        Err(e) => {
            log_for_debugging(
                &format!("Failed to read credential from file descriptor {fd}: {e}"),
                DebugLogLevel::Error,
            );
            None
        }
    }
}

/// CCR 子进程兜底读：well-known 文件只在 CCR 内有意义，ENOENT 是常态。
fn read_token_from_well_known_file(path: &str, label: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim();
            if token.is_empty() {
                return None;
            }
            log_for_debugging(
                &format!("Read {label} from well-known file {path}"),
                DebugLogLevel::Debug,
            );
            Some(token.to_string())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log_for_debugging(
                &format!("Failed to read {label} from {path}: {e}"),
                DebugLogLevel::Debug,
            );
            None
        }
    }
}

/// CCR 内把 token 落盘，供无法继承 FD 的子进程读取。
fn maybe_persist_token_for_subprocesses(path: &str, token: &str, label: &str) {
    if !is_env_truthy(env("CLAUDE_CODE_REMOTE").as_deref()) {
        return;
    }
    let result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(CCR_TOKEN_DIR)?;
            std::fs::set_permissions(CCR_TOKEN_DIR, std::fs::Permissions::from_mode(0o700))?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(token.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, token);
        }
        Ok(())
    })();
    match result {
        Ok(()) => log_for_debugging(
            &format!("Persisted {label} to {path} for subprocess access"),
            DebugLogLevel::Debug,
        ),
        Err(e) => log_for_debugging(
            &format!("Failed to persist {label} to disk (non-fatal): {e}"),
            DebugLogLevel::Error,
        ),
    }
}

/// 共享的「FD 或 well-known 文件」凭据读取（带 bootstrap 全局缓存）。
fn get_credential_from_fd(
    env_var: &str,
    well_known_path: &str,
    label: &str,
    get_cached: fn() -> Option<String>,
    set_cached: fn(Option<String>),
) -> Option<String> {
    if let Some(cached) = get_cached() {
        return Some(cached);
    }
    let Some(fd_env) = env(env_var) else {
        // 无 FD 环境变量：子进程走 well-known 文件兜底。
        let from_file = read_token_from_well_known_file(well_known_path, label);
        set_cached(from_file.clone());
        return from_file;
    };
    match fd_env.parse::<u32>() {
        Ok(fd) => match read_fd(fd) {
            Some(token) => {
                log_for_debugging(
                    &format!("Successfully read {label} from file descriptor {fd}"),
                    DebugLogLevel::Debug,
                );
                set_cached(Some(token.clone()));
                maybe_persist_token_for_subprocesses(well_known_path, &token, label);
                Some(token)
            }
            None => {
                // FD 环境变量存在但读取失败（典型：继承了 env 但没继承 FD）。
                let from_file = read_token_from_well_known_file(well_known_path, label);
                set_cached(from_file.clone());
                from_file
            }
        },
        Err(_) => {
            log_for_debugging(
                &format!("{env_var} must be a valid file descriptor number, got: {fd_env}"),
                DebugLogLevel::Error,
            );
            set_cached(None);
            None
        }
    }
}

/// CCR 注入的 OAuth token（env `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR`）。
pub fn get_oauth_token_from_file_descriptor() -> Option<String> {
    get_credential_from_fd(
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        CCR_OAUTH_TOKEN_PATH,
        "OAuth token",
        get_oauth_token_from_fd,
        set_oauth_token_from_fd,
    )
}

/// CCR 注入的 API key（env `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`）。
pub fn get_api_key_from_file_descriptor() -> Option<String> {
    get_credential_from_fd(
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        CCR_API_KEY_PATH,
        "API key",
        get_api_key_from_fd,
        set_api_key_from_fd,
    )
}

// ---------------------------------------------------------------------------
// SecureStorage（.credentials.json 明文后端）
// ---------------------------------------------------------------------------

fn secure_storage_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join(".credentials.json")
}

/// 读 `.credentials.json`；缺失/损坏返回 None（错误记录到日志）。
fn read_secure_storage() -> Option<Value> {
    match std::fs::read(secure_storage_path()) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| log_error(e))
            .ok(),
        Err(e) => {
            // ENOENT 是常态（未登录），不噪音。
            if e.kind() != std::io::ErrorKind::NotFound {
                log_error(e);
            }
            None
        }
    }
}

async fn read_secure_storage_async() -> Option<Value> {
    match tokio::fs::read(secure_storage_path()).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| log_error(e))
            .ok(),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log_error(e);
            }
            None
        }
    }
}

fn update_secure_storage(data: &Value) -> AuthResult<()> {
    let path = secure_storage_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(data)?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// macOS keychain（security CLI；secureStorage 模块尚未移植）
// ---------------------------------------------------------------------------

/// TS `macOsKeychainHelpers.getMacOsKeychainStorageServiceName`：
/// `Claude Code{oauth文件后缀}{configDir sha256 前 8 位}`。
fn mac_os_keychain_storage_service_name() -> String {
    let config_dir = get_claude_config_home_dir();
    let is_default_dir = std::env::var("CLAUDE_CONFIG_DIR").is_err();
    let dir_hash = if is_default_dir {
        String::new()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(config_dir.as_bytes());
        let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        format!("-{}", &hex[..8])
    };
    let suffix = get_oauth_config()
        .map(|c| c.oauth_file_suffix)
        .unwrap_or_default();
    format!("Claude Code{suffix}{dir_hash}")
}

/// TS `getUsername`：`$USER` → 兜底常量。
fn get_username() -> String {
    env("USER")
        .or_else(|| env("USERPROFILE"))
        .unwrap_or_else(|| "claude-code-user".to_string())
}

/// `security find-generic-password -a $USER -w -s "..."`（shell 展开 $USER）。
fn keychain_read() -> Option<String> {
    let service = mac_os_keychain_storage_service_name();
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "security find-generic-password -a $USER -w -s \"{service}\""
        ))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

/// `security delete-generic-password -a $USER -s "..."`，失败时返回 Err。
fn keychain_delete() -> AuthResult<()> {
    let service = mac_os_keychain_storage_service_name();
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "security delete-generic-password -a $USER -s \"{service}\""
        ))
        .output()?;
    if !output.status.success() {
        return Err(AuthError::Io("Failed to delete keychain entry".to_string()));
    }
    Ok(())
}

/// `security -i` 交互模式 + `-X`十六进制写入，凭据不进命令行参数。
fn keychain_write(api_key: &str) -> AuthResult<()> {
    let service = mac_os_keychain_storage_service_name();
    let username = get_username();
    let hex: String = api_key.bytes().map(|b| format!("{b:02x}")).collect();
    let command = format!(
        "add-generic-password -U -a \"{username}\" -s \"{service}\" -X \"{hex}\"\n"
    );
    let mut child = std::process::Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AuthError::Io("failed to open security stdin".to_string()))?;
        stdin.write_all(command.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(AuthError::Io(
            "security -i add-generic-password failed".to_string(),
        ))
    }
}

/// 失败不抛（对齐 TS `maybeRemoveApiKeyFromMacOSKeychain`）。
fn maybe_remove_api_key_from_macos_keychain() -> AuthResult<()> {
    if get_platform() != Platform::Macos {
        return Ok(());
    }
    if let Err(e) = keychain_delete() {
        log_error(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// mock 订阅（services/mockRateLimits.ts 尚未移植）
// ---------------------------------------------------------------------------

static MOCK_ENABLED: AtomicBool = AtomicBool::new(false);
static MOCK_SUBSCRIPTION_TYPE: Mutex<Option<SubscriptionType>> = Mutex::new(None);
const DEFAULT_MOCK_SUBSCRIPTION: SubscriptionType = SubscriptionType::Max;

fn is_ant_user() -> bool {
    env("USER_TYPE").as_deref() == Some("ant")
}

/// 设置 mock 订阅（供测试）。非 ant 用户为 no-op（对齐 TS 守卫）。
pub fn set_mock_subscription(enabled: bool, subscription: Option<SubscriptionType>) {
    if !is_ant_user() {
        return;
    }
    MOCK_ENABLED.store(enabled, Ordering::SeqCst);
    *lock(&MOCK_SUBSCRIPTION_TYPE) = subscription;
}

/// TS `shouldUseMockSubscription`。
fn should_use_mock_subscription() -> bool {
    MOCK_ENABLED.load(Ordering::SeqCst)
        && lock(&MOCK_SUBSCRIPTION_TYPE).is_some()
        && is_ant_user()
}

/// TS `getMockSubscriptionType`。
fn get_mock_subscription_type() -> Option<SubscriptionType> {
    if !MOCK_ENABLED.load(Ordering::SeqCst) || !is_ant_user() {
        return None;
    }
    Some(
        lock(&MOCK_SUBSCRIPTION_TYPE)
            .clone()
            .unwrap_or(DEFAULT_MOCK_SUBSCRIPTION),
    )
}

// ---------------------------------------------------------------------------
// OAuth 纯函数（services/oauth/client.ts 尚未移植）
// ---------------------------------------------------------------------------

/// TS `isOAuthTokenExpired`：null 视为不过期；提前 5 分钟判定。
pub fn is_oauth_token_expired(expires_at: Option<i64>) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };
    now_ms() + OAUTH_EXPIRY_BUFFER_MS >= expires_at
}

/// TS `shouldUseClaudeAIAuth`：scopes 含 `user:inference`。
pub fn should_use_claude_ai_auth(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == CLAUDE_AI_INFERENCE_SCOPE)
}

/// 「有 refresh token 且已过期」双条件（TS 刷新流程的守卫语义）。
fn still_expired(tokens: &Option<OAuthTokens>) -> bool {
    match tokens {
        Some(t) => t.refresh_token.is_some() && is_oauth_token_expired(t.expires_at),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// OAuth token 读取与缓存
// ---------------------------------------------------------------------------

/// TS lodash `memoize` 语义：首次读取后永久缓存，直到显式 clear。
static OAUTH_TOKENS_CACHE: Mutex<Option<OAuthTokens>> = Mutex::new(None);
static LAST_CREDENTIALS_MTIME_MS: Mutex<i64> = Mutex::new(0);

/// 清空 OAuth token 缓存（401 时调用，强制下次重读磁盘）。
pub fn clear_oauth_token_cache() {
    *lock(&OAUTH_TOKENS_CACHE) = None;
    // TS 另有 `clearKeychainCache()`——keychain 后端未移植，此处等价 no-op。
}

fn compute_claude_ai_oauth_tokens() -> Option<OAuthTokens> {
    // --bare：仅 API key，无 OAuth env token / keychain / 凭据文件。
    if is_bare_mode() {
        return None;
    }
    // 环境变量强制设置的 token：inference-only。
    if let Some(token) = env("CLAUDE_CODE_OAUTH_TOKEN") {
        return Some(OAuthTokens::inference_only(token));
    }
    // FD token（CCR）：inference-only。
    if let Some(token) = get_oauth_token_from_file_descriptor() {
        return Some(OAuthTokens::inference_only(token));
    }
    // 明文 SecureStorage 后端。
    let oauth = read_secure_storage().and_then(|s| s.get("claudeAiOauth").cloned());
    match oauth {
        Some(o) => match serde_json::from_value::<OAuthTokens>(o) {
            Ok(tokens) if !tokens.access_token.is_empty() => Some(tokens),
            _ => None,
        },
        None => None,
    }
}

/// TS `getClaudeAIOAuthTokens`（memoize）。
pub fn get_claude_ai_oauth_tokens() -> Option<OAuthTokens> {
    {
        let cache = lock(&OAUTH_TOKENS_CACHE);
        if let Some(tokens) = cache.as_ref() {
            return Some(tokens.clone());
        }
    }
    let tokens = compute_claude_ai_oauth_tokens()?;
    *lock(&OAUTH_TOKENS_CACHE) = Some(tokens.clone());
    Some(tokens)
}

/// TS `getClaudeAIOAuthTokensAsync`：env/FD 走同步路径，存储走异步 fs。
pub async fn get_claude_ai_oauth_tokens_async() -> AuthResult<Option<OAuthTokens>> {
    if is_bare_mode() {
        return Ok(None);
    }
    if env("CLAUDE_CODE_OAUTH_TOKEN").is_some()
        || get_oauth_token_from_file_descriptor().is_some()
    {
        return Ok(get_claude_ai_oauth_tokens());
    }
    let oauth = read_secure_storage_async()
        .await
        .and_then(|s| s.get("claudeAiOauth").cloned());
    match oauth {
        Some(o) => match serde_json::from_value::<OAuthTokens>(o) {
            Ok(tokens) if !tokens.access_token.is_empty() => Ok(Some(tokens)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

/// 跨进程失效：`.credentials.json` 的 mtime 变了就清缓存（CC-1096、GH#24317）。
async fn invalidate_oauth_cache_if_disk_changed() {
    match tokio::fs::metadata(secure_storage_path()).await {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let mut last = lock(&LAST_CREDENTIALS_MTIME_MS);
            if mtime != *last {
                *last = mtime;
                drop(last);
                clear_oauth_token_cache();
            }
        }
        Err(_) => {
            // ENOENT（keychain 迁移后文件被删）：只清 memoize，
            // 避免在已删除的文件之上永久缓存。
            *lock(&OAUTH_TOKENS_CACHE) = None;
        }
    }
}

/// TS `saveOAuthTokensIfNeeded`。
pub fn save_oauth_tokens_if_needed(tokens: &OAuthTokens) -> SaveStatus {
    if !should_use_claude_ai_auth(&tokens.scopes) {
        log_event("tengu_oauth_tokens_not_claude_ai");
        return SaveStatus { success: true, warning: None };
    }
    // inference-only token（来自 env var）跳过持久化。
    if tokens.refresh_token.is_none() || tokens.expires_at.is_none() {
        log_event("tengu_oauth_tokens_inference_only");
        return SaveStatus { success: true, warning: None };
    }
    // 刷新时 profile 拉取可能返回 null——不清掉已存的合法值。
    let existing = read_secure_storage().and_then(|s| s.get("claudeAiOauth").cloned());
    let existing_subscription = existing.as_ref().and_then(|o| o.get("subscriptionType").cloned());
    let existing_tier = existing.as_ref().and_then(|o| o.get("rateLimitTier").cloned());

    let mut storage = read_secure_storage().unwrap_or_else(|| Value::Object(Default::default()));
    let oauth = serde_json::json!({
        "accessToken": tokens.access_token,
        "refreshToken": tokens.refresh_token,
        "expiresAt": tokens.expires_at,
        "scopes": tokens.scopes,
        "subscriptionType": tokens
            .subscription_type
            .map(|s| Value::String(s.as_str().to_string()))
            .or(existing_subscription),
        "rateLimitTier": tokens.rate_limit_tier.clone().map(Value::String).or(existing_tier),
    });
    if let Some(obj) = storage.as_object_mut() {
        obj.insert("claudeAiOauth".to_string(), oauth);
    }
    match update_secure_storage(&storage) {
        Ok(()) => {
            log_event("tengu_oauth_tokens_saved");
            clear_oauth_token_cache();
            SaveStatus { success: true, warning: None }
        }
        Err(e) => {
            log_error(e);
            SaveStatus {
                success: false,
                warning: Some("Failed to save OAuth tokens".to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// token 源判定
// ---------------------------------------------------------------------------

/// TS `getAPIProvider`（providers.ts 尚未移植，纯逻辑内联）。
fn get_api_provider(settings: &SettingsJson) -> ApiProvider {
    if let Some(model_type) = settings.model_type {
        match model_type {
            ModelTypeSetting::Openai => return ApiProvider::Openai,
            ModelTypeSetting::Gemini => return ApiProvider::Gemini,
            ModelTypeSetting::Grok => return ApiProvider::Grok,
            ModelTypeSetting::Anthropic => {}
        }
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_BEDROCK").as_deref()) {
        return ApiProvider::Bedrock;
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_VERTEX").as_deref()) {
        return ApiProvider::Vertex;
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_FOUNDRY").as_deref()) {
        return ApiProvider::Foundry;
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_OPENAI").as_deref()) {
        return ApiProvider::Openai;
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_GEMINI").as_deref()) {
        return ApiProvider::Gemini;
    }
    if is_env_truthy(env("CLAUDE_CODE_USE_GROK").as_deref()) {
        return ApiProvider::Grok;
    }
    ApiProvider::FirstParty
}

/// TS `isAnthropicAuthEnabled`：是否走 Anthropic 直连 1P 认证。
pub fn is_anthropic_auth_enabled() -> bool {
    // --bare：仅 API key，永不走 OAuth。
    if is_bare_mode() {
        return false;
    }
    // claude ssh 远端：ANTHROPIC_UNIX_SOCKET 走本地代理注入认证。
    // 只有占位 CLAUDE_CODE_OAUTH_TOKEN 存在时才启用（带 oauth beta header）。
    if env("ANTHROPIC_UNIX_SOCKET").is_some() {
        return env("CLAUDE_CODE_OAUTH_TOKEN").is_some();
    }
    let settings = get_initial_settings();
    let is_3p = is_third_party_api_provider(get_api_provider(&settings))
        || env("OPENAI_BASE_URL").is_some()
        || env("GEMINI_BASE_URL").is_some();
    if is_3p {
        return false;
    }
    let has_external_auth_token = env("ANTHROPIC_AUTH_TOKEN").is_some()
        || settings.api_key_helper.is_some()
        || env("CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR").is_some();
    // API key 是否来自外部（非 /login 管理）。
    let (_, api_key_source) = get_anthropic_api_key_with_source(GetApiKeyOpts {
        skip_retrieving_key_from_api_key_helper: true,
    });
    let has_external_api_key = matches!(
        api_key_source,
        ApiKeySource::AnthropicApiKey | ApiKeySource::ApiKeyHelper
    );
    // 第三方已在上面处理。以下两种情况禁用 Anthropic 认证：
    // 1. 外部 API key；2. 外部 auth token。（托管 OAuth 上下文豁免）
    let should_disable_auth = (has_external_auth_token || has_external_api_key)
        && !is_managed_oauth_context();
    !should_disable_auth
}

/// TS `getAuthTokenSource`。返回 `(source, has_token)`。
pub fn get_auth_token_source() -> (AuthTokenSource, bool) {
    // --bare：仅 apiKeyHelper（来自 --settings）是合法的 bearer-token 源。
    if is_bare_mode() {
        if get_configured_api_key_helper().is_some() {
            return (AuthTokenSource::ApiKeyHelper, true);
        }
        return (AuthTokenSource::None, false);
    }
    if env("ANTHROPIC_AUTH_TOKEN").is_some() && !is_managed_oauth_context() {
        return (AuthTokenSource::AnthropicAuthToken, true);
    }
    if env("CLAUDE_CODE_OAUTH_TOKEN").is_some() {
        return (AuthTokenSource::ClaudeCodeOauthToken, true);
    }
    // FD（或其 CCR 磁盘兜底）来源的 OAuth token。
    if get_oauth_token_from_file_descriptor().is_some() {
        // 区分 FD 与磁盘兜底，避免 org-mismatch 提示让用户去删不存在的 env var。
        return if env("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR").is_some() {
            (AuthTokenSource::ClaudeCodeOauthTokenFileDescriptor, true)
        } else {
            (AuthTokenSource::CcrOauthTokenFile, true)
        };
    }
    // apiKeyHelper 配置存在性检查（不执行，防 trust 前执行任意代码）。
    if get_configured_api_key_helper().is_some() && !is_managed_oauth_context() {
        return (AuthTokenSource::ApiKeyHelper, true);
    }
    let oauth_tokens = get_claude_ai_oauth_tokens();
    if let Some(tokens) = oauth_tokens.as_ref() {
        if should_use_claude_ai_auth(&tokens.scopes) {
            return (AuthTokenSource::ClaudeAi, true);
        }
    }
    (AuthTokenSource::None, false)
}

// ---------------------------------------------------------------------------
// API key
// ---------------------------------------------------------------------------

/// `getAnthropicApiKeyWithSource` 的选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct GetApiKeyOpts {
    /// true 时 apiKeyHelper 返回 key=null（不阻塞执行 helper）。
    pub skip_retrieving_key_from_api_key_helper: bool,
}

/// TS `getAnthropicApiKey`。
pub fn get_anthropic_api_key() -> Option<String> {
    let (key, _) = get_anthropic_api_key_with_source(GetApiKeyOpts::default());
    key
}

/// TS `hasAnthropicApiKeyAuth`。
pub fn has_anthropic_api_key_auth() -> bool {
    let (key, source) = get_anthropic_api_key_with_source(GetApiKeyOpts {
        skip_retrieving_key_from_api_key_helper: true,
    });
    key.is_some() && source != ApiKeySource::None
}

/// TS `getAnthropicApiKeyWithSource`。同步返回——缓存未就绪时 key 为 None
/// 但 source 正确（apiKeyHelper 冷缓存语义，调用方应先 await 异步预热）。
pub fn get_anthropic_api_key_with_source(opts: GetApiKeyOpts) -> (Option<String>, ApiKeySource) {
    // --bare：密闭认证。只认 env API key 和 --settings 的 apiKeyHelper。
    if is_bare_mode() {
        if let Some(key) = env("ANTHROPIC_API_KEY") {
            return (Some(key), ApiKeySource::AnthropicApiKey);
        }
        if get_configured_api_key_helper().is_some() {
            return (
                if opts.skip_retrieving_key_from_api_key_helper {
                    None
                } else {
                    get_api_key_from_api_key_helper_cached()
                },
                ApiKeySource::ApiKeyHelper,
            );
        }
        return (None, ApiKeySource::None);
    }

    // homespace 上不用 ANTHROPIC_API_KEY（用 Console key）。
    let api_key_env = if is_running_on_homespace() {
        None
    } else {
        env("ANTHROPIC_API_KEY")
    };

    // `claude --print` 等场景优先直接读 env（CI 友好）。
    if prefer_third_party_authentication() && api_key_env.is_some() {
        return (api_key_env, ApiKeySource::AnthropicApiKey);
    }

    if is_env_truthy(env("CI").as_deref()) || env("NODE_ENV").as_deref() == Some("test") {
        if let Some(key) = get_api_key_from_file_descriptor() {
            return (Some(key), ApiKeySource::AnthropicApiKey);
        }
        if api_key_env.is_none()
            && env("CLAUDE_CODE_OAUTH_TOKEN").is_none()
            && env("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR").is_none()
        {
            // 对齐 TS 的 throw：详见 [get_anthropic_api_key_or_error]。
            return (None, ApiKeySource::None);
        }
        if let Some(key) = api_key_env {
            return (Some(key), ApiKeySource::AnthropicApiKey);
        }
        // OAuth token 存在但本函数只返回 API key。
        return (None, ApiKeySource::None);
    }

    // 已批准的 env API key 优先于 apiKeyHelper / /login key。
    if let Some(key) = api_key_env.as_ref() {
        let approved = config_approved_api_keys(&get_global_config());
        if approved.contains(&normalize_api_key_for_config(key)) {
            return (api_key_env, ApiKeySource::AnthropicApiKey);
        }
    }

    // FD API key。
    if let Some(key) = get_api_key_from_file_descriptor() {
        return (Some(key), ApiKeySource::AnthropicApiKey);
    }

    // apiKeyHelper：同步缓存读，永不阻塞；缓存冷时返回 key=null 但 source 正确。
    if get_configured_api_key_helper().is_some() {
        return (
            if opts.skip_retrieving_key_from_api_key_helper {
                None
            } else {
                get_api_key_from_api_key_helper_cached()
            },
            ApiKeySource::ApiKeyHelper,
        );
    }

    // /login 管理的 key（macOS keychain 或 config）。
    if let Some((key, source)) = get_api_key_from_config_or_macos_keychain() {
        return (Some(key), source);
    }

    (None, ApiKeySource::None)
}

/// CI 模式下 TS 抛错的 Rust 入口（对齐 TS `throw new Error`）。
pub fn get_anthropic_api_key_or_error() -> AuthResult<Option<String>> {
    let (key, source) = get_anthropic_api_key_with_source(GetApiKeyOpts::default());
    if key.is_none() && source == ApiKeySource::None {
        if (is_env_truthy(env("CI").as_deref())
            || env("NODE_ENV").as_deref() == Some("test"))
            && !is_bare_mode()
            && env("ANTHROPIC_API_KEY").is_none()
            && get_api_key_from_file_descriptor().is_none()
            && env("CLAUDE_CODE_OAUTH_TOKEN").is_none()
            && env("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR").is_none()
        {
            return Err(AuthError::MissingApiKeyOrOAuthToken);
        }
    }
    Ok(key)
}

/// TS `getConfiguredApiKeyHelper`。
pub fn get_configured_api_key_helper() -> Option<String> {
    if is_bare_mode() {
        return get_settings_for_source(SettingSource::FlagSettings)?.api_key_helper;
    }
    get_initial_settings().api_key_helper
}

/// apiKeyHelper 是否来自 project/local settings（执行前需过 trust 检查）。
fn is_api_key_helper_from_project_or_local_settings() -> bool {
    let Some(helper) = get_configured_api_key_helper() else {
        return false;
    };
    let project = get_settings_for_source(SettingSource::ProjectSettings);
    let local = get_settings_for_source(SettingSource::LocalSettings);
    project.and_then(|s| s.api_key_helper).as_deref() == Some(helper.as_str())
        || local.and_then(|s| s.api_key_helper).as_deref() == Some(helper.as_str())
}

/// TS `calculateApiKeyHelperTTL`。
pub fn calculate_api_key_helper_ttl() -> u64 {
    if let Some(env_ttl) = env("CLAUDE_CODE_API_KEY_HELPER_TTL_MS") {
        if let Ok(parsed) = env_ttl.parse::<u64>() {
            return parsed;
        }
        log_for_debugging(
            &format!(
                "Found CLAUDE_CODE_API_KEY_HELPER_TTL_MS env var, but it was not a valid number. Got {env_ttl}"
            ),
            DebugLogLevel::Error,
        );
    }
    DEFAULT_API_KEY_HELPER_TTL_MS
}

/// TS `normalizeApiKeyForConfig`（authPortable.ts）：取末 20 字符。
pub fn normalize_api_key_for_config(api_key: &str) -> String {
    let chars: Vec<char> = api_key.chars().collect();
    let len = chars.len();
    if len <= 20 {
        api_key.to_string()
    } else {
        chars[len - 20..].iter().collect()
    }
}

// --- apiKeyHelper 缓存：SWR（stale-while-revalidate）+ epoch 防写穿 ---

#[derive(Clone)]
struct ApiKeyHelperCacheEntry {
    value: Option<String>,
    timestamp: i64,
}

static API_KEY_HELPER_CACHE: Mutex<Option<ApiKeyHelperCacheEntry>> = Mutex::new(None);
/// 冷启动执行开始时间（非 0 表示用户在等）。
static API_KEY_HELPER_INFLIGHT_STARTED_AT: Mutex<Option<i64>> = Mutex::new(None);
static API_KEY_HELPER_EPOCH: AtomicU64 = AtomicU64::new(0);
/// in-flight 执行的 watch 广播器（冷缓存并发去重）。
static API_KEY_HELPER_INFLIGHT: Mutex<Option<watch::Sender<Option<AuthResult<Option<String>>>>>> =
    Mutex::new(None);

/// TS `getApiKeyHelperElapsedMs`。
pub fn get_api_key_helper_elapsed_ms() -> i64 {
    lock(&API_KEY_HELPER_INFLIGHT_STARTED_AT)
        .map(|started| now_ms() - started)
        .unwrap_or(0)
}

/// TS `getApiKeyFromApiKeyHelperCached`：同步读缓存（含过期值），未执行过返回 None。
pub fn get_api_key_from_api_key_helper_cached() -> Option<String> {
    lock(&API_KEY_HELPER_CACHE)
        .as_ref()
        .and_then(|c| c.value.clone())
}

/// TS `clearApiKeyHelperCache`。
pub fn clear_api_key_helper_cache() {
    API_KEY_HELPER_EPOCH.fetch_add(1, Ordering::SeqCst);
    *lock(&API_KEY_HELPER_CACHE) = None;
    *lock(&API_KEY_HELPER_INFLIGHT_STARTED_AT) = None;
    *lock(&API_KEY_HELPER_INFLIGHT) = None;
}

/// TS `getApiKeyFromApiKeyHelper`：SWR——新鲜直返；过期返旧值 + 后台刷新；
/// 冷缓存并发去重。
pub async fn get_api_key_from_api_key_helper(is_non_interactive: bool) -> AuthResult<Option<String>> {
    if get_configured_api_key_helper().is_none() {
        return Ok(None);
    }
    let ttl = calculate_api_key_helper_ttl();
    let stale_entry = {
        let cache = lock(&API_KEY_HELPER_CACHE);
        if let Some(entry) = cache.as_ref() {
            if now_ms() - entry.timestamp < ttl as i64 {
                return Ok(entry.value.clone());
            }
            Some(entry.clone())
        } else {
            None
        }
    };
    if let Some(entry) = stale_entry {
        // 过期：先返旧值，后台刷新。
        let epoch = API_KEY_HELPER_EPOCH.load(Ordering::SeqCst);
        let need_spawn = lock(&API_KEY_HELPER_INFLIGHT).is_none();
        if need_spawn {
            let (tx, _) = watch::channel(None);
            *lock(&API_KEY_HELPER_INFLIGHT_STARTED_AT) = None;
            *lock(&API_KEY_HELPER_INFLIGHT) = Some(tx.clone());
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = run_and_cache(is_non_interactive, false, epoch, Some(tx)).await;
                });
            }
        }
        return Ok(entry.value);
    }
    // 冷缓存：并发去重。
    if let Some(tx) = lock(&API_KEY_HELPER_INFLIGHT).as_ref().cloned() {
        let mut rx = tx.subscribe();
        loop {
            if let Some(result) = &*rx.borrow() {
                return match result {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(e.clone()),
                };
            }
            if rx.changed().await.is_err() {
                // 广播器已 Drop 且未发结果：回退到同步读。
                return Ok(get_api_key_from_api_key_helper_cached());
            }
        }
    }
    let epoch = API_KEY_HELPER_EPOCH.load(Ordering::SeqCst);
    let (tx, _) = watch::channel(None);
    *lock(&API_KEY_HELPER_INFLIGHT) = Some(tx.clone());
    *lock(&API_KEY_HELPER_INFLIGHT_STARTED_AT) = Some(now_ms());
    run_and_cache(is_non_interactive, true, epoch, Some(tx)).await
}

/// TS `_runAndCache`：epoch 匹配才写缓存；失败时 SWR 保命、冷缓存写 ' ' 哨兵。
/// `finally` 语义（摘除 in-flight）在任何分支上都执行。
async fn run_and_cache(
    is_non_interactive: bool,
    is_cold: bool,
    epoch: u64,
    tx: Option<watch::Sender<Option<AuthResult<Option<String>>>>>,
) -> AuthResult<Option<String>> {
    // 内层 async 块允许任意提前返回，外层统一收口（finally 对齐）。
    let result = async {
        match execute_api_key_helper(is_non_interactive).await {
            Ok(Some(value)) => {
                if epoch == API_KEY_HELPER_EPOCH.load(Ordering::SeqCst) {
                    *lock(&API_KEY_HELPER_CACHE) = Some(ApiKeyHelperCacheEntry {
                        value: Some(value.clone()),
                        timestamp: now_ms(),
                    });
                }
                Ok(Some(value))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                let detail = error_message(&err);
                eprintln!("apiKeyHelper failed: {detail}");
                log_for_debugging(
                    &format!("Error getting API key from apiKeyHelper: {detail}"),
                    DebugLogLevel::Error,
                );
                // SWR：瞬时失败不该用 ' ' 哨兵顶掉还能用的旧值。
                if !is_cold {
                    if let Some(entry) = lock(&API_KEY_HELPER_CACHE).as_ref() {
                        if entry.value.as_deref() != Some(" ") {
                            return Ok(entry.value.clone());
                        }
                    }
                }
                // 冷缓存或此前已失败：缓存 ' ' 哨兵，阻止调用方回退到 OAuth。
                if epoch == API_KEY_HELPER_EPOCH.load(Ordering::SeqCst) {
                    *lock(&API_KEY_HELPER_CACHE) = Some(ApiKeyHelperCacheEntry {
                        value: Some(" ".to_string()),
                        timestamp: now_ms(),
                    });
                }
                Ok(Some(" ".to_string()))
            }
        }
    }
    .await;
    // finally：epoch 未变则摘除 in-flight（对齐 TS 的 finally 语义）。
    if epoch == API_KEY_HELPER_EPOCH.load(Ordering::SeqCst) {
        *lock(&API_KEY_HELPER_INFLIGHT_STARTED_AT) = None;
        *lock(&API_KEY_HELPER_INFLIGHT) = None;
    }
    if let Some(tx) = tx {
        let _ = tx.send(Some(result.clone()));
    }
    result
}

/// TS `_executeApiKeyHelper`：含 trust 前置检查与 10 分钟超时。
async fn execute_api_key_helper(is_non_interactive: bool) -> AuthResult<Option<String>> {
    let Some(api_key_helper) = get_configured_api_key_helper() else {
        return Ok(None);
    };
    if is_api_key_helper_from_project_or_local_settings()
        && !check_has_trust_dialog_accepted()
        && !is_non_interactive
    {
        let error = AuthError::Io(format!(
            "{SECURITY_PREFIX} apiKeyHelper executed before workspace trust is confirmed. If you see this message, post in {FEEDBACK_CHANNEL}."
        ));
        log_ant_error("apiKeyHelper invoked before trust check", &error);
        log_event("tengu_apiKeyHelper_missing_trust11");
        return Ok(None);
    }
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&api_key_helper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match timeout(Duration::from_millis(API_KEY_HELPER_TIMEOUT_MS), output).await {
        Ok(Ok(out)) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stdout.is_empty() {
                return Err(AuthError::ApiKeyHelperEmpty);
            }
            Ok(Some(stdout))
        }
        Ok(Ok(out)) => {
            let code = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(AuthError::ApiKeyHelperFailed {
                detail: if stderr.is_empty() {
                    format!("exited {code}")
                } else {
                    format!("exited {code}: {stderr}")
                },
            })
        }
        Ok(Err(e)) => Err(AuthError::ApiKeyHelperFailed {
            detail: e.to_string(),
        }),
        Err(_) => Err(AuthError::ApiKeyHelperFailed {
            detail: "timed out".to_string(),
        }),
    }
}

/// TS `prefetchApiKeyFromApiKeyHelperIfSafe`：trust 未过时不预取（避免误报埋点）。
pub fn prefetch_api_key_from_api_key_helper_if_safe(is_non_interactive: bool) {
    if is_api_key_helper_from_project_or_local_settings()
        && !check_has_trust_dialog_accepted()
    {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = get_api_key_from_api_key_helper(is_non_interactive).await;
        });
    }
}

// ---------------------------------------------------------------------------
// /login 管理的 API key（config 或 keychain；TS lodash memoize → 单槽永缓存）
// ---------------------------------------------------------------------------

static CONFIG_OR_KEYCHAIN_CACHE: Mutex<Option<(String, ApiKeySource)>> = Mutex::new(None);

/// TS `getApiKeyFromConfigOrMacOSKeychain`（@private）。
pub fn get_api_key_from_config_or_macos_keychain() -> Option<(String, ApiKeySource)> {
    {
        let cache = lock(&CONFIG_OR_KEYCHAIN_CACHE);
        if let Some(entry) = cache.as_ref() {
            return Some(entry.clone());
        }
    }
    let result = compute_api_key_from_config_or_macos_keychain();
    if let Some(entry) = &result {
        *lock(&CONFIG_OR_KEYCHAIN_CACHE) = Some(entry.clone());
    }
    result
}

/// TS `getApiKeyFromConfigOrMacOSKeychain.cache.clear?.()`。
pub fn clear_api_key_from_config_or_keychain_cache() {
    *lock(&CONFIG_OR_KEYCHAIN_CACHE) = None;
}

fn compute_api_key_from_config_or_macos_keychain() -> Option<(String, ApiKeySource)> {
    if is_bare_mode() {
        return None;
    }
    // macOS：先试 keychain（对齐 TS sync `security` 子进程调用）。
    if get_platform() == Platform::Macos {
        if let Some(key) = keychain_read() {
            return Some((key, ApiKeySource::LoginManagedKey));
        }
    }
    let config = get_global_config();
    let primary = config.get("primaryApiKey").and_then(|v| v.as_str());
    match primary {
        Some(key) if !key.is_empty() => Some((key.to_string(), ApiKeySource::LoginManagedKey)),
        _ => None,
    }
}

fn is_valid_api_key(api_key: &str) -> bool {
    // 仅允许字母数字、中划线、下划线（对齐 TS 正则 ^[a-zA-Z0-9-_]+$）。
    !api_key.is_empty()
        && api_key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// TS `saveApiKey`。
pub async fn save_api_key(api_key: &str) -> AuthResult<()> {
    if !is_valid_api_key(api_key) {
        return Err(AuthError::InvalidApiKeyFormat);
    }
    maybe_remove_api_key_from_macos_keychain();
    let mut saved_to_keychain = false;
    if get_platform() == Platform::Macos {
        match keychain_write(api_key) {
            Ok(()) => {
                log_event("tengu_api_key_saved_to_keychain");
                saved_to_keychain = true;
            }
            Err(e) => {
                log_error(e);
                log_event("tengu_api_key_keychain_error");
                log_event("tengu_api_key_saved_to_config");
            }
        }
    } else {
        log_event("tengu_api_key_saved_to_config");
    }
    let normalized_key = normalize_api_key_for_config(api_key);
    let keychain_ok = saved_to_keychain;
    save_global_config(&move |current| {
        let mut updated = current.clone();
        let mut new_approved = config_approved_api_keys(current);
        if !new_approved.contains(&normalized_key) {
            new_approved.push(normalized_key.clone());
        }
        // keychain 存成功则不动 config 中的旧值（对齐 TS 的 `current.primaryApiKey`）。
        if !keychain_ok {
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("primaryApiKey".to_string(), Value::String(api_key.to_string()));
            }
        }
        if let Some(obj) = updated.as_object_mut() {
            obj.insert(
                "customApiKeyResponses".to_string(),
                serde_json::json!({
                    "approved": new_approved,
                    "rejected": current
                        .get("customApiKeyResponses")
                        .and_then(|r| r.get("rejected"))
                        .cloned()
                        .unwrap_or(Value::Array(Vec::new())),
                }),
            );
        }
        updated
    });
    clear_api_key_from_config_or_keychain_cache();
    Ok(())
}

/// TS `isCustomApiKeyApproved`。
pub fn is_custom_api_key_approved(api_key: &str) -> bool {
    let normalized = normalize_api_key_for_config(api_key);
    config_approved_api_keys(&get_global_config()).contains(&normalized)
}

/// TS `removeApiKey`。
pub async fn remove_api_key() -> AuthResult<()> {
    maybe_remove_api_key_from_macos_keychain();
    save_global_config(&|current| {
        let mut updated = current.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.remove("primaryApiKey");
        }
        updated
    });
    clear_api_key_from_config_or_keychain_cache();
    Ok(())
}

// ---------------------------------------------------------------------------
// AWS / GCP 凭据刷新
// ---------------------------------------------------------------------------

/// TS `getConfiguredAwsAuthRefresh`。
fn get_configured_aws_auth_refresh() -> Option<String> {
    get_initial_settings().aws_auth_refresh
}

/// TS `isAwsAuthRefreshFromProjectSettings`。
pub fn is_aws_auth_refresh_from_project_settings() -> bool {
    let Some(value) = get_configured_aws_auth_refresh() else {
        return false;
    };
    let project = get_settings_for_source(SettingSource::ProjectSettings);
    let local = get_settings_for_source(SettingSource::LocalSettings);
    project.and_then(|s| s.aws_auth_refresh).as_deref() == Some(value.as_str())
        || local.and_then(|s| s.aws_auth_refresh).as_deref() == Some(value.as_str())
}

/// TS `getConfiguredAwsCredentialExport`。
fn get_configured_aws_credential_export() -> Option<String> {
    get_initial_settings().aws_credential_export
}

/// TS `isAwsCredentialExportFromProjectSettings`。
pub fn is_aws_credential_export_from_project_settings() -> bool {
    let Some(value) = get_configured_aws_credential_export() else {
        return false;
    };
    let project = get_settings_for_source(SettingSource::ProjectSettings);
    let local = get_settings_for_source(SettingSource::LocalSettings);
    project.and_then(|s| s.aws_credential_export).as_deref() == Some(value.as_str())
        || local.and_then(|s| s.aws_credential_export).as_deref() == Some(value.as_str())
}

/// TS `getConfiguredGcpAuthRefresh`。
fn get_configured_gcp_auth_refresh() -> Option<String> {
    get_initial_settings().gcp_auth_refresh
}

/// TS `isGcpAuthRefreshFromProjectSettings`。
pub fn is_gcp_auth_refresh_from_project_settings() -> bool {
    let Some(value) = get_configured_gcp_auth_refresh() else {
        return false;
    };
    let project = get_settings_for_source(SettingSource::ProjectSettings);
    let local = get_settings_for_source(SettingSource::LocalSettings);
    project.and_then(|s| s.gcp_auth_refresh).as_deref() == Some(value.as_str())
        || local.and_then(|s| s.gcp_auth_refresh).as_deref() == Some(value.as_str())
}

/// provider 刷新命令的通用执行（streaming 简化为收尾后批量喂状态管理器）。
async fn run_provider_auth_refresh(command: &str, provider: &str) -> bool {
    log_for_debugging(
        &format!("Running {provider} auth refresh command"),
        DebugLogLevel::Debug,
    );
    let manager = AwsAuthStatusManager::get_instance();
    manager.start_authentication();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match timeout(
        Duration::from_millis(PROVIDER_AUTH_REFRESH_TIMEOUT_MS),
        cmd.output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stdout.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    manager.add_output(line);
                    log_for_debugging(line, DebugLogLevel::Debug);
                }
            }
            for line in stderr.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    manager.set_error(line);
                    log_for_debugging(line, DebugLogLevel::Error);
                }
            }
            if output.status.success() {
                log_for_debugging(
                    &format!("{provider} auth refresh completed successfully"),
                    DebugLogLevel::Debug,
                );
                manager.end_authentication(true);
                true
            } else {
                eprintln!("Error running {provider}AuthRefresh (in settings or ~/.claude.json):");
                manager.end_authentication(false);
                false
            }
        }
        _ => {
            eprintln!(
                "{provider} auth refresh timed out after 3 minutes. Run your auth command manually in a separate terminal."
            );
            manager.end_authentication(false);
            false
        }
    }
}

/// TS `refreshAwsAuth`。
pub async fn refresh_aws_auth(aws_auth_refresh: &str) -> AuthResult<bool> {
    Ok(run_provider_auth_refresh(aws_auth_refresh, "AWS").await)
}

/// TS `runAwsAuthRefresh`：先探活 STS，失败才真正刷新。
async fn run_aws_auth_refresh() -> AuthResult<bool> {
    let Some(aws_auth_refresh) = get_configured_aws_auth_refresh() else {
        return Ok(false); // 未配置视为成功
    };
    if is_aws_auth_refresh_from_project_settings()
        && !check_has_trust_dialog_accepted()
        && !get_is_non_interactive_session()
    {
        let error = AuthError::Io(format!(
            "{SECURITY_PREFIX} awsAuthRefresh executed before workspace trust is confirmed. If you see this message, post in {FEEDBACK_CHANNEL}."
        ));
        log_ant_error("awsAuthRefresh invoked before trust check", &error);
        log_event("tengu_awsAuthRefresh_missing_trust");
        return Ok(false);
    }
    match check_sts_caller_identity().await {
        Ok(()) => {
            log_for_debugging(
                "Fetched AWS caller identity, skipping AWS auth refresh command",
                DebugLogLevel::Debug,
            );
            Ok(false)
        }
        Err(_) => refresh_aws_auth(&aws_auth_refresh).await,
    }
}

/// TS `isValidAwsStsOutput`：Credentials 三字段均为非空字符串。
fn is_valid_aws_sts_output(value: &Value) -> bool {
    let Some(credentials) = value.get("Credentials") else {
        return false;
    };
    ["AccessKeyId", "SecretAccessKey", "SessionToken"].iter().all(|field| {
        credentials
            .get(field)
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    })
}

/// TS `getAwsCredsFromCredentialExport`：探活失败才执行导出命令。
async fn get_aws_creds_from_credential_export() -> AuthResult<Option<AwsCredentials>> {
    let Some(aws_credential_export) = get_configured_aws_credential_export() else {
        return Ok(None);
    };
    if is_aws_credential_export_from_project_settings()
        && !check_has_trust_dialog_accepted()
        && !get_is_non_interactive_session()
    {
        let error = AuthError::Io(format!(
            "{SECURITY_PREFIX} awsCredentialExport executed before workspace trust is confirmed. If you see this message, post in {FEEDBACK_CHANNEL}."
        ));
        log_ant_error("awsCredentialExport invoked before trust check", &error);
        log_event("tengu_awsCredentialExport_missing_trust");
        return Ok(None);
    }
    if check_sts_caller_identity().await.is_ok() {
        log_for_debugging(
            "Fetched AWS caller identity, skipping AWS credential export command",
            DebugLogLevel::Debug,
        );
        return Ok(None);
    }
    log_for_debugging("Running AWS credential export command", DebugLogLevel::Debug);
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&aws_credential_export)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(AuthError::InvalidAwsStsOutput);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parsed = json_parse(&stdout)?;
    if !is_valid_aws_sts_output(&parsed) {
        return Err(AuthError::InvalidAwsStsOutput);
    }
    log_for_debugging(
        "AWS credentials retrieved from awsCredentialExport",
        DebugLogLevel::Debug,
    );
    let credentials = &parsed["Credentials"];
    Ok(Some(AwsCredentials {
        access_key_id: credentials["AccessKeyId"].as_str().unwrap_or("").to_string(),
        secret_access_key: credentials["SecretAccessKey"].as_str().unwrap_or("").to_string(),
        session_token: credentials["SessionToken"].as_str().unwrap_or("").to_string(),
    }))
}

/// `memoize_with_ttl_async` 的 boxed-future 类型别名。
type BoxCredsFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<Option<AwsCredentials>, Box<dyn std::error::Error + Send + Sync>>,
            > + Send,
    >,
>;
type BoxBoolFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<bool, Box<dyn std::error::Error + Send + Sync>>,
            > + Send,
    >,
>;

static AWS_CREDS_MEMO: OnceLock<Arc<TtlMemoizedAsync<(), Option<AwsCredentials>, BoxCredsFuture>>> =
    OnceLock::new();

/// TS `refreshAndGetAwsCredentials`（memoizeWithTTLAsync，1h TTL）。
pub async fn refresh_and_get_aws_credentials() -> AuthResult<Option<AwsCredentials>> {
    let memo = AWS_CREDS_MEMO
        .get_or_init(|| {
            memoize_with_ttl_async(
                |_: &()| Box::pin(aws_creds_body()) as BoxCredsFuture,
                DEFAULT_AWS_STS_TTL_MS,
            )
        })
        .clone();
    memo.call(&())
        .await
        .map_err(|e| AuthError::Io(error_message(&*e)))
}

async fn aws_creds_body() -> Result<Option<AwsCredentials>, Box<dyn std::error::Error + Send + Sync>> {
    let refreshed = run_aws_auth_refresh().await.unwrap_or(false);
    let credentials = get_aws_creds_from_credential_export()
        .await
        .unwrap_or(None);
    // 刷新或拿到新凭据后清 INI 缓存，确保下游使用新值。
    if refreshed || credentials.is_some() {
        clear_aws_ini_cache().await;
    }
    Ok(credentials)
}

/// TS `clearAwsCredentialsCache`。
pub async fn clear_aws_credentials_cache() {
    if let Some(memo) = AWS_CREDS_MEMO.get() {
        memo.clear().await;
    }
}

/// TS `clearAwsIniCache`：因 AWS SDK 未移植，目前为 no-op（TS 本身吞错）。
/// **TODO(aws-sdk)**：接入 credential-providers 后实现 `fromIni({ ignoreCache: true })`。
pub async fn clear_aws_ini_cache() {}

/// TS `refreshGcpAuth`。AwsAuthStatusManager 名称虽带 AWS，实为通用 provider 状态。
pub async fn refresh_gcp_auth(gcp_auth_refresh: &str) -> AuthResult<bool> {
    Ok(run_provider_auth_refresh(gcp_auth_refresh, "GCP").await)
}

/// TS `checkGcpCredentialsValid`：需要 google-auth-library，暂不可用。
/// **TODO(reqwest/gcp)**：接入 ADC 鉴权链（5s 探活超时）。
pub async fn check_gcp_credentials_valid() -> AuthResult<bool> {
    Err(AuthError::HttpClientUnavailable("check_gcp_credentials_valid"))
}

/// TS `runGcpAuthRefresh`：凭据有效就跳过刷新。
async fn run_gcp_auth_refresh() -> AuthResult<bool> {
    let Some(gcp_auth_refresh) = get_configured_gcp_auth_refresh() else {
        return Ok(false);
    };
    if is_gcp_auth_refresh_from_project_settings()
        && !check_has_trust_dialog_accepted()
        && !get_is_non_interactive_session()
    {
        let error = AuthError::Io(format!(
            "{SECURITY_PREFIX} gcpAuthRefresh executed before workspace trust is confirmed. If you see this message, post in {FEEDBACK_CHANNEL}."
        ));
        log_ant_error("gcpAuthRefresh invoked before trust check", &error);
        log_event("tengu_gcpAuthRefresh_missing_trust");
        return Ok(false);
    }
    if check_gcp_credentials_valid().await.unwrap_or(false) {
        log_for_debugging(
            "GCP credentials are valid, skipping auth refresh command",
            DebugLogLevel::Debug,
        );
        return Ok(false);
    }
    refresh_gcp_auth(&gcp_auth_refresh).await
}

static GCP_CREDS_MEMO: OnceLock<Arc<TtlMemoizedAsync<(), bool, BoxBoolFuture>>> = OnceLock::new();

/// TS `refreshGcpCredentialsIfNeeded`（memoizeWithTTLAsync，1h TTL）。
pub async fn refresh_gcp_credentials_if_needed() -> AuthResult<bool> {
    let memo = GCP_CREDS_MEMO
        .get_or_init(|| {
            memoize_with_ttl_async(
                |_: &()| Box::pin(gcp_creds_body()) as BoxBoolFuture,
                DEFAULT_GCP_CREDENTIAL_TTL_MS,
            )
        })
        .clone();
    memo.call(&())
        .await
        .map_err(|e| AuthError::Io(error_message(&*e)))
}

async fn gcp_creds_body() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let refreshed = run_gcp_auth_refresh().await.unwrap_or(false);
    Ok(refreshed)
}

/// TS `clearGcpCredentialsCache`。
pub async fn clear_gcp_credentials_cache() {
    if let Some(memo) = GCP_CREDS_MEMO.get() {
        memo.clear().await;
    }
}

/// TS `prefetchGcpCredentialsIfSafe`：项目级配置仅在 trust 已过时预取。
pub fn prefetch_gcp_credentials_if_safe() {
    if get_configured_gcp_auth_refresh().is_none() {
        return;
    }
    if is_gcp_auth_refresh_from_project_settings()
        && !check_has_trust_dialog_accepted()
        && !get_is_non_interactive_session()
    {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = refresh_gcp_credentials_if_needed().await;
        });
    }
}

/// TS `prefetchAwsCredentialsAndBedRockInfoIfSafe`。
/// **偏差**：`getModelStrings()` 依赖 model/modelStrings.ts（未移植），已省略。
pub fn prefetch_aws_credentials_and_bedrock_info_if_safe() {
    let aws_auth_refresh = get_configured_aws_auth_refresh();
    let aws_credential_export = get_configured_aws_credential_export();
    if aws_auth_refresh.is_none() && aws_credential_export.is_none() {
        return;
    }
    if is_aws_auth_refresh_from_project_settings()
        || is_aws_credential_export_from_project_settings()
    {
        if !check_has_trust_dialog_accepted() && !get_is_non_interactive_session() {
            return;
        }
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = refresh_and_get_aws_credentials().await;
        });
    }
}

// ---------------------------------------------------------------------------
// OAuth 刷新流程
// ---------------------------------------------------------------------------

/// TS `refreshOAuthToken`：需要 HTTP 客户端。
/// **TODO(reqwest)**：POST `${token_url}`，body `{grant_type: refresh_token,
/// refresh_token, client_id, scope}`（scope 缺省取 `CLAUDE_AI_OAUTH_SCOPES`），
/// 15s 超时，见 TS services/oauth/client.ts。
pub async fn refresh_oauth_token(
    _refresh_token: &str,
    _scopes: Option<Vec<String>>,
) -> AuthResult<OAuthTokens> {
    Err(AuthError::HttpClientUnavailable("refresh_oauth_token"))
}

/// TS `checkStsCallerIdentity`：需要 AWS SDK。
/// **TODO(aws-sdk)**：STSClient + GetCallerIdentityCommand。
pub async fn check_sts_caller_identity() -> AuthResult<()> {
    Err(AuthError::HttpClientUnavailable("check_sts_caller_identity"))
}

/// 跨进程锁：独占创建 `.auth-refresh.lock`。
/// **偏差**：TS 用 OSS lockfile 锁目录本身（ELOCKED），此处近似。
struct LockGuard(PathBuf);

async fn acquire_lock(dir: &Path) -> AuthResult<LockGuard> {
    let lock_path = dir.join(".auth-refresh.lock");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_) => Ok(LockGuard(lock_path)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(AuthError::LockBusy),
        Err(e) => Err(AuthError::Io(e.to_string())),
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 401 in-flight 去重：watch channel 广播结果（key = 失败的 access token）。
static PENDING_401: Mutex<HashMap<String, watch::Sender<Option<AuthResult<bool>>>>> =
    Mutex::new(HashMap::new());

/// TS `handleOAuth401Error`：服务端说过期就强制刷新（本地判定分歧时也照做）。
/// 同 token 的并发调用去重为一次存储读 + 刷新。
pub async fn handle_oauth_401_error(failed_access_token: &str) -> AuthResult<bool> {
    if let Some(tx) = lock(&PENDING_401)
        .get(failed_access_token)
        .cloned()
    {
        return wait_for_result(&tx).await;
    }
    let (tx, _) = watch::channel(None);
    lock(&PENDING_401).insert(failed_access_token.to_string(), tx.clone());
    let result = handle_oauth_401_error_impl(failed_access_token).await;
    lock(&PENDING_401).remove(failed_access_token);
    let _ = tx.send(Some(result.clone()));
    result
}

async fn wait_for_result(tx: &watch::Sender<Option<AuthResult<bool>>>) -> AuthResult<bool> {
    let mut rx = tx.subscribe();
    loop {
        if let Some(result) = &*rx.borrow() {
            return match result {
                Ok(v) => Ok(*v),
                Err(e) => Err(e.clone()),
            };
        }
        if rx.changed().await.is_err() {
            // 广播器已 Drop 且未发结果：保守返回 false。
            return Ok(false);
        }
    }
}

async fn handle_oauth_401_error_impl(failed_access_token: &str) -> AuthResult<bool> {
    // 清缓存，强制下次重读存储（keychain 读约 100ms，异步）。
    clear_oauth_token_cache();
    let current_tokens = get_claude_ai_oauth_tokens_async().await?;
    let Some(current_tokens) = current_tokens else {
        return Ok(false);
    };
    if current_tokens.refresh_token.is_none() {
        return Ok(false);
    }
    // 存储里 token 不同 → 另一个 tab 已刷新，直接用。
    if current_tokens.access_token != failed_access_token {
        log_event("tengu_oauth_401_recovered_from_keychain");
        return Ok(true);
    }
    // 同一个失败的 token：绕过本地过期判定强制刷新。
    check_and_refresh_oauth_token_if_needed(0, true).await
}

/// 并发刷新去重槽。
static PENDING_REFRESH_CHECK: Mutex<Option<watch::Sender<Option<AuthResult<bool>>>>> =
    Mutex::new(None);

/// TS `checkAndRefreshOAuthTokenIfNeeded`。retry_count=0 且 !force 时并发去重。
pub async fn check_and_refresh_oauth_token_if_needed(
    retry_count: u32,
    force: bool,
) -> AuthResult<bool> {
    if retry_count == 0 && !force {
        if let Some(tx) = lock(&PENDING_REFRESH_CHECK).as_ref().cloned() {
            return wait_for_result(&tx).await;
        }
        let (tx, _) = watch::channel(None);
        *lock(&PENDING_REFRESH_CHECK) = Some(tx.clone());
        let result = check_and_refresh_oauth_token_if_needed_impl(retry_count, force).await;
        if let Some(current) = lock(&PENDING_REFRESH_CHECK).as_ref().cloned() {
            let _ = current.send(Some(result.clone()));
        }
        *lock(&PENDING_REFRESH_CHECK) = None;
        return result;
    }
    check_and_refresh_oauth_token_if_needed_impl(retry_count, force).await
}

async fn check_and_refresh_oauth_token_if_needed_impl(
    retry_count: u32,
    force: bool,
) -> AuthResult<bool> {
    invalidate_oauth_cache_if_disk_changed().await;

    // 先用缓存值判过期（force=true 时跳过：服务端已判定 token 失效）。
    let tokens = get_claude_ai_oauth_tokens();
    if !force && !still_expired(&tokens) {
        return Ok(false);
    }
    let (refresh_token, scopes) = match tokens {
        Some(t) => match t.refresh_token {
            Some(rt) => (rt, t.scopes),
            None => return Ok(false),
        },
        None => return Ok(false),
    };
    if !should_use_claude_ai_auth(&scopes) {
        return Ok(false);
    }

    // 异步重读：可能另一个进程已刷新。
    clear_oauth_token_cache();
    let fresh_tokens = get_claude_ai_oauth_tokens_async().await?;
    if !still_expired(&fresh_tokens) {
        return Ok(false);
    }

    // 过确立凿：拿锁再刷一次。
    let claude_dir = PathBuf::from(get_claude_config_home_dir());
    let _ = tokio::fs::create_dir_all(&claude_dir).await;

    let release = match acquire_lock(&claude_dir).await {
        Ok(release) => release,
        Err(AuthError::LockBusy) => {
            // 别的进程持锁：退避重试。
            if retry_count < MAX_LOCK_RETRIES {
                log_event("tengu_oauth_token_refresh_lock_retry");
                let jitter = (rand::random::<u64>() % 1000) + 1;
                tokio::time::sleep(Duration::from_millis(1000 + jitter)).await;
                return Box::pin(check_and_refresh_oauth_token_if_needed_impl(
                    retry_count + 1,
                    force,
                ))
                .await;
            }
            log_event("tengu_oauth_token_refresh_lock_retry_limit_reached");
            return Ok(false);
        }
        Err(err) => {
            log_error(err);
            log_event("tengu_oauth_token_refresh_lock_error");
            return Ok(false);
        }
    };
    // 持锁后再验一次（竞态化解）。
    clear_oauth_token_cache();
    let locked_tokens = get_claude_ai_oauth_tokens_async().await?;
    if !still_expired(&locked_tokens) {
        log_event("tengu_oauth_token_refresh_race_resolved");
        drop(release);
        return Ok(false);
    }
    log_event("tengu_oauth_token_refresh_starting");
    // Claude.ai subscriber 刷新时不传 scopes：允许服务端 scope 扩容。
    let requested_scopes = if should_use_claude_ai_auth(&scopes) {
        None
    } else {
        Some(scopes.clone())
    };
    match refresh_oauth_token(&refresh_token, requested_scopes).await {
        Ok(refreshed_tokens) => {
            let _ = save_oauth_tokens_if_needed(&refreshed_tokens);
            clear_oauth_token_cache();
            drop(release);
            Ok(true)
        }
        Err(err) => {
            log_error(err);
            clear_oauth_token_cache();
            let current_tokens = get_claude_ai_oauth_tokens_async().await?;
            if let Some(current) = current_tokens {
                if !is_oauth_token_expired(current.expires_at) {
                    log_event("tengu_oauth_token_refresh_race_recovered");
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// 订阅等级判定
// ---------------------------------------------------------------------------

/// TS `isClaudeAISubscriber`。
pub fn is_claude_ai_subscriber() -> bool {
    if !is_anthropic_auth_enabled() {
        return false;
    }
    get_claude_ai_oauth_tokens()
        .map(|t| should_use_claude_ai_auth(&t.scopes))
        .unwrap_or(false)
}

/// TS `hasProfileScope`：env/FD service-key token 的 scopes 恒为
/// `['user:inference']`，用来挡 profile 域端点的 403。
pub fn has_profile_scope() -> bool {
    get_claude_ai_oauth_tokens()
        .map(|t| t.scopes.iter().any(|s| s == CLAUDE_AI_PROFILE_SCOPE))
        .unwrap_or(false)
}

/// TS `is1PApiCustomer`：非 Claude.ai 订阅者、非 Vertex/Bedrock/Foundry 用户。
pub fn is_1p_api_customer() -> bool {
    if is_env_truthy(env("CLAUDE_CODE_USE_BEDROCK").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_VERTEX").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_FOUNDRY").as_deref())
    {
        return false;
    }
    if is_claude_ai_subscriber() {
        return false;
    }
    true
}

/// TS `getOauthAccountInfo`：仅 Anthropic 认证启用时返回。
pub fn get_oauth_account_info() -> Option<OauthAccountInfo> {
    if !is_anthropic_auth_enabled() {
        return None;
    }
    let account = get_global_config().get("oauthAccount")?;
    serde_json::from_value(account.clone()).ok()
}

/// TS `isOverageProvisioningAllowed`：仅 Stripe / 移动计费类型可买额外用量。
pub fn is_overage_provisioning_allowed() -> bool {
    let account_info = get_oauth_account_info();
    let billing_type = account_info.and_then(|a| a.billing_type);
    if !is_claude_ai_subscriber() || billing_type.is_none() {
        return false;
    }
    matches!(
        billing_type.as_deref(),
        Some("stripe_subscription")
            | Some("stripe_subscription_contracted")
            | Some("apple_subscription")
            | Some("google_play_subscription")
    )
}

/// TS `getSubscriptionType`。
pub fn get_subscription_type() -> Option<SubscriptionType> {
    // mock 订阅（ant 内部测试）优先。
    if should_use_mock_subscription() {
        return get_mock_subscription_type();
    }
    if !is_anthropic_auth_enabled() {
        return None;
    }
    get_claude_ai_oauth_tokens().and_then(|t| t.subscription_type)
}

/// TS `hasOpusAccess`：无论订阅还是 PayG 都判 Opus 可用性；
/// subscriptionType 为 None 时不限制（覆盖 API 用户与未填类型的订阅者）。
pub fn has_opus_access() -> bool {
    matches!(
        get_subscription_type(),
        Some(SubscriptionType::Max)
            | Some(SubscriptionType::Enterprise)
            | Some(SubscriptionType::Team)
            | Some(SubscriptionType::Pro)
            | None
    )
}

/// TS `isMaxSubscriber`。
pub fn is_max_subscriber() -> bool {
    matches!(get_subscription_type(), Some(SubscriptionType::Max))
}

/// TS `isTeamSubscriber`。
pub fn is_team_subscriber() -> bool {
    matches!(get_subscription_type(), Some(SubscriptionType::Team))
}

/// TS `isTeamPremiumSubscriber`：team 且限流层为 `default_claude_max_5x`。
pub fn is_team_premium_subscriber() -> bool {
    matches!(get_subscription_type(), Some(SubscriptionType::Team))
        && get_rate_limit_tier().as_deref() == Some("default_claude_max_5x")
}

/// TS `isEnterpriseSubscriber`。
pub fn is_enterprise_subscriber() -> bool {
    matches!(get_subscription_type(), Some(SubscriptionType::Enterprise))
}

/// TS `isProSubscriber`。
pub fn is_pro_subscriber() -> bool {
    matches!(get_subscription_type(), Some(SubscriptionType::Pro))
}

/// TS `getRateLimitTier`。
pub fn get_rate_limit_tier() -> Option<String> {
    if !is_anthropic_auth_enabled() {
        return None;
    }
    get_claude_ai_oauth_tokens().and_then(|t| t.rate_limit_tier)
}

/// TS `getSubscriptionName`。
pub fn get_subscription_name() -> String {
    match get_subscription_type() {
        Some(SubscriptionType::Enterprise) => "Claude Enterprise",
        Some(SubscriptionType::Team) => "Claude Team",
        Some(SubscriptionType::Max) => "Claude Max",
        Some(SubscriptionType::Pro) => "Claude Pro",
        Some(SubscriptionType::Free) | None => "Claude API",
    }
    .to_string()
}

/// TS `isUsing3PServices`：仅按 `CLAUDE_CODE_USE_*` 环境变量判定
/// （刻意不看 settings.modelType）。与 providers.ts 保持同步。
pub fn is_using_3p_services() -> bool {
    is_env_truthy(env("CLAUDE_CODE_USE_BEDROCK").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_VERTEX").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_FOUNDRY").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_OPENAI").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_GEMINI").as_deref())
        || is_env_truthy(env("CLAUDE_CODE_USE_GROK").as_deref())
}

/// TS `isConsumerPlan`。
fn is_consumer_plan(plan: &SubscriptionType) -> bool {
    matches!(plan, SubscriptionType::Max | SubscriptionType::Pro)
}

/// TS `isConsumerSubscriber`。
pub fn is_consumer_subscriber() -> bool {
    let subscription_type = get_subscription_type();
    is_claude_ai_subscriber()
        && matches!(subscription_type, Some(ref p) if is_consumer_plan(p))
}

// ---------------------------------------------------------------------------
// otelHeadersHelper
// ---------------------------------------------------------------------------

/// TS `getConfiguredOtelHeadersHelper`。
fn get_configured_otel_headers_helper() -> Option<String> {
    get_initial_settings().otel_headers_helper
}

/// TS `isOtelHeadersHelperFromProjectOrLocalSettings`。
pub fn is_otel_headers_helper_from_project_or_local_settings() -> bool {
    let Some(helper) = get_configured_otel_headers_helper() else {
        return false;
    };
    let project = get_settings_for_source(SettingSource::ProjectSettings);
    let local = get_settings_for_source(SettingSource::LocalSettings);
    project.and_then(|s| s.otel_headers_helper).as_deref() == Some(helper.as_str())
        || local.and_then(|s| s.otel_headers_helper).as_deref() == Some(helper.as_str())
}

static CACHED_OTEL_HEADERS: Mutex<Option<(Vec<(String, String)>, i64)>> = Mutex::new(None);

/// TS `getOtelHeadersFromHelper`：29 分钟防抖 + trust 门禁 + JSON 校验。
/// **偏差**：30s 命令超时未强制（std::process 无 deadline 原语）。
pub fn get_otel_headers_from_helper() -> AuthResult<Vec<(String, String)>> {
    let Some(helper) = get_configured_otel_headers_helper() else {
        return Ok(Vec::new());
    };
    let debounce_ms = env("CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_OTEL_HEADERS_DEBOUNCE_MS);
    if let Some((headers, timestamp)) = lock(&CACHED_OTEL_HEADERS).clone() {
        if now_ms() - timestamp < debounce_ms as i64 {
            return Ok(headers);
        }
    }
    if is_otel_headers_helper_from_project_or_local_settings()
        && !check_has_trust_dialog_accepted()
    {
        return Ok(Vec::new());
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&helper)
        .output()?;
    if !output.status.success() {
        return Err(AuthError::OtelHeadersInvalid(
            "otelHeadersHelper did not return a valid value".to_string(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err(AuthError::OtelHeadersInvalid(
            "otelHeadersHelper did not return a valid value".to_string(),
        ));
    }
    let parsed = json_parse(&stdout)?;
    let headers = match parsed {
        Value::Object(map) => {
            let mut result = Vec::new();
            for (key, value) in map {
                let value = value.as_str().ok_or_else(|| {
                    AuthError::OtelHeadersInvalid(format!(
                        "otelHeadersHelper returned non-string value for key \"{key}\""
                    ))
                })?;
                result.push((key, value.to_string()));
            }
            result
        }
        _ => {
            return Err(AuthError::OtelHeadersInvalid(
                "otelHeadersHelper must return a JSON object with string key-value pairs".to_string(),
            ));
        }
    };
    *lock(&CACHED_OTEL_HEADERS) = Some((headers.clone(), now_ms()));
    Ok(headers)
}

// ---------------------------------------------------------------------------
// 账户信息 / 组织校验
// ---------------------------------------------------------------------------

/// TS `getAccountInformation`。
pub fn get_account_information() -> Option<UserAccountInfo> {
    if get_api_provider(&get_initial_settings()) != ApiProvider::FirstParty {
        return None;
    }
    let (auth_token_source, _has_token) = get_auth_token_source();
    let mut account_info = UserAccountInfo::default();
    match auth_token_source {
        AuthTokenSource::ClaudeCodeOauthToken
        | AuthTokenSource::ClaudeCodeOauthTokenFileDescriptor => {
            account_info.token_source = Some(auth_token_source);
        }
        _ if is_claude_ai_subscriber() => {
            account_info.subscription = Some(get_subscription_name());
        }
        _ => {
            account_info.token_source = Some(auth_token_source);
        }
    }
    let (api_key, api_key_source) = get_anthropic_api_key_with_source(GetApiKeyOpts::default());
    if api_key.is_some() {
        account_info.api_key_source = Some(api_key_source);
    }
    let oauth_account = get_oauth_account_info();
    let managed_credentials = auth_token_source == AuthTokenSource::ClaudeAi
        || api_key_source == ApiKeySource::LoginManagedKey;
    // 外部 key/token 场景下组织不可知。
    if managed_credentials {
        if let Some(org_name) = oauth_account.as_ref().and_then(|a| a.organization_name.clone()) {
            account_info.organization = Some(org_name);
        }
    }
    if managed_credentials {
        if let Some(email) = oauth_account.as_ref().and_then(|a| a.email_address.clone()) {
            account_info.email = Some(email);
        }
    }
    Some(account_info)
}

/// profile 端点响应的 duck-type 最小结构。
#[derive(Debug, Clone)]
pub struct OauthProfile {
    pub organization_uuid: String,
}

/// TS `getOauthProfileFromOauthToken`：需要 HTTP 客户端。
/// **TODO(reqwest)**：GET `${base_api_url}/api/oauth/profile`（Bearer token，
/// 错误吞掉返回 None，见 TS getOauthProfile.ts）。
pub async fn get_oauth_profile_from_oauth_token(
    _access_token: &str,
) -> AuthResult<Option<OauthProfile>> {
    Err(AuthError::HttpClientUnavailable("get_oauth_profile_from_oauth_token"))
}

/// TS `validateForceLoginOrg`：fail closed——拿不到 org 信息就判失败。
/// profile 端点拉取需要 HTTP 客户端，未就绪前返回 Err。
pub async fn validate_force_login_org() -> AuthResult<OrgValidationResult> {
    // claude ssh 远端：真实认证在本地由代理注入，占位 token 无法验。
    if env("ANTHROPIC_UNIX_SOCKET").is_some() {
        return Ok(OrgValidationResult::Valid);
    }
    if !is_anthropic_auth_enabled() {
        return Ok(OrgValidationResult::Valid);
    }
    let required_org_uuid = get_settings_for_source(SettingSource::PolicySettings)
        .and_then(|s| s.force_login_org_uuid);
    let Some(required_org_uuid) = required_org_uuid else {
        return Ok(OrgValidationResult::Valid);
    };

    // 访问 profile 端点前先确保 token 新鲜（env-var token 为 no-op）。
    check_and_refresh_oauth_token_if_needed(0, false).await?;

    let tokens = get_claude_ai_oauth_tokens();
    let Some(tokens) = tokens else {
        return Ok(OrgValidationResult::Valid);
    };

    // 始终从服务端取权威 org UUID——~/.claude.json 里的缓存用户可改，不可信。
    let auth_source = get_auth_token_source().0;
    let is_env_var_token = matches!(
        auth_source,
        AuthTokenSource::ClaudeCodeOauthToken
            | AuthTokenSource::ClaudeCodeOauthTokenFileDescriptor
    );

    let profile = get_oauth_profile_from_oauth_token(&tokens.access_token).await?;
    let Some(profile) = profile else {
        return Ok(OrgValidationResult::Invalid {
            message: format!(
                "Unable to verify organization for the current authentication token.\n\
                 This machine requires organization {required_org_uuid} but the profile could not be fetched.\n\
                 This may be a network error, or the token may lack the user:profile scope required for\n\
                 verification (tokens from 'claude setup-token' do not include this scope).\n\
                 Try again, or obtain a full-scope token via 'claude auth login'."
            ),
        });
    };
    if profile.organization_uuid == required_org_uuid {
        return Ok(OrgValidationResult::Valid);
    }
    if is_env_var_token {
        let env_var_name = if auth_source == AuthTokenSource::ClaudeCodeOauthToken {
            "CLAUDE_CODE_OAUTH_TOKEN"
        } else {
            "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR"
        };
        return Ok(OrgValidationResult::Invalid {
            message: format!(
                "The {env_var_name} environment variable provides a token for a\n\
                 different organization than required by this machine's managed settings.\n\n\
                 Required organization: {required_org_uuid}\n\
                 Token organization:   {}\n\n\
                 Remove the environment variable or obtain a token for the correct organization.",
                profile.organization_uuid
            ),
        });
    }
    Ok(OrgValidationResult::Invalid {
        message: format!(
            "Your authentication token belongs to organization {},\n\
             but this machine requires organization {required_org_uuid}.\n\n\
             Please log in with the correct organization: claude auth login",
            profile.organization_uuid
        ),
    })
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn inference_tokens() -> OAuthTokens {
        OAuthTokens::inference_only("at-123".to_string())
    }

    #[test]
    fn inference_only_token_shape() {
        let tokens = inference_tokens();
        assert!(should_use_claude_ai_auth(&tokens.scopes));
        assert!(!tokens.scopes.iter().any(|s| s == CLAUDE_AI_PROFILE_SCOPE));
        assert!(tokens.refresh_token.is_none());
        assert!(tokens.expires_at.is_none());
    }

    #[test]
    fn expiry_buffer_and_none() {
        assert!(!is_oauth_token_expired(None));
        let far_future = now_ms() + 3600_000;
        assert!(!is_oauth_token_expired(Some(far_future)));
        let near = now_ms() + 60_000;
        assert!(is_oauth_token_expired(Some(near)));
        let past = now_ms() - 1_000;
        assert!(is_oauth_token_expired(Some(past)));
    }

    #[test]
    fn subscription_type_roundtrip() {
        for s in ["free", "pro", "max", "team", "enterprise"] {
            let t = SubscriptionType::from_str(s).unwrap();
            assert_eq!(t.as_str(), s);
        }
        assert!(SubscriptionType::from_str("unknown").is_none());
    }

    #[test]
    fn normalize_api_key_takes_last_20() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwx";
        let normalized = normalize_api_key_for_config(key);
        assert_eq!(normalized.len(), 20);
        assert!(key.ends_with(&normalized));
        assert_eq!(normalize_api_key_for_config("short"), "short");
    }

    #[test]
    fn valid_api_key_charset() {
        assert!(is_valid_api_key("sk-ant-api03-abc_123"));
        assert!(!is_valid_api_key("sk ant"));
        assert!(!is_valid_api_key(""));
        assert!(!is_valid_api_key("sk/ant"));
    }

    #[test]
    fn aws_sts_output_validation() {
        let valid = serde_json::json!({
            "Credentials": {
                "AccessKeyId": "AKIA...",
                "SecretAccessKey": "secret",
                "SessionToken": "token",
            }
        });
        assert!(is_valid_aws_sts_output(&valid));
        assert!(!is_valid_aws_sts_output(&serde_json::json!({})));
        assert!(!is_valid_aws_sts_output(&serde_json::json!({"Credentials": {}})));
    }

    #[test]
    fn consumer_plan_classification() {
        assert!(!is_consumer_plan(&SubscriptionType::Free));
        assert!(is_consumer_plan(&SubscriptionType::Max));
        assert!(is_consumer_plan(&SubscriptionType::Pro));
        assert!(!is_consumer_plan(&SubscriptionType::Team));
        assert!(!is_consumer_plan(&SubscriptionType::Enterprise));
    }

    #[test]
    fn ttl_default_when_unset() {
        // 仅当未设置环境变量时断言默认值，避免与 CI 环境冲突。
        if std::env::var("CLAUDE_CODE_API_KEY_HELPER_TTL_MS").is_err() {
            assert_eq!(calculate_api_key_helper_ttl(), DEFAULT_API_KEY_HELPER_TTL_MS);
        }
    }

    #[test]
    fn error_is_clone_for_broadcast() {
        // watch channel 广播需要 Clone。
        let e = AuthError::HttpClientUnavailable("test");
        assert_eq!(e, e.clone());
        assert!(e.to_string().contains("reqwest"));
    }

    #[test]
    fn oauth_tokens_roundtrip_json() {
        let tokens = inference_tokens();
        let json = serde_json::to_string(&tokens).unwrap();
        let back: OAuthTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "at-123");
        assert!(back.refresh_token.is_none());
    }

    #[test]
    fn account_info_serde_camel_case() {
        let info = OauthAccountInfo {
            billing_type: Some("stripe_subscription".to_string()),
            organization_name: Some("acme".to_string()),
            email_address: Some("a@b.c".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("billingType"));
        assert!(json.contains("organizationName"));
        assert!(json.contains("emailAddress"));
    }

    #[test]
    fn mock_subscription_guard_non_ant() {
        // 默认非 ant（CI 不设 USER_TYPE=ant），setter 为 no-op。
        set_mock_subscription(true, Some(SubscriptionType::Max));
        if !is_ant_user() {
            assert!(!should_use_mock_subscription());
        }
    }
}
