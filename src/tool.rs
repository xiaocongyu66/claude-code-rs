//! tool.rs — 工具契约层，重写自 TS `src/Tool.ts`。
//!
//! 语义对齐说明：
//! - TS 的 zod 泛型（`Input` / `Output` / `P`）在 Rust 侧统一为
//!   `serde_json::Value`；`inputSchema`（zod）与可选的 `inputJSONSchema`
//!   合并为 `input_schema()`，返回 JSON Schema（zod→JSON Schema 的转换由
//!   工具实现侧负责）；
//! - TS 接口中的「可选方法」映射为 trait 默认实现，默认值与 TS `buildTool`
//!   的 `TOOL_DEFAULTS` 对齐（fail-closed：`isReadOnly` /
//!   `isConcurrencySafe` 默认 false，`checkPermissions` 默认 allow
//!   passthrough，`userFacingName` 默认 `name`，`toAutoClassifierInput`
//!   默认空串跳过分类器）。TS 的 `buildTool()` 对象展开模式在 Rust 侧由
//!   trait 默认方法取代，不再需要 `ToolDef` 中间类型；
//! - 返回 `Promise<X>` 的方法映射为 `BoxFuture<'_, X>`
//!   （`Pin<Box<dyn Future<Output = X> + Send>>`，object-safe，与项目既有
//!   约定一致，见 `constants/system_prompt_sections.rs`）；
//! - TS `call()` 以异常上抛错误 → Rust `Result<ToolResult, ToolError>`；
//! - React 渲染相关方法（`render*`）在 Rust 端无 React，契约为
//!   `RenderNode = String` 占位；
//! - 尚未转换的依赖（messages / state / services::mcp / hooks 等）以
//!   `serde_json::Value` 占位别名表达（见下方「占位类型」段）。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constants::query_source::QuerySource;
use crate::types::ids::AgentId;
use crate::types::permissions::{PermissionAllowDecision, PermissionResult};
use crate::types::tools::ToolProgressData;
use crate::utils::abort_controller::CancellationToken;
use crate::utils::permissions::denial_tracking::DenialTrackingState;

// ============================================================================
// 异步返回类型
// ============================================================================

/// object-safe 的盒装 Future（对齐项目既有约定）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ============================================================================
// 占位类型（依赖未转换的模块）
// ============================================================================

// TODO(messages): `types/message.ts` 转换后替换为真实消息枚举。
pub type Message = Value;
pub type AssistantMessage = Value;
pub type UserMessage = Value;
pub type SystemMessage = Value;
pub type AttachmentMessage = Value;
pub type ProgressMessage = Value;
// TODO(commands): `commands.ts` 转换后替换为真实 Command 类型。
pub type Command = Value;
// TODO(state): `state/AppState.tsx` 转换后替换为真实 AppState。
pub type AppState = Value;
// TODO(thinking): `utils/thinking.ts` 转换后替换。
pub type ThinkingConfig = Value;
// TODO(mcp): `services/mcp/types.ts` 转换后替换。
pub type McpServerConnection = Value;
pub type ServerResource = Value;
// TODO(builtin-tools): AgentDefinition / AgentDefinitionsResult 来自
// `builtin-tools/tools/AgentTool/loadAgentsDir.ts`。
pub type AgentDefinition = Value;
pub type AgentDefinitionsResult = Value;
// TODO(context): `context/notifications.ts` 转换后替换。
pub type Notification = Value;
// TODO(mcp): ElicitRequestURLParams / ElicitResult 来自 MCP SDK 类型。
pub type ElicitRequestUrlParams = Value;
pub type ElicitResult = Value;
// TODO(hooks): `types/hooks.ts` 转换后替换。
pub type PromptRequest = Value;
pub type PromptResponse = Value;
// TODO(utils): `utils/fileStateCache.ts` 转换后替换。
pub type FileStateCache = Value;
// TODO(utils): `utils/fileHistory.ts` 转换后替换。
pub type FileHistoryState = Value;
// TODO(utils): `utils/commitAttribution.ts` 转换后替换。
pub type AttributionState = Value;
// TODO(utils): `utils/toolResultStorage.ts` 转换后替换。
pub type ContentReplacementState = Value;
// TODO(utils): `utils/systemPromptType.ts` 转换后替换（TS 中为 branded string）。
pub type SystemPrompt = String;
/// UI 渲染占位：Rust 端无 React，渲染方法暂以纯文本契约表达。
pub type RenderNode = String;
/// 主题名占位（`utils/theme.ts` 未转换）。
pub type ThemeName = String;

// ============================================================================
// 基础类型
// ============================================================================

/// MCP 工具可直接以 JSON Schema 声明输入（对齐 TS `ToolInputJSONSchema`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInputJsonSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<HashMap<String, Value>>,
    /// 其余 JSON Schema 键（required / additionalProperties / $schema …）。
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// 查询链追踪（对齐 TS `QueryChainTracking`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryChainTracking {
    pub chain_id: String,
    pub depth: u32,
}

/// 工具输入校验结果。TS：`{ result: true } | { result: false, message, errorCode }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ValidationResult {
    Pass(PassValidationResult),
    Fail(FailValidationResult),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassValidationResult {
    pub result: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FailValidationResult {
    pub result: bool,
    pub message: String,
    pub error_code: u32,
}

impl ValidationResult {
    pub fn pass() -> Self {
        Self::Pass(PassValidationResult { result: true })
    }

    pub fn fail(message: impl Into<String>, error_code: u32) -> Self {
        Self::Fail(FailValidationResult {
            result: false,
            message: message.into(),
            error_code,
        })
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass(_))
    }
}

/// 压缩进度事件（对齐 TS `CompactProgressEvent`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactHookType {
    #[serde(rename = "pre_compact")]
    PreCompact,
    #[serde(rename = "post_compact")]
    PostCompact,
    #[serde(rename = "session_start")]
    SessionStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactProgressEvent {
    HooksStart { hook_type: CompactHookType },
    CompactStart,
    CompactEnd,
}

// ============================================================================
// 上下文回调类型
// ============================================================================

pub type GetAppStateFn = Arc<dyn Fn() -> AppState + Send + Sync>;
pub type AppStateUpdater = Arc<dyn Fn(AppState) -> AppState + Send + Sync>;
pub type SetAppStateFn = Arc<dyn Fn(AppStateUpdater) + Send + Sync>;
/// 通用状态更新器（对齐 TS `(updater: (prev: S) => S) => void`）。
pub type StateUpdater = Arc<dyn Fn(Value) -> Value + Send + Sync>;
pub type UpdateStateFn = Arc<dyn Fn(StateUpdater) + Send + Sync>;

pub type StringSetUpdater = Arc<dyn Fn(HashSet<String>) -> HashSet<String> + Send + Sync>;
pub type SetInProgressToolUseIdsFn = Arc<dyn Fn(StringSetUpdater) + Send + Sync>;

pub type NumberUpdater = Arc<dyn Fn(usize) -> usize + Send + Sync>;
pub type SetNumberFn = Arc<dyn Fn(NumberUpdater) + Send + Sync>;
pub type SetBoolFn = Arc<dyn Fn(bool) + Send + Sync>;

/// URL 问询处理器（MCP -32042 错误），在 REPL 模式下不可用。
pub type HandleElicitationFn = Arc<
    dyn for<'a> Fn(String, ElicitRequestUrlParams, &'a CancellationToken) -> BoxFuture<'a, ElicitResult>
        + Send
        + Sync,
>;

/// 工具 JSX 注入参数（对齐 TS `SetToolJSXFn` 的 args）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetToolJsxArgs {
    pub jsx: Option<RenderNode>,
    pub should_hide_prompt_input: bool,
    pub should_continue_animation: Option<bool>,
    pub show_spinner: Option<bool>,
    pub is_local_jsx_command: Option<bool>,
    pub is_immediate: Option<bool>,
    /// 置位时清除一次本地 JSX 命令（如 onDone 回调）。
    pub clear_local_jsx: Option<bool>,
}

pub type SetToolJsxFn = Arc<dyn Fn(Option<SetToolJsxArgs>) + Send + Sync>;
pub type AddNotificationFn = Arc<dyn Fn(Notification) + Send + Sync>;
pub type AppendSystemMessageFn = Arc<dyn Fn(SystemMessage) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsNotification {
    pub message: String,
    pub notification_type: String,
}

pub type SendOsNotificationFn = Arc<dyn Fn(OsNotification) + Send + Sync>;

/// 工具读取限额（对齐 TS `ToolUseContext.fileReadingLimits`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReadingLimits {
    pub max_tokens: Option<usize>,
    pub max_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobLimits {
    pub max_results: Option<usize>,
}

/// 工具权限决策的审计记录（对齐 TS `toolDecisions` map 值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDecision {
    pub source: String,
    pub decision: ToolDecisionKind,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolDecisionKind {
    Accept,
    Reject,
}

/// 交互式 prompt 回调（仅 REPL 上下文可用）。
pub type PromptCallback =
    Arc<dyn Fn(PromptRequest) -> BoxFuture<'static, PromptResponse> + Send + Sync>;
pub type RequestPromptFn = Arc<dyn Fn(String, Option<String>) -> PromptCallback + Send + Sync>;

/// 工具集刷新回调（如 MCP 服务器在查询中途连上后重新取工具）。
pub type RefreshToolsFn = Arc<dyn Fn() -> Tools + Send + Sync>;

// ============================================================================
// ToolUseContext
// ============================================================================

/// 工具执行选项（对齐 TS `ToolUseContext.options`）。
#[derive(Clone)]
pub struct ToolUseOptions {
    pub commands: Vec<Command>,
    pub debug: bool,
    pub main_loop_model: String,
    pub tools: Tools,
    pub verbose: bool,
    pub thinking_config: ThinkingConfig,
    pub mcp_clients: Vec<McpServerConnection>,
    pub mcp_resources: HashMap<String, Vec<ServerResource>>,
    pub is_non_interactive_session: bool,
    pub agent_definitions: AgentDefinitionsResult,
    pub max_budget_usd: Option<f64>,
    /// 替换默认 system prompt 的自定义提示。
    pub custom_system_prompt: Option<String>,
    /// 追加在主 system prompt 之后的附加提示。
    pub append_system_prompt: Option<String>,
    pub query_source: Option<QuerySource>,
    pub refresh_tools: Option<RefreshToolsFn>,
    /// @internal TEST-ONLY ESCAPE HATCH（对齐 TS 同名字段注释）。
    pub allow_background_forked_slash_commands: Option<bool>,
}

/// 工具调用上下文（对齐 TS `ToolUseContext`）。
///
/// 字段名由 TS camelCase 转为 Rust snake_case；所有 UI/状态回调以
/// `Arc<dyn Fn>` 表达。尚未转换的依赖类型见文件顶部占位别名。
#[derive(Clone)]
pub struct ToolUseContext {
    pub options: ToolUseOptions,
    pub abort_controller: Arc<CancellationToken>,
    pub read_file_state: FileStateCache,
    pub get_app_state: GetAppStateFn,
    pub set_app_state: SetAppStateFn,
    /// 永远可达根 store 的 setAppState（子代理基础设施用；主线程回退到
    /// set_app_state）。
    pub set_app_state_for_tasks: Option<SetAppStateFn>,
    pub handle_elicitation: Option<HandleElicitationFn>,
    pub set_tool_jsx: Option<SetToolJsxFn>,
    pub add_notification: Option<AddNotificationFn>,
    pub append_system_message: Option<AppendSystemMessageFn>,
    pub send_os_notification: Option<SendOsNotificationFn>,
    pub nested_memory_attachment_triggers: Option<HashSet<String>>,
    /// 已作为 nested_memory 注入过的 CLAUDE.md 路径去重集合。
    pub loaded_nested_memory_paths: Option<HashSet<String>>,
    pub dynamic_skill_dir_triggers: Option<HashSet<String>>,
    /// skill_discovery 暴露过的技能名（遥测，feeds was_discovered）。
    pub discovered_skill_names: Option<HashSet<String>>,
    pub user_modified: Option<bool>,
    pub set_in_progress_tool_use_ids: SetInProgressToolUseIdsFn,
    /// 仅交互式（REPL）上下文接线；SDK/QueryEngine 不设。
    pub set_has_interruptible_tool_in_progress: Option<SetBoolFn>,
    pub set_response_length: Option<SetNumberFn>,
    /// Ant-only：OTPS 追踪的新 API metrics 条目。
    pub push_api_metrics_entry: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    pub set_stream_mode: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_compact_progress: Option<Arc<dyn Fn(CompactProgressEvent) + Send + Sync>>,
    pub set_sdk_status: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub open_message_selector: Option<Arc<dyn Fn() + Send + Sync>>,
    pub update_file_history_state: UpdateStateFn,
    pub update_attribution_state: UpdateStateFn,
    pub set_conversation_id: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// 仅子代理设值；hooks 用它区分子代理调用。
    pub agent_id: Option<AgentId>,
    /// 子代理类型名。
    pub agent_type: Option<String>,
    /// 置位时即使 hooks 自动批准也必须调用 canUseTool（投机执行用）。
    pub require_can_use_tool: Option<bool>,
    pub messages: Vec<Message>,
    pub file_reading_limits: Option<FileReadingLimits>,
    pub glob_limits: Option<GlobLimits>,
    pub tool_decisions: Option<HashMap<String, ToolDecision>>,
    pub query_tracking: Option<QueryChainTracking>,
    pub request_prompt: Option<RequestPromptFn>,
    pub tool_use_id: Option<String>,
    pub critical_system_reminder_experimental: Option<String>,
    /// Langfuse 观测 span（trace / rootTrace / batchSpan），Value 占位。
    pub langfuse_trace: Option<Value>,
    pub langfuse_root_trace: Option<Value>,
    pub langfuse_batch_span: Option<Value>,
    /// 置位时子代理的 toolUseResult 也保留（进程内 teammates）。
    pub preserve_tool_use_results: Option<bool>,
    /// 异步子代理的本地拒绝计数（setAppState 为 no-op 时用）。
    pub local_denial_tracking: Option<DenialTrackingState>,
    /// 工具结果预算的按线程内容替换状态。
    pub content_replacement_state: Option<ContentReplacementState>,
    /// 回合开始时冻结的父代理渲染后 system prompt 字节（fork 子代理共享缓存）。
    pub rendered_system_prompt: Option<SystemPrompt>,
}

/// 回调字段无法派生 `Debug`，这里给出不含字段内容的占位实现（仅用于
/// 携带 `ToolUseContext` 的结构体派生 `Debug`，如 compact 的
/// `CacheSafeParams`）。
impl std::fmt::Debug for ToolUseContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolUseContext").finish_non_exhaustive()
    }
}

// ============================================================================
// 进度类型
// ============================================================================

/// `ToolProgressData | HookProgress` —— 二者占位均为 Value。
pub type Progress = ToolProgressData;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProgress {
    pub tool_use_id: String,
    pub data: ToolProgressData,
}

/// 工具调用进度回调（对齐 TS `ToolCallProgress`）。
pub type ToolCallProgress = Arc<dyn Fn(ToolProgress) + Send + Sync>;

/// 过滤掉无 data 或 hook_progress 的进度消息（对齐 TS
/// `filterToolProgressMessages`）。
pub fn filter_tool_progress_messages(progress_messages_for_message: &[ProgressMessage]) -> Vec<ProgressMessage> {
    progress_messages_for_message
        .iter()
        .filter(|msg| match msg.get("data") {
            None | Some(Value::Null) => false,
            Some(data) => data.get("type").and_then(Value::as_str) != Some("hook_progress"),
        })
        .cloned()
        .collect()
}

/// 工具返回结果（对齐 TS `ToolResult<T>`，`Output` 泛型默认 Value）。
#[derive(Clone)]
pub struct ToolResult<T = Value> {
    pub data: T,
    /// 随工具结果一起追加到会话的新消息。
    pub new_messages: Option<Vec<Message>>,
    /// 仅对非并发安全工具生效的上下文修改器。
    pub context_modifier: Option<ContextModifierFn>,
    /// MCP 协议元数据（structuredContent / _meta），透传给 SDK 消费者。
    pub mcp_meta: Option<ToolResultMcpMeta>,
}

impl<T> ToolResult<T> {
    pub fn new(data: T) -> Self {
        Self {
            data,
            new_messages: None,
            context_modifier: None,
            mcp_meta: None,
        }
    }
}

/// TS：`(context: ToolUseContext) => ToolUseContext`。Rust 侧以引用传入、
/// 返回全新上下文（上下文字段均为 Arc，克隆廉价）。
pub type ContextModifierFn = Arc<dyn Fn(&ToolUseContext) -> ToolUseContext + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMcpMeta {
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<HashMap<String, Value>>,
}

// ============================================================================
// 权限匹配 / 错误
// ============================================================================

/// hook `if` 条件匹配器（权限规则模式匹配，如 "git *"）。
pub type PermissionMatcher = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// 工具调用错误（TS 侧以抛异常表达）。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{message}")]
    Generic { message: String },
    #[error("validation failed (code {error_code}): {message}")]
    Validation { message: String, error_code: u32 },
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("tool call aborted")]
    Aborted,
}

// ============================================================================
// 行为标记枚举
// ============================================================================

/// 用户在工具运行期间提交新消息时的行为（默认 `block`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptBehavior {
    Cancel,
    Block,
}

/// 搜索/读取折叠判定（对齐 TS `isSearchOrReadCommand` 返回值）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOrReadCommand {
    pub is_search: bool,
    pub is_read: bool,
    pub is_list: Option<bool>,
}

/// MCP 工具的服务器/工具名（未规范化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInfo {
    pub server_name: String,
    pub tool_name: String,
}

// ============================================================================
// 方法选项结构
// ============================================================================

#[derive(Clone)]
pub struct DescriptionOptions {
    pub is_non_interactive_session: bool,
    pub tool_permission_context: ToolPermissionContext,
    pub tools: Tools,
}

#[derive(Clone)]
pub struct PromptOptions {
    pub get_tool_permission_context: GetToolPermissionContextFn,
    pub tools: Tools,
    pub agents: Vec<AgentDefinition>,
    pub allowed_agent_types: Option<Vec<String>>,
}

/// `getToolPermissionContext` 回调（异步返回快照）。
pub type GetToolPermissionContextFn =
    Arc<dyn Fn() -> BoxFuture<'static, ToolPermissionContext> + Send + Sync>;

/// canUseTool 调用参数（对齐 TS `CanUseToolFn` 的形参列表）。
pub struct CanUseToolRequest<'a> {
    pub tool: &'a dyn Tool,
    pub input: &'a Value,
    pub context: &'a ToolUseContext,
    pub assistant_message: &'a Value,
    pub tool_use_id: &'a str,
    pub force_decision: Option<&'a PermissionResult>,
}

pub type CanUseToolFn = Arc<
    dyn for<'a> Fn(CanUseToolRequest<'a>) -> BoxFuture<'a, PermissionResult> + Send + Sync,
>;

// ============================================================================
// UI 渲染选项（RenderNode 占位）
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderStyle {
    #[serde(rename = "condensed")]
    Condensed,
}

pub struct RenderToolUseMessageOptions<'a> {
    pub theme: ThemeName,
    pub verbose: bool,
    pub commands: Option<&'a Vec<Command>>,
}

pub struct RenderToolResultMessageOptions<'a> {
    pub style: Option<RenderStyle>,
    pub theme: ThemeName,
    pub tools: &'a Tools,
    pub verbose: bool,
    pub is_transcript_mode: Option<bool>,
    pub is_brief_only: Option<bool>,
    /// 原始 tool_use 输入（可用于紧凑摘要）。
    pub input: Option<&'a Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u32,
    pub rows: u32,
}

pub struct RenderToolUseProgressMessageOptions<'a> {
    pub tools: &'a Tools,
    pub verbose: bool,
    pub terminal_size: Option<TerminalSize>,
    pub in_progress_tool_call_count: Option<usize>,
    pub is_transcript_mode: Option<bool>,
}

pub struct RenderToolUseRejectedMessageOptions<'a> {
    pub columns: u32,
    pub messages: &'a [Message],
    pub style: Option<RenderStyle>,
    pub theme: ThemeName,
    pub tools: &'a Tools,
    pub verbose: bool,
    pub progress_messages_for_message: &'a [ProgressMessage],
    pub is_transcript_mode: Option<bool>,
}

pub struct RenderToolUseErrorMessageOptions<'a> {
    pub progress_messages_for_message: &'a [ProgressMessage],
    pub tools: &'a Tools,
    pub verbose: bool,
    pub is_transcript_mode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupedToolUseResult {
    pub param: Value,
    pub output: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupedToolUse {
    pub param: Value,
    pub is_resolved: bool,
    pub is_error: bool,
    pub is_in_progress: bool,
    pub progress_messages: Vec<Value>,
    pub result: Option<GroupedToolUseResult>,
}

pub struct RenderGroupedToolUseOptions<'a> {
    pub should_animate: bool,
    pub tools: &'a Tools,
}

// ============================================================================
// Tool trait
// ============================================================================

/// 工具契约（对齐 TS `Tool<Input, Output, P>` 接口）。
///
/// Rust 侧以 `serde_json::Value` 承载输入/输出；TS 的可选方法映射为默认
/// 实现，默认值取自 TS `buildTool` 的 `TOOL_DEFAULTS`。
pub trait Tool: Send + Sync + 'static {
    // ---- 标识 ----

    fn name(&self) -> &str;

    /// 工具重命名后的向后兼容别名。
    fn aliases(&self) -> &[String] {
        &[]
    }

    /// SearchExtraTools 关键词匹配用的一行能力描述（3–10 词，无句号）。
    fn search_hint(&self) -> Option<&str> {
        None
    }

    fn mcp_info(&self) -> Option<&McpInfo> {
        None
    }

    fn is_mcp(&self) -> bool {
        false
    }

    fn is_lsp(&self) -> bool {
        false
    }

    /// 置位时以 defer_loading: true 发送，需先经 SearchExtraTools 发现。
    fn should_defer(&self) -> bool {
        false
    }

    /// 置位时永不延迟（首轮即出现在 prompt 中）。
    fn always_load(&self) -> bool {
        false
    }

    /// 启用后 API 更严格遵循工具指令与参数 schema（tengu_tool_pear 门控）。
    fn strict(&self) -> bool {
        false
    }

    /// 工具结果落盘前的最大字符数；`usize::MAX` 表示永不落盘。
    fn max_result_size_chars(&self) -> usize;

    // ---- Schema ----

    /// 输入 schema（TS 的 zod `inputSchema` / MCP 的 `inputJSONSchema` 合并）。
    fn input_schema(&self) -> &ToolInputJsonSchema;

    // ---- 执行 ----

    /// 执行工具调用。TS 中的异常上抛映射为 `Err(ToolError)`。
    fn call(
        &self,
        args: &Value,
        context: &ToolUseContext,
        can_use_tool: &CanUseToolFn,
        parent_message: &Value,
        on_progress: Option<&ToolCallProgress>,
    ) -> BoxFuture<'_, Result<ToolResult<Value>, ToolError>>;

    /// 输入合法性校验（异步；告模型以失败原因，不直接显示 UI）。
    fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolUseContext,
    ) -> BoxFuture<'_, ValidationResult> {
        Box::pin(async { ValidationResult::pass() })
    }

    /// 权限检查（validate_input 通过后调用）。默认交由通用权限系统放行。
    fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolUseContext,
    ) -> BoxFuture<'_, PermissionResult> {
        let input = input.clone();
        Box::pin(async move {
            PermissionResult::Allow(PermissionAllowDecision {
                updated_input: Some(input),
                user_modified: None,
                decision_reason: None,
                tool_use_id: None,
                accept_feedback: None,
                content_blocks: None,
            })
        })
    }

    /// 权限规则模式匹配器（如 "Bash(git *)"）；未实现时仅按工具名匹配。
    fn prepare_permission_matcher(
        &self,
        _input: &Value,
    ) -> Option<BoxFuture<'static, PermissionMatcher>> {
        None
    }

    // ---- 元信息 / 门控 ----

    /// 工具描述（随输入与权限上下文动态生成）。
    fn description(
        &self,
        input: &Value,
        options: &DescriptionOptions,
    ) -> BoxFuture<'_, String>;

    /// 权限提示文案。
    fn prompt(&self, options: &PromptOptions) -> BoxFuture<'_, String>;

    fn user_facing_name(&self, _input: Option<&Value>) -> String {
        self.name().to_string()
    }

    /// 用户可见名背景色（主题键），默认无。
    fn user_facing_name_background_color(&self, _input: Option<&Value>) -> Option<&'static str> {
        None
    }

    fn is_enabled(&self) -> bool {
        true
    }

    /// 是否只读（默认 false：假定为写操作）。
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// 是否不可逆操作（删除/覆写/发送）；默认 false。
    fn is_destructive(&self, _input: &Value) -> bool {
        false
    }

    /// 是否并发安全（默认 false：假定不安全）。
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    /// 运行期间用户提交新消息的行为；默认 block。
    fn interrupt_behavior(&self) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    fn requires_user_interaction(&self) -> bool {
        false
    }

    fn is_open_world(&self, _input: &Value) -> bool {
        false
    }

    /// 是否为可折叠显示的搜索/读取/列目录操作。
    fn is_search_or_read_command(&self, _input: &Value) -> SearchOrReadCommand {
        SearchOrReadCommand {
            is_search: false,
            is_read: false,
            is_list: None,
        }
    }

    /// 透明包装器（如 REPL）：自身不渲染，委托给内部工具调用的渲染。
    fn is_transparent_wrapper(&self) -> bool {
        false
    }

    /// 两次输入是否等价（默认深比较）。
    fn inputs_equivalent(&self, a: &Value, b: &Value) -> bool {
        a == b
    }

    /// 操作文件路径工具的路径提取。
    fn get_path(&self, _input: &Value) -> Option<String> {
        None
    }

    /// 紧凑视图用的短摘要；null 不显示。
    fn get_tool_use_summary(&self, _input: Option<&Value>) -> Option<String> {
        None
    }

    /// spinner 用的进行时活动描述；null 回退到工具名。
    fn get_activity_description(&self, _input: Option<&Value>) -> Option<String> {
        None
    }

    /// auto-mode 安全分类器的紧凑输入；返回空串跳过该工具。
    fn to_auto_classifier_input(&self, _input: &Value) -> Value {
        Value::String(String::new())
    }

    /// 观察者见到 tool_use 输入前的回填（原地修改，须幂等）。
    fn backfill_observable_input(&self, _input: &mut Value) {}

    // ---- 结果序列化 ----

    /// 将输出序列化为 API 的 tool_result 块。
    fn map_tool_result_to_tool_result_block_param(
        &self,
        content: &Value,
        tool_use_id: &str,
    ) -> Value;

    /// 非紧凑渲染是否被截断（控制点击展开）；默认 false。
    fn is_result_truncated(&self, _output: &Value) -> bool {
        false
    }

    /// transcript 模式下 `render_tool_result_message` 的可索引纯文本。
    fn extract_search_text(&self, _output: &Value) -> Option<String> {
        None
    }

    // ---- UI 渲染（RenderNode 占位） ----

    /// 渲染 tool_use 消息（输入可能尚未流式完成，故为部分输入）。
    fn render_tool_use_message(
        &self,
        input: &Value,
        options: &RenderToolUseMessageOptions,
    ) -> RenderNode;

    /// 渲染 tool_result 消息；省略时等价于返回 null（结果另有出口）。
    fn render_tool_result_message(
        &self,
        _content: &Value,
        _progress_messages_for_message: &[ProgressMessage],
        _options: &RenderToolResultMessageOptions,
    ) -> RenderNode {
        String::new()
    }

    /// tool_use 消息后的附加标签（超时/模型/resume ID 等）。
    fn render_tool_use_tag(&self, _input: &Value) -> Option<RenderNode> {
        None
    }

    /// 运行中的进度 UI；省略时不显示。
    fn render_tool_use_progress_message(
        &self,
        _progress_messages_for_message: &[ProgressMessage],
        _options: &RenderToolUseProgressMessageOptions,
    ) -> RenderNode {
        String::new()
    }

    /// 排队中的 UI。
    fn render_tool_use_queued_message(&self) -> RenderNode {
        String::new()
    }

    /// 拒绝 UI；省略时回退到通用 fallback。
    fn render_tool_use_rejected_message(
        &self,
        _input: &Value,
        _options: &RenderToolUseRejectedMessageOptions,
    ) -> RenderNode {
        String::new()
    }

    /// 错误 UI；省略时回退到通用 fallback。
    fn render_tool_use_error_message(
        &self,
        _result_content: &Value,
        _options: &RenderToolUseErrorMessageOptions,
    ) -> RenderNode {
        String::new()
    }

    /// 并行多实例的分组渲染（仅非 verbose 模式）；null 回退到逐个渲染。
    fn render_grouped_tool_use(
        &self,
        _tool_uses: &[GroupedToolUse],
        _options: &RenderGroupedToolUseOptions,
    ) -> Option<RenderNode> {
        None
    }
}

// ============================================================================
// 工具集合与查找
// ============================================================================

/// 工具集合（TS `Tools = readonly Tool[]`）。用 `Vec<Arc<dyn Tool>>`
/// 表达，便于跨异步边界克隆传递。
pub type Tools = Vec<Arc<dyn Tool>>;

/// 工具是否匹配给定名称（主名或别名）。
pub fn tool_matches_name(tool: &dyn Tool, name: &str) -> bool {
    tool.name() == name || tool.aliases().iter().any(|alias| alias == name)
}

/// 按名称（或别名）查找工具。
pub fn find_tool_by_name(tools: &Tools, name: &str) -> Option<Arc<dyn Tool>> {
    tools
        .iter()
        .find(|tool| tool_matches_name(tool.as_ref(), name))
        .cloned()
}

// ============================================================================
// 权限上下文（打破循环依赖的集中导出，TS 同名 re-export）
// ============================================================================

pub use crate::types::permissions::PermissionMode;
pub use crate::types::permissions::ToolPermissionContext;
pub use crate::types::permissions::ToolPermissionRulesBySource;

/// 空权限上下文（对齐 TS `getEmptyToolPermissionContext`）。
pub fn get_empty_tool_permission_context() -> ToolPermissionContext {
    ToolPermissionContext {
        mode: crate::types::permissions::PermissionMode::Default,
        additional_working_directories: HashMap::new(),
        always_allow_rules: ToolPermissionRulesBySource::default(),
        always_deny_rules: ToolPermissionRulesBySource::default(),
        always_ask_rules: ToolPermissionRulesBySource::default(),
        is_bypass_permissions_mode_available: true,
        stripped_dangerous_rules: None,
        should_avoid_permission_prompts: None,
        await_automated_checks_before_dialog: None,
        pre_plan_mode: None,
    }
}
