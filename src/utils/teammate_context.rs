//! teammate_context.rs — 内嵌 teammate 的运行时上下文。
//!
//! 重写自 `src/utils/teammate.ts` 对应模块的 `teammateContext.ts`。
//! 语义对齐：
//!
//! - TS 用 AsyncLocalStorage 让并发执行的内嵌 teammate 各自看到自己的
//!   上下文，不与全局状态冲突。
//! - 与其他 teammate 身份机制的关系（照搬 TS 注释）：
//!   - 环境变量 CLAUDE_CODE_AGENT_ID：经 tmux 拉起的进程级 teammate
//!   - teammate.ts 的 dynamicTeamContext：运行时加入的进程级 teammate
//!   - 本模块：AsyncLocalStorage 的内嵌 teammate
//! - Rust 侧 API 形状为同步闭包，用 thread_local 落地（偏差：ALS 按
//!   异步任务传播隔离，thread_local 按线程隔离；当前调用方为同步闭包，
//!   需要跨 await 隔离时再换 `tokio::task_local!`）。

use std::cell::RefCell;
use std::sync::Arc;

use crate::utils::abort_controller::CancellationToken;

/// 内嵌 teammate 的运行时上下文。
#[derive(Clone)]
pub struct TeammateContext {
    /// 完整 agent ID，如 "researcher@my-team"
    pub agent_id: String,
    /// 展示名，如 "researcher"
    pub agent_name: String,
    /// 所属 team 名
    pub team_name: String,
    /// 分配给该 teammate 的 UI 颜色
    pub color: Option<String>,
    /// 是否必须先进入 plan mode 再实现
    pub plan_mode_required: bool,
    /// 主导者会话 ID（用于 transcript 关联）
    pub parent_session_id: String,
    /// 判别子 —— 内嵌 teammate 恒为 true
    pub is_in_process: bool,
    /// 生命周期管理的 abort token（通常独立于主导者，主导者查询被中断
    /// 时 teammate 继续运行）
    pub abort_controller: Arc<CancellationToken>,
}

thread_local! {
    static TEAMMATE_CONTEXT: RefCell<Option<TeammateContext>> = const { RefCell::new(None) };
}

/// 获取当前内嵌 teammate 上下文；不在内嵌 teammate 上下文中时返回 None。
pub fn get_teammate_context() -> Option<TeammateContext> {
    TEAMMATE_CONTEXT.with(|c| c.borrow().clone())
}

/// 在 teammate 上下文中执行 `fn`（spawn 内嵌 teammate 时建立其执行上下文）。
pub fn run_with_teammate_context<T>(context: TeammateContext, f: impl FnOnce() -> T) -> T {
    let saved = TEAMMATE_CONTEXT.with(|c| c.replace(Some(context)));
    let result = f();
    TEAMMATE_CONTEXT.with(|c| {
        *c.borrow_mut() = saved;
    });
    result
}

/// 当前执行是否在内嵌 teammate 中（比 get_teammate_context 更轻的判定）。
pub fn is_in_process_teammate() -> bool {
    TEAMMATE_CONTEXT.with(|c| c.borrow().is_some())
}

/// 从 spawn 配置创建 TeammateContext。abort_controller 由调用方传入
/// （内嵌 teammate 通常用独立 controller）。
pub fn create_teammate_context(config: TeammateContextConfig) -> TeammateContext {
    TeammateContext {
        agent_id: config.agent_id,
        agent_name: config.agent_name,
        team_name: config.team_name,
        color: config.color,
        plan_mode_required: config.plan_mode_required,
        parent_session_id: config.parent_session_id,
        is_in_process: true,
        abort_controller: config.abort_controller,
    }
}

/// spawn 配置（对齐 TS 的 config 参数形状）。
pub struct TeammateContextConfig {
    pub agent_id: String,
    pub agent_name: String,
    pub team_name: String,
    pub color: Option<String>,
    pub plan_mode_required: bool,
    pub parent_session_id: String,
    pub abort_controller: Arc<CancellationToken>,
}
