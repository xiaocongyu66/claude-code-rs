//! `services/compact/compactWarningState.ts` — autocompact 警告抑制开关。
//!
//! 压缩成功后立即抑制 "context left until autocompact" 警告（下一次 API
//! 响应前 token 计数不准确）。用 `Mutex` + 订阅者列表实现 TS 的
//! `createStore<boolean>` 语义。

use std::sync::{Arc, Mutex};

type Listener = Arc<dyn Fn() + Send + Sync>;

/// TS `createStore<boolean>(false)` 的最小实现。
pub struct CompactWarningStore {
    state: Mutex<bool>,
    listeners: Mutex<Vec<Listener>>,
}

impl CompactWarningStore {
    const fn new() -> Self {
        Self {
            state: Mutex::new(false),
            listeners: Mutex::new(Vec::new()),
        }
    }

    /// TS `store.getState()`。
    pub fn get_state(&self) -> bool {
        *self.state.lock().unwrap_or(&false)
    }

    /// TS `store.setState(fn)`：应用新值并通知订阅者。
    pub fn set_state(&self, value: bool) {
        if let Ok(mut state) = self.state.lock() {
            *state = value;
        }
        let listeners = self.listeners.lock().map(|l| l.clone()).unwrap_or_default();
        for listener in listeners {
            listener();
        }
    }

    /// TS `store.subscribe(cb)`：返回取消订阅函数。
    pub fn subscribe(&self, callback: Listener) -> impl Fn() + '_ {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        let index = listeners.len();
        listeners.push(callback);
        move || {
            if let Ok(mut listeners) = self.listeners.lock() {
                if index < listeners.len() {
                    listeners.remove(index);
                }
            }
        }
    }
}

/// TS `compactWarningStore`（模块级单例）。`Mutex::new` 在 Rust 1.63+
// 是 const fn，可直接做 static 初始化。
pub static COMPACT_WARNING_STORE: CompactWarningStore = CompactWarningStore::new();

/// TS `suppressCompactWarning()`：压缩成功后抑制警告。
pub fn suppress_compact_warning() {
    COMPACT_WARNING_STORE.set_state(true);
}

/// TS `clearCompactWarningSuppression()`：新一轮压缩开始前清除抑制。
pub fn clear_compact_warning_suppression() {
    COMPACT_WARNING_STORE.set_state(false);
}
