//! `services/compaction2/methods/remote/transport.ts` — 远程压缩 lane 的
//! provider 绑定传输层。
//!
//! lane 只知道如何构造请求；本模块提供 provider 特定的 base URL 与鉴权
//! header。凭据按请求惰性读取，且永不记录。除非调用方显式注入结果
//! `fetch`，这里的一切都不改变行为。
//!
//! Rust 对齐说明：
//! - TS 的 `AbortSignal` 用 `AbortSignalRef` trait 对象表达。
//! - `google-auth-library`（Vertex ADC）与 `@aws-sdk/client-bedrock-runtime`
//!   在 Rust 侧未接线，按仓内惯例 TODO 存根。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::services::compaction2::contracts::AbortSignalRef;
use crate::services::compaction2::methods::remote::anthropic::{
    resolve_anthropic_messages_endpoint, ANTHROPIC_COMPACTION_BETA,
};
use crate::services::compaction2::methods::remote::{
    RemoteFetch, RemoteFetchInit, RemoteFetchResponse,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// TS `RemoteAuthHeaderInput`：lane 可能依赖的鉴权 header；值是机密，
/// 永不记录。
pub struct RemoteAuthHeaderInput<'a> {
    pub provider: &'a str,
    /// Anthropic API key（firstParty）。
    pub api_key: Option<&'a str>,
    /// Claude.ai OAuth access token（firstParty 回落）。
    pub oauth_token: Option<&'a str>,
    /// Google Cloud access token（vertex）。
    pub bearer_token: Option<&'a str>,
}

/// TS `buildRemoteAuthHeaders(input)`。
///
/// 构造 provider 的鉴权 header。provider 不支持或没有可用凭据时返回
/// `None`。
pub fn build_remote_auth_headers(
    input: &RemoteAuthHeaderInput<'_>,
) -> Option<HashMap<String, String>> {
    if input.provider == "firstParty" {
        if input.api_key.is_none() && input.oauth_token.is_none() {
            return None;
        }
        let mut headers = HashMap::new();
        headers.insert(
            "anthropic-version".to_string(),
            "2023-06-01".to_string(),
        );
        headers.insert(
            "anthropic-beta".to_string(),
            ANTHROPIC_COMPACTION_BETA.to_string(),
        );
        if let Some(api_key) = input.api_key {
            headers.insert("x-api-key".to_string(), api_key.to_string());
        } else if let Some(oauth_token) = input.oauth_token {
            headers.insert(
                "authorization".to_string(),
                format!("Bearer {oauth_token}"),
            );
        }
        return Some(headers);
    }
    if input.provider == "openai" {
        let api_key = input.api_key?;
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {api_key}"),
        );
        return Some(headers);
    }
    if input.provider == "vertex" {
        let bearer_token = input.bearer_token?;
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {bearer_token}"),
        );
        return Some(headers);
    }
    None
}

/// TS `normalizeRemoteBaseUrl(baseUrl, fallback?)`：修剪调用方提供的
/// base URL 的空白与尾随斜杠。
pub fn normalize_remote_base_url(base_url: Option<&str>, fallback: Option<&str>) -> Option<String> {
    let raw = base_url
        .map(|base| base.trim())
        .filter(|base| !base.is_empty())
        .or_else(|| {
            fallback
                .map(|fallback| fallback.trim())
                .filter(|fallback| !fallback.is_empty())
        })?;
    Some(raw.trim_end_matches('/').to_string())
}

/// TS `BuiltRemoteTransport`。
pub struct BuiltRemoteTransport {
    pub fetch: Arc<dyn RemoteFetch>,
    /// Anthropic API key（firstParty 可用时）。
    pub api_key: Option<String>,
    /// 归一化的 provider base URL。
    pub base_url: Option<String>,
}

/// TS `BedrockConverseInput`。
#[derive(Debug, Clone)]
pub struct BedrockConverseInput {
    pub model_id: String,
    pub body: Value,
}

/// TS `BedrockConverseRuntime`。
///
/// Bedrock 传输层用到的最小 Converse 运行时表面。生产适配器包裹
/// `BedrockRuntimeClient` + `ConverseCommand`；测试注入普通对象。保持
/// 窄接口，避免把 AWS SDK 类型泄漏进压缩层。
pub trait BedrockConverseRuntime: Send + Sync {
    fn send_converse<'a>(
        &'a self,
        input: BedrockConverseInput,
        signal: Option<&'a dyn AbortSignalRef>,
    ) -> BoxFuture<'a, Result<Value, String>>;
}

/// TS `BuildRemoteTransportInput`。
pub struct BuildRemoteTransportInput<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub env: Option<&'a HashMap<String, Option<String>>>,
    /// 底层 fetch；默认为 reqwest 网关。测试注入 mock。
    pub fetch: Option<Arc<dyn RemoteFetch>>,
    pub get_anthropic_api_key: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub get_oauth_token: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub get_open_ai_api_key: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub get_google_access_token:
        Option<Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>>,
    /// 既有 Bedrock Converse 运行时（生产：共享的
    /// `createBedrockRuntimeClient()`；测试：存根）。存在时它是权威的：
    /// 缺失运行时禁用传输而不是回落 SDK，让测试保持确定。
    pub get_bedrock_converse_runtime:
        Option<Arc<dyn Fn() -> Option<Arc<dyn BedrockConverseRuntime>> + Send + Sync>>,
}

fn read_env_value(env: Option<&HashMap<String, Option<String>>>, key: &str) -> Option<String> {
    env?.get(key).and_then(|value| value.clone()).filter(|value| !value.is_empty())
}

/// `createAuthorizedFetch`：给底层 fetch 叠加异步解析的鉴权 header。
fn create_authorized_fetch(
    base_fetch: Arc<dyn RemoteFetch>,
    resolve_headers: Arc<dyn Fn() -> BoxFuture<'static, Option<HashMap<String, String>>> + Send + Sync>,
) -> Arc<dyn RemoteFetch> {
    struct AuthorizedFetch {
        base_fetch: Arc<dyn RemoteFetch>,
        resolve_headers: Arc<dyn Fn() -> BoxFuture<'static, Option<HashMap<String, String>>> + Send + Sync>,
    }

    impl RemoteFetch for AuthorizedFetch {
        fn fetch<'a>(
            &'a self,
            input: &'a str,
            init: RemoteFetchInit<'a>,
        ) -> Pin<Box<dyn Future<Output = Result<RemoteFetchResponse, String>> + Send + 'a>> {
            Box::pin(async move {
                let headers = (self.resolve_headers)().await;
                let Some(headers) = headers else {
                    return self.base_fetch.fetch(input, init).await;
                };
                let merged = merge_headers(init.headers.as_ref(), &headers);
                self.base_fetch
                    .fetch(
                        input,
                        RemoteFetchInit {
                            method: init.method,
                            headers: Some(merged),
                            body: init.body,
                        },
                    )
                    .await
            })
        }
    }

    Arc::new(AuthorizedFetch {
        base_fetch,
        resolve_headers,
    })
}

/// 大小写不敏感合并；传输层鉴权 header 优先于 lane header。
fn merge_headers(
    base: Option<&HashMap<String, String>>,
    extra: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged: HashMap<String, String> = base.cloned().unwrap_or_default();
    for (name, value) in extra {
        let existing = merged
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))
            .cloned();
        if let Some(existing) = existing {
            merged.remove(&existing);
        }
        merged.insert(name.clone(), value.clone());
    }
    merged
}

struct ReqwestGateway;

impl RemoteFetch for ReqwestGateway {
    fn fetch<'a>(
        &'a self,
        input: &'a str,
        init: RemoteFetchInit<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<RemoteFetchResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let method = init.method.unwrap_or("GET");
            let mut request = reqwest::Client::new().request(method.parse().unwrap_or(reqwest::Method::GET), input);
            if let Some(headers) = init.headers.as_ref() {
                for (name, value) in headers {
                    request = request.header(name, value);
                }
            }
            if let Some(body) = init.body {
                request = request.body(body.to_string());
            }
            let response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let status_text = response.status().canonical_reason().map(String::from);
            let ok = response.status().is_success();
            let text = response.text().await.map_err(|error| error.to_string())?;
            Ok(RemoteFetchResponse {
                ok,
                status,
                status_text,
                text,
            })
        })
    }
}

/// 从 `/model/<id>/converse` 形态的 Converse URL 提取模型 id。
fn model_id_from_converse_url(url: &str) -> Option<String> {
    static PATTERN: once_cell::sync::Lazy<Option<regex::Regex>> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new("/(?:model|inference-profile)/([^/?#]+)/converse").ok()
        });
    let captures = PATTERN.as_ref()?.captures(url)?;
    Some(captures[1].to_string())
}

/// `bedrockConverseFetch`：把 HTTP 请求改道到注入的 Converse 运行时。
fn bedrock_converse_fetch(runtime: Arc<dyn BedrockConverseRuntime>) -> Arc<dyn RemoteFetch> {
    struct BedrockFetch {
        runtime: Arc<dyn BedrockConverseRuntime>,
    }

    impl RemoteFetch for BedrockFetch {
        fn fetch<'a>(
            &'a self,
            input: &'a str,
            init: RemoteFetchInit<'a>,
        ) -> Pin<Box<dyn Future<Output = Result<RemoteFetchResponse, String>> + Send + 'a>> {
            Box::pin(async move {
                let model_id = model_id_from_converse_url(input)
                    .ok_or("Bedrock Converse endpoint is missing a model id")?;
                let body: Value = match init.body {
                    Some(body) if !body.is_empty() => {
                        serde_json::from_str(body).unwrap_or_else(|_| Value::Object(Default::default()))
                    }
                    _ => Value::Object(Default::default()),
                };
                let output = self
                    .runtime
                    .send_converse(BedrockConverseInput { model_id, body }, None)
                    .await?;
                let text = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
                Ok(RemoteFetchResponse {
                    ok: true,
                    status: 200,
                    status_text: None,
                    text,
                })
            })
        }
    }

    Arc::new(BedrockFetch { runtime })
}

/// TS `resolveBedrockBaseUrl`。
fn resolve_bedrock_base_url(
    base_url: Option<&str>,
    env: Option<&HashMap<String, Option<String>>>,
) -> String {
    let env_base = read_env_value(env, "ANTHROPIC_BEDROCK_BASE_URL");
    let explicit = normalize_remote_base_url(base_url.or(env_base.as_deref()), None);
    if let Some(explicit) = explicit {
        return explicit;
    }
    let region = read_env_value(env, "AWS_REGION")
        .or_else(|| read_env_value(env, "AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".to_string());
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// TS `resolveVertexBaseUrl`。
fn resolve_vertex_base_url(
    model: &str,
    base_url: Option<&str>,
    env: &HashMap<String, Option<String>>,
) -> Option<String> {
    let env_base = read_env_value(Some(env), "ANTHROPIC_BASE_URL");
    let explicit = normalize_remote_base_url(base_url.or(env_base.as_deref()), None);
    if let Some(explicit) = explicit {
        return Some(explicit);
    }
    // 调用方未提供端点：用规范的 rawPredict URL，Anthropic lane 原样透传。
    resolve_anthropic_messages_endpoint("vertex", None, model, env)
}

/// TS `buildRemoteTransport(input)`。
///
/// 为远程压缩 lane 构造 provider 绑定的传输。
///
/// 对没有可复用 side-request 传输的 provider（bedrock 需要运行时 SDK 或
/// 注入的 Converse 运行时），或无法解析出凭据时返回 `None`。
pub fn build_remote_transport(input: BuildRemoteTransportInput<'_>) -> Option<BuiltRemoteTransport> {
    let env = input.env;
    let base_fetch: Arc<dyn RemoteFetch> = input
        .fetch
        .clone()
        .or_else(|| Some(Arc::new(ReqwestGateway)))
        .unwrap();

    if input.provider == "firstParty" {
        // 注入的 getter 是权威的（测试依赖确定性）；只有未提供时才回落
        // 真实凭据链。
        let api_key = input
            .get_anthropic_api_key
            .as_ref()
            .map(|getter| getter())
            .unwrap_or_else(|| {
                read_env_value(env, "ANTHROPIC_API_KEY")
                    .or_else(|| default_anthropic_api_key())
            });
        let oauth_token = input
            .get_oauth_token
            .as_ref()
            .map(|getter| getter())
            .unwrap_or_else(default_oauth_token);
        let headers = build_remote_auth_headers(&RemoteAuthHeaderInput {
            provider: "firstParty",
            api_key: api_key.as_deref(),
            oauth_token: oauth_token.as_deref(),
            bearer_token: None,
        })?;
        let api_key = api_key.filter(|key| !key.is_empty());
        let env_base = read_env_value(env, "ANTHROPIC_BASE_URL");
        return Some(BuiltRemoteTransport {
            fetch: create_authorized_fetch(
                base_fetch,
                Arc::new(move || {
                    let headers = headers.clone();
                    Box::pin(async move { Some(headers) })
                }),
            ),
            api_key,
            base_url: normalize_remote_base_url(input.base_url.or(env_base.as_deref()), None),
        });
    }

    if input.provider == "openai" {
        let api_key = input
            .get_open_ai_api_key
            .as_ref()
            .map(|getter| getter())
            .unwrap_or_else(|| read_env_value(env, "OPENAI_API_KEY"))?;
        let headers = build_remote_auth_headers(&RemoteAuthHeaderInput {
            provider: "openai",
            api_key: Some(&api_key),
            oauth_token: None,
            bearer_token: None,
        })?;
        let env_base = read_env_value(env, "OPENAI_BASE_URL");
        return Some(BuiltRemoteTransport {
            fetch: create_authorized_fetch(
                base_fetch,
                Arc::new(move || {
                    let headers = headers.clone();
                    Box::pin(async move { Some(headers) })
                }),
            ),
            api_key: Some(api_key),
            base_url: normalize_remote_base_url(
                input.base_url.or(env_base.as_deref()),
                Some("https://api.openai.com/v1"),
            ),
        });
    }

    if input.provider == "vertex" {
        let model = input.model.unwrap_or("");
        let getter: Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync> = input
            .get_google_access_token
            .clone()
            .unwrap_or_else(|| Arc::new(|| Box::pin(default_google_access_token())));
        return Some(BuiltRemoteTransport {
            fetch: create_authorized_fetch(base_fetch, {
                let getter = getter.clone();
                Arc::new(move || {
                    let getter = getter.clone();
                    Box::pin(async move {
                        let bearer_token = getter().await?;
                        build_remote_auth_headers(&RemoteAuthHeaderInput {
                            provider: "vertex",
                            api_key: None,
                            oauth_token: None,
                            bearer_token: Some(&bearer_token),
                        })
                    })
                })
            }),
            api_key: None,
            base_url: resolve_vertex_base_url(model, input.base_url, env.unwrap_or(&HashMap::new())),
        });
    }

    if input.provider == "bedrock" {
        let base_url = resolve_bedrock_base_url(input.base_url, env);
        // 注入的 getter 是权威的，让测试/嵌入方与 SDK 是否恰好安装解耦。
        if let Some(getter) = input.get_bedrock_converse_runtime.as_ref() {
            let runtime: Arc<dyn BedrockConverseRuntime> = getter()?;
            return Some(BuiltRemoteTransport {
                fetch: bedrock_converse_fetch(runtime),
                api_key: None,
                base_url: Some(base_url),
            });
        }
        // 未注入运行时：仅当 lane 显式启用时探测 SDK。
        // `CCH_BEDROCK_COMPACTION !== '1'` 不应为每次压缩付出模块解析成本。
        if read_env_value(env, "CCH_BEDROCK_COMPACTION").as_deref() != Some("1") {
            return None;
        }
        // TODO(@aws-sdk/client-bedrock-runtime)：Rust 侧未接线，
        // defaultBedrockConverseRuntime() 不可用。
        return None;
    }

    // foundry/gemini/grok：没有 side 传输。
    None
}

fn default_anthropic_api_key() -> Option<String> {
    // utils/auth.rs 已转换；读取失败按未配置处理。
    crate::utils::auth::get_anthropic_api_key()
}

fn default_oauth_token() -> Option<String> {
    crate::utils::auth::get_claude_ai_oauth_tokens()
        .map(|tokens| tokens.access_token)
        .filter(|token| !token.is_empty())
}

/// TS `defaultGoogleAccessToken`：经 Vertex SDK 同款 ADC 链解析 Google
/// Cloud access token。`google-auth-library` 未接线，返回 `None`。
/// `CLAUDE_CODE_SKIP_VERTEX_AUTH` 的空串语义（代理模式）由调用方注入的
/// getter 表达。
async fn default_google_access_token() -> Option<String> {
    // TODO(google-auth-library): GoogleAuth({scopes:[cloud-platform]}).getAccessToken()。
    None
}
