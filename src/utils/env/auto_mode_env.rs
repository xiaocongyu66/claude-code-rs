//! auto_mode_env.rs — Auto-mode 环境门禁。
//!
//! 重写自 `src/utils/env/autoModeEnv.ts`。
//!
//! 官方锚点：CLAUDE_CODE_ENABLE_AUTO_MODE 列于
//! 0011-chunk-bxerm1mk/module-003-env-vars.js 与
//! 0044-chunk-zrkp23rq/module-002-env-vars.js 的布尔开关中
//! （"ENABLE_" = 选择性启用，默认关闭）。
//!
//! 统一读出面：auto-mode 的运行时门禁以 permissionSetup.isAutoModeGateEnabled
//! 为主（settings/circuit-breaker/model 检查），该文件在本波次范围外，
//! 采用此门禁的迁移延后；在此之前本模块是 CLAUDE_CODE_ENABLE_AUTO_MODE
//! 的唯一读出面，bridge/hooks 等 allowlist 内的调用方用本模块替代散落的
//! 原始 process.env 读取。

use crate::utils::env_utils::is_env_truthy;
use std::env;

pub const AUTO_MODE_ENV_VAR: &str = "CLAUDE_CODE_ENABLE_AUTO_MODE";

/// 仅当操作者显式启用（'1'/'true'/'yes'/'on'）时为 true。
/// 默认关闭：'0'/'false' 与未设置都解析为禁用。
pub fn is_auto_mode_enabled() -> bool {
    is_env_truthy(env::var(AUTO_MODE_ENV_VAR).ok().as_deref())
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
    fn defaults_off_and_truthy_enables() {
        let _lock = test_lock();
        std::env::remove_var(AUTO_MODE_ENV_VAR);
        assert!(!is_auto_mode_enabled());
        std::env::set_var(AUTO_MODE_ENV_VAR, "1");
        assert!(is_auto_mode_enabled());
        std::env::set_var(AUTO_MODE_ENV_VAR, "false");
        assert!(!is_auto_mode_enabled());
        std::env::remove_var(AUTO_MODE_ENV_VAR);
    }
}
