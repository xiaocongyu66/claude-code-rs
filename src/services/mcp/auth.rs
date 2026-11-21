//! auth.rs — MCP OAuth 客户端授权（重写自 `auth.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - RFC 9728（PRM）→ RFC 8414（AS metadata）发现链；可配置
//!   `authServerMetadataUrl`（必须 https）。
//! - PKCE 授权码流程（公客户端）：DCR 动态注册 → authorization URL →
//!   loopback 回调服务器（state 校验防 CSRF，5 分钟超时，手动粘贴回调
//!   支持）→ token exchange → 存 token。
//! - Token 刷新：lockfile 串行化（MAX_LOCK_RETRIES=5）、重读再判过期、
//!   invalid_grant 清 token、瞬态错误指数回退（1s/2s/4s，最多 3 次）。
//! - 403 `insufficient_scope`（step-up）：transport 层感知 →
//!   `mark_step_up_pending` → tokens() 隐去 refresh_token（RFC 6749 §6
//!   禁止刷新提权）。
//! - OAuth 2.0 撤销（RFC 7009）：先 refresh_token 后 access_token，
//!   client_secret_basic 为主、401 时 Bearer 兜底。
//! - XAA（Cross-App Access，SEP-990）：见 xaa.rs / xaaIdpLogin.rs。
//!
//! 已知偏差（依赖未转换 → 本地最小实现）：
//! - `utils/secureStorage`（macOS keychain）：以 `~/.claude` 下的
//!   JSON 文件 duck-type（进程内写穿透缓存；**无 OS keychain 加密**，
//!   权限 0600）。
//! - `utils/browser.ts`（openBrowser）：占位（返回 false + debug 日志），
//!   靠 onAuthorizationUrl 回调把 URL 交给 UI。
//! - `utils/lockfile.ts` 已有 `lock()`/`LockGuard`：直接复用（ELOCKED
//!   由 lockfile.rs 的错误形态近似判定）。
//! - analytics（logEvent）未转换 → debug 日志占位。
//! - `wrapFetchWithStepUpDetection` 在 TS 里包装 fetch；本目录传输层
//!   直接把 401 映射为 needs-auth，step-up scope 提取由
//!   `mark_step_up_pending` 的调用方完成（同 transport）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::constants::oauth::MCP_CLIENT_METADATA_URL;
use crate::services::mcp::oauth_port::{build_redirect_uri, find_available_port};
use crate::services::mcp::types::{McpServerConfig, ScopedMcpServerConfig};
use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::log::log_mcp_debug;

/// OAuth 请求超时（发现、刷新等）。
pub const AUTH_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 刷新 lockfile 最大重试次数。
const MAX_LOCK_RETRIES: u32 = 5;

/// 需要脱敏的 OAuth query 参数。
const SENSITIVE_OAUTH_PARAMS: &[&str] = &[
    "state",
    "nonce",
    "code_challenge",
    "code_verifier",
    "code",
];

/// 非标准 invalid_grant 别名（Slack 等返回 200 + error body 的 AS）。
const NONSTANDARD_INVALID_GRANT_ALIASES: &[&str] =
    &["invalid_refresh_token", "expired_refresh_token", "token_expired"];

/// 用户取消认证（TS `AuthenticationCancelledError`）。
#[derive(Debug, Error)]
#[error("Authentication was cancelled")]
pub struct AuthenticationCancelledError;

/// XAA token 交换失败（携带是否应清 id_token 的语义）。
/// （定义于 xaa.rs，此处重导出。）
pub use crate::services::mcp::xaa::XaaTokenExchangeError;

/// OAuth tokens（对齐 SDK `OAuthTokens`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    /// 签发方绑定（SDK 1.32.x advisory）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

/// 安全存储条目（对齐 TS `SecureStorageData` 的 MCP 相关字段子集）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecureStorageData {
    #[serde(default, rename = "mcpOAuth", skip_serializing_if = "HashMap::is_empty")]
    pub mcp_oauth: HashMap<String, McpOAuthEntry>,
    #[serde(
        default,
        rename = "mcpOAuthClientConfig",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub mcp_oauth_client_config: HashMap<String, McpClientConfigEntry>,
    #[serde(default, rename = "mcpXaaIdp", skip_serializing_if = "HashMap::is_empty")]
    pub mcp_xaa_idp: HashMap<String, XaaIdpTokenEntry>,
    #[serde(
        default,
        rename = "mcpXaaIdpConfig",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub mcp_xaa_idp_config: HashMap<String, XaaIdpConfigEntry>,
}

/// 单个 MCP 服务器的 OAuth 条目（对齐 TS 的存储形状）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default)]
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_up_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_state: Option<PersistedDiscoveryState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedDiscoveryState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_server_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClientConfigEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XaaIdpTokenEntry {
    pub id_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XaaIdpConfigEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

// ---------------------------------------------------------------------------
// 安全存储（keychain 占位：JSON 文件，0600）
// ---------------------------------------------------------------------------

static STORAGE_CACHE: std::sync::Mutex<Option<SecureStorageData>> = std::sync::Mutex::new(None);

fn storage_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join("mcp-secure-storage.json")
}

/// 读安全存储（进程内缓存）。
pub fn get_secure_storage() -> SecureStorageData {
    {
        let cache = STORAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref() {
            return cached.clone();
        }
    }
    let data: SecureStorageData = std::fs::read(storage_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut cache = STORAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some(data.clone());
    data
}

/// 写安全存储（原子重命名 + 缓存同步）。
pub fn update_secure_storage(data: &SecureStorageData) {
    let path = storage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(data) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            if std::fs::rename(&tmp, &path).is_ok() {
                let mut cache = STORAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                *cache = Some(data.clone());
            }
        }
    }
}

/// 服务器凭据 key：`name|<sha256 前 16 位>`（type + url + headers）。
pub fn get_server_key(
    server_name: &str,
    config: &McpServerConfig,
) -> String {
    let (type_str, url, headers) = match config {
        McpServerConfig::SSE(c) => ("sse", c.url.clone(), c.headers.clone()),
        McpServerConfig::HTTP(c) => ("http", c.url.clone(), c.headers.clone()),
        other => (other.type_str(), String::new(), None),
    };
    let config_json = json!({
        "type": type_str,
        "url": url,
        "headers": headers.unwrap_or_default(),
    });
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_string(&config_json).unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{server_name}|{hex}")
}

/// 曾探测过（discovery state 存在）但没有凭据可试 —— 连接必 401。
pub fn has_mcp_discovery_but_no_token(
    server_name: &str,
    config: &McpServerConfig,
) -> bool {
    // XAA 服务器可用缓存的 id_token 静默换 token —— 不跳过
    if crate::services::mcp::xaa_idp_login::is_xaa_enabled() {
        if let Some(oauth) = config_sse_http_oauth(config) {
            if oauth.xaa.unwrap_or(false) {
                return false;
            }
        }
    }
    let server_key = get_server_key(server_name, config);
    let storage = get_secure_storage();
    match storage.mcp_oauth.get(&server_key) {
        Some(entry) => entry.access_token.is_empty() && entry.refresh_token.is_none(),
        None => false,
    }
}

fn config_sse_http_oauth(config: &McpServerConfig) -> Option<&crate::services::mcp::types::McpOAuthConfig> {
    match config {
        McpServerConfig::SSE(c) => c.oauth.as_ref(),
        McpServerConfig::HTTP(c) => c.oauth.as_ref(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// OAuth 元数据发现（RFC 9728 PRM → RFC 8414）
// ---------------------------------------------------------------------------

/// 授权服务器元数据（按用的字段建模 AS metadata / OIDC metadata 并集）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint_auth_methods_supported: Option<Vec<String>>,
    /// PRM 才有：authorization_servers。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// 带超时的 auth HTTP 客户端。
fn auth_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(AUTH_REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// RFC 8414 AS metadata 发现（`<base>/.well-known/oauth-authorization-server`，
/// path-aware）。
async fn discover_authorization_server_metadata(
    base_url: &str,
) -> Result<AuthorizationServerMetadata, String> {
    let trimmed = base_url.trim_end_matches('/');
    let url = format!("{trimmed}/.well-known/oauth-authorization-server");
    let response = auth_client()?
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }
    response
        .json::<AuthorizationServerMetadata>()
        .await
        .map_err(|e| format!("invalid metadata from {url}: {e}"))
}

/// RFC 9728 PRM 发现：`/.well-known/oauth-protected-resource` →
/// `authorization_servers[0]` → RFC 8414。
async fn discover_protected_resource(
    server_url: &str,
) -> Result<Option<AuthorizationServerMetadata>, String> {
    let trimmed = server_url.trim_end_matches('/');
    let url = format!("{trimmed}/.well-known/oauth-protected-resource");
    let response = auth_client()?
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let prm: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("invalid PRM from {url}: {e}"))?;
    let authorization_servers = prm
        .get("authorization_servers")
        .and_then(Value::as_array);
    let Some(servers) = authorization_servers else {
        return Ok(None);
    };
    let Some(first) = servers.first().and_then(Value::as_str) else {
        return Ok(None);
    };
    discover_authorization_server_metadata(first)
        .await
        .map(Some)
}

/// 发现顺序：配置的 metadata URL → RFC 9728 → RFC 8414 path-aware 兜底。
pub async fn fetch_auth_server_metadata(
    server_name: &str,
    server_url: &str,
    configured_metadata_url: Option<&str>,
) -> Result<Option<AuthorizationServerMetadata>, String> {
    if let Some(configured) = configured_metadata_url {
        if !configured.starts_with("https://") {
            return Err(format!(
                "authServerMetadataUrl must use https:// (got: {configured})"
            ));
        }
        let response = auth_client()?
            .get(configured)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("GET {configured} failed: {e}"))?;
        if response.status().is_success() {
            let metadata = response
                .json::<AuthorizationServerMetadata>()
                .await
                .map_err(|e| format!("invalid metadata from {configured}: {e}"))?;
            return Ok(Some(metadata));
        }
        return Err(format!(
            "HTTP {} fetching configured auth server metadata from {configured}",
            response.status()
        ));
    }

    // RFC 9728 PRM
    match discover_protected_resource(server_url).await {
        Ok(Some(metadata)) => return Ok(Some(metadata)),
        Ok(None) => {}
        Err(e) => log_mcp_debug(
            server_name,
            &format!("RFC 9728 discovery failed, falling back: {e}"),
        ),
    }

    // 兜底：path-aware RFC 8414（根 URL 时 SDK 已探测同一端点，返回 None）
    let parsed_path = server_url.split("://").nth(1).unwrap_or("");
    if parsed_path.contains('/') {
        return discover_authorization_server_metadata(server_url)
            .await
            .map(Some);
    }
    Ok(None)
}

/// 安全提取元数据中的 scope（TS `getScopeFromMetadata`）。
pub fn get_scope_from_metadata(metadata: Option<&AuthorizationServerMetadata>) -> Option<String> {
    let metadata = metadata?;
    if let Some(scope) = &metadata.scope {
        return Some(scope.clone());
    }
    if let Some(supported) = &metadata.grant_types_supported {
        if !supported.is_empty() {
            return Some(supported.join(" "));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// PKCE / 授权码工具
// ---------------------------------------------------------------------------

/// URL query 值百分号编码（无 percent-encoding crate 的手动实现）。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// base64url 编码（无 padding）。
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

/// 生成 32 字节随机 state（base64url）。
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64url(&bytes)
}

/// 生成 PKCE code_verifier（64 字符 unreserved）。
pub fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..64)
        .map(|_| CHARSET[rng.next_u32() as usize % CHARSET.len()] as char)
        .collect()
}

/// S256 code_challenge（base64url）。
pub fn code_challenge_s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url(&hasher.finalize())
}

/// 脱敏 URL 中的敏感 OAuth 参数（日志安全）。
pub fn redact_sensitive_url_params(url: &str) -> String {
    let mut result = url.to_string();
    for param in SENSITIVE_OAUTH_PARAMS {
        // 手写 query 值替换（无 url crate）
        let token = format!("{param}=");
        if let Some(pos) = result.find(&token) {
            let start = pos + token.len();
            let end = result[start..]
                .find(|c: char| c == '&' || c == '#' || c == ' ')
                .map(|e| start + e)
                .unwrap_or(result.len());
            result.replace_range(start..end, "[REDACTED]");
        }
    }
    result
}

// ---------------------------------------------------------------------------
// ClaudeAuthProvider（OAuthClientProvider 实现）
// ---------------------------------------------------------------------------

/// OAuth 客户端元数据（对齐 SDK `OAuthClientMetadata`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OAuthClientMetadata {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// 用户在 UI 中看到授权 URL 的回调。
pub type OnAuthorizationUrl = Arc<dyn Fn(&str) + Send + Sync>;

/// MCP 服务器的 OAuth 提供者（对齐 TS `ClaudeAuthProvider`）。
pub struct ClaudeAuthProvider {
    server_name: String,
    server_config: ScopedMcpServerConfig,
    redirect_uri: String,
    handle_redirection: bool,
    on_authorization_url: Option<OnAuthorizationUrl>,
    skip_browser_open: bool,
    inner: tokio::sync::Mutex<ProviderInner>,
}

struct ProviderInner {
    code_verifier: Option<String>,
    authorization_url: Option<String>,
    state: Option<String>,
    scopes: Option<String>,
    metadata: Option<AuthorizationServerMetadata>,
    refresh_in_progress: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Option<OAuthTokens>> + Send>>,
    >,
    pending_step_up_scope: Option<String>,
}

impl ClaudeAuthProvider {
    pub fn new(
        server_name: impl Into<String>,
        server_config: ScopedMcpServerConfig,
        redirect_uri: Option<String>,
        handle_redirection: bool,
        on_authorization_url: Option<OnAuthorizationUrl>,
        skip_browser_open: bool,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            redirect_uri: redirect_uri.unwrap_or_else(|| build_redirect_uri(None)),
            server_config,
            handle_redirection,
            on_authorization_url,
            skip_browser_open,
            inner: tokio::sync::Mutex::new(ProviderInner {
                code_verifier: None,
                authorization_url: None,
                state: None,
                scopes: None,
                metadata: None,
                refresh_in_progress: None,
                pending_step_up_scope: None,
            }),
        }
    }

    fn server_key(&self) -> String {
        get_server_key(&self.server_name, &self.server_config.config)
    }

    /// 客户端元数据（CIMD：client_id_metadata_document_supported 时 SDK 用
    /// 该 URL 作为 client_id，跳过 DCR）。
    pub async fn client_metadata(&self) -> OAuthClientMetadata {
        let inner = self.inner.lock().await;
        let scope = get_scope_from_metadata(inner.metadata.as_ref());
        OAuthClientMetadata {
            client_name: format!("Claude Code ({})", self.server_name),
            redirect_uris: vec![self.redirect_uri.clone()],
            grant_types: vec!["authorization_code".to_string(), "refresh_token".to_string()],
            response_types: vec!["code".to_string()],
            token_endpoint_auth_method: "none".to_string(), // 公客户端
            scope,
        }
    }

    /// CIMD URL（环境变量覆盖 > 默认）。
    pub fn client_metadata_url(&self) -> Option<String> {
        std::env::var("MCP_OAUTH_CLIENT_METADATA_URL")
            .ok()
            .or_else(|| Some(MCP_CLIENT_METADATA_URL.to_string()))
    }

    /// 注入元数据（供授权流程取 scope）。
    pub async fn set_metadata(&self, metadata: AuthorizationServerMetadata) {
        self.inner.lock().await.metadata = Some(metadata);
    }

    /// 传输层检测到 403 insufficient_scope 时调用：tokens() 将隐去
    /// refresh_token，迫使 SDK 走 PKCE 提权而不是无意义的刷新。
    pub async fn mark_step_up_pending(&self, scope: &str) {
        log_mcp_debug(&self.server_name, &format!("Marked step-up pending: {scope}"));
        self.inner.lock().await.pending_step_up_scope = Some(scope.to_string());
    }

    /// 生成（幂等）OAuth state。
    pub async fn state(&self) -> String {
        let mut inner = self.inner.lock().await;
        if inner.state.is_none() {
            log_mcp_debug(&self.server_name, "Generated new OAuth state");
            inner.state = Some(generate_state());
        }
        inner.state.clone().unwrap_or_default()
    }

    /// 存储中的客户端信息（DCR/上次授权结果；缺失返回 None 触发注册）。
    pub async fn client_information(
        &self,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let storage = get_secure_storage();
        let server_key = self.server_key();
        if let Some(entry) = storage.mcp_oauth.get(&server_key) {
            if let Some(client_id) = &entry.client_id {
                log_mcp_debug(&self.server_name, "Found client info");
                return Some((
                    client_id.clone(),
                    entry.client_secret.clone(),
                    entry.issuer.clone(),
                ));
            }
        }

        // 回退：配置中的预置 clientId
        if let Some(configured) = config_sse_http_oauth(&self.server_config.config)
            .and_then(|o| o.client_id.clone())
        {
            log_mcp_debug(&self.server_name, "Using pre-configured client ID");
            let secret = storage
                .mcp_oauth_client_config
                .get(&server_key)
                .and_then(|c| c.client_secret.clone());
            return Some((configured, secret, None));
        }

        log_mcp_debug(&self.server_name, "No client info found");
        None
    }

    /// 保存 DCR/授权得到的客户端信息。
    pub async fn save_client_information(
        &self,
        client_id: &str,
        client_secret: Option<&str>,
        issuer: Option<&str>,
    ) {
        let mut storage = get_secure_storage();
        let server_key = self.server_key();
        let entry = storage.mcp_oauth.entry(server_key.clone()).or_default();
        entry.server_name = Some(self.server_name.clone());
        entry.server_url = self.server_config.config.url().map(str::to_string);
        entry.client_id = Some(client_id.to_string());
        entry.client_secret = client_secret.map(str::to_string);
        entry.issuer = issuer.map(str::to_string);
        update_secure_storage(&storage);
    }

    /// 当前存储的 OAuth tokens（含 XAA 静默换 token、主动刷新、step-up
    /// 隐去 refresh_token 的完整语义）。
    pub async fn tokens(&self) -> Option<OAuthTokens> {
        let server_key = self.server_key();
        let storage = get_secure_storage();
        let token_data = storage.mcp_oauth.get(&server_key).cloned();
        let needs_step_up = {
            let inner = self.inner.lock().await;
            match &inner.pending_step_up_scope {
                Some(pending) => {
                    let current_scopes: Vec<&str> = token_data
                        .as_ref()
                        .and_then(|t| t.scope.as_deref())
                        .map(|s| s.split(' ').collect())
                        .unwrap_or_default();
                    pending.split(' ').any(|s| !current_scopes.contains(&s))
                }
                None => false,
            }
        };

        // XAA 静默换 token：无 refresh_token 且 access_token 过期/缺失时
        let xaa_enabled = crate::services::mcp::xaa_idp_login::is_xaa_enabled()
            && config_sse_http_oauth(&self.server_config.config)
                .map(|o| o.xaa.unwrap_or(false))
                .unwrap_or(false);
        let token_data_ref = &token_data;
        let no_refresh = token_data_ref
            .as_ref()
            .map(|t| t.refresh_token.is_none())
            .unwrap_or(true);
        let expiring = token_data_ref
            .as_ref()
            .map(|t| (t.expires_at - now_ms()) / 1000 <= 300)
            .unwrap_or(true);
        if xaa_enabled && no_refresh && (token_data.is_none() || expiring) {
            let mut inner = self.inner.lock().await;
            if inner.refresh_in_progress.is_none() {
                let provider_future = {
                    let server_name = self.server_name.clone();
                    let config = self.server_config.clone();
                    Box::pin(async move {
                        crate::services::mcp::auth::xaa_refresh(&server_name, &config).await
                    })
                        as std::pin::Pin<
                            Box<dyn std::future::Future<Output = Option<OAuthTokens>> + Send>,
                        >
                };
                inner.refresh_in_progress = Some(provider_future);
            }
            // 取已存在的 future 引用困难（Mutex 内 BoxFuture 无 Clone）：
            // 释放锁后重读一次并等待（同一进程内并发刷新最多重复一次交换）
            let future = inner.refresh_in_progress.take();
            drop(inner);
            if let Some(future) = future {
                let refreshed = future.await;
                if let Some(tokens) = refreshed {
                    return Some(tokens);
                }
            }
        }

        let Some(token_data) = token_data else {
            log_mcp_debug(&self.server_name, "No token data found");
            return None;
        };

        let expires_in = (token_data.expires_at - now_ms()) / 1000;

        // 过期且无 refresh_token → undefined（needs-auth）
        if expires_in <= 0 && token_data.refresh_token.is_none() {
            log_mcp_debug(&self.server_name, "Token expired without refresh token");
            return None;
        }

        // 主动 refresh（5 分钟内过期 + 有 refresh_token + 非 step-up）
        if expires_in <= 300 && token_data.refresh_token.is_some() && !needs_step_up {
            let refresh_token = token_data.refresh_token.clone().unwrap_or_default();
            match self.refresh_authorization(&refresh_token).await {
                Some(refreshed) => {
                    log_mcp_debug(&self.server_name, "Token refreshed successfully");
                    let stored_issuer = get_secure_storage()
                        .mcp_oauth
                        .get(&server_key)
                        .and_then(|e| e.issuer.clone());
                    return Some(OAuthTokens {
                        issuer: refreshed.issuer.or(stored_issuer),
                        ..refreshed
                    });
                }
                None => {
                    log_mcp_debug(&self.server_name, "Token refresh failed, returning current tokens")
                }
            }
        }

        Some(OAuthTokens {
            access_token: token_data.access_token,
            refresh_token: if needs_step_up {
                None
            } else {
                token_data.refresh_token
            },
            expires_in: Some(expires_in),
            scope: token_data.scope,
            token_type: Some("Bearer".to_string()),
            issuer: token_data.issuer,
        })
    }

    /// 保存 tokens（清 step-up 标记；issuer 缺失时保留存储中的绑定）。
    pub async fn save_tokens(&self, tokens: &OAuthTokens) {
        let mut inner = self.inner.lock().await;
        inner.pending_step_up_scope = None;
        drop(inner);

        let mut storage = get_secure_storage();
        let server_key = self.server_key();
        let entry = storage.mcp_oauth.entry(server_key.clone()).or_default();
        entry.server_name = Some(self.server_name.clone());
        entry.server_url = self.server_config.config.url().map(str::to_string);
        entry.access_token = tokens.access_token.clone();
        entry.refresh_token = tokens.refresh_token.clone();
        entry.expires_at =
            now_ms() + tokens.expires_in.unwrap_or(3600) * 1000;
        entry.scope = tokens.scope.clone();
        let prev_issuer = entry.issuer.clone();
        entry.issuer = tokens.issuer.clone().or(prev_issuer);
        update_secure_storage(&storage);
    }

    /// 存授权 URL + scope；handleRedirection 时通知 UI 并开浏览器。
    pub async fn redirect_to_authorization(&self, authorization_url: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.authorization_url = Some(authorization_url.to_string());

        let scopes = extract_query_value(authorization_url, "scope");
        if let Some(scopes) = &scopes {
            inner.scopes = Some(scopes.clone());
        } else {
            inner.scopes = get_scope_from_metadata(inner.metadata.as_ref());
        }

        // step-up scope 持久化（仅 transport 附带的 provider：!handleRedirection）
        if let Some(scopes) = &inner.scopes {
            if !self.handle_redirection {
                let mut storage = get_secure_storage();
                let server_key = self.server_key();
                if let Some(entry) = storage.mcp_oauth.get_mut(&server_key) {
                    entry.step_up_scope = Some(scopes.clone());
                    update_secure_storage(&storage);
                }
            }
        }
        drop(inner);

        if !self.handle_redirection {
            log_mcp_debug(&self.server_name, "Redirection handling is disabled, skipping redirect");
            return Ok(());
        }

        if !authorization_url.starts_with("http://") && !authorization_url.starts_with("https://") {
            return Err("Invalid authorization URL: must use http:// or https:// scheme".to_string());
        }

        let redacted = redact_sensitive_url_params(authorization_url);
        log_mcp_debug(&self.server_name, &format!("Authorization URL: {redacted}"));

        // 通知 UI（先于浏览器打开，作为浏览器失败时的回退）
        if let Some(callback) = &self.on_authorization_url {
            callback(authorization_url);
        }

        if !self.skip_browser_open {
            let success = open_browser(authorization_url).await;
            if !success {
                log_mcp_debug(&self.server_name, "Browser didn't open automatically. URL is shown in UI.");
            }
        }
        Ok(())
    }

    /// 保存 PKCE code_verifier（内存）。
    pub async fn save_code_verifier(&self, code_verifier: &str) {
        self.inner.lock().await.code_verifier = Some(code_verifier.to_string());
    }

    /// 读取 code_verifier。
    pub async fn code_verifier(&self) -> Result<String, String> {
        let inner = self.inner.lock().await;
        inner
            .code_verifier
            .clone()
            .ok_or_else(|| "No code verifier saved".to_string())
    }

    /// 失效凭据（all/client/tokens/verifier/discovery）。
    pub async fn invalidate_credentials(&self, scope: &str) {
        let mut inner = self.inner.lock().await;
        if scope == "verifier" {
            inner.code_verifier = None;
            return;
        }
        drop(inner);

        let mut storage = get_secure_storage();
        let server_key = self.server_key();
        let Some(entry) = storage.mcp_oauth.get_mut(&server_key) else {
            return;
        };
        match scope {
            "all" => {
                storage.mcp_oauth.remove(&server_key);
            }
            "client" => {
                entry.client_id = None;
                entry.client_secret = None;
            }
            "tokens" => {
                entry.access_token = String::new();
                entry.refresh_token = None;
                entry.expires_at = 0;
            }
            "discovery" => {
                entry.discovery_state = None;
                entry.step_up_scope = None;
            }
            _ => {}
        }
        update_secure_storage(&storage);
        log_mcp_debug(&self.server_name, &format!("Invalidated credentials (scope: {scope})"));
    }

    /// 保存发现状态（只存 URL，不存完整 metadata blob —— keychain 大小）。
    pub async fn save_discovery_state(
        &self,
        authorization_server_url: Option<&str>,
        resource_metadata_url: Option<&str>,
    ) {
        let mut storage = get_secure_storage();
        let server_key = self.server_key();
        let entry = storage.mcp_oauth.entry(server_key).or_default();
        entry.server_name = Some(self.server_name.clone());
        entry.server_url = self.server_config.config.url().map(str::to_string);
        entry.discovery_state = Some(PersistedDiscoveryState {
            authorization_server_url: authorization_server_url.map(str::to_string),
            resource_metadata_url: resource_metadata_url.map(str::to_string),
        });
        update_secure_storage(&storage);
    }

    /// 读取发现状态（存储 > 配置的 metadata URL）。
    pub async fn discovery_state(&self) -> Option<PersistedDiscoveryState> {
        let storage = get_secure_storage();
        let server_key = self.server_key();
        if let Some(cached) = storage.mcp_oauth.get(&server_key).and_then(|e| e.discovery_state.as_ref())
        {
            if cached.authorization_server_url.is_some() {
                return Some(cached.clone());
            }
        }
        // 配置直连 metadata URL
        let metadata_url = config_sse_http_oauth(&self.server_config.config)
            .and_then(|o| o.auth_server_metadata_url.clone());
        if let Some(url) = metadata_url {
            if let Ok(Some(metadata)) =
                fetch_auth_server_metadata(&self.server_name, "", Some(&url)).await
            {
                return Some(PersistedDiscoveryState {
                    authorization_server_url: metadata.issuer,
                    resource_metadata_url: None,
                });
            }
        }
        None
    }

    /// 刷新授权（lockfile 串行 + 重读再判 + 指数回退重试）。
    pub async fn refresh_authorization(&self, refresh_token: &str) -> Option<OAuthTokens> {
        let server_key = self.server_key();
        let claude_dir = PathBuf::from(get_claude_config_home_dir());
        let _ = std::fs::create_dir_all(&claude_dir);
        let sanitized_key: String = server_key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let lock_path = claude_dir.join(format!("mcp-refresh-{sanitized_key}.lock"));

        // lockfile：ELOCKED 时重试等待（对齐 TS MAX_LOCK_RETRIES）
        let mut guard = None;
        for retry in 0..MAX_LOCK_RETRIES {
            let options = crate::utils::lockfile::LockOptions::default();
            match crate::utils::lockfile::lock(&lock_path, &options).await {
                Ok(g) => {
                    guard = Some(g);
                    break;
                }
                Err(_) => {
                    log_mcp_debug(
                        &self.server_name,
                        &format!("Refresh lock held by another process, waiting (attempt {}/{MAX_LOCK_RETRIES})", retry + 1),
                    );
                    let jitter = (rand::random::<u64>() % 1000) + 1000;
                    tokio::time::sleep(Duration::from_millis(jitter)).await;
                }
            }
        }
        let _guard = guard; // drop 时释放锁

        // 重读：另一进程可能已刷新
        let storage = get_secure_storage();
        let mut refresh_token = refresh_token.to_string();
        if let Some(token_data) = storage.mcp_oauth.get(&server_key) {
            let expires_in = (token_data.expires_at - now_ms()) / 1000;
            if expires_in > 300 {
                log_mcp_debug(
                    &self.server_name,
                    &format!("Another process already refreshed tokens (expires in {expires_in}s)"),
                );
                return Some(OAuthTokens {
                    access_token: token_data.access_token.clone(),
                    refresh_token: token_data.refresh_token.clone(),
                    expires_in: Some(expires_in),
                    scope: token_data.scope.clone(),
                    token_type: Some("Bearer".to_string()),
                    issuer: token_data.issuer.clone(),
                });
            }
            if let Some(stored_refresh) = &token_data.refresh_token {
                refresh_token = stored_refresh.clone();
            }
        }

        self.do_refresh(&refresh_token).await
    }

    /// 刷新主逻辑（最多 3 次；invalid_grant 清 token；瞬态错误 1s/2s/4s 回退）。
    async fn do_refresh(&self, refresh_token: &str) -> Option<OAuthTokens> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 1;
        while attempt <= MAX_ATTEMPTS {
            let result = self.do_refresh_once(refresh_token).await;
            match result {
                Ok(Some(tokens)) => {
                    self.save_tokens(&tokens).await;
                    let stored_issuer = get_secure_storage()
                        .mcp_oauth
                        .get(&self.server_key())
                        .and_then(|e| e.issuer.clone());
                    return Some(OAuthTokens {
                        issuer: tokens.issuer.or(stored_issuer),
                        ..tokens
                    });
                }
                Ok(None) => {
                    return None;
                }
                Err(reason) => {
                    if reason == RefreshFailureReason::InvalidGrant {
                        // 另一进程可能已刷新：重读判断
                        let storage = get_secure_storage();
                        if let Some(token_data) = storage.mcp_oauth.get(&self.server_key()) {
                            let expires_in = (token_data.expires_at - now_ms()) / 1000;
                            if expires_in > 300 {
                                return Some(OAuthTokens {
                                    access_token: token_data.access_token.clone(),
                                    refresh_token: token_data.refresh_token.clone(),
                                    expires_in: Some(expires_in),
                                    scope: token_data.scope.clone(),
                                    token_type: Some("Bearer".to_string()),
                                    issuer: token_data.issuer.clone(),
                                });
                            }
                        }
                        self.invalidate_credentials("tokens").await;
                        return None;
                    }
                    if !reason.is_retryable() || attempt >= MAX_ATTEMPTS {
                        return None;
                    }
                    let delay_ms = 1000 * 2u64.pow(attempt - 1);
                    log_mcp_debug(
                        &self.server_name,
                        &format!("Token refresh failed, retrying in {delay_ms}ms (attempt {attempt}/{MAX_ATTEMPTS})"),
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            attempt += 1;
        }
        None
    }

    async fn do_refresh_once(
        &self,
        refresh_token: &str,
    ) -> Result<Option<OAuthTokens>, RefreshFailureReason> {
        // 元数据：内存 > 持久化发现 state > 完整再发现
        let metadata = {
            let inner = self.inner.lock().await;
            inner.metadata.clone()
        };
        let metadata = match metadata {
            Some(m) => m,
            None => {
                let Some(cached) = self.discovery_state().await else {
                    return Err(RefreshFailureReason::MetadataDiscoveryFailed);
                };
                let Some(as_url) = cached.authorization_server_url.clone() else {
                    return Err(RefreshFailureReason::MetadataDiscoveryFailed);
                };
                discover_authorization_server_metadata(&as_url)
                    .await
                    .map_err(|_| RefreshFailureReason::MetadataDiscoveryFailed)?
            }
        };
        let client_info = self
            .client_information()
            .await
            .ok_or(RefreshFailureReason::NoClientInfo)?;

        let Some(token_endpoint) = metadata.token_endpoint.clone() else {
            return Err(RefreshFailureReason::MetadataDiscoveryFailed);
        };

        let mut params = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), client_info.0),
        ];
        if let Some(scope) = &metadata.scope {
            params.push(("scope".to_string(), scope.clone()));
        }
        if let Some(secret) = &client_info.1 {
            params.push(("client_secret".to_string(), secret.clone()));
        }

        let result = post_form(&token_endpoint, &params)
            .await
            .map_err(|_| RefreshFailureReason::RequestFailed)?;
        match classify_token_response(&result, false) {
            TokenResponse::Tokens(tokens) => Ok(Some(tokens)),
            TokenResponse::InvalidGrant => Err(RefreshFailureReason::InvalidGrant),
            TokenResponse::Transient => Err(RefreshFailureReason::Transient),
            TokenResponse::Other => Err(RefreshFailureReason::RequestFailed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshFailureReason {
    MetadataDiscoveryFailed,
    NoClientInfo,
    InvalidGrant,
    Transient,
    RequestFailed,
}

impl RefreshFailureReason {
    fn is_retryable(&self) -> bool {
        matches!(self, RefreshFailureReason::Transient)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// token 请求分类（兼容 200 + error body 的 AS）
// ---------------------------------------------------------------------------

enum TokenResponse {
    Tokens(OAuthTokens),
    InvalidGrant,
    Transient,
    Other,
}

struct TokenRequestResult {
    status: reqwest::StatusCode,
    body: Value,
}

async fn post_form(url: &str, params: &[(String, String)]) -> Result<TokenRequestResult, String> {
    let client = auth_client()?;
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("POST {url} failed: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    Ok(TokenRequestResult { status, body })
}

fn classify_token_response(result: &TokenRequestResult, _is_auth_code: bool) -> TokenResponse {
    // error body（200 或 4xx）
    if let Some(error) = result.body.get("error").and_then(Value::as_str) {
        if error == "invalid_grant" || NONSTANDARD_INVALID_GRANT_ALIASES.contains(&error) {
            return TokenResponse::InvalidGrant;
        }
        if error == "server_error" || error == "temporarily_unavailable" || error == "slow_down" {
            return TokenResponse::Transient;
        }
        return TokenResponse::Other;
    }
    if result.status.as_u16() == 429 {
        return TokenResponse::Transient;
    }
    if !result.status.is_success() {
        return TokenResponse::Other;
    }
    let access_token = result
        .body
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(access_token) = access_token else {
        return TokenResponse::Other;
    };
    TokenResponse::Tokens(OAuthTokens {
        access_token,
        refresh_token: result
            .body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_in: result
            .body
            .get("expires_in")
            .and_then(Value::as_i64),
        scope: result.body.get("scope").and_then(Value::as_str).map(str::to_string),
        token_type: Some(
            result
                .body
                .get("token_type")
                .and_then(Value::as_str)
                .unwrap_or("Bearer")
                .to_string(),
        ),
        issuer: result.body.get("issuer").and_then(Value::as_str).map(str::to_string),
    })
}

/// 从 URL query 取单个值（无 url crate 的手动解析）。
fn extract_query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return parts.next().map(|v| percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
            }
            b'+' => out.push(b' '),
            _ => out.push(bytes[i]),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 打开浏览器（utils/browser.ts 未转换：占位返回 false，让 UI 回退到
/// 显示授权 URL）。
async fn open_browser(_url: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// XAA 静默刷新（cached id_token → 4 步交换 → 新 access_token）
// ---------------------------------------------------------------------------

/// XAA 静默刷新（provider.tokens() 的 XAA 分支调用）。
pub async fn xaa_refresh(
    server_name: &str,
    config: &ScopedMcpServerConfig,
) -> Option<OAuthTokens> {
    let idp = crate::services::mcp::xaa_idp_login::get_xaa_idp_settings();
    let idp = idp?;
    let id_token = crate::services::mcp::xaa_idp_login::get_cached_idp_id_token(&idp.issuer)?;
    let server_key = get_server_key(server_name, &config.config);

    let client_id = config_sse_http_oauth(&config.config).and_then(|o| o.client_id.clone());
    let client_config = get_mcp_client_config_by_key(&server_key);
    let (client_id, client_secret) = match (client_id, client_config.and_then(|c| c.client_secret)) {
        (Some(id), Some(secret)) => (id, secret),
        _ => {
            log_mcp_debug(
                server_name,
                "XAA: missing clientId or clientSecret in config — skipping silent refresh",
            );
            return None;
        }
    };

    let idp_client_secret =
        crate::services::mcp::xaa_idp_login::get_idp_client_secret(&idp.issuer);

    let oidc = match crate::services::mcp::xaa_idp_login::discover_oidc(&idp.issuer).await {
        Ok(oidc) => oidc,
        Err(e) => {
            log_mcp_debug(
                server_name,
                &format!("XAA: OIDC discovery failed in silent refresh: {e}"),
            );
            return None;
        }
    };

    let xaa_config = crate::services::mcp::xaa::XaaConfig {
        client_id,
        client_secret,
        idp_client_id: idp.client_id.clone(),
        idp_client_secret,
        idp_id_token: id_token,
        idp_token_endpoint: oidc.token_endpoint,
    };
    let server_url = config.config.url().unwrap_or("").to_string();
    let tokens = match crate::services::mcp::xaa::perform_cross_app_access(
        &server_url,
        &xaa_config,
        server_name,
        None,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(crate::services::mcp::xaa::XaaError::Exchange(xaa_error))
            if xaa_error.should_clear_id_token =>
        {
            crate::services::mcp::xaa_idp_login::clear_idp_id_token(&idp.issuer);
            log_mcp_debug(server_name, "XAA: cleared id_token after exchange failure");
            return None;
        }
        Err(_) => return None,
    };

    // 直写存储（saveTokens 不会写 clientId/clientSecret）
    let mut storage = get_secure_storage();
    let entry = storage.mcp_oauth.entry(server_key.clone()).or_default();
    entry.server_name = Some(server_name.to_string());
    entry.server_url = Some(server_url);
    entry.access_token = tokens.access_token.clone();
    if tokens.refresh_token.is_some() {
        entry.refresh_token = tokens.refresh_token.clone();
    }
    entry.expires_at = now_ms() + tokens.expires_in.unwrap_or(3600) * 1000;
    entry.scope = tokens.scope.clone();
    entry.client_id = Some(xaa_config.client_id.clone());
    entry.client_secret = Some(xaa_config.client_secret.clone());
    entry.discovery_state = Some(PersistedDiscoveryState {
        authorization_server_url: Some(tokens.authorization_server_url.clone()),
        resource_metadata_url: None,
    });
    update_secure_storage(&storage);

    Some(OAuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
        scope: tokens.scope,
        token_type: Some("Bearer".to_string()),
        issuer: None,
    })
}

// ---------------------------------------------------------------------------
// 撤销（RFC 7009）
// ---------------------------------------------------------------------------

/// 撤销单个 token（先 client_secret_basic/none；401 时 Bearer 兜底）。
async fn revoke_token(
    server_name: &str,
    endpoint: &str,
    token: &str,
    token_type_hint: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    access_token: Option<&str>,
) -> Result<(), String> {
    let mut params = vec![
        ("token".to_string(), token.to_string()),
        ("token_type_hint".to_string(), token_type_hint.to_string()),
    ];

    let mut headers = vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )];
    if let (Some(client_id), Some(client_secret)) = (client_id, client_secret) {
        let basic = base64_standard(&format!(
            "{}:{}",
            percent_encode(client_id),
            percent_encode(client_secret)
        ));
        headers.push(("Authorization".to_string(), format!("Basic {basic}")));
    } else if let Some(client_id) = client_id {
        params.push(("client_id".to_string(), client_id.to_string()));
    } else {
        log_mcp_debug(
            server_name,
            &format!("No client_id available for {token_type_hint} revocation - server may reject"),
        );
    }

    let client = auth_client()?;
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut request = client.post(endpoint).body(body);
    for (k, v) in &headers {
        request = request.header(k, v);
    }
    let response = request.send().await.map_err(|e| format!("revoke POST failed: {e}"))?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(access_token) = access_token {
            log_mcp_debug(
                server_name,
                &format!("Got 401, retrying {token_type_hint} revocation with Bearer auth"),
            );
            let params = params
                .iter()
                .filter(|(k, _)| k != "client_id" && k != "client_secret")
                .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            let client = auth_client()?;
            let _ = client
                .post(endpoint)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Authorization", format!("Bearer {access_token}"))
                .body(params)
                .send()
                .await;
        }
    }
    log_mcp_debug(server_name, &format!("Successfully revoked {token_type_hint}"));
    Ok(())
}

fn base64_standard(input: &str) -> String {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 撤销服务器 tokens（先 refresh_token，后 access_token；随后清本地）。
pub async fn revoke_server_tokens(
    server_name: &str,
    config: &McpServerConfig,
    preserve_step_up_state: bool,
) {
    let storage = get_secure_storage();
    let server_key = get_server_key(server_name, config);
    let token_data = storage.mcp_oauth.get(&server_key).cloned();
    let Some(token_data) = token_data else {
        return;
    };

    if !token_data.access_token.is_empty() || token_data.refresh_token.is_some() {
        let as_url = token_data
            .discovery_state
            .as_ref()
            .and_then(|d| d.authorization_server_url.clone())
            .unwrap_or_else(|| config.url().unwrap_or("").to_string());
        let configured = config_sse_http_oauth(config).and_then(|o| o.auth_server_metadata_url.clone());

        match fetch_auth_server_metadata(server_name, &as_url, configured.as_deref()).await {
            Ok(Some(metadata)) => {
                if let Some(revocation_endpoint) = &metadata.revocation_endpoint {
                    // refresh_token 优先（长寿命凭据）
                    if let Some(refresh_token) = &token_data.refresh_token {
                        if let Err(e) = revoke_token(
                            server_name,
                            revocation_endpoint,
                            refresh_token,
                            "refresh_token",
                            token_data.client_id.as_deref(),
                            token_data.client_secret.as_deref(),
                            Some(&token_data.access_token),
                        )
                        .await
                        {
                            log_mcp_debug(server_name, &format!("Failed to revoke refresh token: {e}"));
                        }
                    }
                    if !token_data.access_token.is_empty() {
                        if let Err(e) = revoke_token(
                            server_name,
                            revocation_endpoint,
                            &token_data.access_token,
                            "access_token",
                            token_data.client_id.as_deref(),
                            token_data.client_secret.as_deref(),
                            Some(&token_data.access_token),
                        )
                        .await
                        {
                            log_mcp_debug(server_name, &format!("Failed to revoke access token: {e}"));
                        }
                    }
                } else {
                    log_mcp_debug(server_name, "Server does not support token revocation");
                }
            }
            Ok(None) => log_mcp_debug(server_name, "No OAuth metadata found"),
            Err(e) => log_mcp_debug(server_name, &format!("Failed to revoke tokens: {e}")),
        }
    } else {
        log_mcp_debug(server_name, "No tokens to revoke");
    }

    clear_server_tokens_from_storage(server_name, config);

    // 重认证时保留 step-up 状态（scope + discovery）
    if preserve_step_up_state && (token_data.step_up_scope.is_some() || token_data.discovery_state.is_some()) {
        let mut fresh = get_secure_storage();
        let entry = fresh.mcp_oauth.entry(server_key.clone()).or_default();
        entry.server_name = Some(server_name.to_string());
        entry.server_url = config.url().map(str::to_string);
        if let Some(scope) = &token_data.step_up_scope {
            entry.step_up_scope = Some(scope.clone());
        }
        if let Some(discovery) = &token_data.discovery_state {
            entry.discovery_state = Some(PersistedDiscoveryState {
                authorization_server_url: discovery.authorization_server_url.clone(),
                resource_metadata_url: discovery.resource_metadata_url.clone(),
            });
        }
        update_secure_storage(&fresh);
        log_mcp_debug(server_name, "Preserved step-up auth state across revocation");
    }
}

/// 清除本地存储的服务器 tokens。
pub fn clear_server_tokens_from_storage(server_name: &str, config: &McpServerConfig) {
    let mut storage = get_secure_storage();
    let server_key = get_server_key(server_name, config);
    if storage.mcp_oauth.remove(&server_key).is_some() {
        update_secure_storage(&storage);
        log_mcp_debug(server_name, "Cleared stored tokens");
    }
}

// ---------------------------------------------------------------------------
// performMCPOAuthFlow：授权码 + PKCE 主流程
// ---------------------------------------------------------------------------

/// 手动粘贴回调 URL 的提交句柄（无 localhost 环境）。
pub type SubmitCallbackUrl = Arc<dyn Fn(&str) + Send + Sync>;

/// 授权流程选项。
pub struct OAuthFlowOptions {
    pub skip_browser_open: bool,
    pub on_waiting_for_callback: Option<Arc<dyn Fn(SubmitCallbackUrl) + Send + Sync>>,
}

/// MCP OAuth 主流程（XAA 优先走 xaa.rs；否则 PKCE 授权码 + 回调服务器）。
pub async fn perform_mcp_o_auth_flow(
    server_name: &str,
    server_config: &ScopedMcpServerConfig,
    on_authorization_url: OnAuthorizationUrl,
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
    options: OAuthFlowOptions,
) -> Result<(), String> {
    // XAA 短路
    if config_sse_http_oauth(&server_config.config)
        .map(|o| o.xaa.unwrap_or(false))
        .unwrap_or(false)
    {
        if !crate::services::mcp::xaa_idp_login::is_xaa_enabled() {
            return Err(format!(
                "XAA is not enabled (set CLAUDE_CODE_ENABLE_XAA=1). Remove 'oauth.xaa' from server '{server_name}' to use the standard consent flow."
            ));
        }
        return crate::services::mcp::xaa::perform_mcp_xaa_auth(
            server_name,
            server_config,
            on_authorization_url,
            abort_signal,
            options.skip_browser_open,
        )
        .await;
    }

    // 缓存的 step-up scope / resource metadata URL（清 token 前先读出）
    let server_key = get_server_key(server_name, &server_config.config);
    let cached_entry = get_secure_storage().mcp_oauth.get(&server_key).cloned();
    let cached_step_up_scope = cached_entry.as_ref().and_then(|e| e.step_up_scope.clone());
    let cached_resource_metadata_url = cached_entry
        .as_ref()
        .and_then(|e| e.discovery_state.as_ref())
        .and_then(|d| d.resource_metadata_url.clone());

    clear_server_tokens_from_storage(server_name, &server_config.config);

    // 配置的 callback port 或找可用端口
    let configured_callback_port = config_sse_http_oauth(&server_config.config)
        .and_then(|o| o.callback_port.map(|p| p as u16));
    let port = match configured_callback_port {
        Some(port) => port,
        None => find_available_port().map_err(|_| {
            "No available port for OAuth redirect".to_string()
        })?,
    };
    let redirect_uri = build_redirect_uri(Some(port));
    log_mcp_debug(
        server_name,
        &format!(
            "Using redirect port: {port}{}",
            if configured_callback_port.is_some() { " (from config)" } else { "" }
        ),
    );

    let provider = Arc::new(ClaudeAuthProvider::new(
        server_name,
        server_config.clone(),
        Some(redirect_uri.clone()),
        true,
        Some(on_authorization_url.clone()),
        options.skip_browser_open,
    ));

    // 元数据（scope 信息）
    let metadata_result = fetch_auth_server_metadata(
        server_name,
        server_config.config.url().unwrap_or(""),
        config_sse_http_oauth(&server_config.config)
            .and_then(|o| o.auth_server_metadata_url.clone())
            .as_deref(),
    )
    .await;
    if let Ok(Some(metadata)) = &metadata_result {
        provider.set_metadata(metadata.clone()).await;
        log_mcp_debug(
            server_name,
            &format!(
                "Fetched OAuth metadata with scope: {}",
                get_scope_from_metadata(Some(metadata)).unwrap_or_else(|| "NONE".to_string())
            ),
        );
    } else if let Err(e) = &metadata_result {
        log_mcp_debug(server_name, &format!("Failed to fetch OAuth metadata: {e}"));
    }

    let oauth_state = provider.state().await;

    // DCR（元数据带 registration_endpoint 且无 client 信息时）
    let client_info = provider.client_information().await;
    let (client_id, client_secret) = match client_info {
        Some(info) => (info.0, info.1),
        None => {
            let metadata = metadata_result.clone().ok().flatten();
            let Some(registration_endpoint) =
                metadata.as_ref().and_then(|m| m.registration_endpoint.clone())
            else {
                return Err("No OAuth client information and no registration endpoint".to_string());
            };
            let metadata_doc = provider.client_metadata().await;
            let response = auth_client()?
                .post(&registration_endpoint)
                .header("Content-Type", "application/json")
                .json(&metadata_doc)
                .send()
                .await
                .map_err(|e| format!("DCR failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("DCR failed: HTTP {}", response.status()));
            }
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let registered_id = body
                .get("client_id")
                .and_then(Value::as_str)
                .ok_or("DCR response missing client_id")?
                .to_string();
            let registered_secret =
                body.get("client_secret").and_then(Value::as_str).map(str::to_string);
            let issuer = metadata.as_ref().and_then(|m| m.issuer.clone());
            provider
                .save_client_information(
                    &registered_id,
                    registered_secret.as_deref(),
                    issuer.as_deref(),
                )
                .await;
            (registered_id, registered_secret)
        }
    };

    // PKCE
    let code_verifier = generate_code_verifier();
    provider.save_code_verifier(&code_verifier).await;
    let challenge = code_challenge_s256(&code_verifier);

    let metadata = metadata_result.ok().flatten();
    let scope = cached_step_up_scope
        .clone()
        .or_else(|| get_scope_from_metadata(metadata.as_ref()));
    let authorization_endpoint = metadata
        .as_ref()
        .and_then(|m| m.authorization_endpoint.clone())
        .ok_or("No authorization endpoint in OAuth metadata")?;

    // 构造授权 URL
    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        authorization_endpoint,
        percent_encode(&client_id),
        percent_encode(&redirect_uri),
        percent_encode(&challenge),
        percent_encode(&oauth_state),
    );
    if let Some(scope) = &scope {
        auth_url.push_str(&format!("&scope={}", percent_encode(scope)));
    }
    if let Some(resource) = metadata.as_ref().and_then(|m| m.resource.as_deref()) {
        auth_url.push_str(&format!("&resource={}", percent_encode(resource)));
    }

    // redirectToAuthorization：通知 UI + 开浏览器
    provider.redirect_to_authorization(&auth_url).await?;

    // 回调等待：loopback TCP 服务器 + 5 分钟超时 + 手动粘贴支持
    let authorization_code = wait_for_callback(
        port,
        &oauth_state,
        options.on_waiting_for_callback.clone(),
        abort_signal,
    )
    .await?;

    // token exchange
    let token_endpoint = metadata
        .as_ref()
        .and_then(|m| m.token_endpoint.clone())
        .ok_or("No token endpoint in OAuth metadata")?;
    let mut params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), authorization_code),
        ("redirect_uri".to_string(), redirect_uri.clone()),
        ("client_id".to_string(), client_id.clone()),
        ("code_verifier".to_string(), code_verifier),
    ];
    if let Some(secret) = &client_secret {
        params.push(("client_secret".to_string(), secret.clone()));
    }
    let result = post_form(&token_endpoint, &params)
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;
    match classify_token_response(&result, true) {
        TokenResponse::Tokens(tokens) => {
            let issuer = metadata.as_ref().and_then(|m| m.issuer.clone());
            let tokens = OAuthTokens {
                issuer: tokens.issuer.or(issuer),
                ..tokens
            };
            provider.save_tokens(&tokens).await;
            // 持久化发现 state（刷新用）
            provider
                .save_discovery_state(
                    metadata.as_ref().and_then(|m| m.issuer.as_deref()),
                    cached_resource_metadata_url.as_deref(),
                )
                .await;
            Ok(())
        }
        TokenResponse::InvalidGrant => Err("OAuth error: invalid_grant".to_string()),
        TokenResponse::Transient => Err("OAuth error: server temporarily unavailable".to_string()),
        TokenResponse::Other => Err("token exchange failed".to_string()),
    }
}

/// 等待授权码回调（loopback HTTP 服务器；state 校验；5 分钟超时；
/// 支持手动粘贴回调 URL）。
async fn wait_for_callback(
    port: u16,
    expected_state: &str,
    on_waiting_for_callback: Option<Arc<dyn Fn(SubmitCallbackUrl) + Send + Sync>>,
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| {
            format!(
                "OAuth callback server failed: {e} — port {} may be in use",
                e
            )
        })?;

    let (code_tx, mut code_rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
    let expected_state = expected_state.to_string();
    let expected_state_for_server = expected_state.clone();

    // 回调 HTTP 服务器循环
    let code_tx_for_server = code_tx.clone();
    let server_task = tokio::spawn(async move {
        loop {
            let (mut stream, _addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            let mut buf = Vec::with_capacity(1024);
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            let request_line = request.lines().next().unwrap_or("");
            // 解析请求路径与 query
            let path = request_line.split_whitespace().nth(1).unwrap_or("");
            let (pathname, query) = path.split_once('?').unwrap_or((path, ""));

            if pathname != "/callback" {
                let _ = respond(&mut stream, 404, "Not Found").await;
                continue;
            }

            let params = parse_query(query);
            let error = params.get("error").cloned();
            let error_description = params.get("error_description").cloned();
            let code = params.get("code").cloned();
            let state = params.get("state").cloned();

            // state 校验（CSRF 防护）
            if error.is_none() && state.as_deref() != Some(&expected_state_for_server) {
                let _ = respond(
                    &mut stream,
                    400,
                    "<h1>Authentication Error</h1><p>Invalid state parameter. Please try again.</p><p>You can close this window.</p>",
                )
                .await;
                let _ = code_tx_for_server.send(Err(
                    "OAuth state mismatch - possible CSRF attack".to_string(),
                ));
                break;
            }

            if let Some(error) = error {
                let _ = respond(
                    &mut stream,
                    200,
                    &format!(
                        "<h1>Authentication Error</h1><p>{}</p><p>You can close this window.</p>",
                        escape_html(&error)
                    ),
                )
                .await;
                let mut message = format!("OAuth error: {error}");
                if let Some(description) = error_description {
                    message.push_str(&format!(" - {description}"));
                }
                let _ = code_tx_for_server.send(Err(message));
                break;
            }

            if let Some(code) = code {
                let _ = respond(
                    &mut stream,
                    200,
                    "<h1>Authentication Successful</h1><p>You can close this window. Return to Claude Code.</p>",
                )
                .await;
                let _ = code_tx_for_server.send(Ok(code));
                break;
            }
            let _ = respond(&mut stream, 400, "Bad Request").await;
        }
    });

    // 手动粘贴回调 URL 支持
    if let Some(on_waiting) = &on_waiting_for_callback {
        let code_tx = code_tx.clone();
        let expected_state = expected_state.clone();
        let callback = Arc::new(move |callback_url: &str| {
            let Some((_, query)) = callback_url.split_once('?') else {
                return;
            };
            let params = parse_query(query);
            let error = params.get("error").cloned();
            let code = params.get("code").cloned();
            let state = params.get("state").cloned();
            if error.is_none() && state.as_deref() != Some(&expected_state) {
                let _ = code_tx.send(Err("OAuth state mismatch - possible CSRF attack".to_string()));
                return;
            }
            if let Some(error) = error {
                let _ = code_tx.send(Err(format!("OAuth error: {error}")));
                return;
            }
            if let Some(code) = code {
                let _ = code_tx.send(Ok(code));
            }
        }) as SubmitCallbackUrl;
        on_waiting(callback);
    }

    // 等待结果 / 超时 / abort
    let result = tokio::select! {
        received = code_rx.recv() => {
            received.ok_or_else(|| "OAuth callback server closed".to_string())?
        }
        _ = tokio::time::sleep(Duration::from_secs(5 * 60)) => {
            Err("Authentication timeout".to_string())
        }
        _ = token_cancelled_or_abort(abort_signal) => {
            Err("Authentication was cancelled".to_string())
        }
    };
    server_task.abort();
    result
}

fn token_cancelled_or_abort(
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    match abort_signal {
        Some(token) => {
            let token = token.clone();
            Box::pin(async move {
                loop {
                    if token.aborted() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
        }
        None => Box::pin(std::future::pending()),
    }
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            params.insert(key.to_string(), percent_decode(value));
        }
    }
    params
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// ---------------------------------------------------------------------------
// MCP 客户端 secret 读写（--client-secret 配置）
// ---------------------------------------------------------------------------

/// 读 MCP 客户端 secret：环境变量 MCP_CLIENT_SECRET 或 TTY 提示。
pub async fn read_client_secret() -> Result<String, String> {
    if let Ok(secret) = std::env::var("MCP_CLIENT_SECRET") {
        return Ok(secret);
    }
    Err(
        "No TTY available to prompt for client secret. Set MCP_CLIENT_SECRET env var instead."
            .to_string(),
    )
}

/// 保存 MCP 客户端 secret 到安全存储。
pub fn save_mcp_client_secret(server_name: &str, config: &McpServerConfig, client_secret: &str) {
    let mut storage = get_secure_storage();
    let server_key = get_server_key(server_name, config);
    storage
        .mcp_oauth_client_config
        .insert(server_key, McpClientConfigEntry {
            client_secret: Some(client_secret.to_string()),
        });
    update_secure_storage(&storage);
}

/// 清除 MCP 客户端配置。
pub fn clear_mcp_client_config(server_name: &str, config: &McpServerConfig) {
    let mut storage = get_secure_storage();
    let server_key = get_server_key(server_name, config);
    if storage.mcp_oauth_client_config.remove(&server_key).is_some() {
        update_secure_storage(&storage);
    }
}

pub fn get_mcp_client_config_by_key(server_key: &str) -> Option<McpClientConfigEntry> {
    get_secure_storage().mcp_oauth_client_config.get(server_key).cloned()
}

/// 读 MCP 客户端配置（secret）。
pub fn get_mcp_client_config(server_name: &str, config: &McpServerConfig) -> Option<McpClientConfigEntry> {
    get_mcp_client_config_by_key(&get_server_key(server_name, config))
}

/// 传输层 Authorization 头 provider（client.rs 用）。
///
/// 同步读存储中的 access_token（不做主动刷新 —— 刷新在 tokens() 路径中
/// 完成；此处为 transport 的 per-request 注入口）。
pub fn auth_header_provider(
    server_name: &str,
    config: &ScopedMcpServerConfig,
) -> Option<Arc<dyn Fn() -> Option<String> + Send + Sync>> {
    let server_name = server_name.to_string();
    let config = config.config.clone();
    Some(Arc::new(move || {
        let server_key = get_server_key(&server_name, &config);
        get_secure_storage()
            .mcp_oauth
            .get(&server_key)
            .filter(|entry| !entry.access_token.is_empty())
            .map(|entry| entry.access_token.clone())
    }))
}





#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_key_is_stable_and_config_scoped() {
        let config = McpServerConfig::SSE(crate::services::mcp::types::McpSSEServerConfig {
            type_field: "sse".to_string(),
            url: "https://example.com/sse".to_string(),
            headers: None,
            headers_helper: None,
            oauth: None,
        });
        let key = get_server_key("srv", &config);
        let again = get_server_key("srv", &config);
        assert_eq!(key, again);
        assert!(key.starts_with("srv|"));
    }

    #[test]
    fn encodes_and_decodes_query_values() {
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_decode("a%20b%26c%3Dd"), "a b&c=d");
    }

    #[test]
    fn redacts_sensitive_params() {
        let url = "https://as/authorize?state=secret&code_challenge=abc&client_id=x";
        let redacted = redact_sensitive_url_params(url);
        assert!(redacted.contains("state=[REDACTED]"));
        assert!(redacted.contains("code_challenge=[REDACTED]"));
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("client_id=x"));
    }

    #[test]
    fn generates_pkce_material() {
        let verifier = generate_code_verifier();
        assert_eq!(verifier.len(), 64);
        let challenge = code_challenge_s256(&verifier);
        assert!(!challenge.is_empty());
    }

    #[test]
    fn builds_base64() {
        assert_eq!(base64_standard("abc"), "YWJj");
        assert_eq!(base64_standard("ab"), "YWI=");
    }

    #[test]
    fn classifies_token_responses() {
        let ok = TokenRequestResult {
            status: reqwest::StatusCode::OK,
            body: json!({"access_token": "a", "refresh_token": "r", "expires_in": 3600}),
        };
        match classify_token_response(&ok, true) {
            TokenResponse::Tokens(t) => {
                assert_eq!(t.access_token, "a");
                assert_eq!(t.refresh_token.as_deref(), Some("r"));
            }
            _ => panic!("expected tokens"),
        }

        let bad = TokenRequestResult {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: json!({"error": "invalid_grant"}),
        };
        assert!(matches!(
            classify_token_response(&bad, false),
            TokenResponse::InvalidGrant
        ));

        let slack = TokenRequestResult {
            status: reqwest::StatusCode::OK,
            body: json!({"error": "invalid_refresh_token"}),
        };
        assert!(matches!(
            classify_token_response(&slack, false),
            TokenResponse::InvalidGrant
        ));

        let transient = TokenRequestResult {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": "server_error"}),
        };
        assert!(matches!(
            classify_token_response(&transient, false),
            TokenResponse::Transient
        ));
    }
}
