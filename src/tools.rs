//! tools.rs — 工具注册表，重写自 TS `src/tools.ts`。
//!
//! 语义对齐说明：
//! - TS 的「startup-lazy require」加载器（避免 commander 解析 argv 前执行
//!   40+ 个工具模块）在 Rust 侧无对应成本，注册表以普通函数表达；各工具
//!   实现来自尚未转换的 `builtin-tools` 包，以 TODO + 注册清单表达（见
//!   `get_all_base_tools` 注释）；
//! - TS 的 feature flag / env 门控（`feature('X')`、`process.env`）在 Rust
//!   侧统一为 TODO 桩函数，待 feature flag 与 builtin-tools 体系落地后替换；
//! - 集合类型 `Tools` 与匹配函数来自 `crate::tool`；
//! - `constants/tools.ts`（ALL_AGENT_DISALLOWED_TOOLS 等）尚未转换为 Rust，
//!   TS 侧的 re-export 暂以 TODO 占位。

use std::collections::HashSet;

use crate::tool::{tool_matches_name, Tool, ToolPermissionContext, Tools};
use crate::utils::env_utils::is_env_truthy;

// ============================================================================
// REPL 常量（占位）
// ============================================================================

/// TODO(builtin-tools): 来自 `builtin-tools/tools/REPLTool/constants.js`，
/// 待包转换后替换为真实导出。
pub const REPL_TOOL_NAME: &str = "REPL";

/// REPL 模式启用时对模型隐藏的原始工具（VM 内仍可用）。
/// TODO(builtin-tools): 同上替换为 `REPL_ONLY_TOOLS` 真实集合。
pub const REPL_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "Bash",
    "NotebookEdit",
    "Agent",
];

/// TODO(builtin-tools): 来自 `builtin-tools/tools/SyntheticOutputTool` 等，
/// 待包转换后替换为真实工具名常量。
const LIST_MCP_RESOURCES_TOOL_NAME: &str = "ListMcpResourcesTool";
const READ_MCP_RESOURCE_TOOL_NAME: &str = "ReadMcpResourceTool";
const SYNTHETIC_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

// ============================================================================
// 门控桩函数（占位）
// ============================================================================

/// TODO(builtin-tools): 来自 `builtin-tools/tools/REPLTool/constants.js`
/// 的 `isReplModeEnabled`。默认关闭。
fn is_repl_mode_enabled() -> bool {
    false
}

/// TODO(utils): `utils/tasks.ts` 的 `isTodoV2Enabled` 尚未转换。默认关闭。
#[allow(dead_code)]
fn is_todo_v2_enabled() -> bool {
    false
}

/// TODO(utils): `utils/searchExtraTools.ts` 的
/// `isSearchExtraToolsEnabledOptimistic` 尚未转换。默认关闭。
#[allow(dead_code)]
fn is_search_extra_tools_enabled_optimistic() -> bool {
    false
}

/// TODO(permissions): TS `getDenyRuleForTool` 位于
/// `utils/permissions/permissions.ts`，尚未转换为 Rust。待迁移后删除本桩，
/// 改为从 `crate::utils::permissions` 导入。
///
/// 语义：返回匹配该工具的 blanket deny 规则（含 `mcp__server` 前缀规则）；
/// `None` 表示放行。
fn get_deny_rule_for_tool(
    _permission_context: &ToolPermissionContext,
    _tool: &dyn Tool,
) -> Option<()> {
    None
}

// ============================================================================
// Preset
// ============================================================================

/// 可与 `--tools` 搭配的预设（对齐 TS `TOOL_PRESETS`）。
pub const TOOL_PRESETS: &[&str] = &["default"];

pub type ToolPreset = &'static str;

/// 解析预设名；非法时返回 None（对齐 TS `parseToolPreset`）。
pub fn parse_tool_preset(preset: &str) -> Option<ToolPreset> {
    let preset_string = preset.to_ascii_lowercase();
    if TOOL_PRESETS.contains(&preset_string.as_str()) {
        // 目前仅 "default" 一个预设。
        Some(TOOL_PRESETS[0])
    } else {
        None
    }
}

// ============================================================================
// 工具注册表
// ============================================================================

/// 默认预设下的工具名清单（过滤掉 `isEnabled()` 为 false 的工具）。
pub fn get_tools_for_default_preset() -> Vec<String> {
    get_all_base_tools()
        .iter()
        .filter(|tool| tool.is_enabled())
        .map(|tool| tool.name().to_string())
        .collect()
}

/// 当前环境下可能出现的全部工具（对齐 TS `getAllBaseTools`）。
///
/// TODO(builtin-tools): 60 个工具的实现尚未转换为 Rust，本函数暂返回空集。
/// 迁移时按下列注册清单（对齐 TS `src/tools.ts` 的顺序与门控）逐项接入：
///
/// 恒注册：AgentTool、TaskOutputTool、BashTool、GlobTool、GrepTool、
/// ExitPlanModeV2Tool、FileReadTool、FileEditTool、FileWriteTool、
/// NotebookEditTool、ArtifactTool、WebFetchTool、TodoWriteTool、
/// WebSearchTool、TaskStopTool、AskUserQuestionTool、SkillTool、
/// EnterPlanModeTool、LocalMemoryRecallTool、CdTool、VaultHttpFetchTool、
/// MemoryWriteTool、MemoryReadTool、MemoryListTool、SendMessageTool、
/// TeamCreateTool、TeamDeleteTool、BriefTool、CronCreateTool、
/// CronDeleteTool、CronListTool、ListMcpResourcesTool、
/// ReadMcpResourceTool、ExecuteTool。
///
/// 条件注册：
/// - `USER_TYPE === 'ant'`：ConfigTool、TungstenTool、SuggestBackgroundPRTool
/// - `feature('GOAL')`：GoalTool
/// - `feature('WEB_BROWSER_TOOL')`：WebBrowserTool
/// - `isTodoV2Enabled()`：TaskCreateTool、TaskGetTool、TaskUpdateTool、TaskListTool
/// - `feature('CONTEXT_COLLAPSE')`：CtxInspectTool
/// - `feature('TERMINAL_PANEL')`：TerminalCaptureTool
/// - `ENABLE_LSP_TOOL` truthy：LSPTool
/// - `isWorktreeModeEnabled()`：EnterWorktreeTool、ExitWorktreeTool
/// - `feature('UDS_INBOX')`：ListPeersTool
/// - `CLAUDE_CODE_VERIFY_PLAN === 'true'`：VerifyPlanExecutionTool
/// - `feature('WORKFLOW_SCRIPTS')`：createWorkflowToolCore()
/// - `feature('PROACTIVE') || feature('KAIROS')`：SleepTool
/// - `feature('AGENT_TRIGGERS_REMOTE')`：RemoteTriggerTool
/// - `feature('MONITOR_TOOL')`：MonitorTool
/// - `feature('KAIROS')`：SendUserFileTool
/// - `feature('KAIROS') || feature('KAIROS_PUSH_NOTIFICATION')`：PushNotificationTool
/// - `feature('KAIROS_GITHUB_WEBHOOKS')`：SubscribePRTool
/// - `feature('REVIEW_ARTIFACT')`：ReviewArtifactTool
/// - `isPowerShellToolEnabled()`：PowerShellTool
/// - `feature('HISTORY_SNIP')`：SnipTool
/// - `feature('EXPERIMENTAL_SKILL_SEARCH')`：DiscoverSkillsTool
/// - `NODE_ENV === 'test'`：TestingPermissionTool
/// - `isSearchExtraToolsEnabledOptimistic()`：SearchExtraToolsTool
///
/// 注意：OverflowTestTool 只导出工具名常量、无实现，从不注册。
pub fn get_all_base_tools() -> Tools {
    Vec::new()
}

// ============================================================================
// 过滤与组装
// ============================================================================

/// 按 blanket deny 规则过滤工具（对齐 TS `filterToolsByDenyRules`）。
///
/// 使用与运行时权限检查相同的匹配器（step 1a）：无 ruleContent 的
/// `mcp__server` 前缀规则会在模型看到工具前整组剔除该服务器的工具。
pub fn filter_tools_by_deny_rules(
    tools: &Tools,
    permission_context: &ToolPermissionContext,
) -> Tools {
    tools
        .iter()
        .filter(|tool| get_deny_rule_for_tool(permission_context, tool.as_ref()).is_none())
        .cloned()
        .collect()
}

/// 获取内置工具列表（对齐 TS `getTools`）。
///
/// 流程：simple 模式短路 → 全量注册表剔除特殊工具 → deny 规则过滤 →
/// REPL 模式隐藏原始工具 → `isEnabled()` 过滤。
pub fn get_tools(permission_context: &ToolPermissionContext) -> Tools {
    // Simple mode：仅 Bash / Read / Edit（+ REPL 与 coordinator 变体）。
    if is_env_truthy(std::env::var("CLAUDE_CODE_SIMPLE").ok().as_deref()) {
        // TODO(builtin-tools): simple 集 = [BashTool, FileReadTool, FileEditTool]；
        // REPL 模式启用且 REPLTool 可用时返回 REPL_ONLY 工具集；
        // coordinator 模式叠加 AgentTool / TaskStopTool / SendMessageTool。
        return filter_tools_by_deny_rules(&Vec::new(), permission_context);
    }

    // 从基础集合中剔除特殊工具（由调用方按需单独注入）。
    // TODO(builtin-tools): 工具名来自 builtin-tools 包常量，待替换。
    let special_tools: HashSet<&str> = [
        LIST_MCP_RESOURCES_TOOL_NAME,
        READ_MCP_RESOURCE_TOOL_NAME,
        SYNTHETIC_OUTPUT_TOOL_NAME,
    ]
    .into_iter()
    .collect();

    let tools: Tools = get_all_base_tools()
        .into_iter()
        .filter(|tool| !special_tools.contains(tool.name()))
        .collect();
    let mut allowed_tools = filter_tools_by_deny_rules(&tools, permission_context);

    // REPL 模式启用时隐藏原始工具（VM 内仍可用）。
    if is_repl_mode_enabled() {
        let repl_enabled = allowed_tools
            .iter()
            .any(|tool| tool_matches_name(tool.as_ref(), REPL_TOOL_NAME));
        if repl_enabled {
            allowed_tools.retain(|tool| !REPL_ONLY_TOOLS.contains(&tool.name()));
        }
    }

    allowed_tools.retain(|tool| tool.is_enabled());
    allowed_tools
}

/// 组装完整工具池：内置工具 + MCP 工具（对齐 TS `assembleToolPool`）。
///
/// 1. 内置工具经 `get_tools()`（含模式过滤）；
/// 2. MCP 工具经 deny 规则过滤；
/// 3. 按名称去重（内置工具优先 —— prompt-cache 稳定性要求内置工具保持
///    连续前缀，故各自排序后拼接，而非整体排序）。
pub fn assemble_tool_pool(permission_context: &ToolPermissionContext, mcp_tools: &Tools) -> Tools {
    let mut built_in_tools = get_tools(permission_context);
    built_in_tools.sort_by(|a, b| a.name().cmp(b.name()));

    let mut allowed_mcp_tools = filter_tools_by_deny_rules(mcp_tools, permission_context);
    allowed_mcp_tools.sort_by(|a, b| a.name().cmp(b.name()));

    // uniqBy('name')：内置在前，插入序即优先级。
    let mut seen: HashSet<String> = HashSet::new();
    built_in_tools
        .into_iter()
        .chain(allowed_mcp_tools)
        .filter(|tool| seen.insert(tool.name().to_string()))
        .collect()
}

/// 获取内置 + MCP 的合并列表（对齐 TS `getMergedTools`）。
///
/// 适用于工具搜索阈值计算、含 MCP 工具的 token 计数等场景；
/// 仅需内置工具时用 `get_tools()`。
pub fn get_merged_tools(permission_context: &ToolPermissionContext, mcp_tools: &Tools) -> Tools {
    let mut tools = get_tools(permission_context);
    tools.extend(mcp_tools.iter().cloned());
    tools
}
