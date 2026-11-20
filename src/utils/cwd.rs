//! cwd.rs — 每异步上下文的 cwd 覆盖持有器 + 全局 cwd 访问。
//!
//! 重写自 `src/utils/cwd.ts`。语义对齐：
//!
//! - TS 用 AsyncLocalStorage 携带 `{ value: string }` 盒子，让 runWithCwdOverride
//!   内的所有 pwd()/getCwd() 调用看到覆盖值；盒子可变，Cd 在覆盖上下文
//!   内执行时**原地更新覆盖**（per-agent cwd 变更不泄漏到全局）。
//! - Rust 侧 API 形状为同步闭包，用 thread_local 落地（偏差：ALS 按异步
//!   任务传播；thread_local 按线程隔离。当前调用方为同步闭包，需要跨
//!   await 隔离时再换成 `tokio::task_local!`）。
//! - `set_cwd_override_for_current_context` 写覆盖层时做 NFC 规范化
//!   （对齐全局层 setCwdState 的契约）。
//! - `pwd()` = 覆盖值 ?? `bootstrap::state::get_cwd_state()`。
//! - `get_cwd()` = `pwd()`，拿不到时回退 `get_original_cwd()`。

use std::cell::RefCell;

use unicode_normalization::UnicodeNormalization;

use crate::bootstrap::state;

thread_local! {
    static CWD_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 在覆盖的工作目录中执行 `fn`。
///
/// 覆盖层对当前上下文（及其嵌套调用）生效；并发 agent 各自看到自己的
/// cwd，互不影响。
pub fn run_with_cwd_override<T>(cwd: &str, f: impl FnOnce() -> T) -> T {
    let saved = CWD_OVERRIDE.with(|o| o.replace(Some(cwd.to_string())));
    let result = f();
    CWD_OVERRIDE.with(|o| {
        *o.borrow_mut() = saved;
    });
    result
}

/// 当前上下文是否带有 cwd 覆盖（per-agent 作用域）。
/// CdTool 用它决定 chdir 应用到哪一层。
pub fn has_cwd_override() -> bool {
    CWD_OVERRIDE.with(|o| o.borrow().is_some())
}

/// 为**当前上下文**写入工作目录：有覆盖时更新覆盖（作用域内、
/// 无竞态）；无覆盖时返回 false，调用方自行写全局层
/// （utils/Shell.ts 的 setCwd）。
///
/// 路径规范化与全局契约一致（NFC）。
pub fn set_cwd_override_for_current_context(cwd: &str) -> bool {
    CWD_OVERRIDE.with(|o| {
        let mut override_cell = o.borrow_mut();
        match &mut *override_cell {
            None => false,
            Some(value) => {
                *value = cwd.nfc().collect::<String>();
                true
            }
        }
    })
}

/// 获取当前工作目录（考虑覆盖层）。
pub fn pwd() -> String {
    CWD_OVERRIDE
        .with(|o| o.borrow().clone())
        .unwrap_or_else(state::get_cwd_state)
}

/// 获取当前工作目录；拿不到时回退原始工作目录。
pub fn get_cwd() -> String {
    pwd()
}
