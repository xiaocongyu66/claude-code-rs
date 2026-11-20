//! pipe_mute_state.rs — Master 侧的逻辑断连状态。
//!
//! 重写自 `src/utils/pipeMuteState.ts`。记录当前哪些 slave pipe 处于
//! "muted"（逻辑断连）状态、哪些带有临时 `/send` 覆盖。
//!
//! 仅 master 本地状态，不属于 socket 协议。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

type Listener = Arc<dyn Fn() + Send + Sync>;

/// muted / send-override 状态机；等价于 TS 模块级全局变量集合。
#[derive(Default)]
pub struct PipeMuteState {
    muted_pipes: RwLock<HashSet<String>>,
    send_overrides: RwLock<HashSet<String>>,
    send_override_version: RwLock<u64>,
    // 监听器集合：override 集合变化时通知（Arc 共享，供 Subscription drop 时摘除）
    listeners: Arc<RwLock<Vec<Listener>>>,
}

impl PipeMuteState {
    pub fn new() -> Self {
        Self::default()
    }

    // --- muted set ---------------------------------------------------------

    /// 整表替换 muted 集合（对齐 setMasterMutedPipes(names)）。
    pub fn set_master_muted_pipes(&self, names: impl IntoIterator<Item = String>) {
        let mut set = self.muted_pipes.write().unwrap_or_else(|p| p.into_inner());
        set.clear();
        set.extend(names);
    }

    pub fn is_master_pipe_muted(&self, name: &str) -> bool {
        self.muted_pipes
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(name)
    }

    pub fn remove_master_pipe_mute(&self, name: &str) {
        self.muted_pipes
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
    }

    pub fn clear_master_muted_pipes(&self) {
        self.muted_pipes
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    // --- send overrides ----------------------------------------------------

    fn emit_send_override_changed(&self) {
        {
            let mut v = self
                .send_override_version
                .write()
                .unwrap_or_else(|p| p.into_inner());
            *v += 1;
        }
        let listeners = self
            .listeners
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for listener in listeners {
            listener();
        }
    }

    pub fn add_send_override(&self, name: &str) {
        self.send_overrides
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string());
        self.emit_send_override_changed();
    }

    pub fn remove_send_override(&self, name: &str) {
        let removed = self
            .send_overrides
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        if removed {
            self.emit_send_override_changed();
        }
    }

    pub fn has_send_override(&self, name: &str) -> bool {
        self.send_overrides
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(name)
    }

    pub fn clear_send_overrides(&self) {
        let mut set = self
            .send_overrides
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if !set.is_empty() {
            set.clear();
            drop(set);
            self.emit_send_override_changed();
        }
    }

    /// 订阅 override 变化；返回的句柄 drop 时自动取消订阅。
    pub fn subscribe_send_override<F>(&self, listener: F) -> Subscription
    where
        F: Fn() + Send + Sync + 'static,
    {
        let arc: Listener = Arc::new(listener);
        self.listeners
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::clone(&arc));
        Subscription {
            listeners: Arc::clone(&self.listeners),
            listener: arc,
        }
    }

    pub fn get_send_override_version(&self) -> u64 {
        *self
            .send_override_version
            .read()
            .unwrap_or_else(|p| p.into_inner())
    }
}

/// 订阅句柄；drop 时从列表中移除监听器。
pub struct Subscription {
    listeners: Arc<RwLock<Vec<Listener>>>,
    listener: Listener,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut guard = self.listeners.write().unwrap_or_else(|p| p.into_inner());
        guard.retain(|item| !Arc::ptr_eq(item, &self.listener));
    }
}

static GLOBAL: Mutex<Option<Arc<PipeMuteState>>> = Mutex::new(None);

/// 进程级单例（对齐 TS 的模块级全局 Set）。
pub fn shared() -> Arc<PipeMuteState> {
    let mut guard = GLOBAL.lock().unwrap_or_else(|p| p.into_inner());
    let entry = guard.get_or_insert_with(|| Arc::new(PipeMuteState::new()));
    Arc::clone(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_set_lifecycle() {
        let state = PipeMuteState::new();
        state.set_master_muted_pipes(["a".to_string(), "b".to_string()]);
        assert!(state.is_master_pipe_muted("a"));
        state.remove_master_pipe_mute("a");
        assert!(!state.is_master_pipe_muted("a"));
        state.clear_master_muted_pipes();
        assert!(!state.is_master_pipe_muted("b"));
    }

    #[test]
    fn send_override_versioning_and_subscribe() {
        let state = PipeMuteState::new();
        let hits = Arc::new(Mutex::new(0u32));
        let sub = {
            let hits = Arc::clone(&hits);
            state.subscribe_send_override(move || {
                *hits.lock().unwrap() += 1;
            })
        };
        assert_eq!(state.get_send_override_version(), 0);
        state.add_send_override("x");
        assert!(state.has_send_override("x"));
        assert_eq!(state.get_send_override_version(), 1);
        // remove 不存在的名字不触发版本号
        state.remove_send_override("nope");
        assert_eq!(state.get_send_override_version(), 1);
        state.remove_send_override("x");
        assert_eq!(state.get_send_override_version(), 2);
        drop(sub);
        state.add_send_override("y");
        assert_eq!(*hits.lock().unwrap(), 2);
        state.clear_send_overrides();
        assert_eq!(state.get_send_override_version(), 3);
    }
}
