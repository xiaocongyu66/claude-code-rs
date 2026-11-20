//! forwarding_gates.rs — 转发 kill-switch（W7 task #58 block 4）。
//!
//! 重写自 `src/utils/env/forwardingGates.ts`。
//!
//! 官方锚点：
//!   - CLAUDE_CODE_DISABLE_HOOK_FORWARDING — 0037-chunk-bxerm1mk
//!     module-005-uncategorized.js：BGt() = platform-ok && hook-forward
//!     capable && !env。设置后 handleForwardedHook 路径失效。
//!   - CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING — 0001-chunk-xrzcaagk
//!     module-006-uncategorized.js：plugin-forward worker 的 isEnabled() 加
//!     `&& !env`；0010-chunk-k40f9rxb 上报 reason "disabled" 的
//!     plugin_forwarding_worker_disabled 遥测。
//!
//! 两者默认转发 ENABLED（门禁关闭）——这些环境变量是选择性退出的
//! kill switch，不是选择性启用的 flag。

use crate::utils::env_utils::is_env_truthy;
use std::env;

/// 转发 hook 执行到/自远程会话被禁用时为 true。
pub fn is_hook_forwarding_disabled() -> bool {
    is_env_truthy(
        env::var("CLAUDE_CODE_DISABLE_HOOK_FORWARDING")
            .ok()
            .as_deref(),
    )
}

/// 反向便利方法，供 `&& is_hook_forwarding_enabled()` 式门禁使用。
pub fn is_hook_forwarding_enabled() -> bool {
    !is_hook_forwarding_disabled()
}

/// 转发插件提供的 hook 被禁用时为 true。
pub fn is_plugin_forwarding_disabled() -> bool {
    is_env_truthy(
        env::var("CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING")
            .ok()
            .as_deref(),
    )
}

/// 反向便利方法，供 `&& is_plugin_forwarding_enabled()` 式门禁使用。
pub fn is_plugin_forwarding_enabled() -> bool {
    !is_plugin_forwarding_disabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::{Mutex, MutexGuard};

    // 同模块测试共享一把锁：Rust 测试默认并行，环境变量读写需要串行化。
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn forwarding_enabled_by_default() {
        let _lock = test_lock();
        std::env::remove_var("CLAUDE_CODE_DISABLE_HOOK_FORWARDING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING");
        assert!(!is_hook_forwarding_disabled());
        assert!(is_hook_forwarding_enabled());
        assert!(!is_plugin_forwarding_disabled());
        assert!(is_plugin_forwarding_enabled());
    }

    #[test]
    fn kill_switches_opt_out() {
        let _lock = test_lock();
        std::env::set_var("CLAUDE_CODE_DISABLE_HOOK_FORWARDING", "1");
        std::env::set_var("CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING", "true");
        assert!(is_hook_forwarding_disabled());
        assert!(!is_hook_forwarding_enabled());
        assert!(is_plugin_forwarding_disabled());
        assert!(!is_plugin_forwarding_enabled());
        std::env::remove_var("CLAUDE_CODE_DISABLE_HOOK_FORWARDING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_PLUGIN_FORWARDING");
    }
}
