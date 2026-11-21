//! `services/compaction2/methods/remote/openai.ts` — OpenAI Responses 远程
//! 压缩（移植自 oh-my-pi，MIT）。
//!
//! - V1（`POST /responses/compact`）：发送 Responses 原生历史，保留返回的
//!   `compaction` / `compaction_summary` 条目与 retained 消息作为
//!   `preserveData` 中的替换历史。
//! - V2（流式）：在正常 Responses 流后追加一个 `compaction_trigger` 输入
//!   条目，并要求恰好一个流式 `compaction` 输出条目。
//!
//! CCH 的 OpenAI provider 通过官方 SDK 说 Chat Completions。这里只映射
//! 支持 Responses 的已知端点；其余（未知网关、未配置 compact 端点的
//! ChatGPT Codex、不可映射内容）以 `skipped` 加原因返回。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::services::compact::{Content, Message};
use crate::services::compaction2::contracts::AbortSignalRef;
use crate::services::compaction2::methods::remote::{
    RemoteFetch, RemoteFetchInit, RemoteFetchResponse, RemoteLaneRequest, RemoteLaneResult,
    RemoteLaneSuccess,
};

/// TS `OPENAI_REMOTE_COMPACTION_PRESERVE_KEY`。
pub const OPENAI_REMOTE_COMPACTION_PRESERVE_KEY: &str = "openaiRemoteCompaction";
/// TS `V2_RETAINED_MESSAGE_TOKEN_BUDGET`。
pub const V2_RETAINED_MESSAGE_TOKEN_BUDGET: i64 = 64_000;
const IMAGE_TOKEN_ESTIMATE: i64 = 765;
const CONTEXTUAL_USER_PREFIXES: [&str; 6] = [
    "<environment_context>",
    "<user_instructions>",
    "<additional_context>",
    "<skills",
    "<token_budget>",
    "<model_switch>",
];

/// TS `OpenAiEndpointKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiEndpointKind {
    Official,
    BedrockOpenAi,
    Codex,
    Unknown,
}

fn host_and_path(base_url: &str) -> Option<(String, String)> {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    Some((authority.to_ascii_lowercase(), path.to_string()))
}

/// TS `classifyOpenAiEndpoint(baseUrl)`。
pub fn classify_openai_endpoint(base_url: Option<&str>) -> OpenAiEndpointKind {
    let Some(base_url) = base_url else {
        return OpenAiEndpointKind::Official;
    };
    if base_url.trim().is_empty() {
        return OpenAiEndpointKind::Official;
    }
    let Some((host, path)) = host_and_path(base_url) else {
        return OpenAiEndpointKind::Unknown;
    };
    if host == "api.openai.com" || host.ends_with(".api.openai.com") {
        return OpenAiEndpointKind::Official;
    }
    if host == "chatgpt.com" || path.contains("/codex") {
        return OpenAiEndpointKind::Codex;
    }
    let bedrock_host = host.contains("bedrock-runtime") || host.contains("bedrock-mantle");
    if bedrock_host && (path.contains("/openai") || path.starts_with("/v1")) {
        return OpenAiEndpointKind::BedrockOpenAi;
    }
    OpenAiEndpointKind::Unknown
}

/// TS `isOpenAiCompactionLaneAvailable(input)`。
pub fn is_openai_compaction_lane_available(
    _model: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> bool {
    match classify_openai_endpoint(base_url) {
        OpenAiEndpointKind::Unknown => false,
        // 已知端点，但 Codex 不暴露 compact 端点。
        OpenAiEndpointKind::Codex => true,
        OpenAiEndpointKind::Official | OpenAiEndpointKind::BedrockOpenAi => {
            api_key.map(|key| !key.is_empty()).unwrap_or(false)
        }
    }
}

fn record_of(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn text_of(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        let Some(record) = record_of(item) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(Value::String(text)) = record.get("text") {
                parts.push(text.clone());
            }
        }
    }
    parts.join("")
}

fn to_data_url(block: &serde_json::Map<String, Value>) -> Option<String> {
    let source = record_of(block.get("source")?)?;
    if source.get("type").and_then(Value::as_str) == Some("base64") {
        let media_type = source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = source.get("data").and_then(Value::as_str).unwrap_or_default();
        return Some(format!("data:{media_type};base64,{data}"));
    }
    None
}

/// TS `OpenAiInputBuild`。
#[derive(Debug, Clone, Default)]
pub struct OpenAiInputBuild {
    pub input: Vec<Value>,
    pub unsupported: Option<String>,
}

/// TS `buildOpenAiCompactionInput(messages)`。
///
/// 用 CCH 消息构造 Responses-API 原生历史。不可映射内容
/// （provider 原生推理、服务端工具）让 lane 跳过，而不是悄悄丢掉
/// provider 需要的状态。
pub fn build_openai_compaction_input(messages: &[Message]) -> OpenAiInputBuild {
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        let role = match message {
            Message::Assistant(_) => Some("assistant"),
            Message::User(_) => Some("user"),
            _ => None,
        };
        let Some(role) = role else {
            continue;
        };
        let raw = message.content();
        let text = match raw {
            Some(Content::Text(raw)) => Some(raw.clone()),
            _ => None,
        };
        if let Some(text) = text {
            if !text.trim().is_empty() {
                input.push(serde_json::json!({
                    "type": "message",
                    "role": role,
                    "content": [{
                        "type": if role == "assistant" { "output_text" } else { "input_text" },
                        "text": text,
                    }],
                }));
            }
            continue;
        }
        let Some(Content::Blocks(blocks)) = raw else {
            continue;
        };
        // 只由推理块组成的消息在 OpenAI wire 格式里没有可携带的内容。
        // 静默跳过会丢消息并产生空摘要请求，因此拒绝。推理与
        // text/tool 块混合的消息保留可映射部分。
        let mut has_thinking = false;
        let mut has_mappable = false;
        for block in blocks {
            let typ = block.get("type").and_then(Value::as_str);
            match typ {
                Some("thinking") | Some("redacted_thinking") => has_thinking = true,
                Some(_) => has_mappable = true,
                None => {}
            }
        }
        if has_thinking && !has_mappable {
            return OpenAiInputBuild {
                input: Vec::new(),
                unsupported: Some("openai-unsupported-thinking-block".to_string()),
            };
        }
        let mut out_blocks: Vec<Value> = Vec::new();
        for block in blocks {
            let Some(record) = record_of(block) else {
                continue;
            };
            match record.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(Value::String(text)) = record.get("text") {
                        if !text.is_empty() {
                            out_blocks.push(serde_json::json!({
                                "type": if role == "assistant" { "output_text" } else { "input_text" },
                                "text": text,
                            }));
                        }
                    }
                }
                Some("image") => {
                    if let Some(url) = to_data_url(record) {
                        out_blocks.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": url,
                        }));
                    }
                }
                Some("tool_use") => {
                    if role != "assistant" {
                        return OpenAiInputBuild {
                            input: Vec::new(),
                            unsupported: Some(
                                "openai-tool-use-in-user-message".to_string(),
                            ),
                        };
                    }
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": record.get("id").cloned().unwrap_or(Value::Null),
                        "name": record.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": serde_json::to_string(
                            record.get("input").filter(|value| !value.is_null())
                                .unwrap_or(&serde_json::json!({}))
                        ).unwrap_or_else(|_| "{}".to_string()),
                    }));
                }
                Some("tool_result") => {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": record.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "output": text_of(record.get("content")),
                    }));
                }
                // 推理块不属于 OpenAI wire 格式；跳过而不是为
                // thinking 会话拒绝整个压缩。
                Some("thinking") | Some("redacted_thinking") => {}
                _ => {
                    return OpenAiInputBuild {
                        input: Vec::new(),
                        unsupported: Some("openai-unsupported-content-block".to_string()),
                    };
                }
            }
        }
        if !out_blocks.is_empty() {
            input.push(serde_json::json!({
                "type": "message",
                "role": role,
                "content": out_blocks,
            }));
        }
    }
    if input.is_empty() {
        return OpenAiInputBuild {
            input: Vec::new(),
            unsupported: Some("openai-no-summarizable-messages".to_string()),
        };
    }
    OpenAiInputBuild {
        input,
        unsupported: None,
    }
}

/// TS `resolveOpenAiBase(baseUrl)`。
fn resolve_openai_base(base_url: Option<&str>) -> String {
    let raw = base_url
        .map(|base| base.trim())
        .filter(|base| !base.is_empty())
        .unwrap_or("https://api.openai.com/v1");
    raw.trim_end_matches('/').to_string()
}

/// TS `resolveOpenAiCompactEndpoint(baseUrl)`。
pub fn resolve_openai_compact_endpoint(base_url: Option<&str>) -> String {
    let base = resolve_openai_base(base_url);
    if base.ends_with("/v1") {
        format!("{base}/responses/compact")
    } else {
        format!("{base}/v1/responses/compact")
    }
}

/// TS `resolveOpenAiResponsesEndpoint(baseUrl)`。
pub fn resolve_openai_responses_endpoint(base_url: Option<&str>) -> String {
    let base = resolve_openai_base(base_url);
    if base.ends_with("/v1") {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
    }
}

fn open_ai_headers(api_key: Option<&str>) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "content-type".to_string(),
        "application/json".to_string(),
    );
    if let Some(api_key) = api_key {
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {api_key}"),
        );
    }
    headers
}

/// TS `shouldKeepV1OutputItem(item)`。
fn should_keep_v1_output_item(item: &serde_json::Map<String, Value>) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("compaction") | Some("compaction_summary") => true,
        Some("message") => matches!(
            item.get("role").and_then(Value::as_str),
            Some("assistant") | Some("user")
        ),
        _ => false,
    }
}

/// TS `isCompactionItem(item)`。
fn is_compaction_item(item: &serde_json::Map<String, Value>) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("compaction") => item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some(),
        Some("compaction_summary") => true,
        _ => false,
    }
}

/// TS `OpenAiRemoteCompactionPreserveData`。
#[derive(Debug, Clone)]
pub struct OpenAiRemoteCompactionPreserveData {
    pub provider: Option<String>,
    pub replacement_history: Vec<Value>,
    pub compaction_item: Value,
}

fn preserve_data_with(
    preserve_data: Option<&serde_json::Map<String, Value>>,
    compaction: Option<&OpenAiRemoteCompactionPreserveData>,
) -> Option<serde_json::Map<String, Value>> {
    if let Some(compaction) = compaction {
        let mut merged = preserve_data.cloned().unwrap_or_default();
        let mut value = serde_json::Map::new();
        if let Some(provider) = compaction.provider.as_ref() {
            value.insert("provider".to_string(), Value::String(provider.clone()));
        }
        value.insert(
            "replacementHistory".to_string(),
            Value::Array(compaction.replacement_history.clone()),
        );
        value.insert("compactionItem".to_string(), compaction.compaction_item.clone());
        merged.insert(
            OPENAI_REMOTE_COMPACTION_PRESERVE_KEY.to_string(),
            Value::Object(value),
        );
        return Some(merged);
    }
    let Some(preserve_data) = preserve_data else {
        return None;
    };
    if !preserve_data.contains_key(OPENAI_REMOTE_COMPACTION_PRESERVE_KEY) {
        return Some(preserve_data.clone());
    }
    let rest: serde_json::Map<String, Value> = preserve_data
        .iter()
        .filter(|(key, _)| *key != OPENAI_REMOTE_COMPACTION_PRESERVE_KEY)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// TS `withOpenAiRemoteCompactionPreserveData(preserveData?, compaction?)`。
pub fn with_openai_remote_compaction_preserve_data(
    preserve_data: Option<&serde_json::Map<String, Value>>,
    compaction: Option<&OpenAiRemoteCompactionPreserveData>,
) -> Option<serde_json::Map<String, Value>> {
    preserve_data_with(preserve_data, compaction)
}

/// TS `getPreservedOpenAiRemoteCompactionData(preserveData?)`。
pub fn get_preserved_open_ai_remote_compaction_data(
    preserve_data: Option<&serde_json::Map<String, Value>>,
) -> Option<OpenAiRemoteCompactionPreserveData> {
    let candidate = record_of(preserve_data?.get(OPENAI_REMOTE_COMPACTION_PRESERVE_KEY)?)?;
    let Some(Value::Array(replacement_history)) = candidate.get("replacementHistory") else {
        return None;
    };
    let item = record_of(candidate.get("compactionItem")?)?;
    if !is_compaction_item(item) {
        return None;
    }
    Some(OpenAiRemoteCompactionPreserveData {
        provider: candidate
            .get("provider")
            .and_then(Value::as_str)
            .map(String::from),
        replacement_history: replacement_history.clone(),
        compaction_item: Value::Object(item.clone()),
    })
}

fn error_detail(response: &RemoteFetchResponse) -> String {
    response
        .status_text
        .as_deref()
        .map(|status_text| format!(" ({status_text})"))
        .unwrap_or_default()
}

/// TS `runV1(request, input)`。
async fn run_v1(
    request: &RemoteLaneRequest<'_>,
    input: &[Value],
    fetch: &dyn RemoteFetch,
) -> Result<RemoteLaneResult, String> {
    let endpoint = resolve_openai_compact_endpoint(request.transport.base_url.as_deref());
    let body = serde_json::json!({
        "model": request.transport.model,
        "input": input,
        "instructions": request.instructions,
    });
    let response = fetch
        .fetch(
            &endpoint,
            RemoteFetchInit {
                method: Some("POST"),
                headers: Some(open_ai_headers(request.transport.api_key.as_deref())),
                body: Some(&body.to_string()),
            },
        )
        .await?;
    if !response.ok {
        let detail = response.text.chars().take(200).collect::<String>();
        return Err(format!(
            "OpenAI remote compaction failed ({} {}){}",
            response.status,
            error_detail(&response),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    let data = record_of(&response.json().await.map_err(|error| error.to_string())?)
        .cloned()
        .unwrap_or_default();
    let raw_output = match data.get("output") {
        Some(Value::Array(output)) => output.clone(),
        _ => Vec::new(),
    };
    let replacement_history: Vec<Value> = raw_output
        .iter()
        .filter(|item| {
            record_of(item)
                .map(should_keep_v1_output_item)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let compaction_item = replacement_history
        .iter()
        .rev()
        .find(|item| record_of(item).map(is_compaction_item).unwrap_or(false))
        .cloned()
        .ok_or("Remote compaction response missing compaction item")?;
    let summary = record_of(&compaction_item)
        .and_then(|item| item.get("summary").and_then(Value::as_str))
        .filter(|summary| !summary.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "Conversation compacted by the provider.".to_string());
    Ok(RemoteLaneResult::Success(RemoteLaneSuccess {
        summary,
        preserve_data: with_openai_remote_compaction_preserve_data(
            None,
            Some(&OpenAiRemoteCompactionPreserveData {
                provider: Some("openai".to_string()),
                replacement_history,
                compaction_item,
            }),
        ),
        warning: None,
    }))
}

/// TS `OpenAiV2Usage`。
#[derive(Debug, Clone)]
pub struct OpenAiV2Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
}

/// TS `OpenAiV2Collection`。
#[derive(Debug, Clone)]
pub struct OpenAiV2Collection {
    pub compaction_item: Option<Value>,
    pub usage: Option<OpenAiV2Usage>,
    pub used_tokens: i64,
    pub output_item_count: usize,
    pub completed: bool,
}

/// TS `parseOpenAiCompactionSse(text)`：解析 Responses SSE 流的 `data:` 帧。
pub fn parse_openai_compaction_sse(text: &str) -> Result<Vec<Value>, String> {
    let mut events: Vec<Value> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let dispatch = |data_lines: &mut Vec<String>, events: &mut Vec<Value>| -> Result<(), String> {
        if data_lines.is_empty() {
            return Ok(());
        }
        let data = data_lines.join("\n");
        data_lines.clear();
        if data == "[DONE]" {
            return Ok(());
        }
        let parsed: Value = serde_json::from_str(&data)
            .map_err(|_| "OpenAI V2 compaction stream parse failed".to_string())?;
        if parsed.is_object() {
            events.push(parsed);
        }
        Ok(())
    };
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            dispatch(&mut data_lines, &mut events)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }
    dispatch(&mut data_lines, &mut events)?;
    Ok(events)
}

/// TS `parseUsage(event)`。
fn parse_usage(event: &serde_json::Map<String, Value>) -> Option<OpenAiV2Usage> {
    let response = record_of(event.get("response")?)?;
    let usage = record_of(response.get("usage")?)?;
    let input_tokens = usage.get("input_tokens")?.as_i64()?;
    let output_tokens = usage.get("output_tokens")?.as_i64()?;
    let total_tokens = usage.get("total_tokens")?.as_i64()?;
    let input_details = usage
        .get("input_tokens_details")
        .and_then(|details| record_of(details));
    let output_details = usage
        .get("output_tokens_details")
        .and_then(|details| record_of(details));
    Some(OpenAiV2Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_input_tokens: input_details
            .and_then(|details| details.get("cached_tokens")?.as_i64()),
        reasoning_output_tokens: output_details
            .and_then(|details| details.get("reasoning_tokens")?.as_i64()),
    })
}

/// TS `collectOpenAiCompactionV2Events(events)`。
///
/// 把 Responses 事件折叠为 V2 要求的单一 compaction 条目。
pub fn collect_openai_compaction_v2_events(events: &[Value]) -> Result<OpenAiV2Collection, String> {
    let mut collection = OpenAiV2Collection {
        compaction_item: None,
        usage: None,
        used_tokens: 0,
        output_item_count: 0,
        completed: false,
    };
    let mut compaction_items: Vec<Value> = Vec::new();
    for event in events {
        let Some(record) = record_of(event) else {
            continue;
        };
        let typ = record.get("type").and_then(Value::as_str);
        match typ {
            Some("response.output_item.done") => {
                collection.output_item_count += 1;
                if let Some(item) = record.get("item").and_then(|item| record_of(item)) {
                    if item.get("type").and_then(Value::as_str) == Some("compaction") {
                        compaction_items.push(Value::Object(item.clone()));
                    }
                }
            }
            Some("response.completed") | Some("response.done") => {
                collection.completed = true;
                collection.usage = parse_usage(record);
            }
            Some("response.failed") | Some("response.incomplete") => {
                return Err(format!(
                    "V2 compaction stream {}",
                    typ.unwrap_or("unknown")
                ));
            }
            Some("error") => {
                let message = record
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(format!("V2 compaction stream error: {message}"));
            }
            _ => {}
        }
    }
    if !collection.completed {
        return Err("V2 compaction stream closed before response.completed".to_string());
    }
    if compaction_items.len() != 1 {
        return Err(format!(
            "V2 compaction expected exactly one compaction output item, got {}",
            compaction_items.len()
        ));
    }
    collection.compaction_item = compaction_items.into_iter().next();
    collection.used_tokens = collection
        .usage
        .as_ref()
        .map(|usage| usage.input_tokens)
        .unwrap_or(0);
    Ok(collection)
}

/// TS `collectOpenAiCompactionV2Body(text)`。
///
/// 同时接受原始 SSE body 与非流式 Responses JSON body。
pub fn collect_openai_compaction_v2_body(text: &str) -> Result<OpenAiV2Collection, String> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        let body: Value = serde_json::from_str(trimmed)
            .map_err(|_| "V2 compaction body parse failed".to_string())?;
        let body_record = record_of(&body).cloned().unwrap_or_default();
        let output = match body_record.get("output") {
            Some(Value::Array(output)) => output.clone(),
            _ => Vec::new(),
        };
        let mut events: Vec<Value> = output
            .iter()
            .filter_map(|item| record_of(item))
            .map(|item| {
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": item,
                })
            })
            .collect();
        events.push(serde_json::json!({
            "type": "response.completed",
            "response": { "usage": body_record.get("usage").cloned().unwrap_or(Value::Null) },
        }));
        return collect_openai_compaction_v2_events(&events);
    }
    collect_openai_compaction_v2_events(&parse_openai_compaction_sse(text)?)
}

/// TS `isRetainedUserItem(item)`。
fn is_retained_user_item(item: &serde_json::Map<String, Value>) -> bool {
    let is_message = item.get("type").and_then(Value::as_str) == Some("message")
        || (item.get("type").is_none() && item.get("role").and_then(Value::as_str).is_some());
    if !is_message || item.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(Value::Array(content)) = item.get("content") else {
        return true;
    };
    !content.iter().any(|part| {
        let Some(record) = record_of(part) else {
            return false;
        };
        if record.get("type").and_then(Value::as_str) != Some("input_text") {
            return false;
        }
        let text = record
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start()
            .to_ascii_lowercase();
        CONTEXTUAL_USER_PREFIXES
            .iter()
            .any(|prefix| text.starts_with(prefix))
    })
}

/// TS `approxTokenCount(text)`。
fn approx_token_count(text: &str) -> i64 {
    ((text.chars().count() as f64) / 4.0).ceil() as i64
}

/// TS `itemTokenCount(item)`。
fn item_token_count(item: &serde_json::Map<String, Value>) -> i64 {
    let Some(Value::Array(content)) = item.get("content") else {
        return 0;
    };
    let mut tokens = 0i64;
    for part in content {
        let Some(record) = record_of(part) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("input_image") => tokens += IMAGE_TOKEN_ESTIMATE,
            Some("input_text") | Some("output_text") => {
                tokens += approx_token_count(record.get("text").and_then(Value::as_str).unwrap_or(""))
            }
            _ => {}
        }
    }
    tokens
}

/// TS `truncateTextToBudget(text, maxTokens)`。
fn truncate_text_to_budget(text: &str, max_tokens: i64) -> String {
    if max_tokens <= 0 {
        return String::new();
    }
    let max_chars = max_tokens * 4;
    if text.chars().count() <= max_chars as usize {
        return text.to_string();
    }
    text.chars().take(max_chars as usize).collect()
}

/// TS `truncateItemToBudget(item, maxTokens)`。
fn truncate_item_to_budget(
    item: &serde_json::Map<String, Value>,
    max_tokens: i64,
) -> Option<Value> {
    let Some(Value::Array(content)) = item.get("content") else {
        return None;
    };
    let mut remaining = max_tokens;
    let mut kept: Vec<Value> = Vec::new();
    for part in content {
        let Some(record) = record_of(part) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("input_image") => {
                if remaining < IMAGE_TOKEN_ESTIMATE {
                    continue;
                }
                kept.push(part.clone());
                remaining -= IMAGE_TOKEN_ESTIMATE;
            }
            Some("input_text") | Some("output_text") => {
                if remaining <= 0 {
                    continue;
                }
                let text = record.get("text").and_then(Value::as_str).unwrap_or("");
                let tokens = approx_token_count(text);
                if tokens <= remaining {
                    kept.push(part.clone());
                    remaining -= tokens;
                } else {
                    let truncated = truncate_text_to_budget(text, remaining);
                    if !truncated.is_empty() {
                        let mut replacement = record.clone();
                        replacement.insert("text".to_string(), Value::String(truncated));
                        kept.push(Value::Object(replacement));
                    }
                    remaining = 0;
                }
            }
            _ => {}
        }
    }
    if kept.is_empty() {
        return None;
    }
    let mut replacement = item.clone();
    replacement.insert("content".to_string(), Value::Array(kept));
    Some(Value::Object(replacement))
}

/// TS `buildOpenAiV2ReplacementHistory(input, compactionItem, retainedMessageBudget?)`。
pub fn build_openai_v2_replacement_history(
    input: &[Value],
    compaction_item: &Value,
    retained_message_budget: Option<i64>,
) -> (Vec<Value>, usize) {
    let budget = std::cmp::min(
        V2_RETAINED_MESSAGE_TOKEN_BUDGET,
        std::cmp::max(1, retained_message_budget.unwrap_or(V2_RETAINED_MESSAGE_TOKEN_BUDGET)),
    );
    let mut remaining = budget;
    let mut kept_reversed: Vec<Value> = Vec::new();
    for item in input.iter().rev() {
        if remaining == 0 {
            break;
        }
        let Some(record) = record_of(item) else {
            continue;
        };
        if !is_retained_user_item(record) {
            continue;
        }
        let tokens = std::cmp::max(item_token_count(record), 1);
        if tokens <= remaining {
            kept_reversed.push(item.clone());
            remaining -= tokens;
        } else {
            if let Some(truncated) = truncate_item_to_budget(record, remaining) {
                kept_reversed.push(truncated);
            }
            remaining = 0;
        }
    }
    let mut replacement_history: Vec<Value> = kept_reversed.into_iter().rev().collect();
    let retained_image_count = replacement_history
        .iter()
        .filter_map(|item| record_of(item))
        .filter_map(|item| match item.get("content") {
            Some(Value::Array(content)) => Some(content),
            _ => None,
        })
        .flat_map(|content| content.iter())
        .filter(|part| {
            record_of(part)
                .map(|record| record.get("type").and_then(Value::as_str) == Some("input_image"))
                .unwrap_or(false)
        })
        .count();
    replacement_history.push(compaction_item.clone());
    (replacement_history, retained_image_count)
}

const V2_TIMEOUT_MS: u64 = 300_000;
const V2_MAX_RETRIES: u32 = 2;

/// TS `fetchV2Once`：单次 V2 请求（V2_TIMEOUT_MS 超时；abort 立即失败）。
async fn fetch_v2_once(
    fetch: &dyn RemoteFetch,
    endpoint: &str,
    api_key: Option<&str>,
    body: &Value,
    signal: Option<&dyn AbortSignalRef>,
) -> Result<RemoteFetchResponse, String> {
    if signal.map(|signal| signal.aborted()).unwrap_or(false) {
        return Err("AbortError: V2 remote compaction aborted".to_string());
    }
    let body_string = body.to_string();
    let request = fetch.fetch(
        endpoint,
        RemoteFetchInit {
            method: Some("POST"),
            headers: Some(open_ai_headers(api_key)),
            body: Some(&body_string),
        },
    );
    tokio::time::timeout(Duration::from_millis(V2_TIMEOUT_MS), request)
        .await
        .map_err(|_| "V2 remote compaction timed out".to_string())?
}

/// TS `runV2(request, input)`。
async fn run_v2(
    request: &RemoteLaneRequest<'_>,
    input: &[Value],
    fetch: &dyn RemoteFetch,
) -> Result<RemoteLaneResult, String> {
    let endpoint = resolve_openai_responses_endpoint(request.transport.base_url.as_deref());
    let mut input_with_trigger: Vec<Value> = input.to_vec();
    input_with_trigger.push(serde_json::json!({ "type": "compaction_trigger" }));
    let body = serde_json::json!({
        "model": request.transport.model,
        "input": input_with_trigger,
        "instructions": request.instructions,
        "stream": true,
        "store": false,
    });

    let mut response: Option<RemoteFetchResponse> = None;
    let mut last_error: Option<String> = None;
    for attempt in 0..=V2_MAX_RETRIES {
        match fetch_v2_once(
            fetch,
            &endpoint,
            request.transport.api_key.as_deref(),
            &body,
            request.signal,
        )
        .await
        {
            Ok(result) => {
                response = Some(result);
                break;
            }
            Err(error) => {
                // 绝不把 abort（用户 ESC）吞进一次重复的 V1 请求。
                if request.signal.map(|signal| signal.aborted()).unwrap_or(false)
                    || error.starts_with("AbortError")
                {
                    return Err(error);
                }
                last_error = Some(error);
                if attempt == V2_MAX_RETRIES {
                    return Err(last_error.unwrap_or_else(|| {
                        "V2 remote compaction failed".to_string()
                    }));
                }
                tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
            }
        }
    }
    let Some(response) = response else {
        return Err(last_error.unwrap_or_else(|| "V2 remote compaction failed".to_string()));
    };
    if !response.ok {
        let detail = response.text.chars().take(200).collect::<String>();
        return Err(format!(
            "V2 remote compaction failed ({} {}){}",
            response.status,
            error_detail(&response),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    let collection = collect_openai_compaction_v2_body(&response.text)?;
    let Some(compaction_item) = collection.compaction_item else {
        return Err("V2 remote compaction returned no compaction item".to_string());
    };
    let (replacement_history, retained_image_count) =
        build_openai_v2_replacement_history(input, &compaction_item, None);
    let summary = record_of(&compaction_item)
        .and_then(|item| item.get("summary").and_then(Value::as_str))
        .filter(|summary| !summary.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "Conversation compacted by the provider.".to_string());

    let mut preserve = serde_json::Map::new();
    let mut value = serde_json::Map::new();
    value.insert("version".to_string(), Value::String("v2".to_string()));
    value.insert("provider".to_string(), Value::String("openai".to_string()));
    value.insert("replacementHistory".to_string(), Value::Array(replacement_history));
    value.insert("compactionItem".to_string(), compaction_item);
    value.insert("usedTokens".to_string(), Value::from(collection.used_tokens));
    if let Some(usage) = collection.usage {
        let mut usage_value = serde_json::Map::new();
        usage_value.insert("inputTokens".to_string(), Value::from(usage.input_tokens));
        usage_value.insert("outputTokens".to_string(), Value::from(usage.output_tokens));
        usage_value.insert("totalTokens".to_string(), Value::from(usage.total_tokens));
        if let Some(cached) = usage.cached_input_tokens {
            usage_value.insert("cachedInputTokens".to_string(), Value::from(cached));
        }
        if let Some(reasoning) = usage.reasoning_output_tokens {
            usage_value.insert("reasoningOutputTokens".to_string(), Value::from(reasoning));
        }
        value.insert("usage".to_string(), Value::Object(usage_value));
    }
    value.insert("retainedImageCount".to_string(), Value::from(retained_image_count as i64));
    preserve.insert(
        OPENAI_REMOTE_COMPACTION_PRESERVE_KEY.to_string(),
        Value::Object(value),
    );

    Ok(RemoteLaneResult::Success(RemoteLaneSuccess {
        summary,
        preserve_data: Some(preserve),
        warning: None,
    }))
}

/// TS `runOpenAiCompactionLane(request)`。
pub async fn run_openai_compaction_lane(
    request: &RemoteLaneRequest<'_>,
    fetch: &dyn RemoteFetch,
) -> Result<RemoteLaneResult, String> {
    let kind = classify_openai_endpoint(request.transport.base_url.as_deref());
    if kind == OpenAiEndpointKind::Unknown {
        return Ok(RemoteLaneResult::Skipped(
            "openai-endpoint-unknown".to_string(),
        ));
    }
    if kind == OpenAiEndpointKind::Codex {
        return Ok(RemoteLaneResult::Skipped(
            "openai-codex-compact-unsupported".to_string(),
        ));
    }
    let built = build_openai_compaction_input(&request.ctx.messages);
    if let Some(unsupported) = built.unsupported {
        return Ok(RemoteLaneResult::Skipped(unsupported));
    }

    match request.options.open_ai_version {
        Some(crate::services::compaction2::methods::remote::OpenAiVersion::V1) => {
            run_v1(request, &built.input, fetch).await
        }
        Some(crate::services::compaction2::methods::remote::OpenAiVersion::V2) => {
            run_v2(request, &built.input, fetch).await
        }
        Some(crate::services::compaction2::methods::remote::OpenAiVersion::Auto)
        | None => match run_v2(request, &built.input, fetch).await {
            Ok(result) => Ok(result),
            // 绝不把 abort（用户 ESC）吞进一次重复的 V1 请求。
            Err(error)
                if request
                    .signal
                    .map(|signal| signal.aborted())
                    .unwrap_or(false)
                    || error.starts_with("AbortError") =>
            {
                Err(error)
            }
            Err(_error) => run_v1(request, &built.input, fetch).await,
        },
    }
}
