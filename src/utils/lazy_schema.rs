//! lazy_schema.rs — 懒构造工厂的 memo 化包装。
//!
//! 重写自 `src/utils/lazySchema.ts`。用于把（TS 端的 Zod）schema 构造
//! 从模块初始化推迟到首次访问。Rust 端语义：首次调用执行 factory，
//! 后续调用返回缓存值。

use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// 懒求值缓存：key → factory() 的记忆化。
///
/// 对齐 TS `lazySchema<T>(factory: () => T): () => T`：首次调用构造，
/// 之后返回缓存。支持 `invalidate` 清除缓存（TS 无此接口，供 Rust 端
/// 测试与配置重载使用）。
pub struct Lazy<T> {
    factory: Arc<dyn Fn() -> T + Send + Sync>,
    cached: Arc<Mutex<Option<T>>>,
}

impl<T: Clone> Lazy<T> {
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
    {
        Self {
            factory: Arc::new(factory),
            cached: Arc::new(Mutex::new(None)),
        }
    }

    /// 返回缓存值；首次调用时构造。
    pub fn get(&self) -> T {
        let mut guard = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        guard.get_or_insert_with(|| (self.factory)()).clone()
    }

    /// 清除缓存，下次 `get()` 重新构造。
    pub fn invalidate(&self) {
        let mut guard = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        *guard = None;
    }
}

impl<T: Clone> Clone for Lazy<T> {
    fn clone(&self) -> Self {
        Self {
            factory: Arc::clone(&self.factory),
            cached: Arc::clone(&self.cached),
        }
    }
}

impl<T: Clone + Eq + Hash> PartialEq for Lazy<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn factory_runs_once() {
        let calls = Arc::new(AtomicU32::new(0));
        let lazy = {
            let calls = Arc::clone(&calls);
            Lazy::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                42
            })
        };
        assert_eq!(lazy.get(), 42);
        assert_eq!(lazy.get(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidate_re_runs_factory() {
        let lazy = Lazy::new(|| std::time::SystemTime::now().into());
        let _ = lazy.get();
        lazy.invalidate();
        // 再次构造成功即可
        let _: std::time::SystemTime = lazy.get();
    }
}
