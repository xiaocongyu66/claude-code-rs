//! xaa.rs — Cross-App Access (XAA / SEP-990)（重写自 `xaa.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - Layer 2 发现：RFC 9728 PRM（`/.well-known/oauth-protected-resource`）
//!   + RFC 8414 AS metadata，均带 issuer/resource 一致性校验（mix-up 防护）
//!   与 HTTPS token endpoint 强制。
//! - Layer 2 交换：RFC 8693 token exchange（id_token → ID-JAG）与
//!   RFC 7523 JWT bearer（ID-JAG → access_token）。
//! - Layer 3 编排 `perform_cross_app_access`：PRM → AS metadata（按
//!   grant_types_supported 选择支持 jwt-bearer 的 AS）→ 双交换。
//! - `XaaTokenExchangeError` 携带 `should_clear_id_token`（4xx/坏响应清，
//!   5xx/网络错误保留）。
//! - `perform_mcp_xaa_auth`：auth.rs 的 XAA 短路路径 —— 取 id_token（缓存
//!   或 xaaIdpLogin 的一次浏览器登录）→ 交换 → 直写安全存储。
//!
//! 已知偏差：敏感字段在日志中脱敏（与 TS redactTokens 等价，用正则）。

use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use thiserror::Error;

use crate::services::mcp::auth::{
    get_secure_storage, update_secure_storage, OAuthTokens, OnAuthorizationUrl, PersistedDiscoveryState,
};
use crate::services::mcp::types::ScopedMcpServerConfig;
use crate::utils::log::log_mcp_debug;

const XAA_REQUEST_TIMEOUT_MS: u64 = 30_000;

const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const ID_JAG_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id-jag";
const ID_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id_token";

/// IdP token 交换失败（携带是否清 id_token 的语义信号）。
#[derive(Debug, Error)]
#[error("XAA: {message}")]
pub struct XaaTokenExchangeError {
    pub message: String,
    pub should_clear_id_token: bool,
}

/// XAA 流程错误（业务错误字符串或交换错误）。
#[derive(Debug, Error)]
pub enum XaaError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Exchange(#[from] XaaTokenExchangeError),
}

impl From<String> for XaaError {
    fn from(message: String) -> Self {
        XaaError::Message(message)
    }
}

/// 敏感 token 字段的日志脱敏正则（对齐 TS `SENSITIVE_TOKEN_RE`）。
static SENSITIVE_TOKEN_RE: Lazy<Option<Regex>> = Lazy::new(|| {
    Regex::new(r#""(access_token|refresh_token|id_token|assertion|subject_token|client_secret)"\s*:\s*"[^"]*""#).ok()
});

fn redact_tokens(raw: &str) -> String {
    match SENSITIVE_TOKEN_RE.as_ref() {
        Some(re) => re.replace_all(raw, r#""$1":"[REDACTED]""#).into_owned(),
        None => raw.to_string(),
    }
}

/// 规范化 URL（RFC 3986 §6.2.2 近似：去尾斜杠 + 小写 scheme/host）。
fn normalize_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = host.to_ascii_lowercase();
            let path = path.trim_end_matches('/');
            if path.is_empty() {
                format!("{}://{host}", scheme.to_ascii_lowercase())
            } else {
                format!("{}://{host}/{path}", scheme.to_ascii_lowercase())
            }
        }
        None => url.trim_end_matches('/').to_string(),
    }
}

/// RFC 8414 §3.3 / RFC 9728 §3.3 标识符比较。
fn identifiers_match(expected: &str, actual: &str) -> bool {
    normalize_url(expected) == normalize_url(actual)
}

fn xaa_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(XAA_REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

// ---------------------------------------------------------------------------
// Layer 2: 发现
// ---------------------------------------------------------------------------

/// PRM（RFC 9728）子集。
#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadataRaw {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default, rename = "authorization_servers")]
    authorization_servers: Vec<String>,
}

/// PRM 发现 + resource 一致性校验。
pub async fn discover_protected_resource(
    server_url: &str,
) -> Result<(String, Vec<String>), XaaError> {
    let trimmed = server_url.trim_end_matches('/');
    let url = format!("{trimmed}/.well-known/oauth-protected-resource");
    let response = xaa_client()?
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| XaaError::Message(format!("XAA: PRM discovery failed: {e}")))?;
    if !response.status().is_success() {
        return Err(XaaError::Message(format!(
            "XAA: PRM discovery failed: HTTP {} from {url}",
            response.status()
        )));
    }
    let prm: ProtectedResourceMetadataRaw = response
        .json()
        .await
        .map_err(|e| XaaError::Message(format!("XAA: PRM discovery failed: {e}")))?;

    let resource = prm
        .resource
        .ok_or_else(|| XaaError::Message("XAA: PRM discovery failed: PRM missing resource or authorization_servers".to_string()))?;
    if prm.authorization_servers.is_empty() {
        return Err(XaaError::Message(
            "XAA: PRM discovery failed: PRM missing resource or authorization_servers".to_string(),
        ));
    }
    if !identifiers_match(server_url, &resource) {
        return Err(XaaError::Message(format!(
            "XAA: PRM discovery failed: PRM resource mismatch: expected {server_url}, got {resource}"
        )));
    }
    Ok((resource, prm.authorization_servers))
}

/// AS metadata 子集（RFC 8414 / OIDC）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// AS metadata 发现 + issuer 一致性校验 + HTTPS token endpoint 强制。
pub async fn discover_authorization_server(
    as_url: &str,
) -> Result<AuthorizationServerMetadata, XaaError> {
    let trimmed = as_url.trim_end_matches('/');
    let url = format!("{trimmed}/.well-known/oauth-authorization-server");
    let response = xaa_client()?
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| XaaError::Message(format!("XAA: AS metadata discovery failed: {e}")))?;
    if !response.status().is_success() {
        return Err(XaaError::Message(format!(
            "XAA: AS metadata discovery failed: HTTP {} from {url}",
            response.status()
        )));
    }
    let meta: AuthorizationServerMetadata = response
        .json()
        .await
        .map_err(|e| XaaError::Message(format!("XAA: AS metadata discovery failed: {e}")))?;
    if meta.issuer.is_empty() || meta.token_endpoint.is_empty() {
        return Err(XaaError::Message(format!(
            "XAA: AS metadata discovery failed: no valid metadata at {as_url}"
        )));
    }
    if !identifiers_match(as_url, &meta.issuer) {
        return Err(XaaError::Message(format!(
            "XAA: AS metadata discovery failed: issuer mismatch: expected {as_url}, got {}",
            meta.issuer
        )));
    }
    if !meta.token_endpoint.starts_with("https://") {
        return Err(XaaError::Message(format!(
            "XAA: refusing non-HTTPS token endpoint: {}",
            meta.token_endpoint
        )));
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Layer 2: 交换
// ---------------------------------------------------------------------------

/// RFC 8693 token exchange 响应。
#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: Option<String>,
    issued_token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// RFC 8693 token exchange 结果。
pub struct JwtAuthGrantResult {
    pub jwt_auth_grant: String,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

/// RFC 8693：id_token → ID-JAG。
pub async fn request_jwt_authorization_grant(
    token_endpoint: &str,
    audience: &str,
    resource: &str,
    id_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
    scope: Option<&str>,
) -> Result<JwtAuthGrantResult, XaaTokenExchangeError> {
    let mut params = vec![
        ("grant_type".to_string(), TOKEN_EXCHANGE_GRANT.to_string()),
        ("requested_token_type".to_string(), ID_JAG_TOKEN_TYPE.to_string()),
        ("audience".to_string(), audience.to_string()),
        ("resource".to_string(), resource.to_string()),
        ("subject_token".to_string(), id_token.to_string()),
        ("subject_token_type".to_string(), ID_TOKEN_TYPE.to_string()),
        ("client_id".to_string(), client_id.to_string()),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret".to_string(), secret.to_string()));
    }
    if let Some(scope) = scope {
        params.push(("scope".to_string(), scope.to_string()));
    }

    let response = form_post(token_endpoint, &params).await.map_err(|e| {
        XaaTokenExchangeError {
            message: format!("XAA: token exchange failed: {e}"),
            should_clear_id_token: false,
        }
    })?;
    if response.status.as_u16() >= 400 {
        let body = redact_tokens(&response.body_text).chars().take(200).collect::<String>();
        return Err(XaaTokenExchangeError {
            message: format!("XAA: token exchange failed: HTTP {}: {body}", response.status),
            // 4xx → id_token 被拒（invalid_grant 等），清缓存；5xx → IdP 故障，保留
            should_clear_id_token: response.status.as_u16() < 500,
        });
    }

    let parsed: TokenExchangeResponse = match serde_json::from_str(&response.body_text) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err(XaaTokenExchangeError {
                message: format!(
                    "XAA: token exchange returned non-JSON (captive portal?) at {token_endpoint}"
                ),
                should_clear_id_token: false,
            });
        }
    };
    let Some(access_token) = parsed.access_token else {
        return Err(XaaTokenExchangeError {
            message: "XAA: token exchange response missing access_token".to_string(),
            should_clear_id_token: true,
        });
    };
    if parsed.issued_token_type.as_deref() != Some(ID_JAG_TOKEN_TYPE) {
        return Err(XaaTokenExchangeError {
            message: format!(
                "XAA: token exchange returned unexpected issued_token_type: {:?}",
                parsed.issued_token_type
            ),
            should_clear_id_token: true,
        });
    }
    Ok(JwtAuthGrantResult {
        jwt_auth_grant: access_token,
        expires_in: parsed.expires_in,
        scope: parsed.scope,
    })
}

/// RFC 7523 jwt-bearer 响应。
#[derive(Debug, Deserialize)]
struct JwtBearerResponse {
    access_token: String,
    #[serde(default = "default_bearer")]
    token_type: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

fn default_bearer() -> String {
    "Bearer".to_string()
}

/// XAA token 结果（+ AS issuer URL）。
pub struct XaaResult {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
    /// PRM 发现的 AS issuer：刷新/撤销要用它定位 token endpoint。
    pub authorization_server_url: String,
}

/// RFC 7523：ID-JAG → access_token。
pub async fn exchange_jwt_auth_grant(
    token_endpoint: &str,
    assertion: &str,
    client_id: &str,
    client_secret: &str,
    auth_method: &str,
    scope: Option<&str>,
) -> Result<XaaResult, String> {
    let mut params = vec![
        ("grant_type".to_string(), JWT_BEARER_GRANT.to_string()),
        ("assertion".to_string(), assertion.to_string()),
    ];
    if let Some(scope) = scope {
        params.push(("scope".to_string(), scope.to_string()));
    }

    let mut basic_auth = None;
    if auth_method == "client_secret_basic" {
        let basic = base64_standard(&format!("{}:{}", url_encode(client_id), url_encode(client_secret)));
        basic_auth = Some(format!("Basic {basic}"));
    } else {
        params.push(("client_id".to_string(), client_id.to_string()));
        params.push(("client_secret".to_string(), client_secret.to_string()));
    }

    let client = xaa_client()?;
    let mut req = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded");
    if let Some(basic) = &basic_auth {
        req = req.header("Authorization", basic);
    }
    let response = req.body(form_body(&params))
        .send()
        .await
        .map_err(|e| format!("XAA: jwt-bearer grant failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = redact_tokens(&response.text().await.unwrap_or_default())
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("XAA: jwt-bearer grant failed: HTTP {status}: {body}"));
    }
    let body_text = response
        .text()
        .await
        .map_err(|e| format!("XAA: jwt-bearer grant failed: {e}"))?;
    let parsed: JwtBearerResponse = serde_json::from_str(&body_text)
        .map_err(|e| format!(
            "XAA: jwt-bearer response did not match expected shape: {}",
            redact_tokens(&body_text)
        ))?;
    Ok(XaaResult {
        access_token: parsed.access_token,
        token_type: parsed.token_type,
        expires_in: parsed.expires_in,
        scope: parsed.scope,
        refresh_token: parsed.refresh_token,
        authorization_server_url: String::new(), // 编排器回填
    })
}

// ---------------------------------------------------------------------------
// Layer 3: 编排
// ---------------------------------------------------------------------------

/// XAA 配置（IdP + AS 凭据）。
pub struct XaaConfig {
    pub client_id: String,
    pub client_secret: String,
    pub idp_client_id: String,
    pub idp_client_secret: Option<String>,
    pub idp_id_token: String,
    pub idp_token_endpoint: String,
}

/// 完整 XAA 流程：PRM → AS metadata → token-exchange → jwt-bearer。
pub async fn perform_cross_app_access(
    server_url: &str,
    config: &XaaConfig,
    server_name: &str,
    _abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
) -> Result<XaaResult, XaaError> {
    log_mcp_debug(server_name, &format!("XAA: discovering PRM for {server_url}"));
    let (resource, authorization_servers) = discover_protected_resource(server_url).await?;
    log_mcp_debug(
        server_name,
        &format!(
            "XAA: discovered resource={resource} ASes=[{}]",
            authorization_servers.join(", ")
        ),
    );

    // 逐个 AS：未显式排除 jwt-bearer 即尝试（grant_types_supported 可选）
    let mut as_meta: Option<AuthorizationServerMetadata> = None;
    let mut as_errors: Vec<String> = Vec::new();
    for as_url in &authorization_servers {
        match discover_authorization_server(as_url).await {
            Ok(candidate) => {
                if let Some(grants) = &candidate.grant_types_supported {
                    if !grants.iter().any(|g| g == JWT_BEARER_GRANT) {
                        as_errors.push(format!(
                            "{as_url}: does not advertise jwt-bearer grant (supported: {})",
                            grants.join(", ")
                        ));
                        continue;
                    }
                }
                as_meta = Some(candidate);
                break;
            }
            Err(e) => {
                as_errors.push(format!("{as_url}: {e}"));
            }
        }
    }
    let as_meta = as_meta.ok_or_else(|| {
        XaaError::Message(format!(
            "XAA: no authorization server supports jwt-bearer. Tried: {}",
            as_errors.join("; ")
        ))
    })?;

    // auth method：AS 明确不支持 basic 且支持 post 时用 post，否则 basic
    let auth_method = match &as_meta.token_endpoint_auth_methods_supported {
        Some(methods)
            if !methods.iter().any(|m| m == "client_secret_basic")
                && methods.iter().any(|m| m == "client_secret_post") =>
        {
            "client_secret_post"
        }
        _ => "client_secret_basic",
    };
    log_mcp_debug(
        server_name,
        &format!(
            "XAA: AS issuer={} token_endpoint={} auth_method={auth_method}",
            as_meta.issuer, as_meta.token_endpoint
        ),
    );

    log_mcp_debug(server_name, "XAA: exchanging id_token for ID-JAG at IdP");
    let jag = request_jwt_authorization_grant(
        &config.idp_token_endpoint,
        &as_meta.issuer,
        &resource,
        &config.idp_id_token,
        &config.idp_client_id,
        config.idp_client_secret.as_deref(),
        None,
    )
    .await?;
    log_mcp_debug(server_name, "XAA: ID-JAG obtained");

    log_mcp_debug(server_name, "XAA: exchanging ID-JAG for access_token at AS");
    let mut tokens = exchange_jwt_auth_grant(
        &as_meta.token_endpoint,
        &jag.jwt_auth_grant,
        &config.client_id,
        &config.client_secret,
        auth_method,
        jag.scope.as_deref(),
    )
    .await?;
    tokens.authorization_server_url = as_meta.issuer.clone();
    log_mcp_debug(server_name, "XAA: access_token obtained");
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// performMCPXaaAuth（auth.rs 的 XAA 短路路径）
// ---------------------------------------------------------------------------

/// XAA 失败阶段（analytics 占位 + 日志）。
#[derive(Debug, Clone, Copy)]
enum XaaFailureStage {
    IdpLogin,
    Discovery,
    TokenExchange,
    JwtBearer,
}

impl XaaFailureStage {
    fn as_str(&self) -> &'static str {
        match self {
            XaaFailureStage::IdpLogin => "idp_login",
            XaaFailureStage::Discovery => "discovery",
            XaaFailureStage::TokenExchange => "token_exchange",
            XaaFailureStage::JwtBearer => "jwt_bearer",
        }
    }
}

/// XAA 完整认证：id_token（缓存或一次 IdP 登录）→ 交换 → 存 token。
pub async fn perform_mcp_xaa_auth(
    server_name: &str,
    server_config: &ScopedMcpServerConfig,
    on_authorization_url: OnAuthorizationUrl,
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
    skip_browser_open: bool,
) -> Result<(), String> {
    let oauth = match &server_config.config {
        crate::services::mcp::types::McpServerConfig::SSE(c) => c.oauth.as_ref(),
        crate::services::mcp::types::McpServerConfig::HTTP(c) => c.oauth.as_ref(),
        _ => None,
    };
    let Some(oauth) = oauth else {
        return Err("XAA: oauth.xaa must be set".to_string()); // 由调用方保证
    };
    if !oauth.xaa.unwrap_or(false) {
        return Err("XAA: oauth.xaa must be set".to_string());
    }

    // IdP 配置来自 user 级 settings（settings.xaaIdp）
    let Some(idp) = crate::services::mcp::xaa_idp_login::get_xaa_idp_settings() else {
        return Err(
            "XAA: no IdP connection configured. Run 'claude mcp xaa setup --issuer <url> --client-id <id> --client-secret' to configure.".to_string()
        );
    };

    let Some(client_id) = oauth.client_id.as_ref() else {
        return Err(format!(
            "XAA: server '{server_name}' needs an AS client_id. Re-add with --client-id."
        ));
    };
    let server_key = crate::services::mcp::auth::get_server_key(server_name, &server_config.config);
    let client_secret = crate::services::mcp::auth::get_mcp_client_config_by_key(&server_key)
        .and_then(|c| c.client_secret);
    let Some(client_secret) = client_secret else {
        return Err(format!(
            "XAA: AS client secret not found for '{server_name}'. Re-add with --client-secret."
        ));
    };

    log_mcp_debug(server_name, "XAA: starting cross-app access flow");

    // IdP client secret 在独立槽位（按 IdP issuer），可选
    let idp_client_secret =
        crate::services::mcp::xaa_idp_login::get_idp_client_secret(&idp.issuer);
    let id_token_cache_hit =
        crate::services::mcp::xaa_idp_login::get_cached_idp_id_token(&idp.issuer).is_some();

    let mut failure_stage = XaaFailureStage::IdpLogin;
    let result = async {
        // 1) id_token（缓存或一次 IdP 浏览器登录）
        let id_token = crate::services::mcp::xaa_idp_login::acquire_idp_id_token(
            &crate::services::mcp::xaa_idp_login::IdpLoginOptions {
                idp_issuer: idp.issuer.clone(),
                idp_client_id: idp.client_id.clone(),
                idp_client_secret: idp_client_secret.clone(),
                callback_port: idp.callback_port.map(|p| p as u16),
                on_authorization_url: Some(on_authorization_url.clone()),
                skip_browser_open,
            },
            abort_signal,
        )
        .await?;

        // 2) OIDC 发现（IdP token endpoint）
        failure_stage = XaaFailureStage::Discovery;
        let oidc = crate::services::mcp::xaa_idp_login::discover_oidc(&idp.issuer).await?;

        // 3) 双交换
        failure_stage = XaaFailureStage::TokenExchange;
        let tokens = perform_cross_app_access(
            server_config.config.url().unwrap_or(""),
            &XaaConfig {
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                idp_client_id: idp.client_id.clone(),
                idp_client_secret,
                idp_id_token: id_token,
                idp_token_endpoint: oidc.token_endpoint,
            },
            server_name,
            abort_signal,
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            // 归因修正（对齐 TS 的阶段判定）
            if msg.contains("PRM discovery failed")
                || msg.contains("AS metadata discovery failed")
                || msg.contains("no authorization server supports jwt-bearer")
            {
                failure_stage = XaaFailureStage::Discovery;
            } else if msg.contains("jwt-bearer") {
                failure_stage = XaaFailureStage::JwtBearer;
            }
            // id_token 清除语义交给交换错误
            let should_clear = matches!(&e, XaaError::Exchange(xaa_err) if xaa_err.should_clear_id_token)
                || (matches!(&e, XaaError::Message(_))
                    && (msg.contains("token exchange failed") || msg.contains("jwt-bearer")));
            if should_clear {
                crate::services::mcp::xaa_idp_login::clear_idp_id_token(&idp.issuer);
                log_mcp_debug(server_name, "XAA: cleared cached id_token after failure");
            }
            XaaError::Message(msg)
        })?;

        // 4) 直写安全存储（saveTokens 不写 clientId/clientSecret）
        let mut storage = get_secure_storage();
        let entry = storage.mcp_oauth.entry(server_key.clone()).or_default();
        entry.server_name = Some(server_name.to_string());
        entry.server_url = server_config.config.url().map(str::to_string);
        entry.access_token = tokens.access_token.clone();
        // AS 可能省略 refresh_token：保留已有
        if tokens.refresh_token.is_some() {
            entry.refresh_token = tokens.refresh_token.clone();
        }
        entry.expires_at =
            crate::services::mcp::auth::now_ms() + tokens.expires_in.unwrap_or(3600) * 1000;
        entry.scope = tokens.scope.clone();
        entry.client_id = Some(client_id.clone());
        entry.client_secret = Some(client_secret);
        entry.discovery_state = Some(PersistedDiscoveryState {
            authorization_server_url: Some(tokens.authorization_server_url.clone()),
            resource_metadata_url: None,
        });
        update_secure_storage(&storage);

        log_mcp_debug(server_name, "XAA: tokens saved");
        Ok::<(), XaaError>(())
    }
    .await;

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            log_mcp_debug(
                server_name,
                &format!("XAA flow failed at {}: {e}", failure_stage.as_str()),
            );
            Err(e.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP / 编码工具
// ---------------------------------------------------------------------------

struct FormResponse {
    status: reqwest::StatusCode,
    body_text: String,
}

async fn form_post(url: &str, params: &[(String, String)]) -> Result<FormResponse, String> {
    let client = xaa_client()?;
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(params))
        .send()
        .await
        .map_err(|e| format!("HTTP POST {url} failed: {e}"))?;
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    Ok(FormResponse { status, body_text })
}

fn form_body(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn url_encode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls_for_identifier_comparison() {
        assert_eq!(normalize_url("https://Example.COM/mcp/"), "https://example.com/mcp");
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
        assert!(identifiers_match("https://example.com/mcp/", "https://example.com/mcp"));
    }

    #[test]
    fn redacts_token_fields() {
        let raw = r#"{"access_token":"abc","refresh_token":"xyz","other":1}"#;
        let redacted = redact_tokens(raw);
        assert!(redacted.contains("\"access_token\":\"[REDACTED]\""));
        assert!(!redacted.contains("abc"));
    }

    #[test]
    fn encodes_bearer_basic_auth() {
        let basic = base64_standard(&format!("{}:{}", url_encode("a b"), url_encode("c:d")));
        assert_eq!(basic, "YSUyMEI6Yzpk");
    }
}
