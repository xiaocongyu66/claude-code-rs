//! classifier_decision.rs — auto 模式分类器的工具白名单。
//!
//! 重写自 `src/utils/permissions/classifierDecision.ts`。语义对齐说明：
//!
//! - TS 侧的工具名常量来自 `@claude-code-hardened/builtin-tools/...`
//!   的各 `prompt.ts` / `constants.ts`（值为简单字符串字面量，见每个
//!   条目注释）；Rust 仓库尚无 builtin-tools 对应模块，按 TS 常量的
//!   当前值内联，保持集合内容一致；
//! - TS 用 `feature('...')` 与 `process.env.USER_TYPE === 'ant'` 在模块
//!   加载时做 DCE/条件收集（`WORKFLOW_SCRIPTS`、`TERMINAL_PANEL`、
//!   `OVERFLOW_TEST_TOOL`、ant-only `VerifyPlanExecution`）。Rust 侧
//!   无 Bun 编译期 feature 系统，所有被门控的条目一律纳入常量集合
//!   （`OVERFLOW_TEST_TOOL_NAME` 在 TS 侧本身就是空字符串 `''`，
//!   永远不会匹配真实工具名，为清晰起见不单独列出，行为等价）；
//!   集合判定的语义（`isAutoModeAllowlistedTool`）不变；
//! - 类型 `ClassifierResult` 等已在 `src/types/permissions.rs`，
//!   本模块只提供白名单判定。
//!
//! 该清单的用途：auto 模式分类器跳过这些「安全工具」的 API 调用。
//! 注意：刻意**不**包含写/编辑工具——那些由 acceptEdits 快速路径
//! 处理（CWD 内允许，CWD 外送分类器）。

/// auto 模式直接放行、无需分类器检查的工具名集合（TS `SAFE_YOLO_ALLOWLISTED_TOOLS`）。
pub const SAFE_YOLO_ALLOWLISTED_TOOLS: &[&str] = &[
    // 只读文件操作（FILE_READ_TOOL_NAME = 'Read'）
    "Read",
    // 搜索 / 只读
    "Grep",
    "Glob",
    "LSP",
    "SearchExtraTools",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool", // TS 注释：无导出常量
    // 任务管理（仅元数据）
    "TodoWrite",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "TaskStop",
    "TaskOutput",
    // Plan 模式 / UI
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    // Swarm 协调（仅内部 mailbox/team 状态——teammate 有各自的权限检查，
    // 不构成实际的安全绕过）
    "TeamCreate",
    "TeamDelete",
    "SendMessage",
    // 工作流编排——子代理各自走 canUseTool（WORKFLOW_TOOL_NAME = 'Workflow'，
    // TS 侧 feature('WORKFLOW_SCRIPTS') 收集）
    "Workflow",
    // 杂项安全工具
    "Sleep",
    // ant-only 工具（TS 侧 gates mirror tools.ts）：
    "TerminalCapture",     // feature('TERMINAL_PANEL')
    "VerifyPlanExecution", // process.env.USER_TYPE === 'ant'
    // 内部分类器工具（YOLO_CLASSIFIER_TOOL_NAME = 'classify_result'）
    "classify_result",
];

/// 判断工具是否在 auto 模式允许列表中（TS `isAutoModeAllowlistedTool`）。
pub fn is_auto_mode_allowlisted_tool(tool_name: &str) -> bool {
    SAFE_YOLO_ALLOWLISTED_TOOLS.contains(&tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_tools_match() {
        assert!(is_auto_mode_allowlisted_tool("Read"));
        assert!(is_auto_mode_allowlisted_tool("Grep"));
        assert!(is_auto_mode_allowlisted_tool("Glob"));
        assert!(is_auto_mode_allowlisted_tool("classify_result"));
    }

    #[test]
    fn non_allowlisted_tools_do_not_match() {
        assert!(!is_auto_mode_allowlisted_tool("Bash"));
        assert!(!is_auto_mode_allowlisted_tool("Edit"));
        assert!(!is_auto_mode_allowlisted_tool(""));
        assert!(!is_auto_mode_allowlisted_tool("read"));
    }
}
