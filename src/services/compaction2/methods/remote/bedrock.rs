//! `services/compaction2/methods/remote/bedrock.ts` — Amazon Bedrock Converse
//! 压缩 lane。
//!
//! 参考实现经 Bedrock Runtime 侧请求压缩 Converse 模型。CCH 已有
//! `createBedrockRuntimeClient()`（主 Bedrock provider 的同一客户端/
//! 凭证链），lane 组装标准 Converse 请求；transport 可用时由
//! `transport.rs` 转发，否则 lane 保持 skipped，方法链继续。
//!
//! 侧请求由 `CCH_BEDROCK_COMPACTION=1` 独立门控：为 Anthropic/OpenAI
//! 开启 remote compaction 不得顺带启动 Bedrock 侧请求。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::services::compact::Message;
use crate::services::compaction2::methods::remote::{
    compute_remote_cut, RemoteFetch, RemoteFetchInit, RemoteLaneRequest, RemoteLaneResult,
    RemoteLaneSuccess,
};

/// TS `BEDROCK_COMPACTION_ENV`。
pub const BEDROCK_COMPACTION_ENV: &str = "CCH_BEDROCK_COMPACTION";
/// TS `BEDROCK_COMPACTION_TIMEOUT_ENV`。
pub const BEDROCK_COMPACTION_TIMEOUT_ENV: &str = "CCH_BEDROCK_COMPACTION_TIMEOUT_MS";
/// TS `DEFAULT_BEDROCK_COMPACTION_TIMEOUT_MS`。
pub const DEFAULT_BEDROCK_COMPACTION_TIMEOUT_MS: u64 = 60_000;
/// TS `BEDROCK_COMPACTION_PRESERVE_KEY`。
pub const BEDROCK_COMPACTION_PRESERVE_KEY: &str = "bedrock";
/// TS `BEDROCK_COMPACTION_SKIPPED_REASON`。
pub const BEDROCK_COMPACTION_SKIPPED_REASON: &str = "bedrock-converse-transport-unavailable";
/// TS `BEDROCK_COMPACTION_DISABLED_REASON`。
pub const BEDROCK_COMPACTION_DISABLED_REASON: &str = "bedrock-compaction-disabled";
/// TS `BEDROCK_COMPACTION_EMPTY_REASON`。
pub const BEDROCK_COMPACTION_EMPTY_REASON: &str = "bedrock-no-summarizable-messages";

const DEFAULT_MAX_TOKENS: i64 = 8192;

/// 空文本 tool result 的占位符。Converse 拒收空 `text` 块，因此省略/空
/// 结果不能原样发送。
pub const BEDROCK_EMPTY_TOOL_RESULT_TEXT: &str = "(no output)";

/// 代表完成态的 Converse stop reason。其余（`max_tokens`、
/// `content_filtered`、`guardrail_intervened`……）意味着摘要被截断或
/// 扣留，必须降级而不是接受。
const SUCCESSFUL_STOP_REASONS: [&str; 2] = ["end_turn", "stop_sequence"];

/// Bedrock Converse 摘要不用 Anthropic 的服务端 compaction beta，因此
/// Anthropic 模型目录（Opus/Sonnet 4.6+ ……）不适用。任何经 Converse
/// 可达的 Claude 模型都可摘要；只有明显非 Claude 的模型 id 被跳过。
pub fn supports_bedrock_converse_compaction(model: &str) -> bool {
    let pattern = regex::Regex::new(r"claude[-.]").unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    pattern.is_match(&model.to_ascii_lowercase())
}

/// TS `isBedrockCompactionEnabled(env?)`。
pub fn is_bedrock_compaction_enabled(env: &HashMap<String, Option<String>>) -> bool {
    env.get(BEDROCK_COMPACTION_ENV)
        .and_then(|value| value.as_ref())
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// TS `isBedrockCompactionLaneAvailable(input)`。
pub fn is_bedrock_compaction_lane_available(
    model: &str,
    region: Option<&str>,
    env: Option<&HashMap<String, Option<String>>>,
) -> bool {
    let Some(region) = region else {
        return false;
    };
    if region.trim().is_empty() {
        return false;
    }
    if let Some(env) = env {
        if !is_bedrock_compaction_enabled(env) {
            return false;
        }
    }
    supports_bedrock_converse_compaction(model)
}

/// TS `BedrockConverseContentBlock`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum BedrockConverseContentBlock {
    Text { text: String },
    ToolUse {
        tool_use: BedrockToolUseContent,
    },
    ToolResult {
        tool_result: BedrockToolResultContent,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockToolUseContent {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockToolResultContent {
    pub content: Vec<BedrockTextBlock>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BedrockTextBlock {
    pub text: String,
}

/// TS `BedrockConverseMessage`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BedrockConverseMessage {
    pub role: String,
    pub content: Vec<BedrockConverseContentBlock>,
}

/// TS `BedrockConverseMapping`。
#[derive(Debug, Clone)]
pub struct BedrockConverseMapping {
    pub messages: Vec<BedrockConverseMessage>,
    pub unsupported: Option<String>,
    /// 无 Converse 对应物而被丢弃的块（非致命）。
    pub skipped_blocks: Option<usize>,
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

/// TS `toBedrockConverseMessages(messages)`：把 CCH 的 Anthropic 形态内容
/// 块映射到 Converse 块。
///
/// 只翻译 text、tool_use 与 tool_result。摘要不需要图片，且图片 source
/// 在两个 API 间不同；reasoning 块是 provider 私有的。无 Converse 对应
/// 物的块（document、web_search_tool_result ……）跳过并计数，而不是让
/// 整个请求失败。`tools` 故意省略：`CompactionContext` 不带 tool schema，
/// 摘要请求也不需要。
pub fn to_bedrock_converse_messages(messages: &[Message]) -> BedrockConverseMapping {
    let mut converted: Vec<BedrockConverseMessage> = Vec::new();
    let mut skipped_blocks = 0usize;
    let push = |converted: &mut Vec<BedrockConverseMessage>,
                role: &str,
                block: BedrockConverseContentBlock| {
        // Converse 期望角色交替；合并同角色相邻消息。
        if converted.last_mut().map_or(false, |last| last.role == role) {
            if let Some(last) = converted.last_mut() {
                last.content.push(block);
            }
        } else {
            converted.push(BedrockConverseMessage {
                role: role.to_string(),
                content: vec![block],
            });
        }
    };

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
            Some(crate::services::compact::Content::Text(raw)) => {
                if !raw.trim().is_empty() {
                    push(
                        &mut converted,
                        role,
                        BedrockConverseContentBlock::Text {
                            text: raw.to_string(),
                        },
                    );
                }
            }
            Some(crate::services::compact::Content::Blocks(blocks)) => {
                for block in blocks {
                    let Some(record) = record_of(block) else {
                        continue;
                    };
                    match record.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = record.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    push(
                                        &mut converted,
                                        role,
                                        BedrockConverseContentBlock::Text {
                                            text: text.to_string(),
                                        },
                                    );
                                }
                            }
                        }
                        Some("tool_use") => {
                            if role != "assistant" {
                                return BedrockConverseMapping {
                                    messages: Vec::new(),
                                    unsupported: Some(
                                        "bedrock-tool-use-in-user-message".to_string(),
                                    ),
                                    skipped_blocks: None,
                                };
                            }
                            push(
                                &mut converted,
                                role,
                                BedrockConverseContentBlock::ToolUse {
                                    tool_use: BedrockToolUseContent {
                                        tool_use_id: record
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        name: record
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        input: record
                                            .get("input")
                                            .cloned()
                                            .unwrap_or_else(|| Value::Object(Default::default())),
                                    },
                                },
                            );
                        }
                        Some("tool_result") => {
                            if role != "user" {
                                return BedrockConverseMapping {
                                    messages: Vec::new(),
                                    unsupported: Some(
                                        "bedrock-tool-result-in-assistant-message".to_string(),
                                    ),
                                    skipped_blocks: None,
                                };
                            }
                            let result_text = text_of(record.get("content"));
                            push(
                                &mut converted,
                                role,
                                BedrockConverseContentBlock::ToolResult {
                                    tool_result: BedrockToolResultContent {
                                        content: vec![BedrockTextBlock {
                                            text: if result_text.trim().is_empty() {
                                                BEDROCK_EMPTY_TOOL_RESULT_TEXT.to_string()
                                            } else {
                                                result_text
                                            },
                                        }],
                                        status: Some(
                                            if record.get("is_error").and_then(Value::as_bool)
                                                == Some(true)
                                            {
                                                "error".to_string()
                                            } else {
                                                "success".to_string()
                                            },
                                        ),
                                    },
                                },
                            );
                        }
                        // thinking / redacted_thinking / image：摘要请求不需要。
                        Some("thinking") | Some("redacted_thinking") | Some("image") => {}
                        _ => {
                            // 未知块类型（document、web_search_tool_result ……）无
                            // Converse 对应物。跳过并保留其余——旧前缀的摘要
                            // 不必包含每个 provider 私有产物。
                            skipped_blocks += 1;
                        }
                    }
                }
            }
            None => {}
        }
    }
    if converted.is_empty() {
        return BedrockConverseMapping {
            messages: Vec::new(),
            unsupported: Some(BEDROCK_COMPACTION_EMPTY_REASON.to_string()),
            skipped_blocks: None,
        };
    }
    BedrockConverseMapping {
        messages: converted,
        unsupported: None,
        skipped_blocks: (skipped_blocks > 0).then_some(skipped_blocks),
    }
}

/// TS `BedrockConverseCompactionSideRequest`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BedrockConverseCompactionSideRequest {
    pub model_id: String,
    pub body: Value,
    pub content_type: String,
    pub accept: String,
    /// 被丢弃的不支持块计数（非致命）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_blocks: Option<usize>,
}

/// TS `buildBedrockConverseCompactionSideRequest(request, options?)`。
pub fn build_bedrock_converse_compaction_side_request(
    request: &RemoteLaneRequest<'_>,
    messages: &[Message],
    max_tokens: Option<i64>,
) -> Option<BedrockConverseCompactionSideRequest> {
    // 只摘要切出的前缀，绝摘要完整活动历史。无预算切点的直接调用回退到
    // 方法同一边界。
    let converted = to_bedrock_converse_messages(messages);
    if converted.unsupported.is_some() {
        return None;
    }
    let instructions = request.instructions.trim();
    let mut body = serde_json::Map::new();
    if !instructions.is_empty() {
        // Converse 没有 `compaction.instructions` 字段；等价物是明确要求
        // 摘要的 system prompt。缺了它模型只会续写对话，文本不是摘要。
        body.insert(
            "system".to_string(),
            Value::Array(vec![serde_json::json!({ "text": instructions })]),
        );
    }
    body.insert(
        "messages".to_string(),
        serde_json::to_value(&converted.messages).unwrap_or(Value::Array(Vec::new())),
    );
    body.insert(
        "inferenceConfig".to_string(),
        serde_json::json!({
            "maxTokens": max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
        }),
    );
    Some(BedrockConverseCompactionSideRequest {
        model_id: request.ctx.model.clone(),
        body: Value::Object(body),
        content_type: "application/json".to_string(),
        accept: "application/json".to_string(),
        skipped_blocks: converted.skipped_blocks,
    })
}

/// TS `extractBedrockConverseSummary(payload)`：拼接
/// `output.message.content` 的文本块。
pub fn extract_bedrock_converse_summary(payload: &Value) -> Option<String> {
    let content = payload
        .get("output")
        .and_then(|output| output.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)?;
    let text = content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str).map(String::from))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// TS `resolveBedrockConverseEndpoint(input)`。
pub fn resolve_bedrock_converse_endpoint(
    base_url: Option<&str>,
    model: &str,
    region: Option<&str>,
) -> Option<String> {
    let raw = base_url
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .map(|_| ()) // 优先用显式 base_url
        .and(base_url.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.to_string()))
        .or_else(|| {
            region.map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"))
        });
    let raw = raw?;
    let base = raw.trim_end_matches('/');
    Some(format!("{base}/model/{}/converse", urlencode(model)))
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn resolve_timeout_ms(env: &HashMap<String, Option<String>>) -> u64 {
    let Some(raw) = env.get(BEDROCK_COMPACTION_TIMEOUT_ENV).and_then(|value| value.as_ref()) else {
        return DEFAULT_BEDROCK_COMPACTION_TIMEOUT_MS;
    };
    match raw.trim().parse::<f64>() {
        Ok(parsed) if parsed.is_finite() && parsed > 0.0 => parsed.floor() as u64,
        _ => DEFAULT_BEDROCK_COMPACTION_TIMEOUT_MS,
    }
}

/// TS `BedrockConversePreserveData`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BedrockConversePreserveData {
    pub version: i64,
    pub provider: String,
    pub model: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_tokens: Option<i64>,
}

/// TS `buildBedrockPreserveData(input)`。
pub fn build_bedrock_preserve_data(input: BedrockConversePreserveData) -> serde_json::Map<String, Value> {
    let mut data = serde_json::Map::new();
    if let Ok(value) = serde_json::to_value(&input) {
        data.insert(BEDROCK_COMPACTION_PRESERVE_KEY.to_string(), value);
    }
    data
}

fn usage_of(payload: &Value) -> Option<i64> {
    payload
        .get("usage")
        .and_then(|usage| usage.get("inputTokens"))
        .and_then(Value::as_i64)
}

/// TS `runBedrockCompactionLane(request)`：跑 Converse 侧请求。错误与超时
/// 抛出，让方法链记录 failed 并降级到下一个方法。
pub async fn run_bedrock_compaction_lane(
    request: &RemoteLaneRequest<'_>,
    fetch: &dyn RemoteFetch,
) -> Result<RemoteLaneResult, String> {
    if !is_bedrock_compaction_enabled(&request.env) {
        return Ok(RemoteLaneResult::Skipped(
            BEDROCK_COMPACTION_DISABLED_REASON.to_string(),
        ));
    }

    // 只序列化被摘要的前缀（同 Anthropic lane）。方法在分发前算好切点；
    // 直接 lane 调用回退到现算。
    let prefix: Vec<Message> = match request.cut {
        Some(cut) => cut.messages.clone(),
        None => compute_remote_cut(request.ctx).messages,
    };
    if prefix.is_empty() {
        return Ok(RemoteLaneResult::Skipped(
            BEDROCK_COMPACTION_EMPTY_REASON.to_string(),
        ));
    }
    let built = build_bedrock_converse_compaction_side_request(request, &prefix, None)
        .ok_or_else(|| {
            let mapped = to_bedrock_converse_messages(&prefix);
            mapped
                .unsupported
                .unwrap_or_else(|| BEDROCK_COMPACTION_EMPTY_REASON.to_string())
        })?;

    let region = request
        .env
        .get("AWS_REGION")
        .and_then(|value| value.clone())
        .or_else(|| request.env.get("AWS_DEFAULT_REGION").and_then(|value| value.clone()));
    let endpoint = resolve_bedrock_converse_endpoint(
        request.transport.base_url.as_deref(),
        &built.model_id,
        region.as_deref(),
    )
    .ok_or(BEDROCK_COMPACTION_SKIPPED_REASON)?;

    let body = serde_json::to_string(&built.body).map_err(|error| error.to_string())?;
    let init = RemoteFetchInit {
        method: Some("POST"),
        headers: Some(HashMap::from([
            ("content-type".to_string(), built.content_type.clone()),
            ("accept".to_string(), built.accept.clone()),
        ])),
        body: Some(&body),
    };

    let timeout_ms = resolve_timeout_ms(&request.env);
    let request_future = fetch.fetch(&endpoint, init);
    let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), request_future).await;
    let response = match outcome {
        Ok(result) => match result {
            Ok(response) => response,
            Err(error) => return Err(error),
        },
        Err(_) => {
            return Err(format!("Bedrock compaction timed out after {timeout_ms}ms"))
        }
    };
    if !response.ok {
        let detail = {
            let text = response.text.trim();
            if text.is_empty() {
                String::new()
            } else {
                let first_200: String = text.chars().take(200).collect();
                format!(": {first_200}")
            }
        };
        return Err(format!(
            "Bedrock compaction failed ({} {}){}",
            response.status,
            response.status_text.as_deref().unwrap_or(""),
            detail
        ));
    }
    let payload = response.json().await.map_err(|error| error.to_string())?;

    let stop_reason = payload.get("stopReason").and_then(Value::as_str);
    // 截断（max_tokens）或被过滤（content_filtered、guardrail_intervened）
    // 的生成不是可用摘要，即使返回文本非空：降级到下一个方法。
    if let Some(stop_reason) = stop_reason {
        if !SUCCESSFUL_STOP_REASONS.contains(&stop_reason) {
            return Err(format!(
                "Bedrock Converse compaction stopped with reason \"{stop_reason}\""
            ));
        }
    }
    let summary = extract_bedrock_converse_summary(&payload)
        .ok_or("Bedrock Converse compaction returned no summary")?;

    let mut preserve_data = serde_json::Map::new();
    let preserved = BedrockConversePreserveData {
        version: 1,
        provider: "bedrock".to_string(),
        model: request.ctx.model.clone(),
        model_id: built.model_id.clone(),
        stop_reason: stop_reason.map(String::from),
        used_tokens: usage_of(&payload),
    };
    if let Ok(value) = serde_json::to_value(&preserved) {
        preserve_data.insert(BEDROCK_COMPACTION_PRESERVE_KEY.to_string(), value);
    }

    Ok(RemoteLaneResult::Success(RemoteLaneSuccess {
        summary,
        preserve_data: Some(preserve_data),
        warning: built.skipped_blocks.map(|count| {
            format!("bedrock-skipped-unsupported-blocks:{count}")
        }),
    }))
}
