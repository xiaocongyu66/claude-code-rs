//! session_env_vars.rs — 会话级环境变量（/env 设置）。
//!
//! 重写自 `src/utils/sessionEnvVars.ts`。只作用于 spawn 出来的子进程
//! （由 bash provider 的 env 覆盖逻辑应用），不影响 REPL 进程本身。
//!
//! 对齐 TS：模块级 `Map<string, string>` 单例，本实现用
//! `once_cell::Lazy<Mutex<HashMap<...>>>` 提供。`get_session_env_vars`
//! 返回带锁的 live 视图（对齐 TS ReadonlyMap 的实时语义）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use once_cell::sync::Lazy;

static SESSION_ENV_VARS: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn lock() -> MutexGuard<'static, HashMap<String, String>> {
    SESSION_ENV_VARS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 返回会话环境变量的 live 只读视图（持有锁，迭代期间其他写入会阻塞）。
pub fn get_session_env_vars() -> MutexGuard<'static, HashMap<String, String>> {
    lock()
}

pub fn set_session_env_var(name: &str, value: &str) {
    lock().insert(name.to_string(), value.to_string());
}

pub fn delete_session_env_var(name: &str) {
    lock().remove(name);
}

pub fn clear_session_env_vars() {
    lock().clear();
}
