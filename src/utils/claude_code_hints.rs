//! claude_code_hints.rs — Claude Code hints 协议的解析与暂存。
//!
//! 重写自 `src/utils/claudeCodeHints.ts`。语义对齐：
//!
//! - CLI/SDK 可以在 stderr（由 shell 工具并入 stdout）输出自闭合的
//!   `<claude-code-hint />` 标签。harness 扫描工具输出中的这些标签，
//!   在输出到达模型前剥除，并向用户弹出安装提示 —— 无推断、无主动执行。
//! - `extract_claude_code_hints` 返回解析出的提示与剥除后的输出。
//! - 待定提示的单槽存储（非队列）：每会话最多浮出一个提示。TS 用
//!   `useSyncExternalStore` 订阅；Rust 侧由 `Signal<()>` 提供
//!   `subscribe_pending_hint()`，状态读取为同步函数。
//! - 正则全部照搬（HINT_TAG_RE / ATTR_RE 的 multiline 全局语义用
//!   regex crate 的多行 + 迭代实现）。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::signal::Signal;

/// 提示类型。当前 v1 只有 `plugin`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeCodeHintType {
    Plugin,
}

/// 单条提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeHint {
    /// 发射方声明的规范版本。未知版本被丢弃。
    pub v: u32,
    /// 提示判别器。v1 只定义 `plugin`。
    pub hint_type: ClaudeCodeHintType,
    /// 提示载荷。`type: plugin` 时为 `name@marketplace` 形式的 slug。
    pub value: String,
    /// 产生该提示的 shell 命令的第一个 token，便于用户发现错配。
    pub source_command: String,
}

/// 外层标签匹配。锚定整行（multiline），容忍行首行尾空白。
static HINT_TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^[ \t]*<claude-code-hint\s+([^>]*?)\s*/>[ \t]*$").expect("valid hint tag regex")
});

/// 属性匹配：`key="value"` 或 `key=value`（空白或 `/>` 结束）。
/// 引号形式不支持转义序列。
static ATTR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(\w+)=(?:"([^"]*)"|([^\s/>]+))"#).expect("valid attr regex")
});

/// 空行折叠：剥除提示行后留下的连续空行压成最多一个。
static BLANK_RUN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\n{3,}").expect("valid blank-run regex")
});

fn is_supported_version(v: u32) -> bool {
    v == 1
}

fn is_supported_type(t: &str) -> bool {
    t == "plugin"
}

/// 解析标签内容中的键值属性。
pub fn parse_attrs(tag_body: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    for caps in ATTR_RE.captures_iter(tag_body) {
        let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let value = caps
            .get(2)
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        attrs.insert(key.to_string(), value.to_string());
    }
    attrs
}

/// 取命令的第一个空白分隔 token。
pub fn first_command_token(command: &str) -> String {
    let trimmed = command.trim();
    match trimmed.find(char::is_whitespace) {
        Some(idx) => trimmed[..idx].to_string(),
        None => trimmed.to_string(),
    }
}

/// 扫描 shell 工具输出中的提示标签，返回解析出的提示与剥除提示行后的输出。
pub fn extract_claude_code_hints(
    output: &str,
    command: &str,
) -> (Vec<ClaudeCodeHint>, String) {
    // 快速路径：无标签开头序列 → 无工作，无分配。
    if !output.contains("<claude-code-hint") {
        return (Vec::new(), output.to_string());
    }

    let source_command = first_command_token(command);
    let mut hints = Vec::new();

    // 对每个匹配行：校验属性，有效则记录提示并把该行替换为空串。
    let mut stripped = String::with_capacity(output.len());
    let mut last_end = 0usize;
    let mut stripped_any = false;
    for m in HINT_TAG_RE.captures_iter(output) {
        let whole = m.get(0).expect("whole match exists");
        stripped.push_str(&output[last_end..whole.start()]);
        last_end = whole.end();

        let attrs = parse_attrs(whole.as_str());
        let v_raw = attrs.get("v").map(String::as_str).unwrap_or("");
        let v: u32 = match v_raw.parse() {
            Ok(v) => v,
            Err(_) => 0,
        };
        let hint_type = attrs.get("type").map(String::as_str).unwrap_or("");
        let value = attrs.get("value").map(String::as_str).unwrap_or("");

        if !is_supported_version(v) {
            log_for_debugging(
                &format!("[claudeCodeHints] dropped hint with unsupported v={v_raw}"),
                DebugLogLevel::Debug,
            );
            stripped_any = true;
            continue;
        }
        if hint_type.is_empty() || !is_supported_type(hint_type) {
            log_for_debugging(
                &format!("[claudeCodeHints] dropped hint with unsupported type={hint_type}"),
                DebugLogLevel::Debug,
            );
            stripped_any = true;
            continue;
        }
        if value.is_empty() {
            log_for_debugging(
                "[claudeCodeHints] dropped hint with empty value",
                DebugLogLevel::Debug,
            );
            stripped_any = true;
            continue;
        }

        hints.push(ClaudeCodeHint {
            v,
            hint_type: ClaudeCodeHintType::Plugin,
            value: value.to_string(),
            source_command: source_command.clone(),
        });
        stripped_any = true;
    }
    stripped.push_str(&output[last_end..]);

    if hints.is_empty() && !stripped_any {
        return (Vec::new(), stripped);
    }

    // 剥除匹配行会留下空行（周围换行仍在）；折叠多余空行，
    // 让模型可见的输出不会增加纵向空白。
    (hints, BLANK_RUN_RE.replace_all(&stripped, "\n\n").to_string())
}

// ---------------------------------------------------------------------------
// 待定提示存储
// ---------------------------------------------------------------------------

struct PendingHintStore {
    pending: Mutex<Option<ClaudeCodeHint>>,
    shown: Mutex<bool>,
    changed: Signal<()>,
}

static STORE: Lazy<PendingHintStore> = Lazy::new(|| PendingHintStore {
    pending: Mutex::new(None),
    shown: Mutex::new(false),
    changed: Signal::new(),
});

fn notify() {
    STORE.changed.emit(&());
}

/// 原始槽位写入。调用方应先做门控（已安装？已展示？达上限？）。
pub fn set_pending_hint(hint: ClaudeCodeHint) {
    if *STORE.shown.lock().unwrap_or_else(|p| p.into_inner()) {
        return;
    }
    *STORE.pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(hint);
    notify();
}

/// 清空槽位但不翻转会话标记 —— 用于被拒绝的提示。
pub fn clear_pending_hint() {
    let mut pending = STORE.pending.lock().unwrap_or_else(|p| p.into_inner());
    if pending.is_some() {
        *pending = None;
        drop(pending);
        notify();
    }
}

/// 翻转每会话一次的标记。仅当对话框确实展示时调用。
pub fn mark_shown_this_session() {
    *STORE.shown.lock().unwrap_or_else(|p| p.into_inner()) = true;
}

/// 订阅待定提示变化（对齐 TS `subscribeToPendingHint`）。
pub fn subscribe_pending_hint<F>(listener: F) -> crate::utils::signal::Subscription<()>
where
    F: FnMut(&()) + Send + Sync + 'static,
{
    STORE.changed.subscribe(listener)
}

pub fn get_pending_hint_snapshot() -> Option<ClaudeCodeHint> {
    STORE
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

pub fn has_shown_hint_this_session() -> bool {
    *STORE.shown.lock().unwrap_or_else(|p| p.into_inner())
}

/// 仅测试用重置。
pub fn reset_claude_code_hint_store_for_tests() {
    *STORE.pending.lock().unwrap_or_else(|p| p.into_inner()) = None;
    *STORE.shown.lock().unwrap_or_else(|p| p.into_inner()) = false;
}
