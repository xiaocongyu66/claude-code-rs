//! sentry.rs — Sentry 集成的 no-op 占位实现。
//!
//! 重写自 `src/utils/sentry.ts`。语义对齐：TS 版在 SENTRY_DSN 未设置时
//! 所有导出均为 no-op；Cargo.toml 无 sentry 依赖，Rust 侧直接落地为
//! 常驻 no-op（`initialized` 恒为 false），保留等价的 API 表面，
//! 调用方无需条件判断。
//!
//! 偏差：TS 版在 SENTRY_DSN 设置时会真正初始化 Sentry SDK；Rust 侧
//! 目前不发送任何事件（对齐仓库中 "Analytics / Sentry: Empty
//! implementations" 的降级定位）。

use std::collections::HashMap;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// 初始化 Sentry SDK。可重复调用，后续调用 no-op。
///
/// 偏差：仅记录调试日志，不真正初始化（无 sentry 依赖）。
pub fn init_sentry() {
    let dsn = std::env::var("SENTRY_DSN").ok();
    if dsn.is_none() {
        log_for_debugging(
            "[sentry] SENTRY_DSN not set, skipping initialization",
            DebugLogLevel::Debug,
        );
        return;
    }
    log_for_debugging("[sentry] Initialized successfully", DebugLogLevel::Debug);
}

/// 捕获异常并发送到 Sentry。未初始化时 no-op。
///
/// 偏差：`context` 参数仅用于签名对齐，当前实现不使用。
pub fn capture_exception(_error: &(dyn std::error::Error + 'static), _context: Option<HashMap<String, String>>) {
    // no-op（未接入 sentry）
}

/// 在当前 scope 上设置 tag（分组/过滤用）。未初始化时 no-op。
pub fn set_tag(_key: &str, _value: &str) {
    // no-op
}

/// 设置用户上下文（错误归因用）。未初始化时 no-op。
pub fn set_user(_user: SentryUser) {
    // no-op
}

/// 用户上下文（对齐 TS `setUser(user)` 的参数形状）。
#[derive(Debug, Clone, Default)]
pub struct SentryUser {
    pub id: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
}

/// 冲刷待发送事件并关闭客户端。优雅关闭时调用。
pub async fn close_sentry(_timeout_ms: u64) {
    // no-op
}

/// Sentry 是否已初始化（条件 UI 渲染用）。
pub fn is_sentry_initialized() -> bool {
    false
}
