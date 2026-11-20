//! workload_context.rs — 回合级 workload 标签（对齐 TS ALS 语义）。
//!
//! 重写自 `src/utils/workloadContext.ts`。语义对齐：
//!
//! - TS 用 AsyncLocalStorage 让 `runWithWorkload(workload, fn)` 内的所有
//!   `getWorkload()` 调用拿到调用时传入的值（含 undefined），并且
//!   **总是建立新上下文边界**——即便 workload 为 None，也防止泄漏到
//!   外层残留的 cron 上下文。
//! - Rust 侧的 API 形状是同步闭包 `run_with_workload(workload, fn)`，
//!   用 thread_local 落地（偏差：AsyncLocalStorage 跨 await 点传播且
//!   按任务隔离；thread_local 按线程隔离。当前所有调用方为同步闭包，
//!   后续若需要跨 await 传播再换成 `tokio::task_local!`）。
//!
//! 独立于 bootstrap/state 的原因（照搬 TS 注释）：browser bundle 不能
//! 引入 Node 的 async_hooks；本模块只被 CLI/SDK 路径引用。

use std::cell::RefCell;

thread_local! {
    static WORKLOAD: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// server 端 sanitizer 只接受 lowercase [a-z0-9_-]{0,32}。
pub const WORKLOAD_CRON: &str = "cron";

/// 当前上下文的 workload 标签；未运行在 workload 上下文中时为 None。
pub fn get_workload() -> Option<String> {
    WORKLOAD.with(|w| w.borrow().clone())
}

/// 在 workload 上下文中执行 `fn`。**总是建立新的上下文边界**，
/// 即便 `workload` 为 None —— 保证 `fn` 内的 `get_workload()`
/// 恰好返回传入值（对齐 TS 注释中的防泄漏语义）。
pub fn run_with_workload<T>(workload: Option<&str>, f: impl FnOnce() -> T) -> T {
    let saved = WORKLOAD.with(|w| w.replace(workload.map(str::to_string)));
    let result = f();
    WORKLOAD.with(|w| {
        *w.borrow_mut() = saved;
    });
    result
}
