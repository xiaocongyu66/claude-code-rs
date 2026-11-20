//! hook_events.rs — hook 执行事件的广播系统（重写自 `hookEvents.ts`）。
//!
//! 与主消息流分离的通用事件系统。处理器（handler）可注册接收事件并自行
//! 决定如何消费（转成 SDK 消息、写日志等）。
//!
//! 语义对齐：
//! - `SessionStart` / `Setup` 始终发射（兼容性低噪生命周期事件）。
//! - 其余事件仅在 `set_all_hook_events_enabled(true)`（SDK
//!   `includeHookEvents` 选项或 CLAUDE_CODE_REMOTE 模式）后发射。
//! - 无 handler 时事件缓冲在 `PENDING_EVENTS`（上限 100，超出丢弃最旧），
//!   handler 注册时即时排空回放（注册回调在锁外调用，避免回调重入死锁）。
//! - `emit_hook_response` 先把完整输出写 debug 日志，再做发射门控。
//! - `start_hook_progress_interval`：tokio 任务按 `interval_ms`（默认
//!   1000ms）轮询 `get_output`，输出未变化则跳过；返回的句柄 stop 等价
//!   TS 的 `clearInterval`。
//!
//! 已知差异：
//! - TS 用 `setInterval` + `unref()`；Rust 端用 `tokio::spawn` +
//!   `tokio::time::interval`，调用方须处于 tokio 运行时上下文。
//! - TS 的未发射分支返回空函数 stop 句柄；Rust 端返回无任务的
//!   `HookProgressInterval`，`stop()` 同样为空操作。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// HOOK_EVENTS（对齐 `src/entrypoints/sdk/coreTypes.ts` 的 27 项事件名）。
pub const HOOK_EVENTS: [&str; 27] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "PermissionRequest",
    "PermissionDenied",
    "Setup",
    "TeammateIdle",
    "TaskCreated",
    "TaskCompleted",
    "Elicitation",
    "ElicitationResult",
    "ConfigChange",
    "WorktreeCreate",
    "WorktreeRemove",
    "InstructionsLoaded",
    "CwdChanged",
    "FileChanged",
];

/// 无论 includeHookEvents 与否都始终发射的事件（原 allowlist，向后兼容）。
const ALWAYS_EMITTED_HOOK_EVENTS: [&str; 2] = ["SessionStart", "Setup"];

const MAX_PENDING_EVENTS: usize = 100;

/// hook 执行结果（对齐 TS `'success' | 'error' | 'cancelled'`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookOutcome {
    Success,
    Error,
    Cancelled,
}

impl HookOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            HookOutcome::Success => "success",
            HookOutcome::Error => "error",
            HookOutcome::Cancelled => "cancelled",
        }
    }
}

/// hook 执行事件（对齐 TS `HookExecutionEvent` 判别联合，serde tag = "type"）。
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookExecutionEvent {
    Started {
        #[serde(rename = "hookId")]
        hook_id: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "hookEvent")]
        hook_event: String,
    },
    Progress {
        #[serde(rename = "hookId")]
        hook_id: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        stdout: String,
        stderr: String,
        output: String,
    },
    Response {
        #[serde(rename = "hookId")]
        hook_id: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        output: String,
        stdout: String,
        stderr: String,
        #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        outcome: HookOutcome,
    },
}

/// 事件处理器（TS `HookEventHandler`）。
pub type HookEventHandler = Arc<dyn Fn(&HookExecutionEvent) + Send + Sync>;

/// `startHookProgressInterval` 的 `getOutput` 异步闭包（Box 化 Future）。
pub type OutputFuture = Pin<Box<dyn Future<Output = (String, String, String)> + Send>>;
pub type OutputFetcher = Arc<dyn Fn() -> OutputFuture + Send + Sync>;

/// 毒锁自愈：poisoned 时取回内部数据而不是 panic。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static EVENT_HANDLER: Mutex<Option<HookEventHandler>> = Mutex::new(None);
static PENDING_EVENTS: Mutex<Vec<HookExecutionEvent>> = Mutex::new(Vec::new());
static ALL_HOOK_EVENTS_ENABLED: Mutex<bool> = Mutex::new(false);

/// 注册事件处理器；若注册时已有缓冲事件，则按顺序回放给新处理器。
/// 传 `None` 等价 TS 的 deregister（后续事件重新进入缓冲）。
pub fn register_hook_event_handler(handler: Option<HookEventHandler>) {
    let drained: Vec<HookExecutionEvent> = {
        let mut handler_guard = lock(&EVENT_HANDLER);
        handler_guard.replace(handler.clone());
        match handler {
            Some(_) => std::mem::take(&mut *lock(&PENDING_EVENTS)),
            None => Vec::new(),
        }
    };
    // 在锁外回放，避免处理器回调重入本模块造成死锁。
    if let Some(handler) = handler {
        for event in drained {
            handler(&event);
        }
    }
}

fn emit(event: HookExecutionEvent) {
    let handler = lock(&EVENT_HANDLER).clone();
    match handler {
        Some(handler) => handler(&event),
        None => {
            let mut pending = lock(&PENDING_EVENTS);
            pending.push(event);
            while pending.len() > MAX_PENDING_EVENTS {
                pending.remove(0);
            }
        }
    }
}

fn should_emit(hook_event: &str) -> bool {
    if ALWAYS_EMITTED_HOOK_EVENTS.contains(&hook_event) {
        return true;
    }
    *lock(&ALL_HOOK_EVENTS_ENABLED) && HOOK_EVENTS.contains(&hook_event)
}

pub fn emit_hook_started(hook_id: &str, hook_name: &str, hook_event: &str) {
    if !should_emit(hook_event) {
        return;
    }
    emit(HookExecutionEvent::Started {
        hook_id: hook_id.to_string(),
        hook_name: hook_name.to_string(),
        hook_event: hook_event.to_string(),
    });
}

/// hook 进度轮询参数（对齐 TS `startHookProgressInterval` 的 params）。
pub struct HookProgressParams {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub get_output: OutputFetcher,
    /// 默认 1000ms。
    pub interval_ms: Option<u64>,
}

/// progress 事件发射的数据载荷。
pub struct HookProgressData {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub stdout: String,
    pub stderr: String,
    pub output: String,
}

pub fn emit_hook_progress(data: &HookProgressData) {
    if !should_emit(&data.hook_event) {
        return;
    }
    emit(HookExecutionEvent::Progress {
        hook_id: data.hook_id.clone(),
        hook_name: data.hook_name.clone(),
        hook_event: data.hook_event.clone(),
        stdout: data.stdout.clone(),
        stderr: data.stderr.clone(),
        output: data.output.clone(),
    });
}

/// 进度轮询任务句柄；`stop()` 等价 TS `clearInterval`。
pub struct HookProgressInterval {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HookProgressInterval {
    /// 停止轮询；未发射事件时为空操作（对齐 TS 的 no-op stop）。
    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// 启动进度轮询任务：按间隔读取输出，output 变化时发射 progress 事件。
/// 与 TS `startHookProgressInterval` 一致，`shouldEmit` 为 false 时返回
/// 空句柄（不 spawn 任务）。
pub fn start_hook_progress_interval(params: HookProgressParams) -> HookProgressInterval {
    if !should_emit(&params.hook_event) {
        return HookProgressInterval { task: None };
    }

    let interval_ms = params.interval_ms.unwrap_or(1000);
    let hook_id = params.hook_id.clone();
    let hook_name = params.hook_name.clone();
    let hook_event = params.hook_event.clone();
    let fetcher = params.get_output.clone();

    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // setInterval 首次触发在间隔之后，跳过 interval 立即产生的首 tick。
        ticker.tick().await;
        let mut last_emitted_output = String::new();
        loop {
            ticker.tick().await;
            let (stdout, stderr, output) = fetcher().await;
            if output == last_emitted_output {
                continue;
            }
            last_emitted_output = output.clone();
            emit_hook_progress(&HookProgressData {
                hook_id: hook_id.clone(),
                hook_name: hook_name.clone(),
                hook_event: hook_event.clone(),
                stdout,
                stderr,
                output,
            });
        }
    });

    HookProgressInterval { task: Some(task) }
}

/// hook 响应事件数据载荷（对齐 TS `emitHookResponse` 的 data）。
pub struct HookResponseData {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub outcome: HookOutcome,
}

pub fn emit_hook_response(data: &HookResponseData) {
    // 无论门控如何，先把完整输出写 debug 日志（verbose 模式排障用）。
    let output_to_log = if !data.stdout.is_empty() {
        &data.stdout
    } else if !data.stderr.is_empty() {
        &data.stderr
    } else {
        &data.output
    };
    if !output_to_log.is_empty() {
        log_for_debugging(
            &format!(
                "Hook {} ({}) {}:\n{}",
                data.hook_name,
                data.hook_event,
                data.outcome.as_str(),
                output_to_log
            ),
            DebugLogLevel::Debug,
        );
    }

    if !should_emit(&data.hook_event) {
        return;
    }

    emit(HookExecutionEvent::Response {
        hook_id: data.hook_id.clone(),
        hook_name: data.hook_name.clone(),
        hook_event: data.hook_event.clone(),
        output: data.output.clone(),
        stdout: data.stdout.clone(),
        stderr: data.stderr.clone(),
        exit_code: data.exit_code,
        outcome: data.outcome,
    });
}

/// 启用全部 hook 事件类型的发射（除 SessionStart/Setup 之外）。
/// SDK `includeHookEvents` 选项置位或 CLAUDE_CODE_REMOTE 模式时调用。
pub fn set_all_hook_events_enabled(enabled: bool) {
    *lock(&ALL_HOOK_EVENTS_ENABLED) = enabled;
}

/// 清空全部事件状态（测试隔离用）。
pub fn clear_hook_event_state() {
    *lock(&EVENT_HANDLER) = None;
    lock(&PENDING_EVENTS).clear();
    *lock(&ALL_HOOK_EVENTS_ENABLED) = false;
}
