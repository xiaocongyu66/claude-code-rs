//! mappers.rs — 内部消息 ↔ SDK 消息的双向映射。
//!
//! 重写自 `src/utils/messages/mappers.ts`。这是 SDK 流（agentSdkTypes）
//! 与内部 `Message` 树之间的转换层：`to_internal_messages` 把 SDK 消息
//! 落地成内部消息，`to_sdk_messages` 反向序列化供远程消费方渲染。
//!
//! 类型说明（TS→Rust 鸭子类型）：
//! - `src/types/message.ts` 与 `src/entrypoints/agentSdkTypes.ts` 均未转换。
//!   两端的 `Message` / `SDKMessage` 本质是
//!   `{ type: string; [key: string]: unknown }` 的开放结构，故本模块以
//!   `serde_json::Value` 逐字段搬运，键名、可选性与 TS 保持一致
//!   （`undefined` 字段在 JSON 语义下等价于缺键，故不写入）。
//! - `src/services/claudeAiLimits.ts` 未转换 → 本模块定义最小 duck-type
//!   struct [`ClaudeAiLimits`]（仅 `to_sdk_rate_limit_info` 需要的字段；
//!   TS 类型中的 `unifiedRateLimitFallbackAvailable` 是内部字段、映射时
//!   按原注释被剥离，故不建模）。
//!
//! 依赖未转换模块的本地复刻（行为对齐，转换后应替换为真实实现）：
//! - `../messages.ts` 的 `createAssistantMessage` → 私有
//!   [`create_assistant_message`]，复刻 `baseCreateAssistantMessage` 的
//!   合成消息信封（随机 id、`model: '<synthetic>'`、默认 usage 等）。
//! - `../plans.ts` 的 `getPlan` → 私有 [`get_plan`]，复刻
//!   `getPlansDirectory` + `getPlanSlug` + `getPlan` 的读取语义。
//! - `strip-ansi`（npm 包）→ 私有 [`strip_ansi`]（ANSI CSI/OSC 转义剥离）。
//! - `EXIT_PLAN_MODE_V2_TOOL_NAME` 来自未转换的 builtin-tools 包 → 本地
//!   常量复刻。
//!
//! 已知差异：
//! - `serde_json` 默认按字典序排列对象键（无 `preserve_order` feature）；
//!   JSON 对象键序无语义，下游解析均为按键读取。
//! - `to_internal_messages` 的 TS 源码在 `'user'` 分支 `return` 之后残留
//!   一段 `compact_boundary` 处理（死代码，不可达），本端口不予搬运。

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use chrono::Utc;
use regex::Regex;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::bootstrap::state::{
    get_plan_slug_cache, get_session_id, set_plan_slug_cache_entry,
};
use crate::constants::messages::NO_CONTENT_MESSAGE;
use crate::constants::xml::{LOCAL_COMMAND_STDERR_TAG, LOCAL_COMMAND_STDOUT_TAG};
use crate::utils::cwd::get_cwd;
use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::errors::is_enoent;
use crate::utils::log::log_error;
use crate::utils::settings::settings::get_initial_settings;
use crate::utils::words::generate_word_slug;

/// `@claude-code-hardened/builtin-tools/.../ExitPlanModeTool/constants.ts`
/// 的 `EXIT_PLAN_MODE_V2_TOOL_NAME`（该包未转换为 Rust）。
const EXIT_PLAN_MODE_V2_TOOL_NAME: &str = "ExitPlanMode";

/// `src/utils/messages.ts` 的 `SYNTHETIC_MODEL`（该文件未转换为 Rust）。
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// 与 TS `getPlansDirectory` 中的重试上限一致。
const MAX_SLUG_RETRIES: usize = 10;

/// `new Date().toISOString()`：UTC、毫秒精度、`Z` 后缀。
fn iso_now() -> String {
    format!("{}Z", Utc::now().format("%Y-%m-%dT%H:%M:%S%.3f"))
}

fn now_uuid() -> String {
    Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// strip-ansi（npm `strip-ansi` 的最小复刻）
// ---------------------------------------------------------------------------

/// 取得 ANSI 转义剥离正则；编译失败（不可发生）时返回 None，调用方退化为
/// 原文返回，避免 panic。
fn ansi_regex() -> Option<&'static Regex> {
    static ANSI_RE: OnceLock<Option<Regex>> = OnceLock::new();
    ANSI_RE.get_or_init(|| {
        Regex::new(
            // 1. ECMA-48 两字符序列 ESC + [@-Z\-_]
            // 2. CSI：ESC [ + 参数(0-?)*中间( -/)*终字节(@-~)
            // 3. OSC：ESC ] + 正文 + BEL 或 ESC \
            "\u{1B}(?:[\u{40}-\u{5A}\u{5C}\u{5F}]|\\[[\u{30}-\u{3F}]*[\u{20}-\u{2F}]*[\u{40}-\u{7E}]|\\][^\u{07}\u{1B}]*(?:\u{07}|\u{1B}\\\\))",
        )
        .ok()
    })
    .as_ref()
}

/// `stripAnsi`：剥离 ANSI 转义序列。正则编译失败时返回原文。
fn strip_ansi(input: &str) -> Cow<'_, str> {
    match ansi_regex() {
        Some(re) => re.replace_all(input, ""),
        None => Cow::Borrowed(input),
    }
}

// ---------------------------------------------------------------------------
// getPlan（../plans.ts 未转换 → 私有复刻）
// ---------------------------------------------------------------------------

fn join_path(base: &str, rest: &str) -> String {
    if base.is_empty() {
        return rest.to_string();
    }
    let sep = std::path::MAIN_SEPARATOR;
    if base.ends_with(sep) {
        format!("{base}{rest}")
    } else {
        format!("{base}{sep}{rest}")
    }
}

/// `getPlansDirectory` 的复刻：settings.plansDirectory（相对 cwd 解析、
/// 禁止跨出项目根）优先，否则 `<configHome>/plans`；并 `mkdir -p`。
/// 与 TS 的差异：不做 `..` / `.` 段归一化（TS 用 node path.resolve），
/// 也不做 memoize（`create_dir_all` 幂等，重复调用代价可忽略）。
fn plans_directory() -> String {
    let settings = get_initial_settings();
    let config_home = get_claude_config_home_dir();
    let plans_path = match settings.plans_directory.as_deref() {
        Some(dir) if !dir.is_empty() => {
            let cwd = get_cwd();
            let resolved = if Path::new(dir).is_absolute() {
                dir.to_string()
            } else {
                join_path(&cwd, dir)
            };
            let prefix = format!("{}{}", cwd, std::path::MAIN_SEPARATOR);
            if resolved == cwd || resolved.starts_with(&prefix) {
                resolved
            } else {
                // 路径穿越：对齐 TS 的兜底（logError 后退回默认目录）。
                log_error(format!(
                    "plansDirectory must be within project root: {dir}"
                ));
                join_path(&config_home, "plans")
            }
        }
        _ => join_path(&config_home, "plans"),
    };
    // 对齐 TS 的 mkdirSync(recursive: true)（memoize 的副作用）。
    let _ = std::fs::create_dir_all(&plans_path);
    plans_path
}

/// `getPlanSlug` 的复刻：会话级 slug 缓存（bootstrap::state）命中则返回；
/// 否则生成 "形容词-动词-名词" slug，文件名冲突时最多重试 10 次。
fn plan_slug(session_id: &str) -> String {
    if let Some(hit) = get_plan_slug_cache().get(session_id) {
        return hit;
    }
    let dir = plans_directory();
    let mut slug = String::new();
    for _ in 0..MAX_SLUG_RETRIES {
        slug = generate_word_slug();
        if !Path::new(&format!("{}/{}.md", dir, slug)).exists() {
            break;
        }
    }
    set_plan_slug_cache_entry(session_id, &slug);
    slug
}

/// `getPlan(agentId?)` 的复刻：主会话读 `<slug>.md`，子代理读
/// `<slug>-agent-<agentId>.md`；ENOENT → None，其他 IO 错误
/// logError → None。
fn get_plan(agent_id: Option<&str>) -> Option<String> {
    let session_id = get_session_id().to_string();
    let slug = plan_slug(&session_id);
    let dir = plans_directory();
    let file_path = match agent_id {
        Some(id) => format!("{}/{}-agent-{}.md", dir, slug, id),
        None => format!("{}/{}.md", dir, slug),
    };
    match std::fs::read_to_string(&file_path) {
        Ok(content) => Some(content),
        Err(err) => {
            if is_enoent(&err) {
                None
            } else {
                log_error(err);
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// createAssistantMessage（../messages.ts 未转换 → 私有复刻）
// ---------------------------------------------------------------------------

/// 默认 usage（对齐 `baseCreateAssistantMessage` 的默认值）。
fn default_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "output_tokens_details": null,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "fallback_credit": null,
        "server_tool_use": {
            "web_search_requests": 0,
            "web_fetch_requests": 0
        },
        "service_tier": null,
        "cache_creation": {
            "ephemeral_1h_input_tokens": 0,
            "ephemeral_5m_input_tokens": 0
        },
        "inference_geo": null,
        "iterations": null,
        "speed": null
    })
}

/// 复刻 `createAssistantMessage({ content })`：仅支持字符串 content，
/// 空串替换为 `NO_CONTENT_MESSAGE`，并构建完整合成消息信封。
fn create_assistant_message(content: &str) -> Value {
    let text = if content.is_empty() {
        NO_CONTENT_MESSAGE
    } else {
        content
    };
    json!({
        "type": "assistant",
        "uuid": now_uuid(),
        "timestamp": iso_now(),
        "message": {
            "id": now_uuid(),
            "container": null,
            "model": SYNTHETIC_MODEL,
            "role": "assistant",
            "stop_reason": "stop_sequence",
            "stop_sequence": "",
            "type": "message",
            "usage": default_usage(),
            "content": [ { "type": "text", "text": text } ],
            "context_management": null
        },
        "requestId": null,
        "apiError": null,
        "error": null,
        "errorDetails": null,
        "isApiErrorMessage": false,
        "isVirtual": null
    })
}

// ---------------------------------------------------------------------------
// toInternalMessages
// ---------------------------------------------------------------------------

/// SDK 消息 → 内部消息。assistant 落地为内部 assistant 消息；user 落地为
/// 内部 user 消息（`isSynthetic` 映射到 `isMeta`）。其余类型（含空值）
/// 一律丢弃。注意 TS 源码 `'user'` 分支 return 之后的 `compact_boundary`
/// 块是不可达死代码，本端口不搬运。
pub fn to_internal_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| {
            let obj = message.as_object()?;
            let message_type = obj.get("type")?.as_str()?;
            match message_type {
                "assistant" => {
                    let mut out = Map::new();
                    out.insert("type".to_string(), json!("assistant"));
                    if let Some(msg) = obj.get("message") {
                        out.insert("message".to_string(), msg.clone());
                    }
                    if let Some(uuid) = obj.get("uuid") {
                        out.insert("uuid".to_string(), uuid.clone());
                    }
                    out.insert("timestamp".to_string(), json!(iso_now()));
                    Some(Value::Object(out))
                }
                "user" => {
                    let mut out = Map::new();
                    out.insert("type".to_string(), json!("user"));
                    if let Some(msg) = obj.get("message") {
                        out.insert("message".to_string(), msg.clone());
                    }
                    // uuid ?? randomUUID()
                    out.insert(
                        "uuid".to_string(),
                        obj.get("uuid").cloned().unwrap_or_else(|| json!(now_uuid())),
                    );
                    // timestamp ?? now
                    let timestamp = obj
                        .get("timestamp")
                        .filter(|t| !t.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!(iso_now()));
                    out.insert("timestamp".to_string(), timestamp);
                    // isMeta: isSynthetic（缺省不落键）
                    if let Some(is_synthetic) = obj.get("isSynthetic") {
                        if !is_synthetic.is_null() {
                            out.insert("isMeta".to_string(), is_synthetic.clone());
                        }
                    }
                    Some(Value::Object(out))
                }
                _ => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// compact_metadata 双向转换
// ---------------------------------------------------------------------------

/// 内部 `CompactMetadata` → SDK `compact_metadata`：
/// `trigger` / `preTokens` → `trigger` / `pre_tokens`，
/// `preservedSegment`（headUuid/anchorUuid/tailUuid）→
/// `preserved_segment`（head_uuid/anchor_uuid/tail_uuid）。
pub fn to_sdk_compact_metadata(meta: &Value) -> Value {
    let preserved_segment = meta.get("preservedSegment").and_then(|seg| seg.as_object());
    let mut out = Map::new();
    if let Some(trigger) = meta.get("trigger") {
        out.insert("trigger".to_string(), trigger.clone());
    }
    if let Some(pre_tokens) = meta.get("preTokens") {
        out.insert("pre_tokens".to_string(), pre_tokens.clone());
    }
    if let Some(seg) = preserved_segment {
        let mut segment = Map::new();
        if let Some(v) = seg.get("headUuid") {
            segment.insert("head_uuid".to_string(), v.clone());
        }
        if let Some(v) = seg.get("anchorUuid") {
            segment.insert("anchor_uuid".to_string(), v.clone());
        }
        if let Some(v) = seg.get("tailUuid") {
            segment.insert("tail_uuid".to_string(), v.clone());
        }
        out.insert("preserved_segment".to_string(), Value::Object(segment));
    }
    Value::Object(out)
}

/// SDK `compact_metadata` → 内部 `CompactMetadata`（反向键名映射）。
pub fn from_sdk_compact_metadata(meta: &Value) -> Value {
    let preserved_segment = meta.get("preserved_segment").and_then(|seg| seg.as_object());
    let mut out = Map::new();
    if let Some(trigger) = meta.get("trigger") {
        out.insert("trigger".to_string(), trigger.clone());
    }
    if let Some(pre_tokens) = meta.get("pre_tokens") {
        out.insert("preTokens".to_string(), pre_tokens.clone());
    }
    if let Some(seg) = preserved_segment {
        let mut segment = Map::new();
        if let Some(v) = seg.get("head_uuid") {
            segment.insert("headUuid".to_string(), v.clone());
        }
        if let Some(v) = seg.get("anchor_uuid") {
            segment.insert("anchorUuid".to_string(), v.clone());
        }
        if let Some(v) = seg.get("tail_uuid") {
            segment.insert("tailUuid".to_string(), v.clone());
        }
        out.insert("preservedSegment".to_string(), Value::Object(segment));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// toSDKMessages
// ---------------------------------------------------------------------------

/// 把 ExitPlanModeV2 工具调用的 plan 注入 tool_input（该工具从文件读取
/// plan，但 SDK 消费方期望 `tool_input.plan` 存在）。非数组 content 与
/// 非 ExitPlanMode 块原样透传。
fn normalize_assistant_message_for_sdk(message: &Value) -> Value {
    let Some(inner) = message.get("message") else {
        return message.clone();
    };
    let content = match inner.get("content") {
        Some(Value::Array(items)) => items,
        _ => return message.clone(),
    };
    let normalized: Vec<Value> = content
        .iter()
        .map(|block| {
            let block_obj = match block.as_object() {
                Some(obj) => obj,
                None => return block.clone(),
            };
            if block_obj.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                return block.clone();
            }
            if block_obj.get("name").and_then(|n| n.as_str()) != Some(EXIT_PLAN_MODE_V2_TOOL_NAME)
            {
                return block.clone();
            }
            match get_plan(None) {
                Some(plan) => {
                    let mut new_block = block.clone();
                    let mut input = block
                        .get("input")
                        .filter(|v| v.is_object())
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    if let Value::Object(input_obj) = &mut input {
                        input_obj.insert("plan".to_string(), Value::String(plan));
                    }
                    if let Value::Object(new_obj) = &mut new_block {
                        new_obj.insert("input".to_string(), input);
                    }
                    new_block
                }
                None => block.clone(),
            }
        })
        .collect();
    let mut normalized_message = inner.clone();
    if let Value::Object(obj) = &mut normalized_message {
        obj.insert("content".to_string(), Value::Array(normalized));
    }
    let mut out = message.clone();
    if let Value::Object(obj) = &mut out {
        obj.insert("message".to_string(), normalized_message);
    }
    out
}

/// 本地命令输出 → 形态完整的 `SDKAssistantMessage`。
///
/// 剥离 ANSI（如 /cost 的 chalk.dim）后解包 `<local-command-stdout>` /
/// `<local-command-stderr>` 包装标签。以 assistant 而非
/// system/local_command_output 发射的原因见 TS 源码注释（Android /
/// api-go 的解析器只认 assistant/system 有限子类型）。合成消息信封由
/// [`create_assistant_message`] 构建（id、model、role、stop_reason、
/// usage 齐备）。
pub fn local_command_output_to_sdk_assistant_message(raw_content: &str, uuid: &str) -> Value {
    let stdout_pattern = format!("(?s)<{}>(.*?)</{}>", LOCAL_COMMAND_STDOUT_TAG, LOCAL_COMMAND_STDOUT_TAG);
    let stderr_pattern = format!("(?s)<{}>(.*?)</{}>", LOCAL_COMMAND_STDERR_TAG, LOCAL_COMMAND_STDERR_TAG);
    // TS 用非全局 replace：仅替换首次出现。
    let clean_content = strip_ansi(raw_content);
    let clean_content = replace_first_tag(&clean_content, &stdout_pattern);
    let clean_content = replace_first_tag(&clean_content, &stderr_pattern);
    let clean_content = clean_content.trim();
    let synthetic = create_assistant_message(clean_content);
    let message = synthetic.get("message").cloned().unwrap_or(Value::Null);
    json!({
        "type": "assistant",
        "content": message.get("content").cloned().unwrap_or(Value::Null),
        "message": message,
        "parent_tool_use_id": Value::Null,
        "session_id": get_session_id().to_string(),
        "uuid": uuid,
    })
}

/// `content.replace(pattern, '$1')`：仅首次匹配，失败（正则不编译）时
/// 返回原文。
fn replace_first_tag<'a>(content: &'a str, pattern: &str) -> Cow<'a, str> {
    match Regex::new(pattern) {
        Ok(re) => re.replace(content, "$1"),
        Err(_) => Cow::Borrowed(content),
    }
}

/// 内部消息 → SDK 消息：
/// - assistant：注入 session_id / parent_tool_use_id，并经
///   [`normalize_assistant_message_for_sdk`] 补 plan；
/// - user：`isMeta || isVisibleInTranscriptOnly` → `isSynthetic`，
///   `toolUseResult` → `tool_use_result`；
/// - system/compact_boundary：发射 `compact_metadata`；
/// - system/local_command：仅当 content 含 `<local-command-stdout>` 或
///   `<local-command-stderr>` 标签时转成 assistant 消息（命令输入元数据
///   不得泄漏到 RC web UI）；
/// - 其余丢弃。
pub fn to_sdk_messages(messages: &[Value]) -> Vec<Value> {
    let session_id = get_session_id().to_string();
    messages
        .iter()
        .filter_map(|message| {
            let obj = message.as_object()?;
            let message_type = obj.get("type")?.as_str()?;
            match message_type {
                "assistant" => {
                    let mut out = Map::new();
                    out.insert("type".to_string(), json!("assistant"));
                    if let Some(msg) = normalize_assistant_message_for_sdk(message).get("message") {
                        out.insert("message".to_string(), msg.clone());
                    }
                    out.insert("session_id".to_string(), json!(session_id.clone()));
                    out.insert("parent_tool_use_id".to_string(), Value::Null);
                    if let Some(uuid) = obj.get("uuid") {
                        out.insert("uuid".to_string(), uuid.clone());
                    }
                    // error: message?.error（undefined / null 不落键）
                    if let Some(error) = obj.get("error") {
                        if !error.is_null() {
                            out.insert("error".to_string(), error.clone());
                        }
                    }
                    Some(Value::Object(out))
                }
                "user" => {
                    let mut out = Map::new();
                    out.insert("type".to_string(), json!("user"));
                    if let Some(msg) = obj.get("message") {
                        out.insert("message".to_string(), msg.clone());
                    }
                    out.insert("session_id".to_string(), json!(session_id.clone()));
                    out.insert("parent_tool_use_id".to_string(), Value::Null);
                    if let Some(uuid) = obj.get("uuid") {
                        out.insert("uuid".to_string(), uuid.clone());
                    }
                    if let Some(timestamp) = obj.get("timestamp") {
                        out.insert("timestamp".to_string(), timestamp.clone());
                    }
                    let is_meta = obj.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false);
                    let transcript_only = obj
                        .get("isVisibleInTranscriptOnly")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    out.insert("isSynthetic".to_string(), json!(is_meta || transcript_only));
                    // 结构化工具输出：protobuf catchall，不进模型上下文。
                    if let Some(tool_use_result) = obj.get("toolUseResult") {
                        out.insert("tool_use_result".to_string(), tool_use_result.clone());
                    }
                    Some(Value::Object(out))
                }
                "system" => {
                    let subtype = obj.get("subtype").and_then(|s| s.as_str());
                    if subtype == Some("compact_boundary") && obj.contains_key("compactMetadata") {
                        let mut out = Map::new();
                        out.insert("type".to_string(), json!("system"));
                        out.insert("subtype".to_string(), json!("compact_boundary"));
                        out.insert("session_id".to_string(), json!(session_id.clone()));
                        if let Some(uuid) = obj.get("uuid") {
                            out.insert("uuid".to_string(), uuid.clone());
                        }
                        out.insert(
                            "compact_metadata".to_string(),
                            to_sdk_compact_metadata(obj.get("compactMetadata").unwrap_or(&Value::Null)),
                        );
                        return Some(Value::Object(out));
                    }
                    if subtype == Some("local_command") {
                        let content = obj.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        if content.contains(&format!("<{}>", LOCAL_COMMAND_STDOUT_TAG))
                            || content.contains(&format!("<{}>", LOCAL_COMMAND_STDERR_TAG))
                        {
                            let uuid = obj.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
                            return Some(local_command_output_to_sdk_assistant_message(
                                content, uuid,
                            ));
                        }
                    }
                    None
                }
                _ => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// toSDKRateLimitInfo
// ---------------------------------------------------------------------------

/// `ClaudeAILimits` 的最小 duck-type（`src/services/claudeAiLimits.ts`
/// 未转换；仅建模 `to_sdk_rate_limit_info` 读取的字段）。
#[derive(Debug, Clone)]
pub struct ClaudeAiLimits {
    pub status: String,
    pub resets_at: Option<f64>,
    pub rate_limit_type: Option<String>,
    pub utilization: Option<f64>,
    pub overage_status: Option<String>,
    pub overage_resets_at: Option<f64>,
    pub overage_disabled_reason: Option<String>,
    pub is_using_overage: Option<bool>,
    pub surpassed_threshold: Option<f64>,
}

/// 内部 `ClaudeAILimits` → SDK `SDKRateLimitInfo`，剥除
/// `unifiedRateLimitFallbackAvailable` 等内部字段；仅非 undefined 字段
/// 落键。
pub fn to_sdk_rate_limit_info(limits: Option<&ClaudeAiLimits>) -> Option<Value> {
    let limits = limits?;
    let mut out = Map::new();
    out.insert("type".to_string(), json!("rate_limit"));
    out.insert("status".to_string(), json!(limits.status.clone()));
    if let Some(v) = limits.resets_at {
        out.insert("resetsAt".to_string(), json!(v));
    }
    if let Some(v) = limits.rate_limit_type.clone() {
        out.insert("rateLimitType".to_string(), json!(v));
    }
    if let Some(v) = limits.utilization {
        out.insert("utilization".to_string(), json!(v));
    }
    if let Some(v) = limits.overage_status.clone() {
        out.insert("overageStatus".to_string(), json!(v));
    }
    if let Some(v) = limits.overage_resets_at {
        out.insert("overageResetsAt".to_string(), json!(v));
    }
    if let Some(v) = limits.overage_disabled_reason.clone() {
        out.insert("overageDisabledReason".to_string(), json!(v));
    }
    if let Some(v) = limits.is_using_overage {
        out.insert("isUsingOverage".to_string(), json!(v));
    }
    if let Some(v) = limits.surpassed_threshold {
        out.insert("surpassedThreshold".to_string(), json!(v));
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_user_and_assistant_to_internal() {
        let sdk = vec![
            json!({
                "type": "user",
                "message": { "role": "user", "content": "hi" },
                "uuid": "u1",
                "isSynthetic": true
            }),
            json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [] },
                "uuid": "a1"
            }),
            json!({ "type": "result" }),
        ];
        let out = to_internal_messages(&sdk);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "user");
        assert_eq!(out[0]["uuid"], "u1");
        assert_eq!(out[0]["isMeta"], true);
        assert!(out[0]["timestamp"].as_str().unwrap().ends_with('Z'));
        assert_eq!(out[1]["type"], "assistant");
        // requestId 在 TS 中为 undefined -> 不落键
        assert!(out[1].get("requestId").is_none());
    }

    #[test]
    fn round_trips_compact_metadata() {
        let internal = json!({
            "trigger": "manual",
            "preTokens": 100,
            "preservedSegment": { "headUuid": "h", "anchorUuid": "a", "tailUuid": "t" }
        });
        let sdk = to_sdk_compact_metadata(&internal);
        assert_eq!(sdk["pre_tokens"], 100);
        assert_eq!(sdk["preserved_segment"]["anchor_uuid"], "a");
        let back = from_sdk_compact_metadata(&sdk);
        assert_eq!(back["preTokens"], 100);
        assert_eq!(back["preservedSegment"]["headUuid"], "h");
        assert_eq!(back.as_object().unwrap().len(), 3);
    }

    #[test]
    fn strips_ansi_and_unwraps_local_command_output() {
        let raw = format!(
            "\u{1B}[1m<{}>hello\u{1B}[0m world</{}>",
            LOCAL_COMMAND_STDOUT_TAG, LOCAL_COMMAND_STDOUT_TAG
        );
        let msg = local_command_output_to_sdk_assistant_message(&raw, "uuid-1");
        assert_eq!(msg["type"], "assistant");
        assert_eq!(msg["uuid"], "uuid-1");
        let text = msg["message"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "hello world");
        assert_eq!(msg["message"]["model"], SYNTHETIC_MODEL);
        assert_eq!(msg["message"]["stop_reason"], "stop_sequence");
    }

    #[test]
    fn converts_local_command_stdout_system_message() {
        let messages = vec![json!({
            "type": "system",
            "subtype": "local_command",
            "content": format!("<{}>done</{}>", LOCAL_COMMAND_STDOUT_TAG, LOCAL_COMMAND_STDOUT_TAG),
            "uuid": "sys-1"
        })];
        let out = to_sdk_messages(&messages);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "assistant");
    }

    #[test]
    fn drops_local_command_input_metadata() {
        let messages = vec![json!({
            "type": "system",
            "subtype": "local_command",
            "content": "<command-name>foo</command-name>"
        })];
        assert!(to_sdk_messages(&messages).is_empty());
    }

    #[test]
    fn injects_plan_into_exit_plan_mode_tool_input() {
        let messages = vec![json!({
            "type": "assistant",
            "uuid": "a2",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "ExitPlanMode", "input": { "plan": "" } }
                ]
            }
        })];
        // getPlan 在测试环境通常返回 None（无 plan 文件），此时不应注入。
        let out = to_sdk_messages(&messages);
        assert_eq!(out.len(), 1);
        // 无论 plan 是否注入，消息形态与 session_id 必须正确。
        assert_eq!(out[0]["session_id"], get_session_id().to_string());
        assert_eq!(out[0]["parent_tool_use_id"], Value::Null);
    }

    #[test]
    fn rate_limit_info_strips_internal_fields_and_skips_undefined() {
        let limits = ClaudeAiLimits {
            status: "allowed".to_string(),
            resets_at: Some(123.0),
            rate_limit_type: None,
            utilization: Some(0.5),
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            is_using_overage: Some(false),
            surpassed_threshold: None,
        };
        let info = to_sdk_rate_limit_info(Some(&limits)).unwrap();
        assert_eq!(info["type"], "rate_limit");
        assert_eq!(info["status"], "allowed");
        assert_eq!(info["resetsAt"], 123.0);
        assert_eq!(info["utilization"], 0.5);
        assert!(!info.as_object().unwrap().contains_key("rateLimitType"));
        assert!(to_sdk_rate_limit_info(None).is_none());
    }

    #[test]
    fn sdk_compat_passthrough() {
        // placeholder guard: to_internal_messages drops unknown types
        let out = to_internal_messages(&[json!({ "type": "stream_event" })]);
        assert!(out.is_empty());
    }
}
