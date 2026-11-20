//! hook_registry.rs — 主线程 hook 注册表（重写自 `hookRegistry.ts`）。
//!
//! 主循环生命周期 hook 的进程内注册表，与 post-sampling hook 注册表同构，
//! 提供完整的 register / replace / clear API。
//!
//! 架构债（与 TS 版一致地继承）：本注册表与 settings 驱动的 hook matcher
//! （hooksSettings / sessionHooks，即 settings.json "hooks" 管道，执行
//! shell/HTTP/prompt hook）**共存但不语义互通**：两套共享事件 *名*，但
//! 配置、匹配与投递保证互不相通。程序化注册（bridge、SDK、lifecycle 三
//! 件套）只能经 `dispatch_main_thread_hook_event` 触达，settings.json 的
//! hook 永不进入本注册表。
//!
//! 转发语义：条目可声明接受的来源。`CLAUDE_CODE_DISABLE_HOOK_FORWARDING`
//! 置位时，标记为 forwarded（远程/bridge 会话中继）的事件被丢弃；
//! `CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING` 置位时，plugin 来源条目对
//! forwarded 事件跳过。本地发起的分发永不被门控—— Kill switch 不得打破
//! 本地循环。`forwarded` 保留给 peer forwarding（未连线）。
//!
//! 语义对齐：
//! - handler 可返回结构化裁决（如 PreModelSwitch 的 permissionDecision
//!   allow/deny/ask）；返回值只让裁决 **可观测**，不改变分发本身。
//! - 单一分发消费点：串行 await 该事件的全部 hook；单个 hook 抛错只记
//!   日志，不中断其余分发（Rust 端通过 `tokio::task::spawn` 隔离 panic）。
//!
//! 已知差异：
//! - TS 的 `MainThreadHookHandler` 是 `(payload) => verdict | Promise<verdict>`；
//!   Rust 端为 `Box` 化 `Send` future 的闭包（`MainThreadHookHandler`）。
//! - handler panic 的等价处理：spawn + JoinHandle Err 路径 `log_error`。
//! - 异步分发须处于 tokio 运行时上下文。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::Utc;
use serde_json::{Map, Value};

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::env::forwarding_gates::{
    is_hook_forwarding_disabled, is_plugin_forwarding_disabled,
};
use crate::utils::log::log_error;

/// hook 来源：plugin 来源条目服从 plugin-forwarding 门控。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MainThreadHookOrigin {
    Local,
    Plugin,
}

/// 结构化裁决（对齐 TS `MainThreadHookVerdict = Record<string, unknown>`）。
pub type MainThreadHookVerdict = Map<String, Value>;

/// handler future：`Pin<Box<dyn Future<Output = Option<Verdict>> + Send>>`。
pub type MainThreadHookFuture =
    Pin<Box<dyn Future<Output = Option<MainThreadHookVerdict>> + Send>>;

/// hook 处理器（对齐 TS `MainThreadHookHandler`）。
/// 闭包接收 payload 引用，返回 'static future（异步 handler 须自行 clone
/// 所需数据，与 TS 闭包捕获语义一致）。
pub type MainThreadHookHandler =
    Arc<dyn Fn(&Map<String, Value>) -> MainThreadHookFuture + Send + Sync>;

/// 注册表中的 hook 条目（对齐 TS `MainThreadHook`）。
#[derive(Clone)]
pub struct MainThreadHook {
    /// 稳定 id（register 返回，replace/remove 使用）。
    pub id: String,
    /// debug 日志用的人类可读名称。
    pub name: String,
    /// 响应的事件名集合。
    pub events: Vec<String>,
    /// 来源；plugin hook 服从 plugin-forwarding 门控。
    pub origin: Option<MainThreadHookOrigin>,
    pub callback: MainThreadHookHandler,
    /// 注册序号（内部）：HashMap 不保序，用它在 get/dispatch 时还原
    /// TS Map 的注册顺序。
    seq: u64,
}

/// 注册参数（对齐 TS `registerMainThreadHook` 的入参，id 可选）。
pub struct RegisterMainThreadHook {
    pub id: Option<String>,
    pub name: String,
    pub events: Vec<String>,
    pub origin: Option<MainThreadHookOrigin>,
    pub callback: MainThreadHookHandler,
}

/// 替换参数（对齐 TS `Partial<Omit<MainThreadHook, 'id'>>`）。
pub struct ReplaceMainThreadHook {
    pub name: Option<String>,
    pub events: Option<Vec<String>>,
    pub origin: Option<Option<MainThreadHookOrigin>>,
    pub callback: Option<MainThreadHookHandler>,
}

/// 分发结果（对齐 TS `MainThreadHookDispatchResult`）。
#[derive(Clone, Debug)]
pub struct MainThreadHookDispatchResult {
    /// 产生裁决的 hook 名称。
    pub hook: String,
    pub id: String,
    /// handler 返回的裁决对象。
    pub verdict: MainThreadHookVerdict,
}

/// 分发选项（对齐 TS `DispatchMainThreadHookEventOptions`）。
#[derive(Clone, Copy, Debug, Default)]
pub struct DispatchMainThreadHookEventOptions {
    /// 事件是否由远程/bridge 会话中继而来。保留字段（未连线）。
    pub forwarded: bool,
}

struct RegistryState {
    hooks: HashMap<String, MainThreadHook>,
    next_ordinal: u64,
}

static REGISTRY: LazyLock<Mutex<RegistryState>> = LazyLock::new(|| {
    Mutex::new(RegistryState {
        hooks: HashMap::new(),
        next_ordinal: 0,
    })
});

/// 毒锁自愈：poisoned 时取回内部数据而不是 panic。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn next_hook_id(state: &mut RegistryState) -> String {
    state.next_ordinal += 1;
    format!(
        "main-thread-hook-{}-{}",
        state.next_ordinal,
        Utc::now().timestamp_millis()
    )
}

/// 注册主线程 hook。支持同 id 的 replace 式重注册（upsert 由
/// `replace_main_thread_hook` 完成）。返回 id。
pub fn register_main_thread_hook(hook: RegisterMainThreadHook) -> String {
    let mut state = lock(&REGISTRY);
    let id = hook.id.clone().unwrap_or_else(|| next_hook_id(&mut state));
    let seq = {
        state.next_ordinal += 1;
        state.next_ordinal
    };
    let events_joined = hook.events.join(", ");
    let hook_id = id.clone();
    let hook_name = hook.name.clone();
    state.hooks.insert(
        id.clone(),
        MainThreadHook {
            id,
            name: hook.name,
            events: hook.events,
            origin: hook.origin,
            callback: hook.callback,
            seq,
        },
    );
    drop(state);
    log_for_debugging(
        &format!(
            "[hookRegistry] Registered {} ({}) for events: {}",
            hook_name, hook_id, events_joined
        ),
        DebugLogLevel::Debug,
    );
    hook_id
}

/// 按 id 替换（upsert）已有 hook。id 未知时返回 false。
pub fn replace_main_thread_hook(id: &str, next: ReplaceMainThreadHook) -> bool {
    let mut state = lock(&REGISTRY);
    let Some(existing) = state.hooks.get(id).cloned() else {
        return false;
    };
    state.hooks.insert(
        id.to_string(),
        MainThreadHook {
            id: existing.id,
            name: next.name.unwrap_or(existing.name),
            events: next.events.unwrap_or(existing.events),
            origin: next.origin.unwrap_or(existing.origin),
            callback: next.callback.unwrap_or(existing.callback),
            // 保持原注册顺序（TS `{...existing, ...next}` 语义）
            seq: existing.seq,
        },
    );
    true
}

/// 按 id 移除单个 hook。有移除时返回 true。
pub fn remove_main_thread_hook(id: &str) -> bool {
    lock(&REGISTRY).hooks.remove(id).is_some()
}

/// 清空已注册 hook——全部，或只清 listening 指定事件者。返回移除条数。
pub fn clear_main_thread_hooks(event: Option<&str>) -> usize {
    let mut state = lock(&REGISTRY);
    match event {
        None => {
            let count = state.hooks.len();
            state.hooks.clear();
            count
        }
        Some(event) => {
            let removed: Vec<String> = state
                .hooks
                .iter()
                .filter(|(_, hook)| hook.events.iter().any(|e| e == event))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &removed {
                state.hooks.remove(id);
            }
            removed.len()
        }
    }
}

/// 快照 listening 指定事件的 hook（按注册顺序——TS Map 的迭代顺序）。
pub fn get_main_thread_hooks(event: &str) -> Vec<MainThreadHook> {
    let state = lock(&REGISTRY);
    let mut hooks: Vec<MainThreadHook> = state
        .hooks
        .values()
        .filter(|hook| hook.events.iter().any(|e| e == event))
        .cloned()
        .collect();
    hooks.sort_by_key(|hook| hook.seq);
    hooks
}

pub fn has_main_thread_hooks(event: &str) -> bool {
    !get_main_thread_hooks(event).is_empty()
}

/// 测试/teardown 辅助——清空注册表并返回移除条数。
pub fn reset_main_thread_hook_registry() -> usize {
    clear_main_thread_hooks(None)
}

/// 单一分发消费点。串行触发该事件的所有注册 hook。单个 hook 出错只记
/// 日志、不中断其余分发（与 `execute_post_sampling_hooks` 行为一致）。
///
/// 返回 handler 实际返回的裁决（跳过 void）。裁决意味着什么由调用方——
/// lifecycle emitter——决定；注册表只负责传达。
pub async fn dispatch_main_thread_hook_event(
    event: &str,
    payload: &Map<String, Value>,
    options: &DispatchMainThreadHookEventOptions,
) -> Vec<MainThreadHookDispatchResult> {
    let forwarded = options.forwarded;
    let hooks = get_main_thread_hooks(event);
    let mut dispatch_results: Vec<MainThreadHookDispatchResult> = Vec::new();
    for hook in hooks {
        // 转发门控收敛为单条跳过决策：事件级 hook kill switch 对所有
        // hook 生效，plugin 来源门控只对 plugin 条目生效。本地分发
        // 永不被门控。
        if forwarded {
            let hook_gate_hits = is_hook_forwarding_disabled();
            let plugin_gate_hits = hook.origin == Some(MainThreadHookOrigin::Plugin)
                && is_plugin_forwarding_disabled();
            if hook_gate_hits || plugin_gate_hits {
                // 优先级注记：双门控同时命中时，hook 级门控是丢弃分发的
                // 那个（先检查）。
                let by_hook_gate = hook_gate_hits || !plugin_gate_hits;
                log_for_debugging(
                    &format!(
                        "[hookRegistry] Skipped {}{} for forwarded {}: {} forwarding disabled",
                        if by_hook_gate { "" } else { "plugin hook " },
                        hook.name,
                        event,
                        if by_hook_gate { "hook" } else { "plugin" },
                    ),
                    DebugLogLevel::Debug,
                );
                continue;
            }
        }
        // spawn 隔离 handler panic（对齐 TS try/catch: logError 不打断其余分发）。
        let name = hook.name.clone();
        let id = hook.id.clone();
        let callback = hook.callback.clone();
        let payload = Arc::new(payload.clone());
        let join = tokio::task::spawn(async move { callback(&payload) }).await;
        match join {
            Ok(Some(verdict)) => dispatch_results.push(MainThreadHookDispatchResult {
                hook: name,
                id,
                verdict,
            }),
            Ok(None) => {}
            Err(join_error) => log_error(join_error),
        }
    }
    dispatch_results
}
