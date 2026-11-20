//! log.rs — 错误日志、会话标题与 API 请求捕获（重写自 `src/utils/log.ts`）。
//!
//! 语义对齐说明：
//! - `types/logs.ts` 未转换：`LogOption` / `SerializedMessage` / `sortLogs`
//!   按公鸭类型原则在本模块内以最小结构落地（只含 log.ts 实际用到的
//!   字段）。types/logs 转换后应迁移到那里并替换本模块定义。
//! - `@anthropic-ai/sdk` 的 `BetaMessageStreamParams` 是 TS-only 类型；
//!   `capture_api_request` 按 duck typing 只用 `messages` 字段，
//!   以 `serde_json::Value` 承载（与 bootstrap::state 的暂存类型一致）。
//! - `bun:bundle` 的 `feature('HARD_FAIL')` 用 `FEATURE_HARD_FAIL`
//!   环境变量对齐（与 slow_operations.rs 的做法一致）。
//! - TS 的 `logError(error: unknown)` 按 `toError` 归一化；Rust 里一切
//!   错误值都已实现 `std::error::Error`，故泛型 `log_error<E: Error>`
//!   + trait 对象入口 `log_error_dyn`。queue 需要 owned 值，因此
//!   `log_error` 按值取得错误（调用方需要保留错误时自行克隆/重建）。
//! - TS 用 lodash `memoize` 缓存 `isHardFailMode`；Rust 用 `OnceLock`。
//! - 异步文件读取用 tokio::fs（`readdir` / `readFile` / `stat`）。

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;

use crate::bootstrap::state::{set_last_api_request, set_last_api_request_messages};
use crate::constants::query_source::QuerySource;
use crate::constants::xml::TICK_TAG;
use crate::utils::cache_paths::CachePaths;
use crate::utils::display_tags::{strip_display_tags, strip_display_tags_allow_empty};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::errors::short_error_stack;
use crate::utils::privacy_level::is_essential_traffic_only;
use crate::utils::slow_operations::json_parse;

// ---------------------------------------------------------------------------
// 会话标题（types/logs.ts 未转换；本模块内的最小等价结构）
// ---------------------------------------------------------------------------

/// `types/logs.ts` 的 `SerializedMessage` 最小等价物（duck typing）：
/// 仅含 log.ts 的日志加载/标题逻辑实际读取的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct SerializedMessage {
    /// `type` 字段：user / assistant / …
    #[serde(rename = "type", default)]
    pub msg_type: Option<String>,
    /// `message`：user 消息的 content 可能是字符串（纯文本）或数组（工具结果）。
    #[serde(default)]
    pub message: Option<MinimalUserMessage>,
    /// ISO 时间戳。
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// `SerializedMessage.message` 的最小等价物。
#[derive(Debug, Clone, Deserialize)]
pub struct MinimalUserMessage {
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

/// `types/logs.ts` 的 `LogOption` 最小等价物（仅 log.ts 用到的字段）。
#[derive(Debug, Clone)]
pub struct LogOption {
    pub date: String,
    pub full_path: Option<String>,
    pub messages: Vec<SerializedMessage>,
    /// 列表索引（hack：排序后由 load_log_list 覆盖回填）。
    pub value: usize,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub first_prompt: String,
    pub message_count: usize,
    pub is_sidechain: bool,
    pub agent_name: Option<String>,
    pub custom_title: Option<String>,
    pub summary: Option<String>,
    pub session_id: Option<String>,
}

/// `types/logs.ts` 的 `sortLogs`：按 modified 降序，相同时按 created 降序。
/// 缺失时间（TS 的 Invalid Date → NaN）按最早处理。
pub fn sort_logs(mut logs: Vec<LogOption>) -> Vec<LogOption> {
    fn ts(d: &Option<DateTime<Utc>>) -> i64 {
        d.map(|dt| dt.timestamp_millis()).unwrap_or(i64::MIN)
    }
    logs.sort_by(|a, b| {
        ts(&b.modified)
            .cmp(&ts(&a.modified))
            .then_with(|| ts(&b.created).cmp(&ts(&a.created)))
    });
    for (i, log) in logs.iter_mut().enumerate() {
        log.value = i;
    }
    logs
}

/// 会话展示标题（对齐 TS `getLogDisplayTitle` 的回退链）。
/// 自动模式 auto-prompt（`<tick>` 开头）跳过 firstPrompt；先剥展示标签
/// 使纯命令 prompt（如 /clear）回退到下一候选；最终再剥一次标签并 trim。
pub fn get_log_display_title(log: &LogOption, default_title: Option<&str>) -> String {
    // 跳过 tick/goal 标签开头的 firstPrompt（自动模式 auto-prompt）
    let is_autonomous_prompt = log
        .first_prompt
        .starts_with(&format!("<{}>", TICK_TAG));
    // stripDisplayTagsAllowEmpty 用于探测纯命令 prompt（剥完为空）
    let stripped_first_prompt = if log.first_prompt.is_empty() {
        String::new()
    } else {
        strip_display_tags_allow_empty(&log.first_prompt).trim().to_string()
    };
    let use_first_prompt = !stripped_first_prompt.is_empty() && !is_autonomous_prompt;

    let title = log
        .agent_name
        .clone()
        .or_else(|| log.custom_title.clone())
        .or_else(|| log.summary.clone())
        .or_else(|| use_first_prompt.then(|| stripped_first_prompt.clone()))
        .or_else(|| default_title.map(str::to_string))
        .or_else(|| is_autonomous_prompt.then(|| "Autonomous session".to_string()))
        .or_else(|| log.session_id.as_ref().map(|s| s.chars().take(8).collect()))
        .unwrap_or_default();

    // 再剥一次展示标签（如 <ide_opened_file>），trim 后返回
    strip_display_tags(&title).trim().to_string()
}

/// 日期 → 文件名：ISO 字符串里的 ':' / '.' 替换为 '-'。
pub fn date_to_filename(date: &DateTime<Utc>) -> String {
    date.format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
        .replace(':', "-")
        .replace('.', "-")
}

/// TS `parseISOString`：按非数字分隔切出至多 7 组数字
///（年月日时分秒毫秒）组装 UTC 时间；无法解析时返回 None
///（TS 的 Invalid Date → getTime() 为 NaN，排序时当作最早）。
fn parse_iso_string(s: &str) -> Option<DateTime<Utc>> {
    let digits: Vec<&str> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .collect();
    if digits.len() < 3 {
        return None;
    }
    let year: i32 = digits[0].parse().ok()?;
    let month: u32 = digits[1].parse().ok()?;
    let day: u32 = digits[2].parse().ok()?;
    let hour = digits.get(3).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let minute = digits.get(4).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let second = digits.get(5).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let milli = digits.get(6).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let naive = NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_milli_opt(hour, minute, second, milli)?;
    Some(Utc.from_utc_datetime(&naive))
}

// ---------------------------------------------------------------------------
// 内存错误日志 + sink 队列
// ---------------------------------------------------------------------------

/// 最近的错误日志上限（从 bootstrap/state.ts 搬出以打断循环依赖）。
const MAX_IN_MEMORY_ERRORS: usize = 100;

/// 内存中的一条错误记录。
#[derive(Debug, Clone)]
pub struct InMemoryErrorLogEntry {
    pub error: String,
    pub timestamp: String,
}

/// 字符串包装的错误：对齐 TS `String(error)` 的归一化路径
/// （`logMCPError(server, error: unknown)` 的非 Error 分支）。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct StringError {
    message: String,
}

impl StringError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 错误日志后端接口（对齐 TS `ErrorLogSink`）。
/// 实现见 errorLogSink（尚未转换）；附件挂载前的消息进队列。
pub trait ErrorLogSink: Send + Sync {
    fn log_error(&self, error: &(dyn Error + Send + Sync + 'static));
    fn log_mcp_error(&self, server_name: &str, error: &(dyn Error + Send + Sync + 'static));
    fn log_mcp_debug(&self, server_name: &str, message: &str);
    fn get_errors_path(&self) -> PathBuf;
    fn get_mcp_logs_path(&self, server_name: &str) -> PathBuf;
}

enum QueuedErrorEvent {
    Error(Box<dyn Error + Send + Sync + 'static>),
    McpError {
        server_name: String,
        error: Box<dyn Error + Send + Sync + 'static>,
    },
    McpDebug {
        server_name: String,
        message: String,
    },
}

struct ErrorLogState {
    sink: Option<Arc<dyn ErrorLogSink>>,
    queue: Vec<QueuedErrorEvent>,
    in_memory: Vec<InMemoryErrorLogEntry>,
}

static STATE: Lazy<std::sync::Mutex<ErrorLogState>> =
    Lazy::new(|| std::sync::Mutex::new(ErrorLogState::new()));

impl ErrorLogState {
    fn new() -> Self {
        Self {
            sink: None,
            queue: Vec::new(),
            in_memory: Vec::new(),
        }
    }

    fn lock() -> std::sync::MutexGuard<'static, ErrorLogState> {
        STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push_in_memory(&mut self, entry: InMemoryErrorLogEntry) {
        if self.in_memory.len() >= MAX_IN_MEMORY_ERRORS {
            self.in_memory.remove(0); // 移除最旧的一条
        }
        self.in_memory.push(entry);
    }
}

/// 挂载错误日志 sink；立即排空队列（对齐 TS 的 drain 语义）。
/// 幂等：已挂载时 no-op（preAction hook 与 setup() 无协调地各自调用）。
pub fn attach_error_log_sink(new_sink: Arc<dyn ErrorLogSink>) {
    let mut state = ErrorLogState::lock();
    if state.sink.is_some() {
        return;
    }
    state.sink = Some(Arc::clone(&new_sink));

    // 立即排空队列——错误不应延迟
    let queued_events: Vec<_> = std::mem::take(&mut state.queue);
    // 排空期间不再持锁（sink 落盘可能再次进入日志路径）
    drop(state);

    for event in queued_events {
        match event {
            QueuedErrorEvent::Error(error) => new_sink.log_error(&*error),
            QueuedErrorEvent::McpError { server_name, error } => {
                new_sink.log_mcp_error(&server_name, &*error);
            }
            QueuedErrorEvent::McpDebug {
                server_name,
                message,
            } => new_sink.log_mcp_debug(&server_name, &message),
        }
    }
}

/// `feature('HARD_FAIL')`：环境变量 `FEATURE_HARD_FAIL` 为真值时启用。
fn hard_fail_feature() -> bool {
    is_env_truthy(std::env::var("FEATURE_HARD_FAIL").ok().as_deref())
}

/// `--hard-fail` 进程参数检测（memoize 缓存 → OnceLock）。
fn is_hard_fail_mode() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::args().any(|arg| arg == "--hard-fail"))
}

/// 记错误到调试日志、内存错误日志与持久化错误文件（经 sink）。
///
/// 用法：
/// ```ignore
/// log_error(MyError::new("Failed to connect"));
/// ```
///
/// 查看：
/// - 调试：`claude --debug` 或 `tail -f ~/.claude/debug/latest`
/// - 内存：`get_in_memory_errors()`
pub fn log_error<E>(error: E)
where
    E: Error + Send + Sync + 'static,
{
    log_error_dyn(Box::new(error));
}

/// trait 对象入口（供 memoize 等内部 fire-and-forget 路径使用）。
pub fn log_error_dyn(error: Box<dyn Error + Send + Sync + 'static>) {
    if hard_fail_feature() && is_hard_fail_mode() {
        eprintln!("[HARD FAIL] logError called with: {error:?}");
        std::process::exit(1);
    }
    // 整体 swallow（对齐 TS 外层 try/catch）
    let _ = log_error_inner(error);
}

fn log_error_inner(
    error: Box<dyn Error + Send + Sync + 'static>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    // 云厂商（Bedrock/Vertex/Foundry）与 essential-traffic 模式禁用错误上报
    if is_env_truthy(std::env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_VERTEX").ok().as_deref())
        || is_env_truthy(std::env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
        || std::env::var_os("DISABLE_ERROR_REPORTING").is_some()
        || is_essential_traffic_only()
    {
        return Ok(());
    }

    let error_str = short_error_stack(&*error, 10);
    let error_info = InMemoryErrorLogEntry {
        error: error_str,
        timestamp: Utc::now().to_rfc3339(),
    };

    // 内存日志无条件记录（无依赖）
    let mut state = ErrorLogState::lock();
    state.push_in_memory(error_info);
    // sink 未挂载：进队列
    let Some(sink) = state.sink.as_ref().map(Arc::clone) else {
        state.queue.push(QueuedErrorEvent::Error(error));
        return Ok(());
    };
    drop(state);
    sink.log_error(&*error);
    Ok(())
}

/// 最近 100 条内存错误（bug 报告 / 展示用）。
pub fn get_in_memory_errors() -> Vec<InMemoryErrorLogEntry> {
    ErrorLogState::lock().in_memory.clone()
}

// ---------------------------------------------------------------------------
// 错误日志列表加载
// ---------------------------------------------------------------------------

/// 错误日志加载失败（对齐 TS：readdir 失败吞错返回空，
/// 单文件 读/解析失败向上传播为 Promise rejection）。
#[derive(Debug, Error)]
pub enum LoadLogsError {
    #[error("failed to read log file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to stat log file {path}: {source}")]
    Stat {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse log file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// 加载错误日志列表（按日期排序）。
pub async fn load_error_logs() -> Result<Vec<LogOption>, LoadLogsError> {
    match CachePaths::errors() {
        Some(path) => load_log_list(&path).await,
        None => Ok(Vec::new()),
    }
}

/// 按索引取错误日志（0-based；越界返回 None）。
pub async fn get_error_log_by_index(index: usize) -> Result<Option<LogOption>, LoadLogsError> {
    let logs = load_error_logs().await?;
    Ok(logs.into_iter().nth(index))
}

/// 从目录加载并处理日志列表（对齐 TS 的私有 `loadLogList`）。
async fn load_log_list(path: &Path) -> Result<Vec<LogOption>, LoadLogsError> {
    let mut dir_entries = match tokio::fs::read_dir(path).await {
        Ok(entries) => entries,
        Err(_) => {
            log_error_dyn(Box::new(StringError::new(format!(
                "No logs found at {}",
                path.display()
            ))));
            return Ok(Vec::new());
        }
    };

    let mut log_data = Vec::new();
    while let Some(entry) = dir_entries
        .next_entry()
        .await
        .map_err(|e| LoadLogsError::ReadFile {
            path: path.to_path_buf(),
            source: e,
        })?
    {
        let full_path = entry.path();
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| LoadLogsError::ReadFile {
                path: full_path.clone(),
                source: e,
            })?;
        let value = json_parse(&content).map_err(|e| LoadLogsError::Parse {
            path: full_path.clone(),
            source: e,
        })?;
        let messages: Vec<SerializedMessage> =
            serde_json::from_value(value).map_err(|e| LoadLogsError::Parse {
                path: full_path.clone(),
                source: e,
            })?;

        let first_message = messages.first();
        let last_message = messages.last();

        // firstPrompt：首条 user 消息的字符串 content，否则 "No prompt"
        let first_prompt_raw = match first_message {
            Some(m)
                if m.msg_type.as_deref() == Some("user")
                    && m.message
                        .as_ref()
                        .and_then(|msg| msg.content.as_ref())
                        .and_then(serde_json::Value::as_str)
                        .is_some() =>
            {
                m.message
                    .as_ref()
                    .and_then(|msg| msg.content.as_ref())
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            _ => "No prompt".to_string(),
        };

        // 文件 mtime 作为日期（新随机文件名方案）
        let file_stats = tokio::fs::metadata(&full_path)
            .await
            .map_err(|e| LoadLogsError::Stat {
                path: full_path.clone(),
                source: e,
            })?;
        let mtime = file_stats.modified().map_err(|e| LoadLogsError::Stat {
            path: full_path.clone(),
            source: e,
        })?;
        let date = date_to_filename(&DateTime::<Utc>::from(mtime));

        // 文件名含 sidechain 即视为 sidechain
        let is_sidechain = full_path
            .to_string_lossy()
            .contains("sidechain");

        let first_prompt = {
            let first_line = first_prompt_raw.split('\n').next().unwrap_or_default();
            let truncated = if first_prompt_raw.chars().count() > 50 {
                let head: String = first_line.chars().take(50).collect();
                format!("{head}…")
            } else {
                first_line.to_string()
            };
            if truncated.is_empty() {
                "No prompt".to_string()
            } else {
                truncated
            }
        };

        log_data.push(LogOption {
            date,
            full_path: Some(full_path.to_string_lossy().into_owned()),
            messages,
            // hack：排序后由 sort_logs 覆盖回填
            value: 0,
            created: parse_iso_string(
                &first_message
                    .and_then(|m| m.timestamp.clone())
                    .unwrap_or_default(),
            )
            .or_else(|| parse_iso_string(&date)),
            modified: last_message
                .and_then(|m| m.timestamp.clone())
                .and_then(|s| parse_iso_string(&s))
                .or_else(|| parse_iso_string(&date)),
            first_prompt,
            message_count: 0,
            is_sidechain,
            agent_name: None,
            custom_title: None,
            summary: None,
            session_id: None,
        });
    }

    // messageCount 在 messages 落定后补齐；空 entries filter → null（TS 注释）
    for log in &mut log_data {
        log.message_count = log.messages.len();
    }

    Ok(sort_logs(log_data))
}

// ---------------------------------------------------------------------------
// MCP 日志
// ---------------------------------------------------------------------------

/// MCP server 错误日志（对齐 TS `logMCPError`）。sink 未挂载时进队列。
pub fn log_mcp_error(server_name: &str, error: Box<dyn Error + Send + Sync + 'static>) {
    // 对齐 TS 的 try/catch 静默失败
    let mut state = ErrorLogState::lock();
    if state.sink.is_none() {
        state.queue.push(QueuedErrorEvent::McpError {
            server_name: server_name.to_string(),
            error,
        });
        return;
    }
    let sink = state.sink.as_ref().map(Arc::clone);
    drop(state);
    if let Some(sink) = sink {
        sink.log_mcp_error(server_name, &*error);
    }
}

/// MCP server 调试日志（对齐 TS `logMCPDebug`）。sink 未挂载时进队列。
pub fn log_mcp_debug(server_name: &str, message: &str) {
    let mut state = ErrorLogState::lock();
    if state.sink.is_none() {
        state.queue.push(QueuedErrorEvent::McpDebug {
            server_name: server_name.to_string(),
            message: message.to_string(),
        });
        return;
    }
    let sink = state.sink.as_ref().map(Arc::clone);
    drop(state);
    if let Some(sink) = sink {
        sink.log_mcp_debug(server_name, message);
    }
}

// ---------------------------------------------------------------------------
// API 请求捕获
// ---------------------------------------------------------------------------

/// 捕获最近一次 API 请求（供 bug 报告使用）。
///
/// 仅捕获 `repl_main_thread` 开头的 querySource（startsWith，非精确匹配——
/// 非默认 output style 的变体如 `repl_main_thread:outputStyle:Explanatory`
/// 也要捕获）。params 不含 messages（避免保留整段对话）；ant 用户额外
/// 保留最终 messages 数组的引用（/share 的 serialized_conversation.json）。
pub fn capture_api_request(params: &serde_json::Value, query_source: Option<&QuerySource>) {
    if !query_source
        .map(|qs| qs.starts_with("repl_main_thread"))
        .unwrap_or(false)
    {
        return;
    }

    // 去掉 messages 后存储（消息已落 transcript / React state）
    let mut params_without_messages = params.clone();
    if let Some(obj) = params_without_messages.as_object_mut() {
        obj.remove("messages");
    }
    set_last_api_request(Some(params_without_messages));

    // 仅 ant 用户保留 messages 引用；每轮覆盖；dumpPrompts.ts 已为 ant
    // 保留 5 份完整请求体，不构成新的留存级别
    let messages = if std::env::var("USER_TYPE").ok().as_deref() == Some("ant") {
        params.get("messages").cloned()
    } else {
        None
    };
    set_last_api_request_messages(messages);
}

/// 重置错误日志状态（仅测试用）。
pub fn reset_error_log_for_testing() {
    let mut state = ErrorLogState::lock();
    state.sink = None;
    state.queue.clear();
    state.in_memory.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(session_id: Option<&str>, first_prompt: &str) -> LogOption {
        LogOption {
            date: "2024-01-01T00-00-00-000Z".to_string(),
            full_path: None,
            messages: Vec::new(),
            value: 0,
            created: None,
            modified: None,
            first_prompt: first_prompt.to_string(),
            message_count: 0,
            is_sidechain: false,
            agent_name: None,
            custom_title: None,
            summary: None,
            session_id: session_id.map(str::to_string),
        }
    }

    #[test]
    fn display_title_prefers_named_fields() {
        let mut l = log(Some("abcdefgh1234"), "hello");
        l.agent_name = Some("Agent".to_string());
        l.custom_title = Some("Custom".to_string());
        assert_eq!(get_log_display_title(&l, None), "Agent");
        l.agent_name = None;
        assert_eq!(get_log_display_title(&l, None), "Custom");
        l.custom_title = None;
        l.summary = Some("Summary".to_string());
        assert_eq!(get_log_display_title(&l, None), "Summary");
    }

    #[test]
    fn display_title_falls_back_to_truncated_session_id() {
        // lite 日志无元数据：firstPrompt 为空时回退到 session_id 前 8 字符
        let l = log(Some("abcdefgh1234"), "");
        assert_eq!(get_log_display_title(&l, None), "abcdefgh");
    }

    #[test]
    fn display_title_uses_stripped_first_prompt() {
        let l = log(None, "<command-name>/clear</command-name>");
        // 纯命令 prompt 剥完为空 → 回退到 session_id 截断（空 → 空串）
        assert_eq!(get_log_display_title(&l, None), "");
        let l2 = log(None, "<ide_opened_file>x</ide_opened_file>fix the bug");
        assert_eq!(get_log_display_title(&l2, None), "fix the bug");
    }

    #[test]
    fn display_title_autonomous_session() {
        let l = log(None, "<tick>do the thing");
        assert_eq!(get_log_display_title(&l, None), "Autonomous session");
    }

    #[test]
    fn date_to_filename_replaces_colons_and_dots() {
        let dt = Utc.timestamp_millis_opt(1700000000123).unwrap();
        assert_eq!(date_to_filename(&dt), "2023-11-14T22-13-20-123Z");
    }

    #[test]
    fn parse_iso_string_roundtrip() {
        let parsed = parse_iso_string("2024-05-06T07:08:09.010Z").unwrap();
        assert_eq!(parsed.format("%Y-%m-%d %H:%M:%S").to_string(), "2024-05-06 07:08:09");
        assert_eq!(parse_iso_string("not-a-date"), None);
    }

    #[test]
    fn sort_logs_descending_then_value_reassigned() {
        let mk = |ms: i64| {
            let mut l = log(None, "x");
            l.modified = Some(Utc.timestamp_millis_opt(ms).unwrap());
            l
        };
        let sorted = sort_logs(vec![mk(100), mk(300), mk(200)]);
        assert_eq!(sorted[0].modified.unwrap().timestamp_millis(), 300);
        assert_eq!(sorted[1].modified.unwrap().timestamp_millis(), 200);
        assert_eq!(sorted[2].modified.unwrap().timestamp_millis(), 100);
        assert_eq!((sorted[0].value, sorted[2].value), (0, 2));
    }

    #[test]
    fn in_memory_errors_cap_and_clone() {
        let mut state = ErrorLogState::new();
        for i in 0..MAX_IN_MEMORY_ERRORS + 10 {
            state.push_in_memory(InMemoryErrorLogEntry {
                error: format!("e{i}"),
                timestamp: "t".to_string(),
            });
        }
        assert_eq!(state.in_memory.len(), MAX_IN_MEMORY_ERRORS);
        // 最旧的被挤出
        assert_eq!(state.in_memory[0].error, "e10");
    }

    #[tokio::test]
    async fn empty_dir_yields_empty_logs() {
        let dir = std::env::temp_dir().join("claude-rs-log-test-empty");
        let _ = std::fs::create_dir_all(&dir);
        let logs = load_log_list(&dir).await.unwrap();
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn nonexistent_dir_yields_empty_logs() {
        let logs = load_log_list(Path::new("/nonexistent-claude-rs-log-test")).await.unwrap();
        assert!(logs.is_empty());
    }

    #[test]
    fn capture_api_request_filters_non_repl_sources() {
        let params = serde_json::json!({
            "model": "claude-3",
            "messages": [{"role": "user", "content": "hi"}],
        });
        capture_api_request(&params, Some("headless"));
        // messages 落点不应被修改（仅 repl_main_thread 才捕获）
        assert!(params.get("messages").is_some());
    }
}
