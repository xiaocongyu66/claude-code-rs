//! `services/compaction2/methods/remote/anthropic.ts` — Anthropic 按需压缩
//! （`compact-2026-09-04` beta）。
//!
//! 请求只发要摘要的前缀（即切点）与顶层 `compaction: { type: "summarize",
//! instructions }` 参数；保留的尾部从不发送。API 回一个签名
//! `compaction` 内容块；块（content、signature、可选 tool_changes）存进
//! `preserveData`，压缩能力端点上的后续轮可原样重放。
//!
//! 与 oh-my-pi 的差异：`CompactionContext` 只暴露消息数组，没有 system
//! prompt、工具定义或 thinking 设置，所以本请求只摘要切出的前缀；omp
//! 重放的是活动请求字节，这里刻意不伪造。header 名已对照参考实现：
//! `anthropic-beta`（Vertex rawPredict 把 `anthropic_beta` 放进请求体）。

use std::collections::HashMap;

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::methods::remote::{
    compute_remote_cut, RemoteFetch, RemoteFetchInit, RemoteLaneRequest, RemoteLaneResult,
    RemoteLaneSuccess,
};

/// TS `ANTHROPIC_BETA_HEADER`。
pub const ANTHROPIC_BETA_HEADER: &str = "anthropic-beta";
/// TS `ANTHROPIC_COMPACTION_BETA`。
pub const ANTHROPIC_COMPACTION_BETA: &str = "compact-2026-09-04";
/// TS `ANTHROPIC_LEGACY_COMPACTION_BETA`。
pub const ANTHROPIC_LEGACY_COMPACTION_BETA: &str = "compact-2026-01-12";
/// TS `ANTHROPIC_COMPACTION_PRESERVE_KEY`。
pub const ANTHROPIC_COMPACTION_PRESERVE_KEY: &str = "anthropicCompaction";

const DEFAULT_ANTHROPIC_MAX_TOKENS: i64 = 8192;

/// TS `AnthropicCompactionBlock`。
#[derive(Debug, Clone)]
pub struct AnthropicCompactionBlock {
    pub content: String,
    pub signature: String,
    pub tool_changes: Option<Vec<Value>>,
    pub encrypted_content: Option<String>,
}

/// TS `AnthropicCompactionPreserveData`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicCompactionPreserveData {
    pub provider: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_changes: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_tokens: Option<i64>,
}

/// beta 的保守模型门控。目录规则是 Opus 4.6+、Sonnet 4.6+、
/// Fable/Mythos 5+；解析不了的都不支持。
pub fn supports_anthropic_server_compaction(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    if let Some(captures) = regex::Regex::new(r"claude-(opus|sonnet)-(\d+)[.-](\d+)")
        .ok()
        .and_then(|re| re.captures(&id))
    {
        let major = captures[2].parse::<i64>().unwrap_or(0);
        let minor = captures[3].parse::<i64>().unwrap_or(0);
        return major > 4 || (major == 4 && minor >= 6);
    }
    if let Some(captures) = regex::Regex::new(r"claude-(fable|mythos)-(\d+)")
        .ok()
        .and_then(|re| re.captures(&id))
    {
        return captures[2].parse::<i64>().map(|version| version >= 5).unwrap_or(false);
    }
    false
}

/// 端点门控：只有已知的 Anthropic API host 算第一方。
pub fn is_known_anthropic_endpoint(base_url: Option<&str>) -> bool {
    let Some(base_url) = base_url else {
        return true;
    };
    if base_url.trim().is_empty() {
        return true;
    }
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    host == "api.anthropic.com"
}

/// TS `resolveAnthropicMessagesEndpoint(input)`。
pub fn resolve_anthropic_messages_endpoint(
    provider: &str,
    base_url: Option<&str>,
    model: &str,
    env: &HashMap<String, Option<String>>,
) -> Option<String> {
    let env_value = |key: &str| {
        env.get(key)
            .and_then(|value| value.clone())
            .filter(|value| !value.is_empty())
    };
    let base = base_url.map(|base| base.trim_end_matches('/').to_string());
    if let Some(base) = base {
        if provider == "vertex" {
            // 调用方提供的 Vertex 端点已是完整路径。
            if base.contains(":rawPredict") {
                return Some(base);
            }
            return Some(format!("{base}/v1/messages"));
        }
        return Some(if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        });
    }
    match provider {
        "firstParty" => Some("https://api.anthropic.com/v1/messages".to_string()),
        "vertex" => {
            let project = env_value("ANTHROPIC_VERTEX_PROJECT_ID")?;
            let region = env_value("CLOUD_ML_REGION").unwrap_or_else(|| "us-east5".to_string());
            let model = model.rsplit('/').next().unwrap_or(model);
            Some(format!(
                "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict"
            ))
        }
        _ => None,
    }
}

/// TS `isAnthropicCompactionLaneAvailable(input)`。
pub fn is_anthropic_compaction_lane_available(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    env: &HashMap<String, Option<String>>,
) -> bool {
    if !supports_anthropic_server_compaction(model) {
        return false;
    }
    if provider == "firstParty" {
        return is_known_anthropic_endpoint(base_url);
    }
    if provider == "vertex" {
        return resolve_anthropic_messages_endpoint("vertex", base_url, model, env).is_some();
    }
    false
}

/// TS `ConvertedAnthropicMessages`。
#[derive(Debug, Clone)]
struct ConvertedAnthropicMessages {
    messages: Vec<Value>,
    unsupported: Option<String>,
}

fn record_of(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn text_of(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|block| {
            let record = record_of(block)?;
            if record.get("type").and_then(Value::as_str) == Some("text") {
                record.get("text").and_then(Value::as_str).map(String::from)
            } else {
                None
            }
        })
        .collect()
}

/// TS `toAnthropicMessages(messages)`：把 CCH 的 Anthropic 形态消息转成
/// Messages API 请求体。
pub fn to_anthropic_messages(messages: &[Message]) -> ConvertedAnthropicMessages {
    let mut converted: Vec<Value> = Vec::new();
    for message in messages {
        let role = match message {
            Message::Assistant(_) => Some("assistant"),
            Message::User(_) => Some("user"),
            _ => None,
        };
        let Some(role) = role else {
            continue;
        };
        match message.content() {
            Some(Content::Text(raw)) => {
                if !raw.trim().is_empty() {
                    converted.push(serde_json::json!({
                        "role": role,
                        "content": [{ "type": "text", "text": raw }]
                    }));
                }
            }
            Some(Content::Blocks(blocks)) => {
                let mut out_blocks: Vec<Value> = Vec::new();
                let mut unsupported = false;
                for block in blocks {
                    let Some(record) = record_of(block) else {
                        continue;
                    };
                    match record.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = record.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    out_blocks.push(serde_json::json!({
                                        "type": "text",
                                        "text": text
                                    }));
                                }
                            }
                        }
                        Some("image") => {
                            let Some(source) = record.get("source").and_then(record_of) else {
                                continue;
                            };
                            if source.get("type").and_then(Value::as_str) == Some("base64") {
                                out_blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": source.get("media_type").cloned().unwrap_or(Value::Null),
                                        "data": source.get("data").cloned().unwrap_or(Value::Null),
                                    }
                                }));
                            }
                        }
                        Some("tool_use") => {
                            out_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": record.get("id").cloned().unwrap_or(Value::Null),
                                "name": record.get("name").cloned().unwrap_or(Value::Null),
                                "input": record.get("input").cloned().unwrap_or_else(|| serde_json::json!({})),
                            }));
                        }
                        Some("tool_result") => {
                            out_blocks.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": record.get("tool_use_id").cloned().unwrap_or(Value::Null),
                                "content": record.get("content").cloned().unwrap_or_else(|| serde_json::json!("")),
                            }));
                        }
                        // reasoning 不是摘要请求的一部分。
                        Some("thinking") | Some("redacted_thinking") => {}
                        _ => {
                            // 未知块类型无法安全重放。
                            return ConvertedAnthropicMessages {
                                messages: Vec::new(),
                                unsupported: Some(
                                    "anthropic-unsupported-content-block".to_string(),
                                ),
                            };
                        }
                    }
                }
                if !out_blocks.is_empty() {
                    converted.push(serde_json::json!({
                        "role": role,
                        "content": out_blocks
                    }));
                }
                let _ = unsupported;
            }
            None => {}
        }
    }
    if converted.is_empty() {
        return ConvertedAnthropicMessages {
            messages: Vec::new(),
            unsupported: Some("anthropic-no-summarizable-messages".to_string()),
        };
    }
    ConvertedAnthropicMessages {
        messages: converted,
        unsupported: None,
    }
}

/// TS `BuildAnthropicCompactionRequestInput`。
#[derive(Debug, Clone)]
pub struct BuildAnthropicCompactionRequestInput<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub instructions: &'a str,
    pub max_tokens: Option<i64>,
    pub system: Option<&'a str>,
    pub tools: Option<&'a [Value]>,
    pub stream: bool,
    /// Vertex rawPredict 把 beta 放进请求体而非 header。
    pub vertex: bool,
}

/// TS `builtAnthropicCompactionRequest`。
#[derive(Debug, Clone)]
pub struct BuiltAnthropicCompactionRequest {
    pub body: Value,
    pub unsupported: Option<String>,
}

/// TS `buildAnthropicCompactionRequest(input)`。
pub fn build_anthropic_compaction_request(
    input: &BuildAnthropicCompactionRequestInput<'_>,
) -> BuiltAnthropicCompactionRequest {
    let converted = to_anthropic_messages(input.messages);
    if let Some(unsupported) = converted.unsupported {
        return BuiltAnthropicCompactionRequest {
            body: Value::Object(serde_json::Map::new()),
            unsupported: Some(unsupported),
        };
    }
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), Value::String(input.model.to_string()));
    body.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(
            input.max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS),
        )),
    );
    body.insert("messages".to_string(), Value::Array(converted.messages));
    body.insert(
        "compaction".to_string(),
        serde_json::json!({
            "type": "summarize",
            "instructions": input.instructions
        }),
    );
    body.insert("stream".to_string(), Value::Bool(input.stream));
    if let Some(system) = input.system {
        body.insert("system".to_string(), Value::String(system.to_string()));
    }
    if let Some(tools) = input.tools
        && !tools.is_empty()
    {
        body.insert("tools".to_string(), Value::Array(tools.to_vec()));
    }
    if input.vertex {
        body.insert(
            "anthropic_beta".to_string(),
            Value::Array(vec![Value::String(ANTHROPIC_COMPACTION_BETA.to_string())]),
        );
    }
    BuiltAnthropicCompactionRequest {
        body: Value::Object(body),
        unsupported: None,
    }
}

/// TS `buildAnthropicCompactionHeaders(input)`。
pub fn build_anthropic_compaction_headers(
    api_key: Option<&str>,
    vertex: bool,
    extra_betas: &[String],
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "application/json".to_string());
    if !vertex {
        // 直连 Messages API 需要 version header；Vertex rawPredict 拒收它
        // （并且用调用方注入的 bearer 鉴权而非 x-api-key）。
        headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
        if let Some(api_key) = api_key {
            headers.insert("x-api-key".to_string(), api_key.to_string());
        }
        let mut betas = vec![ANTHROPIC_COMPACTION_BETA.to_string()];
        betas.extend_from_slice(extra_betas);
        headers.insert(ANTHROPIC_BETA_HEADER.to_string(), betas.join(","));
    }
    headers
}

fn parse_compaction_block(candidate: &serde_json::Map<String, Value>) -> Option<AnthropicCompactionBlock> {
    if candidate.get("type").and_then(Value::as_str) != Some("compaction") {
        return None;
    }
    let content = candidate
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())?;
    let signature = candidate
        .get("signature")
        .and_then(Value::as_str)
        .filter(|signature| !signature.is_empty())?;
    // legacy `encrypted_content` 块只能重放，不是新摘要。
    let tool_changes = match candidate.get("tool_changes") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter(Value::is_object)
                .cloned()
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    Some(AnthropicCompactionBlock {
        content: content.to_string(),
        signature: signature.to_string(),
        tool_changes: tool_changes.filter(|changes| !changes.is_empty()),
        encrypted_content: candidate
            .get("encrypted_content")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// TS `extractAnthropicCompactionBlock(payload)`：从非流式 Messages 响应
/// 提取签名 compaction 块。
pub fn extract_anthropic_compaction_block(payload: &Value) -> Option<AnthropicCompactionBlock> {
    let message = record_of(payload)?;
    if let Some(stop_reason) = message.get("stop_reason").and_then(Value::as_str) {
        if stop_reason != "compaction" {
            return None;
        }
    }
    let content = message.get("content").and_then(Value::as_array)?;
    for item in content {
        let Some(record) = record_of(item) else {
            continue;
        };
        if let Some(block) = parse_compaction_block(record) {
            return Some(block);
        }
    }
    None
}

/// TS `parseAnthropicCompactionEvents(events)`：从流式 Messages 事件提取
/// 签名 compaction 块。块在 `content_block_start` 整体到达；
/// text delta 防御性追加。
pub fn parse_anthropic_compaction_events(events: &[Value]) -> Option<AnthropicCompactionBlock> {
    let mut block: Option<AnthropicCompactionBlock> = None;
    let mut block_index: Option<i64> = None;
    for event in events {
        let Some(record) = record_of(event) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                if let Some(candidate) = record.get("content_block").and_then(record_of) {
                    if let Some(parsed) = parse_compaction_block(candidate) {
                        block = Some(parsed);
                        block_index = record.get("index").and_then(Value::as_i64);
                    }
                }
            }
            Some("content_block_delta")
                if block.is_some()
                    && (block_index.is_none()
                        || record.get("index").and_then(Value::as_i64) == block_index) =>
            {
                if let Some(delta) = record.get("delta").and_then(record_of)
                    && delta.get("type").and_then(Value::as_str) == Some("text_delta")
                {
                    if let (Some(block), Some(text)) = (
                        block.as_ref(),
                        delta.get("text").and_then(Value::as_str),
                    ) {
                        let mut merged = block.clone();
                        merged.content.push_str(text);
                        block = Some(merged);
                    }
                }
            }
            _ => {}
        }
    }
    block
}

/// TS `withAnthropicCompactionPreserveData(preserveData?, compaction?)`。
pub fn with_anthropic_compaction_preserve_data(
    preserve_data: Option<&serde_json::Map<String, Value>>,
    compaction: Option<&AnthropicCompactionPreserveData>,
) -> Option<serde_json::Map<String, Value>> {
    if let Some(compaction) = compaction {
        let mut merged = preserve_data.cloned().unwrap_or_default();
        if let Ok(value) = serde_json::to_value(compaction) {
            merged.insert(ANTHROPIC_COMPACTION_PRESERVE_KEY.to_string(), value);
        }
        return Some(merged);
    }
    let Some(preserve_data) = preserve_data else {
        return None;
    };
    if !preserve_data.contains_key(ANTHROPIC_COMPACTION_PRESERVE_KEY) {
        return Some(preserve_data.clone());
    }
    let rest: serde_json::Map<String, Value> = preserve_data
        .iter()
        .filter(|(key, _)| *key != ANTHROPIC_COMPACTION_PRESERVE_KEY)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// TS `getPreservedAnthropicCompactionData(preserveData?)`。
pub fn get_preserved_anthropic_compaction_data(
    preserve_data: Option<&serde_json::Map<String, Value>>,
) -> Option<AnthropicCompactionPreserveData> {
    let candidate = preserve_data?.get(ANTHROPIC_COMPACTION_PRESERVE_KEY)?;
    serde_json::from_value(candidate.clone()).ok()
}

/// TS `runAnthropicCompactionLane(request)`。
pub async fn run_anthropic_compaction_lane(
    request: &RemoteLaneRequest<'_>,
    fetch: &dyn RemoteFetch,
) -> Result<RemoteLaneResult, String> {
    // 只序列化被摘要的前缀。方法在分发前算好切点；直接 lane 调用回退到
    // 现算，请求绝不退回完整历史。
    let prefix: Vec<Message> = match request.cut {
        Some(cut) => cut.messages.clone(),
        None => compute_remote_cut(request.ctx).messages,
    };
    run_anthropic_compaction_lane_with_cut(request, fetch, &prefix).await
}

async fn run_anthropic_compaction_lane_with_cut(
    request: &RemoteLaneRequest<'_>,
    fetch: &dyn RemoteFetch,
    prefix: &[Message],
) -> Result<RemoteLaneResult, String> {
    if prefix.is_empty() {
        return Ok(RemoteLaneResult::Skipped(
            "anthropic-empty-prefix".to_string(),
        ));
    }
    let vertex = request.provider.map(String::as_str) == Some("vertex");
    let built = build_anthropic_compaction_request(&BuildAnthropicCompactionRequestInput {
        model: &request.ctx.model,
        messages: prefix,
        instructions: &request.instructions,
        max_tokens: None,
        system: None,
        tools: None,
        stream: false,
        vertex,
    });
    if let Some(unsupported) = built.unsupported {
        return Ok(RemoteLaneResult::Skipped(unsupported));
    }

    let provider = if vertex { "vertex" } else { "firstParty" };
    let endpoint = resolve_anthropic_messages_endpoint(
        provider,
        request.transport.base_url.as_deref(),
        &request.ctx.model,
        &request.env,
    )
    .ok_or("anthropic-endpoint-unresolved")?;

    let headers = build_anthropic_compaction_headers(
        request.transport.api_key.as_deref(),
        vertex,
        &[],
    );
    let body = serde_json::to_string(&built.body).map_err(|error| error.to_string())?;
    let init = RemoteFetchInit {
        method: Some("POST"),
        headers: Some(headers),
        body: Some(&body),
    };
    let response = fetch.fetch(&endpoint, init).await?;
    if !response.ok {
        let detail = error_detail(&response);
        return Err(format!(
            "Anthropic compaction failed ({} {}){}",
            response.status,
            response.status_text.as_deref().unwrap_or(""),
            detail
        ));
    }
    let payload = response.json().await.map_err(|error| error.to_string())?;

    let block = extract_anthropic_compaction_block(&payload).or_else(|| {
        if let Value::Array(events) = &payload {
            parse_anthropic_compaction_events(events)
        } else {
            None
        }
    });
    let block = block.ok_or("Anthropic compaction returned no signed summary")?;

    let used_tokens = payload
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_i64);
    let preserved = AnthropicCompactionPreserveData {
        provider: provider.to_string(),
        content: block.content.clone(),
        signature: Some(block.signature.clone()),
        tool_changes: block.tool_changes.clone(),
        encrypted_content: block.encrypted_content.clone(),
        model: Some(request.ctx.model.clone()),
        used_tokens,
    };
    Ok(RemoteLaneResult::Success(RemoteLaneSuccess {
        summary: block.content,
        preserve_data: with_anthropic_compaction_preserve_data(None, Some(&preserved)),
        warning: None,
    }))
}

fn error_detail(
    response: &crate::services::compaction2::methods::remote::RemoteFetchResponse,
) -> String {
    let detail = response.text.trim();
    if detail.is_empty() {
        String::new()
    } else {
        let first_200: String = detail.chars().take(200).collect();
        format!(": {first_200}")
    }
}
