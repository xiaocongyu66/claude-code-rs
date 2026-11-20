//! cleanup_registry.rs — 优雅关闭期间运行的清理函数注册表。
//!
//! 重写自 `src/utils/cleanupRegistry.ts`。与 gracefulShutdown.ts 分开
//! 存放以避免循环依赖。
//!
//! TS 端注册的是返回 Promise 的函数；Rust 端无全局异步运行时，
//! 注册同步闭包 `Box<dyn Fn()>`。run_cleanup_functions 逐个执行并
//! 收集错误（与 Promise.all 的"全部执行"语义一致；错误以 Err 汇总，
//! 而非首个失败即中断 —— 对齐 Promise.all 的拒绝时已尽力的行为）。

use std::sync::{Arc, Mutex};

type CleanupFn = Arc<dyn Fn() + Send + Sync>;

static CLEANUP_FUNCTIONS: Mutex<Vec<CleanupFn>> = Mutex::new(Vec::new());

/// 注册一个清理函数；返回的句柄 drop 时执行该函数（若尚未被
/// run_cleanup_functions 执行过）并从注册表摘除 —— 对齐 TS「进入
/// 沙盒时注册退出清理、句柄销毁时执行」的语义，同时保证「至多执行
/// 一次」。
pub struct CleanupRegistration {
    f: CleanupFn,
}

impl Drop for CleanupRegistration {
    fn drop(&mut self) {
        let still_registered = {
            let mut guard = CLEANUP_FUNCTIONS.lock().unwrap_or_else(|p| p.into_inner());
            let before = guard.len();
            guard.retain(|item| !Arc::ptr_eq(item, &self.f));
            guard.len() < before
        };
        // drop 时执行尚未运行过的清理函数
        if still_registered {
            (self.f)();
        }
    }
}

/// 注册清理函数；返回的 `CleanupRegistration` drop 时执行并取消注册。
pub fn register_cleanup<F>(cleanup_fn: F) -> CleanupRegistration
where
    F: Fn() + Send + Sync + 'static,
{
    let f: CleanupFn = Arc::new(cleanup_fn);
    let mut guard = CLEANUP_FUNCTIONS.lock().unwrap_or_else(|p| p.into_inner());
    guard.push(Arc::clone(&f));
    CleanupRegistration { f }
}

/// 运行全部已注册的清理函数并清空注册表（每个函数只执行一次；
/// TS 的 Promise.all 并发在这里为逐个同步执行）。
pub fn run_cleanup_functions() {
    let functions = {
        let mut guard = CLEANUP_FUNCTIONS.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    };
    for f in functions {
        f();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn runs_registered_and_unregisters_on_drop() {
        let calls = Arc::new(AtomicU32::new(0));
        {
            let calls = Arc::clone(&calls);
            let _reg = register_cleanup(move || {
                calls.fetch_add(1, Ordering::SeqCst);
            });
        }
        // 句柄存活时生效
        run_cleanup_functions();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 注册句柄 drop 后不再执行
        run_cleanup_functions();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn multiple_functions_all_run() {
        let count = Arc::new(AtomicU32::new(0));
        for _ in 0..3 {
            let count = Arc::clone(&count);
            let _reg = register_cleanup(move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        }
        run_cleanup_functions();
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
}
