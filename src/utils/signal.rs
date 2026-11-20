//! signal.rs — 纯事件信号（无存储状态的监听器集合）。
//!
//! 重写自 `src/utils/signal.ts`。把 `Set` + subscribe/notify 样板折叠成
//! 一个原语：订阅者只需要知道"发生了什么"，不需要快照。
//!
//! 与 store（AppState / createStore）的区别：没有 getState、没有快照。
//!
//! 与 TS 实现的已知差异：
//! - TS 用 JS `Set` 迭代，`emit` 期间新增的监听器在当前轮即被访问；
//!   本实现为避免 `Mutex` 重入死锁，先把监听器列表 swap 出来再回调，
//!   `emit` 期间新增的监听器在下一轮 `emit` 才触发。
//! - `emit` 期间取消订阅：TS 中未遍历到的监听器本轮不再触发；
//!   本实现本轮完成后才移除，已注册的监听器仍会在当前轮触发。

use std::sync::{Arc, AtomicU64, Mutex};

type Listener<E> = Box<dyn FnMut(&E) + Send + Sync>;

struct SignalInner<E> {
    listeners: Mutex<Vec<(u64, Listener<E>)>>,
    next_id: AtomicU64,
}

/// 无存储状态的事件信号。
pub struct Signal<E> {
    inner: Arc<SignalInner<E>>,
}

/// 订阅句柄；drop 时自动取消订阅（对齐 TS subscribe 返回的取消函数）。
pub struct Subscription<E> {
    inner: Arc<SignalInner<E>>,
    id: u64,
}

impl<E> Signal<E> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SignalInner {
                listeners: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// 订阅监听器，返回的 `Subscription` drop 时自动取消订阅。
    pub fn subscribe<F>(&self, listener: F) -> Subscription<E>
    where
        F: FnMut(&E) + Send + Sync + 'static,
    {
        let id = self
            .inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((id, Box::new(listener)));
        Subscription {
            inner: Arc::clone(&self.inner),
            id,
        }
    }

    /// 用给定参数调用所有已订阅的监听器。
    pub fn emit(&self, event: &E) {
        // swap-take 避免 `emit` 回调中再次 `emit` / 订阅时重入死锁。
        let mut listeners = {
            let mut guard = self
                .inner
                .listeners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };
        for (_, listener) in listeners.iter_mut() {
            listener(event);
        }
        // 回调期间新增的监听器合并回去，下一轮 emit 触发。
        let mut guard = self
            .inner
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.extend(listeners);
    }

    /// 移除所有监听器（dispose / reset 路径用）。
    pub fn clear(&self) {
        self.inner
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

impl<E> Default for Signal<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Drop for Subscription<E> {
    fn drop(&mut self) {
        let mut guard = self
            .inner
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|(id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_notifies_listeners_and_unsubscribes() {
        let signal = Signal::<u32>::new();
        let count = Arc::new(Mutex::new(0u32));
        let sub = {
            let count = Arc::clone(&count);
            signal.subscribe(move |_e| {
                *count.lock().unwrap() += 1;
            })
        };
        signal.emit(&1);
        signal.emit(&2);
        assert_eq!(*count.lock().unwrap(), 2);
        drop(sub);
        signal.emit(&3);
        assert_eq!(*count.lock().unwrap(), 2);
    }

    #[test]
    fn clear_removes_all_listeners() {
        let signal = Signal::<()>::new();
        let calls = Arc::new(Mutex::new(0u32));
        let _sub = {
            let calls = Arc::clone(&calls);
            signal.subscribe(move |_| {
                *calls.lock().unwrap() += 1;
            })
        };
        signal.emit(&());
        signal.clear();
        signal.emit(&());
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}
