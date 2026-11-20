//! oauth.rs — OAuth 端点、scope 与配置选择。
//!
//! 重写自 `src/constants/oauth.ts`。默认生产配置，ant 构建可经环境变量
//! 切换到 staging/local 配置；`CLAUDE_CODE_CUSTOM_OAUTH_URL` 仅允许已批准
//! 的 FedStart 基址，未批准时返回 `anyhow::Error`（对齐 TS 的 throw）。
//!
//! 环境读取集中在薄封装（`get_oauth_config` / `file_suffix_for_oauth_config`），
//! 选择逻辑放在纯函数中以便测试（避免测试间共享环境变量的竞态）。

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;

use crate::utils::env_utils::is_env_truthy;

/// OAuth 配置类型：生产 / 预发 / 本地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OauthConfigType {
    Prod,
    Staging,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthConfig {
    pub base_api_url: String,
    pub console_authorize_url: String,
    pub claude_ai_authorize_url: String,
    /// claude.ai web 源。与 claude_ai_authorize_url 分开，因为后者现在经
    /// claude.com/cai/* 做归因跳转——从它取 .origin 会得到 claude.com，
    /// 破坏指向 /code、/settings/connectors 等 claude.ai 页面的链接。
    pub claude_ai_origin: String,
    pub token_url: String,
    pub api_key_url: String,
    pub roles_url: String,
    pub console_success_url: String,
    pub claudeai_success_url: String,
    pub manual_redirect_url: String,
    pub client_id: String,
    pub oauth_file_suffix: String,
    pub mcp_proxy_url: String,
    pub mcp_proxy_path: String,
}

pub const CLAUDE_AI_INFERENCE_SCOPE: &str = "user:inference";
pub const CLAUDE_AI_PROFILE_SCOPE: &str = "user:profile";
const CONSOLE_SCOPE: &str = "org:create_api_key";

/// OAuth beta header（见 src/services/api/claude.ts 的 beta 注入）。
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Console OAuth scopes — 用于经 Console 创建 API key。
pub const CONSOLE_OAUTH_SCOPES: &[&str] = &[CONSOLE_SCOPE, CLAUDE_AI_PROFILE_SCOPE];

/// Claude.ai OAuth scopes — 用于 Claude.ai 订阅用户（Pro/Max/Team/Enterprise）。
pub const CLAUDE_AI_OAUTH_SCOPES: &[&str] = &[
    CLAUDE_AI_PROFILE_SCOPE,
    CLAUDE_AI_INFERENCE_SCOPE,
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// 登录时请求的全部 scope（Console 与 Claude.ai 的去重并集）。
/// 登录时请求全部 scope 以兼容 Console → Claude.ai 跳转。
/// 注意：apps 仓库的 OAuthConsentPage 需与此列表保持同步。
pub fn all_oauth_scopes() -> Vec<String> {
    let mut scopes = Vec::new();
    for scope in CONSOLE_OAUTH_SCOPES.iter().chain(CLAUDE_AI_OAUTH_SCOPES.iter()) {
        if !scopes.contains(&(*scope).to_string()) {
            scopes.push((*scope).to_string());
        }
    }
    scopes
}

/// `CLAUDE_CODE_CUSTOM_OAUTH_URL` 允许覆盖的基址白名单。
/// 仅 FedStart/PubSec 部署允许，防止 OAuth token 被发到任意端点。
const ALLOWED_OAUTH_BASE_URLS: [&str; 3] = [
    "https://beacon.claude-ai.staging.ant.dev",
    "https://claude.fedstart.com",
    "https://claude-staging.fedstart.com",
];

/// 生产 OAuth 配置 — 正常运行使用。
pub static PROD_OAUTH_CONFIG: Lazy<OauthConfig> = Lazy::new(|| OauthConfig {
    base_api_url: "https://api.anthropic.com".to_string(),
    console_authorize_url: "https://platform.claude.com/oauth/authorize".to_string(),
    // 经 claude.com/cai/* 中转，CLI 登录的 claude.com 访问可被归因。
    // 两跳 307 到 claude.ai/oauth/authorize。
    claude_ai_authorize_url: "https://claude.com/cai/oauth/authorize".to_string(),
    claude_ai_origin: "https://claude.ai".to_string(),
    token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
    api_key_url: "https://api.anthropic.com/api/oauth/claude_cli/create_api_key".to_string(),
    roles_url: "https://api.anthropic.com/api/oauth/claude_cli/roles".to_string(),
    console_success_url:
        "https://platform.claude.com/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code"
            .to_string(),
    claudeai_success_url: "https://platform.claude.com/oauth/code/success?app=claude-code"
        .to_string(),
    manual_redirect_url: "https://platform.claude.com/oauth/code/callback".to_string(),
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
    // 生产配置无后缀
    oauth_file_suffix: String::new(),
    mcp_proxy_url: "https://mcp-proxy.anthropic.com".to_string(),
    mcp_proxy_path: "/v1/mcp/{server_id}".to_string(),
});

/// 预发 OAuth 配置 — 仅 ant 构建经启用标记包含。
/// TS 中以 `process.env.USER_TYPE === 'ant'` 字面量检查做死代码消除；
/// Rust 端配置本身常量，访问门由 oauth_config_type() 保证。
pub static STAGING_OAUTH_CONFIG: Lazy<OauthConfig> = Lazy::new(|| OauthConfig {
    base_api_url: "https://api-staging.anthropic.com".to_string(),
    console_authorize_url: "https://platform.staging.ant.dev/oauth/authorize".to_string(),
    claude_ai_authorize_url: "https://claude-ai.staging.ant.dev/oauth/authorize".to_string(),
    claude_ai_origin: "https://claude-ai.staging.ant.dev".to_string(),
    token_url: "https://platform.staging.ant.dev/v1/oauth/token".to_string(),
    api_key_url: "https://api-staging.anthropic.com/api/oauth/claude_cli/create_api_key"
        .to_string(),
    roles_url: "https://api-staging.anthropic.com/api/oauth/claude_cli/roles".to_string(),
    console_success_url: format!(
        "https://platform.staging.ant.dev/buy_credits?returnUrl={}",
        "/oauth/code/success%3Fapp%3Dclaude-code"
    ),
    claudeai_success_url:
        "https://platform.staging.ant.dev/oauth/code/success?app=claude-code".to_string(),
    manual_redirect_url: "https://platform.staging.ant.dev/oauth/code/callback".to_string(),
    client_id: "22422756-60c9-4084-8eb7-27705fd5cf9a".to_string(),
    oauth_file_suffix: "-staging-oauth".to_string(),
    mcp_proxy_url: "https://mcp-proxy-staging.anthropic.com".to_string(),
    mcp_proxy_path: "/v1/mcp/{server_id}".to_string(),
});

/// 去掉单个尾部斜杠（对齐 TS 的 `url.replace(/\/$/, '')`）。
fn strip_one_trailing_slash(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

/// 三个本地开发服务器：:8000 api-proxy（`api dev start -g ccr`）、
/// :4000 claude-ai 前端、:3000 Console 前端。环境变量允许
/// scripts/claude-localhost 在布局不同时覆盖（基址需已去掉尾部斜杠）。
fn local_oauth_config(api: &str, apps: &str, console_base: &str) -> OauthConfig {
    OauthConfig {
        base_api_url: api.to_string(),
        console_authorize_url: format!("{console_base}/oauth/authorize"),
        claude_ai_authorize_url: format!("{apps}/oauth/authorize"),
        claude_ai_origin: apps.to_string(),
        token_url: format!("{api}/v1/oauth/token"),
        api_key_url: format!("{api}/api/oauth/claude_cli/create_api_key"),
        roles_url: format!("{api}/api/oauth/claude_cli/roles"),
        console_success_url: format!(
            "{console_base}/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code"
        ),
        claudeai_success_url: format!("{console_base}/oauth/code/success?app=claude-code"),
        manual_redirect_url: format!("{console_base}/oauth/code/callback"),
        client_id: "22422756-60c9-4084-8eb7-27705fd5cf9a".to_string(),
        oauth_file_suffix: "-local-oauth".to_string(),
        mcp_proxy_url: "http://localhost:8205".to_string(),
        mcp_proxy_path: "/v1/toolbox/shttp/mcp/{server_id}".to_string(),
    }
}

/// MCP OAuth 的 Client ID Metadata Document URL（CIMD / SEP-991）。
/// 当 MCP auth server 声明 client_id_metadata_document_supported: true 时，
/// Claude Code 用此 URL 作为 client_id，而非 Dynamic Client Registration。
/// 该 URL 必须指向 Anthropic 托管的 JSON 文档。
/// 见 https://datatracker.ietf.org/doc/html/draft-ietf-oauth-client-id-metadata-document-00
pub const MCP_CLIENT_METADATA_URL: &str = "https://claude.ai/oauth/claude-code-client-metadata";

/// OAuth 配置类型选择的纯逻辑。
///
/// ant 用户可经 `USE_LOCAL_OAUTH` / `USE_STAGING_OAUTH` 切换；其他用户恒为 prod。
fn oauth_config_type(is_ant: bool, use_local: bool, use_staging: bool) -> OauthConfigType {
    if is_ant {
        if use_local {
            return OauthConfigType::Local;
        }
        if use_staging {
            return OauthConfigType::Staging;
        }
    }
    OauthConfigType::Prod
}

fn is_ant() -> bool {
    std::env::var("USER_TYPE")
        .map(|user_type| user_type == "ant")
        .unwrap_or(false)
}

fn get_oauth_config_type() -> OauthConfigType {
    oauth_config_type(
        is_ant(),
        is_env_truthy(std::env::var("USE_LOCAL_OAUTH").ok().as_deref()),
        is_env_truthy(std::env::var("USE_STAGING_OAUTH").ok().as_deref()),
    )
}

/// OAuth 凭证文件后缀纯逻辑（本地/预发/staging/自定义对应 `.credentials*.json`）。
fn file_suffix_type(
    has_custom_url: bool,
    ty: OauthConfigType,
) -> &'static str {
    if has_custom_url {
        "-custom-oauth"
    } else {
        match ty {
            OauthConfigType::Local => "-local-oauth",
            OauthConfigType::Staging => "-staging-oauth",
            // 生产配置无后缀
            OauthConfigType::Prod => "",
        }
    }
}

/// OAuth 凭证文件后缀（本地/预发/staging/自定义对应的 `.credentials*.json`）。
pub fn file_suffix_for_oauth_config() -> String {
    let has_custom_url = std::env::var("CLAUDE_CODE_CUSTOM_OAUTH_URL")
        .map(|url| !url.is_empty())
        .unwrap_or(false);
    file_suffix_type(has_custom_url, get_oauth_config_type()).to_string()
}

/// 应用自定义基址与 CLIENT_ID 覆盖（纯逻辑）。
///
/// `custom_base` 为 `CLAUDE_CODE_CUSTOM_OAUTH_URL`（非空）；
/// `client_id_override` 为 `CLAUDE_CODE_OAUTH_CLIENT_ID`（空视同未设置）。
fn build_oauth_config(
    mut config: OauthConfig,
    custom_base: Option<&str>,
    client_id_override: Option<&str>,
) -> Result<OauthConfig> {
    // 允许把全部 OAuth URL 指向已批准的 FedStart 部署。
    // 仅白名单基址被接受，防止凭证泄漏。
    if let Some(oauth_base_url) = custom_base {
        let base = strip_one_trailing_slash(oauth_base_url);
        if !ALLOWED_OAUTH_BASE_URLS.contains(&base) {
            return Err(anyhow!(
                "CLAUDE_CODE_CUSTOM_OAUTH_URL is not an approved endpoint."
            ));
        }
        config.base_api_url = base.to_string();
        config.console_authorize_url = format!("{base}/oauth/authorize");
        config.claude_ai_authorize_url = format!("{base}/oauth/authorize");
        config.claude_ai_origin = base.to_string();
        config.token_url = format!("{base}/v1/oauth/token");
        config.api_key_url = format!("{base}/api/oauth/claude_cli/create_api_key");
        config.roles_url = format!("{base}/api/oauth/claude_cli/roles");
        config.console_success_url = format!("{base}/oauth/code/success?app=claude-code");
        config.claudeai_success_url = format!("{base}/oauth/code/success?app=claude-code");
        config.manual_redirect_url = format!("{base}/oauth/code/callback");
        config.oauth_file_suffix = "-custom-oauth".to_string();
    }

    // 允许经环境变量覆盖 CLIENT_ID（如 Xcode 集成）；空字符串视同未设置
    if let Some(client_id) = client_id_override {
        if !client_id.is_empty() {
            config.client_id = client_id.to_string();
        }
    }

    Ok(config)
}

/// 读 `var`，去掉单个尾部斜杠；未设置时回退 `default`。
fn env_base(var: &str, default: &str) -> String {
    std::env::var(var)
        .map(|base| strip_one_trailing_slash(&base).to_string())
        .unwrap_or_else(|_| default.to_string())
}

/// 按 ant/user 环境变量选择 OAuth 配置，并应用自定义基址覆盖。
pub fn get_oauth_config() -> Result<OauthConfig> {
    let base_config = match get_oauth_config_type() {
        OauthConfigType::Local => local_oauth_config(
            &env_base("CLAUDE_LOCAL_OAUTH_API_BASE", "http://localhost:8000"),
            &env_base("CLAUDE_LOCAL_OAUTH_APPS_BASE", "http://localhost:4000"),
            &env_base("CLAUDE_LOCAL_OAUTH_CONSOLE_BASE", "http://localhost:3000"),
        ),
        OauthConfigType::Staging => STAGING_OAUTH_CONFIG.clone(),
        OauthConfigType::Prod => PROD_OAUTH_CONFIG.clone(),
    };
    // TS 真值检查：空字符串的 CLAUDE_CODE_CUSTOM_OAUTH_URL 跳过覆盖
    let custom_base = std::env::var("CLAUDE_CODE_CUSTOM_OAUTH_URL")
        .ok()
        .filter(|url| !url.is_empty());
    build_oauth_config(
        base_config,
        custom_base.as_deref(),
        std::env::var("CLAUDE_CODE_OAUTH_CLIENT_ID").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_scopes_are_deduped_and_ordered() {
        assert_eq!(
            all_oauth_scopes(),
            vec![
                "org:create_api_key",
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:mcp_servers",
                "user:file_upload",
            ]
        );
    }

    #[test]
    fn prod_config() {
        let config = build_oauth_config(PROD_OAUTH_CONFIG.clone(), None, None).unwrap();
        assert_eq!(config.base_api_url, "https://api.anthropic.com");
        assert_eq!(config.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(config.oauth_file_suffix, "");
        assert_eq!(file_suffix_type(false, OauthConfigType::Prod), "");
    }

    #[test]
    fn staging_config() {
        let config = build_oauth_config(STAGING_OAUTH_CONFIG.clone(), None, None).unwrap();
        assert_eq!(config.base_api_url, "https://api-staging.anthropic.com");
        assert_eq!(config.oauth_file_suffix, "-staging-oauth");
        assert_eq!(file_suffix_type(false, OauthConfigType::Staging), "-staging-oauth");
    }

    #[test]
    fn local_config_defaults() {
        let config = build_oauth_config(
            local_oauth_config(
                "http://localhost:8000",
                "http://localhost:4000",
                "http://localhost:3000",
            ),
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.base_api_url, "http://localhost:8000");
        assert_eq!(config.claude_ai_origin, "http://localhost:4000");
        assert_eq!(
            config.console_authorize_url,
            "http://localhost:3000/oauth/authorize"
        );
        assert_eq!(config.token_url, "http://localhost:8000/v1/oauth/token");
        assert_eq!(config.oauth_file_suffix, "-local-oauth");
        assert_eq!(file_suffix_type(false, OauthConfigType::Local), "-local-oauth");
    }

    #[test]
    fn strip_trailing_slash_removes_exactly_one() {
        // 对齐 TS 的 `url.replace(/\/$/, '')`：只去一个尾部斜杠
        assert_eq!(strip_one_trailing_slash("http://localhost:8000/"), "http://localhost:8000");
        assert_eq!(strip_one_trailing_slash("http://localhost:8000//"), "http://localhost:8000/");
        assert_eq!(strip_one_trailing_slash("http://localhost:8000"), "http://localhost:8000");
    }

    #[test]
    fn custom_oauth_url_allowed_base() {
        let config = build_oauth_config(
            PROD_OAUTH_CONFIG.clone(),
            Some("https://claude.fedstart.com"),
            None,
        )
        .unwrap();
        assert_eq!(config.base_api_url, "https://claude.fedstart.com");
        assert_eq!(config.oauth_file_suffix, "-custom-oauth");
        assert_eq!(file_suffix_type(true, OauthConfigType::Prod), "-custom-oauth");
    }

    #[test]
    fn custom_oauth_url_strips_trailing_slash_before_check() {
        let config = build_oauth_config(
            PROD_OAUTH_CONFIG.clone(),
            Some("https://claude.fedstart.com/"),
            None,
        )
        .unwrap();
        assert_eq!(config.base_api_url, "https://claude.fedstart.com");
    }

    #[test]
    fn custom_oauth_url_rejected() {
        let err = build_oauth_config(
            PROD_OAUTH_CONFIG.clone(),
            Some("https://evil.example.com"),
            None,
        )
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "CLAUDE_CODE_CUSTOM_OAUTH_URL is not an approved endpoint."
        );
    }

    #[test]
    fn client_id_override() {
        let config =
            build_oauth_config(PROD_OAUTH_CONFIG.clone(), None, Some("custom-client-id")).unwrap();
        assert_eq!(config.client_id, "custom-client-id");
    }

    #[test]
    fn empty_client_id_override_is_ignored() {
        let config = build_oauth_config(PROD_OAUTH_CONFIG.clone(), None, Some("")).unwrap();
        assert_eq!(config.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    }

    #[test]
    fn config_type_external_user_always_prod() {
        assert_eq!(oauth_config_type(false, true, true), OauthConfigType::Prod);
    }

    #[test]
    fn config_type_ant_flags() {
        assert_eq!(oauth_config_type(true, true, true), OauthConfigType::Local);
        assert_eq!(oauth_config_type(true, false, true), OauthConfigType::Staging);
        assert_eq!(oauth_config_type(true, false, false), OauthConfigType::Prod);
    }
}
