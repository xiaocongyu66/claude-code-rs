//! `services/compact/postCompactCleanup.ts` — 压缩后缓存/跟踪态清理。
//!
//! 语义对齐：子 agent（agent:*）与主线程共享模块级状态，只有主线程
//! 压缩才重置主线程级缓存（context-collapse、memory 文件缓存、
//! getUserContext）。技能内容故意不清除（后续压缩附件还要用）。

use std::sync::Mutex;

use crate::services::compact::micro_compact::reset_microcompact_state;

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// REPL 等长生命周期组件注册的 compact-scoped 清理回调。
static COMPACT_CLEANUP_CALLBACKS: Mutex<Vec<Box<dyn Fn() + Send + Sync>>> = Mutex::new(Vec::new());

/// TS `registerCompactCleanup(callback)`。
pub fn register_compact_cleanup(callback: Box<dyn Fn() + Send + Sync>) {
    if let Ok(mut callbacks) = COMPACT_CLEANUP_CALLBACKS.lock() {
        callbacks.push(callback);
    }
}

/// TS `runPostCompactCleanup(querySource?)`。
pub fn run_post_compact_cleanup(query_source: Option<&str>) {
    // 子 agent 与主线程同进程共享模块级状态；只有主线程压缩才重置。
    // 同 TS `isMainThread` 的 startsWith 判定（index.ts:188）。
    let is_main_thread_compact = match query_source {
        None => true,
        Some(source) => {
            source.starts_with("repl_main_thread") || source == "sdk"
        }
    };

    reset_microcompact_state();
    if feature("CONTEXT_COLLAPSE") && is_main_thread_compact {
        // contextCollapse 存根层：fail-loudly 断言会拦下强行开启的
        // FEATURE_CONTEXT_COLLAPSE=1。
        crate::services::contextCollapse::reset_context_collapse();
    }
    if is_main_thread_compact {
        // getUserContext 是 memoized 外层；只清内层 getMemoryFiles 缓存会
        // 让下一轮命中外层缓存而绕过 InstructionsLoaded hook。主线程压缩
        // 统一在这里清两层（utils/claudemd.ts、context.ts 未转换，记为
        // 待接线）。
        // TODO(claudemd): resetGetMemoryFilesCache('compact')
        // TODO(context): getUserContext.cache.clear()
    }
    // constants/systemPromptSections / classifierApprovals /
    // bashPermissions.clearSpeculativeChecks / telemetry beta state /
    // sessionStorage messages cache 均为未转换依赖，记为待接线。
    // 注意：故意不调用 resetSentSkillNames（重注入 ~4K token 的
    // skill_listing 是纯 cache_creation 开销）。

    if let Ok(callbacks) = COMPACT_CLEANUP_CALLBACKS.lock() {
        for callback in callbacks.iter() {
            // 单个回调抛错不能中断其余清理（对齐 TS try/catch）。
            cleanup_catch(callback);
        }
    }
}

fn cleanup_catch(callback: &(dyn Fn() + Send + Sync)) {
    // 对齐 TS try { cb() } catch { logError(error) }：单个回调的 panic
    // 不能中断其余清理。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
}
