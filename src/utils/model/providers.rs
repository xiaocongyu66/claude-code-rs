//! providers.rs — provider 选择根节点。
//!
//! 重写自 `src/utils/model/providers.ts`。
//! modelType → `CLAUDE_CODE_USE_*` 环境变量 → `firstParty` 的判定优先级
//! 逐条对齐 TS；TS 的 `export { APIProvider, isThirdPartyAPIProvider }`
//! 转发在此以 `pub use` 保留。

use std::env;

use crate::utils::env_utils::is_env_truthy;
use crate::utils::model::provider_classification::{is_third_party_api_provider, ApiProvider};
use crate::utils::settings::settings::get_initial_settings;
use crate::utils::settings::types::ModelTypeSetting;

// 对齐 TS 的 re-export：`export { APIProvider, isThirdPartyAPIProvider }`。
pub use crate::utils::model::provider_classification::ApiProvider as APIProvider;
pub use crate::utils::model::provider_classification::is_third_party_api_provider as isThirdPartyAPIProvider;

/// `getAPIProvider(settings = getInitialSettings())`：解析当前会话使用的 API provider。
///
/// 优先级（逐条对齐 TS）：
/// 1. `settings.modelType` 为 `openai` / `gemini` / `grok` 时直接返回；
/// 2. `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY` 中的一个为真值；
/// 3. `CLAUDE_CODE_USE_OPENAI` / `_GEMINI` / `_GROK` 中的一个为真值；
/// 4. 兜底 `firstParty`。
///
/// 显式传入 `model_type` 的入口为 [`get_api_provider_from`]；
/// 不传时等价 TS 的默认实参 `getInitialSettings()`。
pub fn get_api_provider() -> ApiProvider {
    get_api_provider_from(get_initial_settings().model_type)
}

/// `getAPIProvider(settings)`：以显式 modelType 解析 provider（默认实参版本见
/// [`get_api_provider`]）。`anthropic` 与 `None` 都落入环境变量判定分支。
pub fn get_api_provider_from(model_type: Option<ModelTypeSetting>) -> ApiProvider {
    match model_type {
        Some(ModelTypeSetting::Openai) => return ApiProvider::Openai,
        Some(ModelTypeSetting::Gemini) => return ApiProvider::Gemini,
        Some(ModelTypeSetting::Grok) => return ApiProvider::Grok,
        Some(ModelTypeSetting::Anthropic) | None => {}
    }

    if is_env_truthy(env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref()) {
        return ApiProvider::Bedrock;
    }
    if is_env_truthy(env::var("CLAUDE_CODE_USE_VERTEX").ok().as_deref()) {
        return ApiProvider::Vertex;
    }
    if is_env_truthy(env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref()) {
        return ApiProvider::Foundry;
    }

    if is_env_truthy(env::var("CLAUDE_CODE_USE_OPENAI").ok().as_deref()) {
        return ApiProvider::Openai;
    }
    if is_env_truthy(env::var("CLAUDE_CODE_USE_GEMINI").ok().as_deref()) {
        return ApiProvider::Gemini;
    }
    if is_env_truthy(env::var("CLAUDE_CODE_USE_GROK").ok().as_deref()) {
        return ApiProvider::Grok;
    }

    ApiProvider::FirstParty
}

/// `getAPIProviderForStatsig()`：Statsig 分析元数据用的 provider。
///
/// TS 端返回 branded 的 `AnalyticsMetadata_*`（实为 provider 字面量字符串）；
/// Rust 侧 analytics 模块尚未转换，先以 [`ApiProvider`] 等价表达。
pub fn get_api_provider_for_statsig() -> ApiProvider {
    get_api_provider()
}

/// `isFirstPartyAnthropicBaseUrl()`：`ANTHROPIC_BASE_URL` 是否为 Anthropic 官方端点。
///
/// 未设置时返回 true（默认 API）；设置时仅当 host 为 `api.anthropic.com`
/// （`USER_TYPE === 'ant'` 时额外允许 `api-staging.anthropic.com`）。
/// URL 解析失败（无 scheme 等非合法 URL）返回 false。
///
/// TODO: TS 同源注释——只配置了 openai 协议的用户这里会为 true，存在已知问题。
pub fn is_first_party_anthropic_base_url() -> bool {
    let Ok(base_url) = env::var("ANTHROPIC_BASE_URL") else {
        return true;
    };

    let Some(host) = parse_url_host(&base_url) else {
        return false;
    };

    let mut allowed_hosts = vec!["api.anthropic.com"];
    if env::var("USER_TYPE").as_deref() == Ok("ant") {
        allowed_hosts.push("api-staging.anthropic.com");
    }
    allowed_hosts.contains(&host.as_str())
}

/// 极简 URL host 解析：对齐 `new URL(baseUrl).host` 的取值范围。
///
/// 仅接受 `http://` / `https://` 前缀（无 scheme 时 `new URL` 抛错 → false）；
/// 去掉 userinfo 与 path/query/fragment，保留端口号，host 转小写，
/// 并去除协议默认端口（http 80 / https 443）。
fn parse_url_host(url: &str) -> Option<String> {
    let (rest, default_port) = url
        .strip_prefix("https://")
        .map(|r| (r, Some(443)))
        .or_else(|| url.strip_prefix("http://").map(|r| (r, Some(80))))?;

    let authority_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // `URL.host` 不含 userinfo（`user@host` → `host`）。
    let host_port = authority.rsplit('@').next()?;
    if host_port.is_empty() {
        return None;
    }

    let host_port = host_port.to_ascii_lowercase();
    // 去掉协议默认端口，保留显式端口。
    match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() => {
            let port_num = port.parse::<u32>().ok()?;
            if (default_port == Some(443) && port_num == 443)
                || (default_port == Some(80) && port_num == 80)
            {
                Some(host.to_string())
            } else {
                Some(host_port)
            }
        }
        _ => Some(host_port),
    }
}
