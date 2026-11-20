//! `services/compact/microCompact.ts` — microcompact（时间触发 / 缓存编辑 /
//! MEMORY_PROJECTION 三条路径）。
//!
//! 语义对齐：
//! - 请求构建期的轻量上下文回收，`microcompactMessages` 依次尝试
//!   MEMORY_PROJECTION 投影 → 时间触发 → 缓存编辑，全部失配则原样返回。
//! - 阈值/占位符/keepRecent 等常量逐条搬运。
//! - token 计数依赖（`countTokensPrecise` / `roughTokenCountEstimation`，
//!   `tokenEstimation.ts` 未转换）以本地近似实现并在注释标注：精确 token
//!   计数需要原生 cl100k 模块，此处退化为 ~4 字符/token。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde_json::Value;

use crate::services::compact::cached_microcompact::{
    self, CacheEditsBlock, CachedMcState, PinnedCacheEdits,
};
use crate::services::compact::compact_warning_state::{
    clear_compact_warning_suppression, suppress_compact_warning,
};
use crate::services::compact::time_based_mc_config::{get_time_based_mc_config, TimeBasedMcConfig};
use crate::services::compact::{Content, Message};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::log::log_error_dyn;

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// `utils/toolResultStorage.ts` 内联常量（该文件未转换，且会拉入
/// sessionStorage → messages → api/errors 的循环依赖——同 TS 内联理由）。
pub const TIME_BASED_MC_CLEARED_MESSAGE: &str = "[Old tool result content cleared]";

/// 图片/文档的保守 token 估值（与 `roughTokenCountEstimationForBlock` 一致）。
const IMAGE_MAX_TOKEN_SIZE: i64 = 2000;

/// 可压缩工具集合（TS `COMPACTABLE_TOOLS`）。
pub const COMPACTABLE_TOOLS: &[&str] = &[
    "Read", "Bash", "PowerShell", "Grep", "Glob", "WebSearch", "WebFetch", "Edit", "Write",
];

/// `utils/toolResultStorage.ts` 的 `PERSISTED_OUTPUT_TAG`（未转换依赖）。
const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";

// ---------------------------------------------------------------------------
// token 计数本地近似（tokenEstimation.ts 未转换）
// ---------------------------------------------------------------------------

/// TS `roughTokenCountEstimation(text, bytesPerToken = 4)`。
pub fn rough_token_count_estimation(text: &str, bytes_per_token: usize) -> usize {
    text.len().div_ceil(bytes_per_token.max(1))
}

/// TS `countTokensPrecise(text)`：原生 cl100k 精确计数（未转换）；
/// 退化为 4 字符/token 近似并标注。
pub fn count_tokens_precise(text: &str) -> usize {
    // TODO(token-counter-napi): 接入原生 cl100k 计数器。
    text.len().div_ceil(4)
}

/// 计算单个 tool_result 内容块的 token 数（对齐 TS `calculateToolResultTokens`）。
pub fn calculate_tool_result_tokens(content: Option<&Value>) -> usize {
    let Some(content) = content else {
        return 0;
    };
    match content {
        Value::String(s) => count_tokens_precise(s),
        Value::Array(items) => {
            let mut sum = 0usize;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            sum += count_tokens_precise(text);
                        }
                    }
                    Some("image") | Some("document") => {
                        // 图片/文档无论格式约 2000 token。
                        sum += IMAGE_MAX_TOKEN_SIZE as usize;
                    }
                    _ => {}
                }
            }
            sum
        }
        _ => 0,
    }
}

/// TS `estimateMessageTokens(messages)`：按块类型累计并用 4/3 系数保守放大。
pub fn estimate_message_tokens(messages: &[Message]) -> usize {
    let mut total_tokens = 0usize;
    for message in messages {
        let Some(content) = message.content() else {
            continue;
        };
        let blocks = match content {
            Content::Text(text) => {
                total_tokens += count_tokens_precise(text);
                continue;
            }
            Content::Blocks(blocks) => blocks,
        };
        for block in blocks {
            let typ = block.get("type").and_then(Value::as_str);
            match typ {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        total_tokens += count_tokens_precise(text);
                    }
                }
                Some("tool_result") => {
                    total_tokens += calculate_tool_result_tokens(block.get("content"));
                }
                Some("image") | Some("document") => {
                    total_tokens += IMAGE_MAX_TOKEN_SIZE as usize;
                }
                Some("thinking") => {
                    // 与 roughTokenCountEstimationForBlock 一致：只计
                    // thinking 正文，不计 JSON 包裹与签名。
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        total_tokens += count_tokens_precise(text);
                    }
                }
                Some("redacted_thinking") => {
                    if let Some(text) = block.get("data").and_then(Value::as_str) {
                        total_tokens += count_tokens_precise(text);
                    }
                }
                Some("tool_use") => {
                    // 计 name + input，不计 JSON 包裹与 id 字段。
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let input_json = serde_json::to_string(&input).unwrap_or_default();
                    total_tokens += count_tokens_precise(&format!("{name}{input_json}"));
                }
                _ => {
                    // server_tool_use / web_search_tool_result 等：整体序列化计费。
                    let serialized = serde_json::to_string(block).unwrap_or_default();
                    total_tokens += count_tokens_precise(&serialized);
                }
            }
        }
    }
    // 4/3 保守放大（近似值）。
    (total_tokens * 4).div_ceil(3)
}

// ---------------------------------------------------------------------------
// 缓存 microcompact 模块级状态（feature('CACHED_MICROCOMPACT') 门控）
// ---------------------------------------------------------------------------

static CACHED_MC_STATE: Mutex<Option<CachedMcState>> = Mutex::new(None);
static PENDING_CACHE_EDITS: Mutex<Option<CacheEditsBlock>> = Mutex::new(None);

/// TS `consumePendingCacheEdits()`：取出待发送的 cache_edits 块并清空。
pub fn consume_pending_cache_edits() -> Option<CacheEditsBlock> {
    let mut pending = PENDING_CACHE_EDITS.lock().unwrap_or_else(|e| e.into_inner());
    pending.take()
}

/// TS `getPinnedCacheEdits()`：已 pin 的历史 cache_edits（重发以保持命中）。
pub fn get_pinned_cache_edits() -> Vec<PinnedCacheEdits> {
    let state = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state
        .as_ref()
        .map(|s| s.pinned_edits.clone())
        .unwrap_or_default()
}

/// TS `pinCacheEdits(userMessageIndex, block)`。
pub fn pin_cache_edits(user_message_index: usize, block: CacheEditsBlock) {
    let mut state = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = state.as_mut() {
        state.pinned_edits.push(PinnedCacheEdits {
            user_message_index,
            block,
        });
    }
}

/// TS `markToolsSentToAPIState()`。
pub fn mark_tools_sent_to_api_state() {
    let mut guard = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        cached_microcompact::mark_tools_sent_to_api(state);
    }
}

/// TS `resetMicrocompactState()`。
pub fn reset_microcompact_state() {
    let mut guard = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        cached_microcompact::reset_cached_mc_state(state);
    }
    let mut pending = PENDING_CACHE_EDITS.lock().unwrap_or_else(|e| e.into_inner());
    *pending = None;
}

/// TS `PendingCacheEdits`。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCacheEdits {
    pub trigger: String,
    pub deleted_tool_ids: Vec<String>,
    /// 上一次 API 响应的累计 cache_deleted_input_tokens 基线（差值计算用）。
    pub baseline_cache_deleted_tokens: i64,
}

/// TS `MicrocompactResult`。
#[derive(Debug, Clone, PartialEq)]
pub struct MicrocompactResult {
    pub messages: Vec<Message>,
    pub compaction_info: Option<PendingCacheEdits>,
    /// 内容被替换为占位符的 tool_use id（调用方应释放原始字符串）。
    pub cleared_tool_use_ids: Vec<String>,
    /// MEMORY_PROJECTION 专用：tool_use_id → 实际替换字符串。
    pub cleared_replacements: HashMap<String, String>,
    /// MEMORY_PROJECTION 专用：需要从 readFileState 移除的 Read 路径。
    pub read_file_state_paths: Vec<String>,
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

/// 收集所有 `tool_use` 块 id（name ∈ COMPACTABLE_TOOLS），按出现顺序。
fn collect_compactable_tool_ids(messages: &[Message]) -> Vec<String> {
    let compactable: HashSet<&str> = COMPACTABLE_TOOLS.iter().copied().collect();
    let mut ids = Vec::new();
    for message in messages {
        let Message::Assistant(_) = message else {
            continue;
        };
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !compactable.contains(name) {
                continue;
            }
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// TS `isMainThreadSource(querySource)`：未设置或前缀匹配
/// `repl_main_thread`（output style 变体也算主线程）。
pub fn is_main_thread_source(query_source: Option<&str>) -> bool {
    match query_source {
        None => true,
        Some(source) => source.starts_with("repl_main_thread"),
    }
}

/// TS `microcompactMessages(messages, toolUseContext?, querySource?)`。
pub async fn microcompact_messages(
    messages: Vec<Message>,
    tool_use_context: Option<&crate::tool::ToolUseContext>,
    query_source: Option<&str>,
) -> MicrocompactResult {
    // 每轮 microcompact 尝试开始时清除抑制标记。
    clear_compact_warning_suppression();

    // MEMORY_PROJECTION：官方时间触发 tool_result_clear 的本地投影。
    if feature("MEMORY_PROJECTION") {
        if let Some(projection) =
            clear_tool_result_projection(&messages, query_source, None).await
        {
            return MicrocompactResult {
                messages: projection.messages,
                compaction_info: None,
                cleared_tool_use_ids: projection.cleared_tool_use_ids,
                cleared_replacements: projection.cleared_replacements,
                read_file_state_paths: projection.read_file_state_paths,
            };
        }
    }

    // 时间触发优先短路。
    if let Some(time_based) = maybe_time_based_microcompact(&messages, query_source) {
        return time_based;
    }

    // 缓存编辑仅主线程运行，避免 fork agent 污染全局状态。
    if feature("CACHED_MICROCOMPACT") {
        let model = tool_use_context
            .map(|ctx| ctx.options.main_loop_model.clone())
            .unwrap_or_else(crate::utils::model::model::get_main_loop_model);
        if cached_microcompact::is_cached_microcompact_enabled()
            && cached_microcompact::is_model_supported_for_cache_editing(&model)
            && is_main_thread_source(query_source)
        {
            return cached_microcompact_path(&messages, query_source).await;
        }
    }

    // 旧 microcompact 路径已移除（tengu_cache_plum_violet 恒真）；其他场景
    // 由 autocompact 处理上下文压力。
    MicrocompactResult {
        messages,
        compaction_info: None,
        cleared_tool_use_ids: Vec::new(),
        cleared_replacements: HashMap::new(),
        read_file_state_paths: Vec::new(),
    }
}

/// TS `cachedMicrocompactPath(messages, querySource)`：缓存编辑路径。
async fn cached_microcompact_path(
    messages: &[Message],
    query_source: Option<&str>,
) -> MicrocompactResult {
    let unchanged = || MicrocompactResult {
        messages: messages.to_vec(),
        compaction_info: None,
        cleared_tool_use_ids: Vec::new(),
        cleared_replacements: HashMap::new(),
        read_file_state_paths: Vec::new(),
    };

    {
        let mut guard = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(cached_microcompact::create_cached_mc_state());
        }
        let Some(state) = guard.as_mut() else {
            return unchanged();
        };
        let compactable_tool_ids: HashSet<String> =
            collect_compactable_tool_ids(messages).into_iter().collect();
        // 注册 tool_result（按 user message 分组）。
        for message in messages {
            let Message::User(_) = message else {
                continue;
            };
            let Some(Content::Blocks(blocks)) = message.content() else {
                continue;
            };
            let mut group_ids: Vec<String> = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                if !compactable_tool_ids.contains(tool_use_id)
                    || state.registered_tools.contains(tool_use_id)
                {
                    continue;
                }
                cached_microcompact::register_tool_result(state, tool_use_id);
                group_ids.push(tool_use_id.to_string());
            }
            cached_microcompact::register_tool_message(state, &group_ids);
        }
    }

    let tools_to_delete = {
        let guard = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|state| cached_microcompact::get_tool_results_to_delete(state))
            .unwrap_or_default()
    };

    if tools_to_delete.is_empty() {
        return unchanged();
    }

    // 创建并排队 cache_edits 块。
    let cache_edits = {
        let guard = CACHED_MC_STATE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|state| {
            cached_microcompact::create_cache_edits_block(state, &tools_to_delete)
        })
    };
    if let Some(cache_edits) = cache_edits {
        let mut pending = PENDING_CACHE_EDITS.lock().unwrap_or_else(|e| e.into_inner());
        *pending = Some(cache_edits);
    }

    crate::services::compact::log_event("tengu_cached_microcompact", None);

    // 压缩成功后抑制警告。
    suppress_compact_warning();

    // 通知缓存断点检测：cache read 即将合理下降（对齐 TS
    // notifyCacheDeletion(querySource ?? 'repl_main_thread')；
    // promptCacheBreakDetection 未转换，记为待接线事件）。
    if feature("PROMPT_CACHE_BREAK_DETECTION") {
        crate::services::compact::log_event("tengu_cache_deletion_notify", None);
    }

    // 消息原样返回：cache_reference / cache_edits 由 API 层附加。边界消息
    // 延迟到 API 响应后（用真实 cache_deleted_input_tokens）。取最近
    // assistant 消息 usage 的累计 cache_deleted_input_tokens 作基线。
    let baseline: i64 = messages
        .iter()
        .rev()
        .find(|m| m.typ() == "assistant")
        .and_then(|m| match m {
            Message::Assistant(a) => a.message.usage.as_ref(),
            _ => None,
        })
        .and_then(|usage| usage.get("cache_deleted_input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    MicrocompactResult {
        messages: messages.to_vec(),
        compaction_info: Some(PendingCacheEdits {
            trigger: "auto".to_string(),
            deleted_tool_ids: tools_to_delete,
            baseline_cache_deleted_tokens: baseline,
        }),
        cleared_tool_use_ids: Vec::new(),
        cleared_replacements: HashMap::new(),
        read_file_state_paths: Vec::new(),
    }
}

/// TS `evaluateTimeBasedTrigger(messages, querySource)`：评估时间触发，
/// 返回 (gapMinutes, config)。需要显式的主线程 querySource。
pub fn evaluate_time_based_trigger(
    messages: &[Message],
    query_source: Option<&str>,
) -> Option<(f64, TimeBasedMcConfig)> {
    let config = get_time_based_mc_config();
    // 必须显式是主线程 querySource：analysis-only 调用（/context、/compact）
    // 不触发。
    if !config.enabled || query_source.is_none() || !is_main_thread_source(query_source) {
        return None;
    }
    let last_assistant = messages.iter().rev().find(|m| m.typ() == "assistant");
    let Some(last_assistant) = last_assistant else {
        return None;
    };
    let timestamp = last_assistant.timestamp();
    let parsed = chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc).timestamp_millis() as f64);
    let Some(last_ms) = parsed else {
        return None;
    };
    let gap_minutes = (now_ms() - last_ms) / 60_000.0;
    if !gap_minutes.is_finite() || gap_minutes < config.gap_threshold_minutes {
        return None;
    }
    Some((gap_minutes, config))
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// TS `maybeTimeBasedMicrocompact`：内容清空旧 tool_result，保留最近 N 个。
fn maybe_time_based_microcompact(
    messages: &[Message],
    query_source: Option<&str>,
) -> Option<MicrocompactResult> {
    let trigger = evaluate_time_based_trigger(messages, query_source)?;
    let (gap_minutes, config) = trigger;

    let compactable_ids = collect_compactable_tool_ids(messages);

    // 至少保留 1 个：slice(-0) 会保留全部，全部清空则无工作上下文。
    let keep_recent = config.keep_recent.max(1);
    let keep_set: HashSet<&str> = compactable_ids
        [compactable_ids.len().saturating_sub(keep_recent)..]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let clear_list: Vec<String> = compactable_ids
        .iter()
        .filter(|id| !keep_set.contains(id.as_str()))
        .cloned()
        .collect();
    let clear_set: HashSet<&str> = clear_list.iter().map(|s| s.as_str()).collect();

    if clear_set.is_empty() {
        return None;
    }

    let mut tokens_saved = 0usize;
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        let Message::User(_) = message else {
            result.push(message.clone());
            continue;
        };
        let Some(Content::Blocks(blocks)) = message.content() else {
            result.push(message.clone());
            continue;
        };
        let mut touched = false;
        let mut new_content = Vec::with_capacity(blocks.len());
        for block in blocks {
            let is_clear_candidate =
                block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(|id| clear_set.contains(id))
                        .unwrap_or(false);
            // 已清空的内容（字符串 === 占位符）跳过。
            let already_cleared = block
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s == TIME_BASED_MC_CLEARED_MESSAGE)
                .unwrap_or(false);
            if is_clear_candidate && !already_cleared {
                tokens_saved += calculate_tool_result_tokens(block.get("content"));
                touched = true;
                let mut blanked = block.clone();
                blanked["content"] = Value::String(TIME_BASED_MC_CLEARED_MESSAGE.to_string());
                new_content.push(blanked);
            } else {
                new_content.push(block.clone());
            }
        }
        if touched {
            result.push(replace_user_content(message, Content::Blocks(new_content)));
        } else {
            result.push(message.clone());
        }
    }

    if tokens_saved == 0 {
        return None;
    }

    let _ = gap_minutes;
    crate::services::compact::log_event("tengu_time_based_microcompact", None);
    suppress_compact_warning();
    reset_microcompact_state();
    if feature("PROMPT_CACHE_BREAK_DETECTION") && query_source.is_some() {
        crate::services::compact::log_event("tengu_cache_deletion_notify", None);
    }

    Some(MicrocompactResult {
        messages: result,
        compaction_info: None,
        cleared_tool_use_ids: clear_list,
        cleared_replacements: HashMap::new(),
        read_file_state_paths: Vec::new(),
    })
}

fn replace_user_content(message: &Message, content: Content) -> Message {
    match message {
        Message::User(user) => Message::User(crate::services::compact::UserMessage {
            message: crate::services::compact::UserMessageBody {
                role: user.message.role.clone(),
                content,
            },
            ..user.clone()
        }),
        _ => message.clone(),
    }
}

// ---------------------------------------------------------------------------
// MEMORY_PROJECTION：官方时间触发 tool_result_clear 的端口
// ---------------------------------------------------------------------------

/// 官方 keepToolUses：保留最近 N 个白名单 tool_use id。
const MEMORY_PROJECTION_KEEP_TOOL_USES: usize = 5;

/// 官方 mun = 20000（_ALL.js:4544）：GROSS 预估必须 >= 20000 才执行
/// （== 20000 也执行）。
const MEMORY_PROJECTION_MIN_TOKENS_SAVED: usize = 20_000;

/// 官方 minIdleSeconds = 3900s。
const MEMORY_PROJECTION_MIN_IDLE_MS: i64 = 3_900_000;

/// 官方 idle 通道下限：会话内 >= 20 次工具调用。
const MEMORY_PROJECTION_MIN_TOTAL_TOOL_USES: usize = 20;

/// 官方 idle 通道下限：>= 3 个可清结果。
const MEMORY_PROJECTION_MIN_CLEARABLE: usize = 3;

/// 官方 _ALL.js:372 的 Tt 包裹前缀与裸标记。
const ARTIFACT_CONTENT_MARKER_PREFIX: &str = "<artifact-content-authored-by-others/>\n";
const ARTIFACT_CONTENT_MARKER: &str = "<artifact-content-authored-by-others/>";

/// TS `MemoryProjectionCleared`。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryProjectionCleared {
    pub messages: Vec<Message>,
    pub cleared_tool_use_ids: Vec<String>,
    pub cleared_replacements: HashMap<String, String>,
    pub read_file_state_paths: Vec<String>,
    pub tokens_saved: usize,
}

/// `utils/toolResultStorage.ts`（未转换）的结果类型最小 duck-type。
#[derive(Debug, Clone)]
pub struct PersistedToolResult {
    pub filepath: String,
}

#[derive(Debug, Clone)]
pub struct PersistToolResultError {
    pub error: String,
}

/// 持久化函数（DI seam；生产用 utils/toolResultStorage.persistToolResult）。
/// 用 `Pin<Box<dyn Future>>` 支持 dyn 分发（无 async-trait 依赖）。
pub trait ProjectionPersist: Send + Sync {
    fn persist<'a>(
        &'a self,
        content: &'a Value,
        tool_use_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<PersistedToolResult, PersistToolResultError>> + Send + 'a>>;
}

fn is_persist_error(result: &Result<PersistedToolResult, PersistToolResultError>) -> bool {
    result.is_err()
}

fn build_large_tool_result_message(result: &PersistedToolResult) -> String {
    format!("{}: {}", PERSISTED_OUTPUT_TAG, result.filepath)
}

/// 官方 :4544 zl —— 已清空检测。
fn is_projection_cleared(content: &Value) -> bool {
    let Some(s) = content.as_str() else {
        return false;
    };
    let body = s
        .strip_prefix(ARTIFACT_CONTENT_MARKER_PREFIX)
        .unwrap_or(s);
    body == TIME_BASED_MC_CLEARED_MESSAGE || body.starts_with(PERSISTED_OUTPUT_TAG)
}

/// 官方 :4544 Nl —— 原始内容是否携带 artifact-authored 标记。
fn content_includes_artifact_marker(content: &Value) -> bool {
    match content {
        Value::String(s) => s.contains(ARTIFACT_CONTENT_MARKER),
        Value::Array(items) => items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("text")
                && item
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains(ARTIFACT_CONTENT_MARKER))
        }),
        _ => false,
    }
}

/// 官方 tr(:4440)：文档标题扩展名决定的 token 除数。
fn projection_document_token_divisor(title: Option<&str>) -> usize {
    if let Some(title) = title {
        if let Some(idx) = title.rfind('.') {
            let ext = title[idx + 1..].to_ascii_lowercase();
            return match ext.as_str() {
                "json" | "jsonl" | "jsonc" => 2,
                _ => 4,
            };
        }
    }
    4
}

/// 官方 dec0043 Rn —— 单个文档块估算。
fn projection_document_tokens(block: &Value) -> usize {
    let Some(source) = block.get("source") else {
        return IMAGE_MAX_TOKEN_SIZE as usize;
    };
    match source.get("type").and_then(Value::as_str) {
        Some("text") => rough_token_count_estimation(
            source.get("data").and_then(Value::as_str).unwrap_or(""),
            projection_document_token_divisor(block.get("title").and_then(Value::as_str)),
        ),
        Some("content") => match source.get("content") {
            Some(Value::String(s)) => rough_token_count_estimation(s, 4),
            Some(Value::Array(items)) => items
                .iter()
                .map(|inner| match inner.get("type").and_then(Value::as_str) {
                    Some("text") => rough_token_count_estimation(
                        inner.get("text").and_then(Value::as_str).unwrap_or(""),
                        4,
                    ),
                    _ => IMAGE_MAX_TOKEN_SIZE as usize,
                })
                .sum(),
            _ => IMAGE_MAX_TOKEN_SIZE as usize,
        },
        _ => IMAGE_MAX_TOKEN_SIZE as usize,
    }
}

/// 官方 :4544 Hl —— tool_result token 估算。
fn projection_tool_result_tokens(content: Option<&Value>) -> usize {
    let Some(content) = content else {
        return 0;
    };
    match content {
        Value::String(s) => rough_token_count_estimation(s, 4),
        Value::Array(items) => {
            let mut total = 0usize;
            for block in items {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        total += rough_token_count_estimation(
                            block.get("text").and_then(Value::as_str).unwrap_or(""),
                            4,
                        );
                    }
                    Some("image") => total += IMAGE_MAX_TOKEN_SIZE as usize,
                    Some("document") => total += projection_document_tokens(block),
                    _ => {}
                }
            }
            total
        }
        _ => 0,
    }
}

fn has_image_or_document_block(content: Option<&Value>) -> bool {
    let Some(Value::Array(items)) = content else {
        return false;
    };
    items.iter().any(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("image") | Some("document")
        )
    })
}

fn collect_total_tool_use_count(messages: &[Message]) -> usize {
    let mut count = 0usize;
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                count += 1;
            }
        }
    }
    count
}

/// TS `clearToolResultProjection(messages, querySource, persist?)`：
/// 官方 tool_result_clear 端口。所有门控（主线程、keepRecent=5、
/// gross >= 20000、闲置/工具数下限）逐条对齐。
pub async fn clear_tool_result_projection(
    messages: &[Message],
    query_source: Option<&str>,
    persist: Option<&dyn ProjectionPersist>,
) -> Option<MemoryProjectionCleared> {
    // 仅主线程：/compact、session_memory 与 agent fork 继承完整会话，
    // 必须看到相同内容（与旧时间触发路径同一谓词）。
    if !query_source.map_or(false, |s| is_main_thread_source(Some(s))) {
        return None;
    }

    let compactable_ids = collect_compactable_tool_ids(messages);
    let keep_set: HashSet<&str> = compactable_ids
        [compactable_ids.len().saturating_sub(MEMORY_PROJECTION_KEEP_TOOL_USES)..]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let clear_id_set: HashSet<String> = compactable_ids
        .iter()
        .filter(|id| !keep_set.contains(id.as_str()))
        .cloned()
        .collect();
    if clear_id_set.is_empty() {
        return None;
    }

    let total_tool_uses = collect_total_tool_use_count(messages);

    // 索引 Read 工具的文件路径（readFileState 释放契约）。
    let mut read_paths_by_id = HashMap::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if block.get("name").and_then(Value::as_str) == Some("Read") {
                if let Some(path) = block
                    .get("input")
                    .and_then(|input| input.get("file_path"))
                    .and_then(Value::as_str)
                {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        read_paths_by_id.insert(id.to_string(), path.to_string());
                    }
                }
            }
        }
    }

    // 官方 PLr 预览：每个未保留且未清空的 tool_result 块都是候选。
    let mut candidates: Vec<(String, Value)> = Vec::new();
    for message in messages {
        let Some(Content::Blocks(blocks)) = message.content() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if !clear_id_set.contains(tool_use_id) {
                continue;
            }
            if is_projection_cleared(block.get("content").unwrap_or(&Value::Null)) {
                continue;
            }
            candidates.push((
                tool_use_id.to_string(),
                block.get("content").cloned().unwrap_or(Value::Null),
            ));
        }
    }
    if candidates.is_empty() {
        return None;
    }

    // 官方单门控：gross 预估 < 20000 直接拒绝（== 20000 放行）。
    let gross_tokens: usize = candidates
        .iter()
        .map(|(_, content)| projection_tool_result_tokens(Some(content)))
        .sum();
    if gross_tokens < MEMORY_PROJECTION_MIN_TOKENS_SAVED {
        return None;
    }

    // idle 通道准入（仅遥测）：与官方相同，gross 20k 门控占主导。
    let last_assistant = messages.iter().rev().find(|m| m.typ() == "assistant");
    let gap_ms: f64 = if let Some(last) = last_assistant {
        match chrono::DateTime::parse_from_rfc3339(last.timestamp()) {
            Ok(dt) => now_ms() - dt.with_timezone(&chrono::Utc).timestamp_millis() as f64,
            Err(_) => f64::INFINITY,
        }
    } else {
        f64::INFINITY
    };
    let idle_admitted = gap_ms.is_finite()
        && gap_ms > MEMORY_PROJECTION_MIN_IDLE_MS as f64
        && total_tool_uses >= MEMORY_PROJECTION_MIN_TOTAL_TOOL_USES
        && candidates.len() >= MEMORY_PROJECTION_MIN_CLEARABLE;

    // 官方 :4544 lMo 持久化阶段：内容 truthiness 决定是否 persist。
    let mut content_by_id = HashMap::new();
    for (tool_use_id, content) in &candidates {
        if content == &Value::Null {
            content_by_id.insert(
                tool_use_id.clone(),
                TIME_BASED_MC_CLEARED_MESSAGE.to_string(),
            );
            continue;
        }
        let persisted = if let Some(persist) = persist {
            persist.persist(content, tool_use_id).await
        } else {
            // 生产等价：utils/toolResultStorage.persistToolResult（未转换）。
            Err(PersistToolResultError {
                error: "tool-result storage not converted".to_string(),
            })
        };
        content_by_id.insert(
            tool_use_id.clone(),
            if is_persist_error(&persisted) {
                TIME_BASED_MC_CLEARED_MESSAGE.to_string()
            } else {
                build_large_tool_result_message(
                    persisted
                        .as_ref()
                        .ok()
                        .unwrap_or(&PersistedToolResult { filepath: String::new() }),
                )
            },
        );
    }

    // 官方 :4544 EFt 投影：视觉内容 → 裸占位符；否则持久化预览；
    // 原内容带 artifact 标记时包一次 Tt 前缀；字节相同则不动。
    let mut cleared_replacements = HashMap::new();
    let mut net_tokens_saved = 0usize;
    let mut projection_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Message::User(_) = message else {
            projection_messages.push(message.clone());
            continue;
        };
        let Some(Content::Blocks(blocks)) = message.content() else {
            projection_messages.push(message.clone());
            continue;
        };
        let mut touched = false;
        let mut new_content = Vec::with_capacity(blocks.len());
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                new_content.push(block.clone());
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                new_content.push(block.clone());
                continue;
            };
            if !content_by_id.contains_key(tool_use_id) {
                new_content.push(block.clone());
                continue;
            }
            let original_content = block.get("content").cloned().unwrap_or(Value::Null);
            let visual = has_image_or_document_block(Some(&original_content));
            let mut replacement = if visual {
                TIME_BASED_MC_CLEARED_MESSAGE.to_string()
            } else {
                content_by_id
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| TIME_BASED_MC_CLEARED_MESSAGE.to_string())
            };
            if !visual
                && !replacement.starts_with(ARTIFACT_CONTENT_MARKER_PREFIX)
                && content_includes_artifact_marker(&original_content)
            {
                replacement = format!("{ARTIFACT_CONTENT_MARKER_PREFIX}{replacement}");
            }
            let as_literal = Value::String(replacement.clone());
            if original_content == as_literal {
                new_content.push(block.clone());
                continue;
            }
            touched = true;
            net_tokens_saved += projection_tool_result_tokens(Some(&original_content))
                .saturating_sub(projection_tool_result_tokens(Some(&as_literal)));
            cleared_replacements.insert(tool_use_id.to_string(), replacement);
            let mut blanked = block.clone();
            blanked["content"] = as_literal;
            new_content.push(blanked);
        }
        if touched {
            projection_messages.push(replace_user_content(
                message,
                Content::Blocks(new_content),
            ));
        } else {
            projection_messages.push(message.clone());
        }
    }

    if cleared_replacements.is_empty() {
        return None;
    }

    let read_file_state_paths: Vec<String> = cleared_replacements
        .keys()
        .filter_map(|id| read_paths_by_id.get(id).cloned())
        .collect();

    let _ = idle_admitted;
    crate::services::compact::log_event("tengu_memory_projection_clear", None);

    suppress_compact_warning();
    reset_microcompact_state();
    if feature("PROMPT_CACHE_BREAK_DETECTION") {
        crate::services::compact::log_event("tengu_cache_deletion_notify", None);
    }

    let _ = net_tokens_saved;
    Some(MemoryProjectionCleared {
        messages: projection_messages,
        cleared_tool_use_ids: cleared_replacements.keys().cloned().collect(),
        cleared_replacements,
        read_file_state_paths,
        // 官方遥测基准：gross 预估 Σ Hl。
        tokens_saved: gross_tokens,
    })
}

/// TS `syncProjectionToMutableMessages`：把投影同步回 REPL 持有的可变
/// 消息列表（原地块交换，释放原始大字符串）。
pub fn sync_projection_to_mutable_messages(
    mutable_messages: &mut [Message],
    replacements: &HashMap<String, String>,
) {
    for message in mutable_messages.iter_mut() {
        let Message::User(user) = message else {
            continue;
        };
        let Content::Blocks(blocks) = &mut user.message.content else {
            continue;
        };
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(replacement) = replacements.get(tool_use_id) else {
                continue;
            };
            let current = block.get("content").and_then(Value::as_str).unwrap_or("");
            if current == replacement {
                continue;
            }
            block["content"] = Value::String(replacement.clone());
        }
    }
}

/// 防止未使用导入告警的占位（log_error_dyn / is_env_truthy 在错误路径中使用）。
#[allow(dead_code)]
fn _unused_imports(_e: Box<dyn std::error::Error + Send + Sync + 'static>, _v: Option<&str>) {
    log_error_dyn(_e);
    let _ = is_env_truthy(_v);
}
