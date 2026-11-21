//! `services/compaction2/methods/remote/index.ts` — provider 原生 server
//! 压缩（模块根，对齐 TS 的 remote/index.ts；remote.ts 已并入本文件）。
//!
//! W5 接通参考实现暴露的三条 lane：
//!
//! - `anthropic`：按需 `compact-2026-09-04` beta
//!   （`compaction: { type: "summarize", instructions }`），仅
//!   firstParty/Vertex。
//! - `openai`：Responses `/responses/compact`（V1）与流式
//!   `compaction_trigger`（V2）。
//! - `bedrock`：经共享 Bedrock Runtime 客户端的 Converse 侧请求，
//!   额外门控 `CCH_BEDROCK_COMPACTION=1`。
//!
//! 可用性刻意保守：`CCH_REMOTE_COMPACTION=1` 才开启，未知 gateway 永不可用。

pub mod anthropic;
pub mod bedrock;
pub mod openai;
pub mod transport;

use std::collections::HashMap;

use crate::services::compact::{uuid_v4, Content, Message, SystemMessage};
use crate::services::compaction2::contracts::{
    CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome, CutPoint,
};
use crate::services::compaction2::methods::remote::anthropic::{
    is_anthropic_compaction_lane_available, run_anthropic_compaction_lane,
};
use crate::services::compaction2::methods::remote::bedrock::{
    is_bedrock_compaction_lane_available, run_bedrock_compaction_lane,
};
use crate::services::compaction2::methods::remote::openai::{
    is_openai_compaction_lane_available, run_openai_compaction_lane,
};
use crate::services::compaction2::reducers::prune::estimate_message_tokens;

/// TS `REMOTE_COMPACTION_ENV`。
pub const REMOTE_COMPACTION_ENV: &str = "CCH_REMOTE_COMPACTION";

/// TS `DEFAULT_REMOTE_COMPACTION_INSTRUCTIONS`（逐字节搬运）。
pub const DEFAULT_REMOTE_COMPACTION_INSTRUCTIONS: &str =
    "Summarize the conversation so far. Preserve decisions, open tasks, file paths, commands, \
     and any constraints the user stated. Return only the summary.";

/// TS `APIProvider`（utils/model/providerClassification.ts 未转换）。
pub type ApiProvider = String;

/// TS `RemoteLaneResult::RemoteLaneSuccess`。
#[derive(Debug, Clone)]
pub struct RemoteLaneSuccess {
    pub summary: String,
    pub preserve_data: Option<serde_json::Map<String, serde_json::Value>>,
    pub warning: Option<String>,
}

/// TS `RemoteLaneResult`。
#[derive(Debug, Clone)]
pub enum RemoteLaneResult {
    Success(RemoteLaneSuccess),
    Skipped(String),
}

/// TS `RemoteLaneTransport`。
#[derive(Default)]
pub struct RemoteLaneTransport {
    /// 注入的 fetch（生产侧绑定 provider）；未注入则 lane 不可用。
    pub fetch: Option<Box<dyn RemoteFetch>>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: String,
}

/// TS `RemoteFetch`：Rust 侧用 trait 对象 + boxed future。
pub trait RemoteFetch: Send + Sync {
    fn fetch<'a>(
        &'a self,
        input: &'a str,
        init: RemoteFetchInit<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RemoteFetchResponse, String>> + Send + 'a>>;
}

/// TS `fetch` 的 init 参数。
#[derive(Debug, Clone, Default)]
pub struct RemoteFetchInit<'a> {
    pub method: Option<&'a str>,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<&'a str>,
}

/// TS `Response` 的最小投影。
#[derive(Debug, Clone)]
pub struct RemoteFetchResponse {
    pub ok: bool,
    pub status: u16,
    pub status_text: Option<String>,
    pub text: String,
}

impl RemoteFetchResponse {
    pub async fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::from_str(&self.text).map_err(|error| error.to_string())
    }

    pub async fn text(&self) -> Result<String, String> {
        Ok(self.text.clone())
    }
}

/// TS `RemoteLaneRequest`。
pub struct RemoteLaneRequest<'a> {
    pub ctx: &'a CompactionContext,
    pub instructions: String,
    pub signal: Option<&'a dyn crate::services::compaction2::contracts::AbortSignalRef>,
    pub provider: Option<&'a ApiProvider>,
    pub env: HashMap<String, Option<String>>,
    pub transport: RemoteLaneTransport,
    /// 方法在分发前算好的切点。摘要前缀的 lane 必须发 `cut.messages`，
    /// 且请求后不得重算边界。
    pub cut: Option<&'a RemoteCut>,
    pub options: RemoteLaneOptions,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteLaneOptions {
    /// OpenAI lane 请求形态；`auto` 先试 V2 再回落 V1。
    pub open_ai_version: Option<OpenAiVersion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiVersion {
    V1,
    V2,
    Auto,
}

/// TS `RemoteMethodDeps`。
pub struct RemoteMethodDeps {
    /// 环境覆盖；默认 `process.env`。
    pub env: Option<HashMap<String, Option<String>>>,
    pub fetch: Option<Box<dyn RemoteFetch>>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub provider: Option<ApiProvider>,
    pub open_ai_version: Option<OpenAiVersion>,
    pub instructions: Option<String>,
    pub resolve_provider: Option<Box<dyn Fn(&CompactionContext) -> Option<ApiProvider> + Send + Sync>>,
    pub resolve_base_url: Option<Box<dyn Fn(&CompactionContext) -> Option<String> + Send + Sync>>,
    pub resolve_api_key: Option<Box<dyn Fn(&CompactionContext) -> Option<String> + Send + Sync>>,
    /// 测试/嵌入方覆盖，先于内置门控评估。
    pub is_available: Option<Box<dyn Fn(&CompactionContext) -> bool + Send + Sync>>,
}

fn read_env(deps: Option<&RemoteMethodDeps>) -> HashMap<String, Option<String>> {
    deps.and_then(|deps| deps.env.clone())
        .unwrap_or_else(read_process_env)
}

fn read_process_env() -> HashMap<String, Option<String>> {
    // process.env 的最小投影：仅保留压缩链读取的键 + 通用遍历代价控制。
    let keys = [
        "CCH_REMOTE_COMPACTION",
        "CCH_BEDROCK_COMPACTION",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BEDROCK_BASE_URL",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ];
    let mut env = HashMap::new();
    for key in keys {
        env.insert(key.to_string(), std::env::var(key).ok());
    }
    env
}

/// TS `isRemoteCompactionEnabled(env?)`：opt-in，只有 `'1'` 开启。
pub fn is_remote_compaction_enabled(env: &HashMap<String, Option<String>>) -> bool {
    env.get(REMOTE_COMPACTION_ENV)
        .and_then(|value| value.as_ref())
        .map(|value| value == "1")
        .unwrap_or(false)
}

fn default_provider() -> Option<ApiProvider> {
    // utils/model/providers.ts 未转换；返回 None 表示未知 gateway
    // （与 TS 的 require 失败回落一致）。
    None
}

fn default_base_url(
    provider: Option<&str>,
    env: &HashMap<String, Option<String>>,
) -> Option<String> {
    let env_value = |key: &str| {
        env.get(key)
            .and_then(|value| value.clone())
            .filter(|value| !value.is_empty())
    };
    match provider {
        Some("openai") => env_value("OPENAI_BASE_URL"),
        Some("firstParty") | Some("vertex") => env_value("ANTHROPIC_BASE_URL"),
        Some("bedrock") => {
            if let Some(override_url) = env_value("ANTHROPIC_BEDROCK_BASE_URL") {
                return Some(override_url);
            }
            let region = env_value("AWS_REGION").or_else(|| env_value("AWS_DEFAULT_REGION"));
            region.map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"))
        }
        _ => None,
    }
}

fn resolve_api_key(
    provider: Option<&str>,
    env: &HashMap<String, Option<String>>,
) -> Option<String> {
    let env_value = |key: &str| {
        env.get(key)
            .and_then(|value| value.clone())
            .filter(|value| !value.is_empty())
    };
    match provider {
        Some("openai") => env_value("OPENAI_API_KEY"),
        Some("firstParty") | Some("vertex") => env_value("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

struct ResolvedRemote {
    provider: Option<ApiProvider>,
    lane: Option<RemoteLane>,
    base_url: Option<String>,
    api_key: Option<String>,
}

enum RemoteLane {
    Anthropic,
    OpenAi,
    Bedrock,
}

fn resolve_remote(
    ctx: &CompactionContext,
    deps: Option<&RemoteMethodDeps>,
) -> ResolvedRemote {
    let env = read_env(deps);
    let provider = deps
        .and_then(|deps| deps.provider.clone())
        .or_else(|| {
            deps.and_then(|deps| deps.resolve_provider.as_ref())
                .and_then(|resolve| resolve(ctx))
        })
        .or_else(default_provider);
    let base_url = deps
        .and_then(|deps| deps.base_url.clone())
        .or_else(|| {
            deps.and_then(|deps| deps.resolve_base_url.as_ref())
                .and_then(|resolve| resolve(ctx))
        })
        .or_else(|| default_base_url(provider.as_deref(), &env));
    let api_key = deps
        .and_then(|deps| deps.api_key.clone())
        .or_else(|| {
            deps.and_then(|deps| deps.resolve_api_key.as_ref())
                .and_then(|resolve| resolve(ctx))
        })
        .or_else(|| resolve_api_key(provider.as_deref(), &env));

    let mut lane = None;
    match provider.as_deref() {
        Some("openai") => {
            if is_openai_compaction_lane_available(
                &ctx.model,
                base_url.as_deref(),
                api_key.as_deref(),
            ) {
                lane = Some(RemoteLane::OpenAi);
            }
        }
        Some("firstParty") | Some("vertex") => {
            let provider_str = provider.as_deref().unwrap_or("");
            if is_anthropic_compaction_lane_available(
                provider_str,
                &ctx.model,
                base_url.as_deref(),
                &env,
            ) {
                lane = Some(RemoteLane::Anthropic);
            }
        }
        Some("bedrock") => {
            let region = env
                .get("AWS_REGION")
                .and_then(|value| value.clone())
                .or_else(|| env.get("AWS_DEFAULT_REGION").and_then(|value| value.clone()));
            if is_bedrock_compaction_lane_available(&ctx.model, region.as_deref(), Some(&env)) {
                lane = Some(RemoteLane::Bedrock);
            }
        }
        _ => {}
    }

    ResolvedRemote {
        provider,
        lane,
        base_url,
        api_key,
    }
}

/// TS `isRemoteMethodAvailable(ctx, deps?)`。
pub fn is_remote_method_available(
    ctx: &CompactionContext,
    deps: Option<&RemoteMethodDeps>,
) -> bool {
    let env = read_env(deps);
    if !is_remote_compaction_enabled(&env) {
        return false;
    }
    if let Some(deps) = deps {
        if let Some(is_available) = deps.is_available.as_ref() {
            return is_available(ctx);
        }
    }
    resolve_remote(ctx, deps).lane.is_some()
}

/// TS `RemoteCut`：算好的 remote 边界——被摘要的前缀与保留的尾部。
pub struct RemoteCut {
    /// remote 摘要替换的前导消息；请求只发这些。
    pub messages: Vec<Message>,
    /// `messages` 的长度；等价于 `ctx.messages.slice(0, index)`。
    pub index: usize,
    pub cut_point: CutPoint,
    pub kept: Vec<Message>,
    pub tokens_after: i64,
}

/// TS `computeRemoteCut(ctx)`：保留最新 `settings.keepRecentTokens` 不动，
/// 且绝不在 assistant tool_call 与其 tool_result 之间下刀。
///
/// token 计数来自 reducers 的 `estimateMessageTokens` 配合模型 tokenizer
/// （`estimateTokens`），而不是 `JSON.stringify().length / 4`：后者对 CJK
/// 低估约 2-3 倍，会保留远超要求的上下文。
pub fn compute_remote_cut(ctx: &CompactionContext) -> RemoteCut {
    let messages = &ctx.messages;
    let keep_recent = std::cmp::max(0, ctx.settings.keep_recent_tokens) as usize;
    let count_text = |text: &str| -> usize {
        crate::services::compaction2::tokens::estimate_tokens(text, &ctx.model).tokens
    };
    let mut index = messages.len();
    if keep_recent > 0 {
        let mut accumulated = 0usize;
        for i in (0..messages.len()).rev() {
            let tokens = estimate_message_tokens(&messages[i], &count_text);
            if accumulated > 0 && accumulated + tokens > keep_recent {
                break;
            }
            accumulated += tokens;
            index = i;
        }
    }
    // 保留的 tool_result 若没有对应的 assistant tool_use，保留尾部会无法
    // 渲染；把切点回拉到调用之前。
    while index > 0
        && index < messages.len()
        && is_tool_result_message(messages.get(index))
    {
        index -= 1;
    }

    let kept = messages[index..].to_vec();
    let tokens_before = ctx.token_count_of(messages);
    let tokens_after = if kept.is_empty() {
        0
    } else {
        std::cmp::min(tokens_before, std::cmp::max(0, ctx.token_count_of(&kept)))
    };
    let first_kept = messages.get(index).map(|message| message.uuid().to_string());
    let cut_point = CutPoint {
        first_kept_entry_id: first_kept.unwrap_or_default(),
        tokens_before,
        boundary_message: create_remote_boundary(ctx, tokens_before),
    };
    RemoteCut {
        messages: messages[..index].to_vec(),
        index,
        cut_point,
        kept,
        tokens_after,
    }
}

fn create_remote_boundary(ctx: &CompactionContext, pre_tokens: i64) -> SystemMessage {
    SystemMessage {
        uuid: uuid_v4(),
        timestamp: crate::services::compact::now_ts(),
        subtype: Some("compact_boundary".to_string()),
        content: "Conversation compacted".to_string(),
        level: Some("info".to_string()),
        is_meta: Some(false),
        compact_metadata: Some(crate::services::compact::CompactMetadata {
            trigger: Some(
                if ctx.trigger
                    == crate::services::compaction2::contracts::TriggerReason::Manual
                {
                    "manual".to_string()
                } else {
                    "auto".to_string()
                },
            ),
            pre_tokens: Some(pre_tokens),
            remote: Some(true),
            ..Default::default()
        }),
        snip_metadata: None,
        logical_parent_uuid: None,
    }
}

fn is_tool_result_message(message: Option<&Message>) -> bool {
    let Some(message) = message else {
        return false;
    };
    let Message::User(user) = message else {
        return false;
    };
    let Content::Blocks(blocks) = &user.message.content else {
        return false;
    };
    blocks
        .iter()
        .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"))
}

/// TS `createRemoteMethod(deps?)`。
pub fn create_remote_method(deps: Option<RemoteMethodDeps>) -> Box<dyn CompactionMethod> {
    struct RemoteMethod {
        deps: Option<RemoteMethodDeps>,
    }

    impl CompactionMethod for RemoteMethod {
        fn name(&self) -> CompactionMethodName {
            CompactionMethodName::Remote
        }

        fn is_available(&self, ctx: &CompactionContext) -> bool {
            is_remote_method_available(ctx, self.deps.as_ref())
        }

        fn run<'a>(
            &'a self,
            ctx: &'a CompactionContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CompactionOutcome> + Send + 'a>> {
            Box::pin(async move {
                let env = read_env(self.deps.as_ref());
                if !is_remote_compaction_enabled(&env) {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Remote,
                        reason: "remote-disabled".to_string(),
                    };
                }

                let resolved = resolve_remote(ctx, self.deps.as_ref());
                let Some(lane) = resolved.lane else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Remote,
                        reason: "remote-lane-unavailable".to_string(),
                    };
                };
                let Some(fetch) = self
                    .deps
                    .as_ref()
                    .and_then(|deps| deps.fetch.as_ref())
                    .map(|fetch| fetch.as_ref() as &dyn RemoteFetch)
                else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Remote,
                        reason: "remote-transport-missing".to_string(),
                    };
                };

                // 边界在请求前算好且只算一次：lane 只序列化这个前缀，
                // outcome 复用同一切点。
                let cut = compute_remote_cut(ctx);
                let cut_point = cut.cut_point.clone();
                let tokens_after = cut.tokens_after;
                let transport = RemoteLaneTransport {
                    fetch: None,
                    api_key: resolved.api_key.clone(),
                    base_url: resolved.base_url.clone(),
                    model: ctx.model.clone(),
                };
                let request = RemoteLaneRequest {
                    ctx,
                    instructions: self
                        .deps
                        .as_ref()
                        .and_then(|deps| deps.instructions.clone())
                        .unwrap_or_else(|| DEFAULT_REMOTE_COMPACTION_INSTRUCTIONS.to_string()),
                    signal: None,
                    provider: resolved.provider.as_ref(),
                    env,
                    transport,
                    cut: Some(&cut),
                    options: RemoteLaneOptions {
                        open_ai_version: self.deps.as_ref().and_then(|deps| deps.open_ai_version),
                    },
                };
                let transport_fetch = fetch;
                let result = match lane {
                    RemoteLane::Anthropic => {
                        run_anthropic_compaction_lane(&request, transport_fetch).await
                    }
                    RemoteLane::OpenAi => {
                        run_openai_compaction_lane(&request, transport_fetch).await
                    }
                    RemoteLane::Bedrock => {
                        run_bedrock_compaction_lane(&request, transport_fetch).await
                    }
                };
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        return CompactionOutcome::Failed {
                            method: CompactionMethodName::Remote,
                            reason: error,
                        }
                    }
                };
                match result {
                    RemoteLaneResult::Skipped(reason) => CompactionOutcome::Skipped {
                        method: CompactionMethodName::Remote,
                        reason,
                    },
                    RemoteLaneResult::Success(success) => {
                        let threshold = ctx.settings.threshold_tokens.unwrap_or(0);
                        let tokens_saved =
                            std::cmp::max(0, cut_point.tokens_before - tokens_after);
                        CompactionOutcome::Compacted {
                            method: CompactionMethodName::Remote,
                            cut_point,
                            tokens_saved,
                            headroom_tokens: if threshold > 0 {
                                threshold - tokens_after
                            } else {
                                0
                            },
                            summary: Some(success.summary),
                            preserve_data: success.preserve_data,
                            warning: success.warning,
                        }
                    }
                }
            })
        }
    }

    Box::new(RemoteMethod { deps })
}
