//! tunnels.rs — MCP tunnels 云 API 客户端（PARTIAL SUBSET）
//! （重写自 `tunnels.ts`）。
//!
//! 官方锚点（docs/reverse-284/decompiled/0032-chunk-dyqmyxqn）：
//! model-provider/anthropic.js — 每次 /v1/tunnels* 调用注入版本头
//! `anthropic-beta: mcp-tunnels-2026-06-22`：
//!   POST /v1/tunnels?beta=true                  (create)
//!   GET  /v1/tunnels/{id}?beta=true             (retrieve)
//!   POST /v1/tunnels/{id}/certificates?beta=true (certificates.create)
//!   GET  /v1/tunnels/{tunnel_id}/certificates/{id}?beta=true (certificates.retrieve)
//!
//! 已实现操作：create / retrieve / certificates.create /
//! certificates.retrieve（对齐 TS 的 PARTIAL SUBSET；archive、
//! reveal_token、rotate_token、certificates.list、certificates.archive 未实现）。
//!
//! 已知偏差：
//! - TS 用可注入 fetchFn；Rust 用 reqwest（Cargo.toml 已有）。
//! - feature('MCP_TUNNELS') 构建期门控：Rust 无 bun:bundle，门控为
//!   `FEATURE_MCP_TUNNELS` 环境变量 + 默认 false（与 DEFAULT_BUILD_FEATURES
//!   不含 MCP_TUNNELS 一致）。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 版本头（每次 /v1/tunnels* 调用携带）。
pub const MCP_TUNNELS_BETA_HEADER: &str = "mcp-tunnels-2026-06-22";

/// tunnel 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelStatus {
    Pending,
    Active,
    Disconnected,
    Closed,
}

/// tunnel 资源（实现子集字段）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tunnel {
    pub id: String,
    #[serde(default)]
    #[allow(dead_code)]
    object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub status: TunnelStatus,
    /// 服务器分配的连接 URL（ws(s)://）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// tunnel 证书（mTLS 材料）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TunnelCertificate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel_id: Option<String>,
    /// PEM 编码客户端证书。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListTunnelsResponseRaw {
    #[serde(default)]
    data: Vec<Tunnel>,
    #[serde(default)]
    has_more: Option<bool>,
}

/// 列表响应（对齐 TS `ListTunnelsResponse`）。
#[derive(Debug, Clone)]
pub struct ListTunnelsResponse {
    pub data: Vec<Tunnel>,
    pub has_more: bool,
}

/// 创建 tunnel 参数。
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateTunnelParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 请求有效期（秒）；服务器可 clamp。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

/// 创建证书参数。
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateTunnelCertificateParams {
    pub tunnel_id: String,
    /// PEM CSR —— 提供时服务器签发证书。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// 客户端选项。
pub struct TunnelsClientOptions {
    /// API base（默认第一方 BASE_API_URL）。
    pub base_url: Option<String>,
    /// Bearer 凭据（无默认解析：由调用方持有）。
    pub access_token: Option<String>,
    /// 额外 anthropic-beta 段。
    pub betas: Vec<String>,
}

impl Default for TunnelsClientOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            access_token: None,
            betas: Vec::new(),
        }
    }
}

/// tunnels 功能未启用（对齐 TS `McpTunnelsDisabledError`）。
#[derive(Debug, thiserror::Error)]
#[error("MCP tunnels feature is disabled (enable with FEATURE_MCP_TUNNELS=1)")]
pub struct McpTunnelsDisabledError;

/// 功能门控：MCP_TUNNELS 不在默认构建 features 中（对齐
/// `feature('MCP_TUNNELS')`：默认 false）。
pub fn is_mcp_tunnels_enabled() -> bool {
    std::env::var("FEATURE_MCP_TUNNELS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

struct TunnelsClient {
    base_url: String,
    access_token: Option<String>,
    betas: Vec<String>,
    http: reqwest::Client,
}

impl TunnelsClient {
    fn new(options: TunnelsClientOptions) -> Result<Self, String> {
        let base_url = match options.base_url {
            Some(base) => base,
            None => crate::constants::oauth::get_oauth_config()
                .map(|c| c.base_api_url)
                .map_err(|e| format!("oauth config: {e}"))?,
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("client build: {e}"))?;
        Ok(Self {
            base_url,
            access_token: options.access_token,
            betas: options.betas,
            http,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.base_url);
        let mut beta_values = vec![MCP_TUNNELS_BETA_HEADER.to_string()];
        beta_values.extend(self.betas.iter().cloned());
        let beta_header = beta_values.join(",");
        let mut request = self
            .http
            .request(method, &url)
            .header("anthropic-beta", beta_header)
            .query(&[("beta", "true")]);
        if let Some(token) = &self.access_token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        request
    }

    /// POST /v1/tunnels — 创建 tunnel。
    pub async fn create_tunnel(&self, params: &CreateTunnelParams) -> Result<Tunnel, String> {
        let response = self
            .request(reqwest::Method::POST, "/v1/tunnels")
            .json(params)
            .send()
            .await
            .map_err(|e| format!("POST /v1/tunnels failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .json::<Tunnel>()
            .await
            .map_err(|e| format!("invalid tunnel response: {e}"))
    }

    /// GET /v1/tunnels — 列表（TS 的 MISSING OPERATIONS 之外保留，
    /// 供 /mcp 菜单展示）。
    pub async fn list_tunnels(&self) -> Result<ListTunnelsResponse, String> {
        let response = self
            .request(reqwest::Method::GET, "/v1/tunnels")
            .send()
            .await
            .map_err(|e| format!("GET /v1/tunnels failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let raw: ListTunnelsResponseRaw = response
            .json()
            .await
            .map_err(|e| format!("invalid list response: {e}"))?;
        Ok(ListTunnelsResponse {
            data: raw.data,
            has_more: raw.has_more.unwrap_or(false),
        })
    }

    /// GET /v1/tunnels/{id} — 查询 tunnel。
    pub async fn retrieve_tunnel(&self, tunnel_id: &str) -> Result<Tunnel, String> {
        let path = format!("/v1/tunnels/{tunnel_id}");
        let response = self
            .request(reqwest::Method::GET, &path)
            .send()
            .await
            .map_err(|e| format!("GET {path} failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .json::<Tunnel>()
            .await
            .map_err(|e| format!("invalid tunnel response: {e}"))
    }

    /// POST /v1/tunnels/{id}/certificates — 创建证书。
    pub async fn create_tunnel_certificate(
        &self,
        params: &CreateTunnelCertificateParams,
    ) -> Result<TunnelCertificate, String> {
        let path = format!("/v1/tunnels/{}/certificates", params.tunnel_id);
        let response = self
            .request(reqwest::Method::POST, &path)
            .json(params)
            .send()
            .await
            .map_err(|e| format!("POST {path} failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .json::<TunnelCertificate>()
            .await
            .map_err(|e| format!("invalid certificate response: {e}"))
    }

    /// GET /v1/tunnels/{tunnel_id}/certificates/{id} — 查询证书。
    pub async fn retrieve_tunnel_certificate(
        &self,
        tunnel_id: &str,
        certificate_id: &str,
    ) -> Result<TunnelCertificate, String> {
        let path = format!("/v1/tunnels/{tunnel_id}/certificates/{certificate_id}");
        let response = self
            .request(reqwest::Method::GET, &path)
            .send()
            .await
            .map_err(|e| format!("GET {path} failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .json::<TunnelCertificate>()
            .await
            .map_err(|e| format!("invalid certificate response: {e}"))
    }
}

/// 构造隧道客户端（功能门控关闭时返回 McpTunnelsDisabledError）。
pub fn build_tunnels_client(
    options: TunnelsClientOptions,
) -> Result<TunnelsClient, McpTunnelsDisabledError> {
    if !is_mcp_tunnels_enabled() {
        return Err(McpTunnelsDisabledError);
    }
    TunnelsClient::new(options).map_err(|_| McpTunnelsDisabledError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_defaults_off() {
        // 无环境变量时门控关闭
        std::env::remove_var("FEATURE_MCP_TUNNELS");
        assert!(!is_mcp_tunnels_enabled());
    }
}
