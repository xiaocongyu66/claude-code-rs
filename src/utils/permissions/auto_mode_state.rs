//! auto_mode_state.rs — auto 模式的进程内全局状态。
//!
//! 重写自 `src/utils/permissions/autoModeState.ts`。该模块独立存在的
//! 原因是调用方按 `feature('TRANSCRIPT_CLASSIFIER')` 条件 require，
//! 本身不导入任何其它模块。Rust 侧同样零依赖，用 `AtomicBool` 承载
//! 三个模块级布尔状态（TS 的 `let` 变量）。

use std::sync::atomic::{AtomicBool, Ordering};

static AUTO_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static AUTO_MODE_FLAG_CLI: AtomicBool = AtomicBool::new(false);
// 由异步 verifyAutoModeGateAccess 检查在读取到 GrowthBook 中
// tengu_auto_mode_config.enabled === 'disabled' 时置位。
// is_auto_mode_circuit_break()() 据此阻止 SDK / 显式重入被踢出后的
// auto 模式。
static AUTO_MODE_CIRCUIT_BROKEN: AtomicBool = AtomicBool::new(false);

pub fn set_auto_mode_active(active: bool) {
    AUTO_MODE_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn is_auto_mode_active() -> bool {
    AUTO_MODE_ACTIVE.load(Ordering::SeqCst)
}

pub fn set_auto_mode_flag_cli(passed: bool) {
    AUTO_MODE_FLAG_CLI.store(passed, Ordering::SeqCst);
}

pub fn get_auto_mode_flag_cli() -> bool {
    AUTO_MODE_FLAG_CLI.load(Ordering::SeqCst)
}

pub fn set_auto_mode_circuit_broken(broken: bool) {
    AUTO_MODE_CIRCUIT_BROKEN.store(broken, Ordering::SeqCst);
}

pub fn is_auto_mode_circuit_broken() -> bool {
    AUTO_MODE_CIRCUIT_BROKEN.load(Ordering::SeqCst)
}

pub fn reset_for_testing() {
    AUTO_MODE_ACTIVE.store(false, Ordering::SeqCst);
    AUTO_MODE_FLAG_CLI.store(false, Ordering::SeqCst);
    AUTO_MODE_CIRCUIT_BROKEN.store(false, Ordering::SeqCst);
}
