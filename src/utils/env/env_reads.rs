//! env_reads.rs — 散落环境变量的统一读出面（W7 资源篮）。
//!
//! 重写自 `src/utils/env/envReads.ts`。
//!
//! 官方锚点（docs/reverse-284/decompiled，env-vars chunks 的 grep 名证据）：
//!   - CLAUDE_CODE_SPAWNED_BY_SERVER  — 0044-chunk-zrkp23rq/module-002-env-vars.js
//!   - CLAUDE_STAGE_FILE_ROOT         — 0004-chunk-zrkp23rq/module-003-env-vars.js
//!     （注意：官方构建中不带 CLAUDE_CODE_ 前缀）
//!   - CLAUDE_CODE_EVAL_CONFINED     — 0001-chunk-xrzcaagk/module-004-env-vars.js
//!     （官方："confined evaluation run"——0039-chunk-zrkp23rq/permissions/checker.js
//!     中 subagent 权限模式的注释）
//!   - CLAUDE_CODE_AGENT_VIEW_RELAUNCH — 0044-chunk-zrkp23rq/module-002-env-vars.js
//!   - CLAUDE_CODE_ARTIFACT_AUTO_OPEN  — 0047-chunk-dxsxk16b/module-001-env-vars.js
//!
//! 所有读取都经 env_utils::is_env_truthy，'0'/'false'/'no'/'off' 视为显式关闭，
//! 与代码库其余部分一致。

use crate::utils::env_utils::is_env_truthy;
use std::env;

/// 由 RCS/server supervisor 设置：本进程是 spawn 出来的会话子进程时为 true。
pub fn is_spawned_by_server() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_SPAWNED_BY_SERVER").ok().as_deref())
}

/// stage 文件（暂存的 prompt/上传）根目录。官方构建使用不带 CODE 前缀的
/// `CLAUDE_STAGE_FILE_ROOT`。空白值视为未设置。
pub fn get_stage_file_root() -> Option<String> {
    match env::var("CLAUDE_STAGE_FILE_ROOT") {
        Ok(root) if !root.trim().is_empty() => Some(root),
        _ => None,
    }
}

/// 处于受限评估运行内时为 true：subagent 声明的权限模式
/// （bypassPermissions/acceptEdits/auto）被钉在父模式上。
pub fn is_eval_confined() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_EVAL_CONFINED").ok().as_deref())
}

/// CLI 被重新启动进入 agent view（web relaunch 流程）时设置。
pub fn is_agent_view_relaunch() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_AGENT_VIEW_RELAUNCH").ok().as_deref())
}

/// 跳过 open-prDescription 确认并自动打开 HTML artifact。
pub fn is_artifact_auto_open() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_ARTIFACT_AUTO_OPEN").ok().as_deref())
}

/// 把 subagent 的 nested-memory（CLAUDE.md）加载触发传播回父上下文。
/// 官方反编译名是较长的 COORDINATOR 形式
/// （0039-chunk-zrkp23rq/module-006-env-vars.js + checker.js 的
/// propagateNestedMemory 清理阶段）；工作规格中出现的短拼法
/// CLAUDE_CODE_PROPAGATE_NESTED_MEMORY 是同一开关的缩写。
pub fn should_propagate_nested_memory() -> bool {
    is_env_truthy(
        env::var("CLAUDE_CODE_COORDINATOR_PROPAGATE_NESTED_MEMORY")
            .ok()
            .as_deref(),
    )
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

    const VARS: [&str; 6] = [
        "CLAUDE_CODE_SPAWNED_BY_SERVER",
        "CLAUDE_STAGE_FILE_ROOT",
        "CLAUDE_CODE_EVAL_CONFINED",
        "CLAUDE_CODE_AGENT_VIEW_RELAUNCH",
        "CLAUDE_CODE_ARTIFACT_AUTO_OPEN",
        "CLAUDE_CODE_COORDINATOR_PROPAGATE_NESTED_MEMORY",
    ];

    fn clear_all() {
        for var in VARS {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn defaults_off() {
        let _lock = test_lock();
        clear_all();
        assert!(!is_spawned_by_server());
        assert_eq!(get_stage_file_root(), None);
        assert!(!is_eval_confined());
        assert!(!is_agent_view_relaunch());
        assert!(!is_artifact_auto_open());
        assert!(!should_propagate_nested_memory());
    }

    #[test]
    fn truthy_flips_gates() {
        let _lock = test_lock();
        clear_all();
        std::env::set_var("CLAUDE_CODE_SPAWNED_BY_SERVER", "1");
        std::env::set_var("CLAUDE_CODE_EVAL_CONFINED", "true");
        std::env::set_var("CLAUDE_CODE_AGENT_VIEW_RELAUNCH", "yes");
        std::env::set_var("CLAUDE_CODE_ARTIFACT_AUTO_OPEN", "on");
        std::env::set_var("CLAUDE_CODE_COORDINATOR_PROPAGATE_NESTED_MEMORY", "1");
        assert!(is_spawned_by_server());
        assert!(is_eval_confined());
        assert!(is_agent_view_relaunch());
        assert!(is_artifact_auto_open());
        assert!(should_propagate_nested_memory());
        clear_all();
    }

    #[test]
    fn stage_file_root_uses_official_name() {
        let _lock = test_lock();
        clear_all();
        assert_eq!(get_stage_file_root(), None);
        std::env::set_var("CLAUDE_STAGE_FILE_ROOT", "/tmp/stage");
        assert_eq!(get_stage_file_root(), Some("/tmp/stage".to_string()));
        std::env::set_var("CLAUDE_STAGE_FILE_ROOT", "   ");
        assert_eq!(get_stage_file_root(), None);
        clear_all();
    }
}
