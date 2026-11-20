//! xaa_idp_login.rs — XAA IdP 登录（重写自 `xaaIdpLogin.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `isXaaEnabled`：`CLAUDE_CODE_ENABLE_XAA` 真值。
//! - `get_xaa_idp_settings`：settings.xaaIdp（{issuer, clientId,
//!   callbackPort?}）。
//! - id_token 缓存：按 `issuerKey`（去尾斜杠 + 小写 host）存安全存储，
//!   过期前 60s 视为有效（ID_TOKEN_EXPIRY_BUFFER_S）。
//! - `discover_oidc`：OIDC discovery §4.1 —— issuer 路径**追加**
//!   `/.well-known/openid-configuration`（Azure AD/Okta/Keycloak 兼容），
//!   非 HTTPS token endpoint 拒绝。
//! - `acquire_idp_id_token`：标准 authorization_code + PKCE 流程
//!   （一次浏览器弹窗），id_token 解码 exp 做缓存 TTL。
//!
//! 已知偏差：
//! - `utils/browser.ts`（openBrowser）未转换：占位（依赖 on_authorization_url
//!   回调把 URL 交给 UI）。
//! - analytics 未转换 → debug 日志占位。
//! - JWT exp 只解码不验签（对齐 TS 的 `jwtExp` 语义 —— IdP 自己会验）。

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::services::mcp::auth::{
    get_secure_storage, update_secure_storage, XaaIdpConfigEntry, XaaIdpTokenEntry,
};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::log::log_mcp_debug;

/// id_token 有效期缓冲（过期前 60s 即视为不可用）。
const ID_TOKEN_EXPIRY_BUFFER_S: i64 = 60;

/// OIDC discovery 超时。
const IDP_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// IdP 登录流程超时（5 分钟）。
const IDP_LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// XAA 是否启用（CLAUDE_CODE_ENABLE_XAA）。
pub fn is_xaa_enabled() -> bool {
    is_env_truthy(std::env::var("CLAUDE_CODE_ENABLE_XAA").ok().as_deref())
}

/// settings.xaaIdp 配置（TS `XaaIdpSettings`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XaaIdpSettings {
    pub issuer: String,
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u32>,
}

/// 从合并 settings 读 xaaIdp（env-gated 字段；passthrough 保留原值）。
pub fn get_xaa_idp_settings() -> Option<XaaIdpSettings> {
    let settings = crate::utils::settings::settings::get_initial_settings();
    let raw = &settings.xaa_idp;
    let raw = raw.as_ref()?;
    let issuer = raw.get("issuer").and_then(serde_json::Value::as_str)?;
    let client_id = raw.get("clientId").and_then(serde_json::Value::as_str)?;
    Some(XaaIdpSettings {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        callback_port: raw
            .get("callbackPort")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32),
    })
}

/// issuer 规范化 key：去尾斜杠 + 小写 host（对齐 TS `issuerKey`）。
pub fn issuer_key(issuer: &str) -> String {
    match issuer.split_once("://") {
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
        None => issuer.trim_end_matches('/').to_string(),
    }
}

/// OIDC discovery 元数据子集（`/.well-known/openid-configuration`）。
#[derive(Debug, Clone, Deserialize)]
pub struct OpenIdProviderDiscoveryMetadata {
    #[allow(dead_code)]
    pub issuer: String,
    pub token_endpoint: String,
    #[allow(dead_code)]
    pub authorization_endpoint: String,
}

/// 读缓存的 id_token（缺失或临过期返回 None）。
pub fn get_cached_idp_id_token(idp_issuer: &str) -> Option<String> {
    let storage = get_secure_storage();
    let entry = storage.mcp_xaa_idp.get(&issuer_key(idp_issuer))?;
    let remaining_ms = entry.expires_at - now_ms();
    if remaining_ms <= ID_TOKEN_EXPIRY_BUFFER_S * 1000 {
        return None;
    }
    Some(entry.id_token.clone())
}

fn save_idp_id_token(idp_issuer: &str, id_token: &str, expires_at: i64) {
    let mut storage = get_secure_storage();
    storage.mcp_xaa_idp.insert(
        issuer_key(idp_issuer),
        XaaIdpTokenEntry {
            id_token: id_token.to_string(),
            expires_at,
        },
    );
    update_secure_storage(&storage);
}

/// 保存外部获得的 id_token（解 JWT 的 exp 做 TTL；无 exp 默认 1 小时）。
/// 返回计算出的 expiresAt。
pub fn save_idp_id_token_from_jwt(idp_issuer: &str, id_token: &str) -> i64 {
    let exp = jwt_exp(id_token);
    let expires_at = exp
        .map(|e| e * 1000)
        .unwrap_or_else(|| now_ms() + 3600 * 1000);
    save_idp_id_token(idp_issuer, id_token, expires_at);
    expires_at
}

/// 清除缓存的 id_token。
pub fn clear_idp_id_token(idp_issuer: &str) {
    let mut storage = get_secure_storage();
    if storage.mcp_xaa_idp.remove(&issuer_key(idp_issuer)).is_some() {
        update_secure_storage(&storage);
    }
}

/// 保存 IdP client secret（独立槽位，按 issuer）。
pub fn save_idp_client_secret(idp_issuer: &str, client_secret: &str) -> bool {
    let mut storage = get_secure_storage();
    storage.mcp_xaa_idp_config.insert(
        issuer_key(idp_issuer),
        XaaIdpConfigEntry {
            client_secret: Some(client_secret.to_string()),
        },
    );
    update_secure_storage(&storage);
    true
}

/// 读 IdP client secret。
pub fn get_idp_client_secret(idp_issuer: &str) -> Option<String> {
    let storage = get_secure_storage();
    storage
        .mcp_xaa_idp_config
        .get(&issuer_key(idp_issuer))
        .and_then(|e| e.client_secret.clone())
}

/// 移除 IdP client secret（`claude mcp xaa clear`）。
pub fn clear_idp_client_secret(idp_issuer: &str) {
    let mut storage = get_secure_storage();
    if storage
        .mcp_xaa_idp_config
        .remove(&issuer_key(idp_issuer))
        .is_some()
    {
        update_secure_storage(&storage);
    }
}

/// OIDC discovery：issuer 路径**追加** `/.well-known/openid-configuration`
/// （不覆盖 pathname —— Azure AD/Okta 自定义授权服务器/Keycloak realm 兼容）。
pub async fn discover_oidc(idp_issuer: &str) -> Result<OpenIdProviderDiscoveryMetadata, String> {
    let base = if idp_issuer.ends_with('/') {
        idp_issuer.to_string()
    } else {
        format!("{idp_issuer}/")
    };
    let url = format!("{base}.well-known/openid-configuration");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(IDP_REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("XAA IdP: OIDC discovery failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "XAA IdP: OIDC discovery failed: HTTP {} at {url}",
            response.status()
        ));
    }
    let metadata: OpenIdProviderDiscoveryMetadata = response
        .json()
        .await
        .map_err(|_| format!("XAA IdP: OIDC discovery returned non-JSON at {url} (captive portal or proxy?)"))?;
    if !metadata.token_endpoint.starts_with("https://") {
        return Err(format!(
            "XAA IdP: refusing non-HTTPS token endpoint: {}",
            metadata.token_endpoint
        ));
    }
    Ok(metadata)
}

/// 解 JWT 的 exp claim（不验签 —— IdP 在 token exchange 时自验）。
pub fn jwt_exp(id_token: &str) -> Option<i64> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    // base64url → bytes（无 padding 补齐）
    let payload_b64: String = parts[1]
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            _ => c,
        })
        .collect();
    let padding = match payload_b64.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    let bytes = base64_decode(&format!("{payload_b64}{padding}"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(serde_json::Value::as_i64)
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut values = Vec::with_capacity(input.len());
    for c in input.chars() {
        if c == '=' {
            break;
        }
        let idx = ALPHABET.iter().position(|&b| b as char == c)?;
        values.push(idx as u8);
    }
    let mut out = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let n = ((chunk[0] as u32) << 18)
            | ((chunk[1] as u32) << 12)
            | ((chunk.get(2).copied().unwrap_or(0) as u32) << 6)
            | (chunk.get(3).copied().unwrap_or(0) as u32);
        out.push((n >> 16 & 0xFF) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8 & 0xFF) as u8);
        }
        if chunk.len() > 3 {
            out.push((n & 0xFF) as u8);
        }
    }
    Some(out)
}

/// IdP 登录选项（TS `IdpLoginOptions` 的 Rust 形态）。
pub struct IdpLoginOptions {
    pub idp_issuer: String,
    pub idp_client_id: String,
    pub idp_client_secret: Option<String>,
    /// 预注册的固定回调端口（未设则随机）。
    pub callback_port: Option<u16>,
    /// 授权 URL 就绪时回调（先于/代替浏览器打开）。
    pub on_authorization_url: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
    pub skip_browser_open: bool,
}

/// 获取 id_token：缓存命中直接返回；否则跑 OIDC code+PKCE（一次浏览器）。
pub async fn acquire_idp_id_token(
    options: &IdpLoginOptions,
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
) -> Result<String, String> {
    if let Some(cached) = get_cached_idp_id_token(&options.idp_issuer) {
        return Ok(cached);
    }

    let oidc = discover_oidc(&options.idp_issuer).await?;

    // 回调端口
    let port = match options.callback_port {
        Some(port) => port,
        None => crate::services::mcp::oauth_port::find_available_port()?,
    };
    let redirect_uri = format!("http://localhost:{port}/callback");

    // PKCE
    let verifier = crate::services::mcp::auth::generate_code_verifier();
    let challenge = crate::services::mcp::auth::code_challenge_s256(&verifier);
    let state = crate::services::mcp::auth::generate_state();

    // 授权 URL
    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}&scope=openid%20profile%20email",
        oidc.authorization_endpoint,
        url_encode(&options.idp_client_id),
        url_encode(&redirect_uri),
        url_encode(&challenge),
        url_encode(&state)
    );
    if let Some(secret) = &options.idp_client_secret {
        auth_url.push_str(&format!("&client_secret={}", url_encode(secret)));
    }

    // 通知 UI（先于浏览器）
    if let Some(callback) = &options.on_authorization_url {
        callback(&auth_url);
    }
    if !options.skip_browser_open {
        // openBrowser 未转换：占位（UI 回调为真实通道）
        log_mcp_debug("xaa", "XAA IdP: browser open delegated to UI callback");
    }

    // 回调服务器
    let authorization_code = wait_for_idp_callback(
        port,
        &state,
        abort_signal,
    )
    .await?;

    // token exchange（含 id_token）
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(IDP_REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("XAA IdP: client build failed: {e}"))?;
    let mut params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), authorization_code),
        ("redirect_uri".to_string(), redirect_uri),
        ("client_id".to_string(), options.idp_client_id.clone()),
        ("code_verifier".to_string(), verifier),
    ];
    if let Some(secret) = &options.idp_client_secret {
        params.push(("client_secret".to_string(), secret.clone()));
    }
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = client
        .post(&oidc.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("XAA IdP: token endpoint failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "XAA IdP: token endpoint HTTP {}",
            response.status()
        ));
    }
    let token_response: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("XAA IdP: invalid token response: {e}"))?;
    let id_token = token_response
        .get("id_token")
        .and_then(serde_json::Value::as_str)
        .ok_or("XAA IdP: token response missing id_token")?
        .to_string();

    // 缓存（exp 来自 JWT）
    let expires_at = jwt_exp(&id_token)
        .map(|e| e * 1000)
        .unwrap_or_else(|| now_ms() + 3600 * 1000);
    save_idp_id_token(&options.idp_issuer, &id_token, expires_at);

    Ok(id_token)
}

/// IdP 回调等待（loopback HTTP；state 校验；5 分钟超时）。
async fn wait_for_idp_callback(
    port: u16,
    expected_state: &str,
    abort_signal: Option<&crate::utils::abort_controller::CancellationToken>,
) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("XAA IdP: callback server failed: {e}"))?;
    let (code_tx, mut code_rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
    let expected_state = expected_state.to_string();
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
            let path = request_line.split_whitespace().nth(1).unwrap_or("");
            let (pathname, query) = path.split_once('?').unwrap_or((path, ""));
            if pathname != "/callback" {
                continue;
            }
            let params = parse_query(query);
            let code = params.get("code").cloned();
            let state = params.get("state").cloned();
            if state.as_deref() != Some(&expected_state) {
                let _ = code_tx.send(Err("OAuth state mismatch - possible CSRF attack".to_string()));
                break;
            }
            if let Some(code) = code {
                let _ = code_tx.send(Ok(code));
                break;
            }
        }
    });

    let result = tokio::select! {
        received = code_rx.recv() => received.ok_or_else(|| "XAA IdP: callback closed".to_string())?,
        _ = tokio::time::sleep(Duration::from_millis(IDP_LOGIN_TIMEOUT_MS)) => {
            Err("XAA IdP: login timeout".to_string())
        }
        _ = idp_abort(abort_signal) => Err("XAA IdP: login cancelled".to_string()),
    };
    server_task.abort();
    result
}

fn idp_abort(
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

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            params.insert(key.to_string(), url_decode(value));
        }
    }
    params
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

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(bytes[i]),
                }
            }
            b'+' => out.push(b' '),
            _ => out.push(bytes[i]),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_keys_normalize() {
        assert_eq!(issuer_key("https://IdP.example.com/"), "https://idp.example.com");
        assert_eq!(
            issuer_key("https://idp.example.com/realms/x/"),
            "https://idp.example.com/realms/x"
        );
    }

    #[test]
    fn reads_jwt_exp() {
        // {"exp": 1234567890} base64url
        let token = format!("header.{}.signature", "eyJleHAiOjEyMzQ1Njc4OTB9");
        assert_eq!(jwt_exp(&token), Some(1234567890));
        assert_eq!(jwt_exp("garbage"), None);
    }

    #[test]
    fn encodes_and_decodes_query() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_decode("a%20b"), "a b");
    }
}
