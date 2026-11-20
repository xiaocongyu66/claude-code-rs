//! query_engine.rs — 会话编排器（重写自 TS `src/QueryEngine.ts`）。
//!
//! 语义对齐说明：
//! - TS 的 `QueryEngine` 是 class，`submitMessage()` 是 AsyncGenerator<SDKMessage>。
//!   Rust 侧沿用 `query.rs` 的 sink 模型：`submit_message(&mut self, prompt,
//!   options, sink: &mut dyn SdkSink)` 把每次 yield 映射为 `sink.emit(SdkMessage)`，
//!   生成器 return 映射为 `Ok(())`，throw 映射为 `Err(QueryEngineError)`。
//! - turn 循环完全委托 `crate::query::query()`：本模块只负责会话级状态
//!   （mutableMessages / usage / 权限拒绝 / 中断令牌）、SDK 消息映射、
//!   结果消息组装与提前终止条件（max_turns / 预算 / 结构化输出重试上限）。
//! - 消息模型使用 `services::api::claude` 的 wire 类型（`types/message.ts`
//!   未转换）。
//!
//! 占位/偏差总览见文件尾「占位清单」。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::bootstrap::state::{
    get_model_usage, get_session_id, get_total_api_duration, get_total_cost_usd,
    is_session_persistence_disabled, set_cwd_state, ModelUsage,
};
use crate::constants::xml::{LOCAL_COMMAND_STDERR_TAG, LOCAL_COMMAND_STDOUT_TAG};
use crate::query::{query, AttachmentKind, QueryError, QueryEvent, QueryParams, QuerySink, SystemNotice, Terminal};
use crate::services::api::claude::{
    self, accumulate_usage, empty_usage, AssistantMessage, Message, MessageContent,
    NonNullableUsage, SystemPrompt, UserMessage,
};
use crate::tool::{
    AgentDefinition, CanUseToolFn, CanUseToolRequest, Command, FileStateCache, GetAppStateFn,
    HandleElicitationFn, McpServerConnection, SetAppStateFn, SetInProgressToolUseIdsFn,
    StateUpdater, StringSetUpdater, ThinkingConfig, ToolUseContext, ToolUseOptions, Tools,
    UpdateStateFn,
};
use crate::types::permissions::PermissionResult;
use crate::utils::abort_controller::CancellationToken;
use crate::utils::log::get_in_memory_errors;
use crate::utils::model::model::get_main_loop_model;

// ---------------------------------------------------------------------------
// 本地常量（TS 对应模块未转换）
// ---------------------------------------------------------------------------

/// TS `SYNTHETIC_OUTPUT_TOOL_NAME`（`builtin-tools` 包未转换 → 本地同名常量）。
const SYNTHETIC_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// ISO 时间戳（与 `query.rs` 的 `new_user_message` 同格式）。
fn now_ts() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

// ---------------------------------------------------------------------------
// 输入类型
// ---------------------------------------------------------------------------

/// 对齐 TS `prompt: string | ContentBlockParam[]`。
#[derive(Debug, Clone)]
pub enum Prompt {
    Text(String),
    Blocks(Vec<Value>),
}

/// `submitMessage` 的 options（TS `{ uuid?: string; isMeta?: boolean }`）。
#[derive(Debug, Clone, Default)]
pub struct SubmitMessageOptions {
    pub uuid: Option<String>,
    pub is_meta: Option<bool>,
}

// ---------------------------------------------------------------------------
// SDK 消息（下沉契约）
// ---------------------------------------------------------------------------

/// SDK 流消息（`entrypoints/agentSdkTypes.ts` 未转换 → 本地等价）。
#[derive(Debug, Clone, Serialize)]
pub enum SdkMessage {
    User(SdkUserMessage),
    Assistant(SdkAssistantMessage),
    System(SdkSystemMessage),
    ToolUseSummary(SdkToolUseSummary),
    Result(SdkResult),
    /// `includePartialMessages` 时旁路的原始流式事件（流式透传未转换，
    /// 当前 query 事件模型无流式事件 → 暂不产出）。
    StreamEvent(Value),
    /// system init 消息（`utils/messages/systemInit.ts` 未转换 → 占位）。
    SystemInit(Value),
}

#[derive(Debug, Clone, Serialize)]
pub struct SdkUserMessage {
    #[serde(rename = "type")]
    pub typ: String, // "user"
    /// `{ role: "user", content }` 消息体。
    pub message: Value,
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub uuid: String,
    pub timestamp: String,
    pub is_replay: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_synthetic: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SdkAssistantMessage {
    #[serde(rename = "type")]
    pub typ: String, // "assistant"
    /// `{ role: "assistant", content }` 消息体。
    pub message: Value,
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub uuid: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// TS 的 system 消息（compact_boundary / api_retry 等）；当前 query 事件
/// 模型仅产出 `SystemNotice{level,text}`，metadata 字段仅在就绪后填充。
#[derive(Debug, Clone, Serialize)]
pub struct SdkSystemMessage {
    #[serde(rename = "type")]
    pub typ: String, // "system"
    pub subtype: String,
    pub session_id: String,
    pub uuid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SdkToolUseSummary {
    #[serde(rename = "type")]
    pub typ: String, // "tool_use_summary"
    pub summary: Value,
    pub preceding_tool_use_ids: Value,
    pub session_id: String,
    pub uuid: String,
}

/// `type: 'result'` 终止消息（TS 的 success / error_* 各 subtype）。
#[derive(Debug, Clone, Serialize)]
pub struct SdkResult {
    #[serde(rename = "type")]
    pub typ: String, // "result"
    pub subtype: String,
    pub is_error: bool,
    pub duration_ms: u128,
    pub duration_api_ms: f64,
    pub num_turns: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub session_id: String,
    pub total_cost_usd: f64,
    pub usage: NonNullableUsage,
    /// `getModelUsage()`（`ModelUsage` 未实现 Serialize → 序列化为 JSON 对象）。
    pub model_usage: Value,
    pub permission_denials: Vec<SdkPermissionDenial>,
    pub uuid: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    /// `getFastModeState` 未转换 → 恒 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_mode_state: Option<Value>,
}

/// TS `SDKPermissionDenial`（canUseTool 拒绝追踪）。
#[derive(Debug, Clone, Serialize)]
pub struct SdkPermissionDenial {
    #[serde(rename = "type")]
    pub typ: String, // "permission_denial"
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
}

/// sink 契约：每次 `yield` 一次调用（`Send` 以兼容跨线程驱动）。
pub trait SdkSink: Send {
    fn emit(&mut self, message: SdkMessage);
}

#[derive(Debug, Error)]
pub enum QueryEngineError {
    #[error("query failed: {0}")]
    Query(#[from] QueryError),
}

// ---------------------------------------------------------------------------
// snip 回调（HISTORY_SNIP 注入接口）
// ---------------------------------------------------------------------------

/// 对齐 TS `snipReplay(yieldedSystemMsg, store)`。
///
/// 回调返回 `Some(executed)` 表示产出的是 snip 边界消息（调用方不再把边界
/// 入 store；`executed=true` 时回调应已就地替换 `store` 的内容），
/// `None` 表示非边界。`services/compact/snip*` 未转换 → `ask()` 暂不注入。
pub type SnipReplayFn = Arc<
    dyn Fn(&SystemNotice, &mut Vec<Message>) -> Option<bool> + Send + Sync,
>;

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 对齐 TS `QueryEngineConfig`。未转换的依赖类型沿用 `tool.rs` 的占位别名。
pub struct QueryEngineConfig {
    pub cwd: String,
    pub tools: Tools,
    pub commands: Vec<Command>,
    pub mcp_clients: Vec<McpServerConnection>,
    pub agents: Vec<AgentDefinition>,
    pub can_use_tool: CanUseToolFn,
    pub get_app_state: GetAppStateFn,
    pub set_app_state: SetAppStateFn,
    pub initial_messages: Option<Vec<Message>>,
    pub read_file_cache: FileStateCache,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub user_specified_model: Option<String>,
    pub fallback_model: Option<String>,
    pub thinking_config: Option<ThinkingConfig>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    /// wire `task_budget.total`（remaining 由 query 循环内部维护）。
    pub task_budget: Option<i64>,
    pub json_schema: Option<Value>,
    pub verbose: bool,
    pub replay_user_messages: bool,
    pub handle_elicitation: Option<HandleElicitationFn>,
    pub include_partial_messages: bool,
    pub set_sdk_status: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub abort_controller: Option<Arc<CancellationToken>>,
    pub orphaned_permission: Option<Value>,
    pub snip_replay: Option<SnipReplayFn>,
}

// ---------------------------------------------------------------------------
// QueryEngine
// ---------------------------------------------------------------------------

/// 会话编排器：一个 QueryEngine 对应一段会话，每次 `submit_message()` 是会话
/// 内的一个新 turn；状态（messages / usage / 文件缓存）跨 turn 持久。
///
/// 对齐 TS `QueryEngine`；class 继承链展开为 struct + 方法。
pub struct QueryEngine {
    config: QueryEngineConfig,
    mutable_messages: Vec<Message>,
    abort_controller: Arc<CancellationToken>,
    ///跨 turn 累积（每次 submit 起始按 TS 语义清空）。Arc<Mutex> 以便
    /// wrappedCanUseTool 在 query() 内部无借用地写入。
    permission_denials: Arc<Mutex<Vec<SdkPermissionDenial>>>,
    total_usage: NonNullableUsage,
    has_handled_orphaned_permission: bool,
    read_file_state: FileStateCache,
    discovered_skill_names: HashSet<String>,
    loaded_nested_memory_paths: HashSet<String>,
    /// COMPACTION_V2 空闲触发时钟（上一次 submit 起始时刻）。
    last_activity_at: Option<u64>,
}

impl QueryEngine {
    pub fn new(config: QueryEngineConfig) -> Self {
        let abort_controller = config
            .abort_controller
            .clone()
            .unwrap_or_else(|| Arc::new(CancellationToken::new()));
        Self {
            read_file_state: config.read_file_cache.clone(),
            mutable_messages: config.initial_messages.clone().unwrap_or_default(),
            abort_controller,
            permission_denials: Arc::new(Mutex::new(Vec::new())),
            total_usage: empty_usage(),
            has_handled_orphaned_permission: false,
            discovered_skill_names: HashSet::new(),
            loaded_nested_memory_paths: HashSet::new(),
            last_activity_at: None,
            config,
        }
    }

    /// 对齐 TS `submitMessage()`：处理用户输入、驱动 query()、yield SDK 消息。
    ///
    /// 注：query 事件先收集后转发（见 CollectSink），SSE 级流式透传待
    /// `includePartialMessages` 管道就绪后再启用（占位清单 #1）。
    pub async fn submit_message(
        &mut self,
        prompt: Prompt,
        options: &SubmitMessageOptions,
        sink: &mut dyn SdkSink,
    ) -> Result<(), QueryEngineError> {
        // ---- 拷贝 config 所需字段，规避 &mut self 与闭包的借用冲突 ----
        let cwd = self.config.cwd.clone();
        let commands = self.config.commands.clone();
        let tools = self.config.tools.clone();
        let mcp_clients = self.config.mcp_clients.clone();
        let agents = self.config.agents.clone();
        let verbose = self.config.verbose;
        let cfg_thinking_config = self.config.thinking_config.clone();
        let max_turns = self.config.max_turns;
        let max_budget_usd = self.config.max_budget_usd;
        let task_budget = self.config.task_budget;
        let custom_system_prompt = self.config.custom_system_prompt.clone();
        let append_system_prompt = self.config.append_system_prompt.clone();
        let user_specified_model = self.config.user_specified_model.clone();
        let fallback_model = self.config.fallback_model.clone();
        let json_schema = self.config.json_schema.clone();
        let get_app_state = self.config.get_app_state.clone();
        let set_app_state = self.config.set_app_state.clone();
        let handle_elicitation = self.config.handle_elicitation.clone();
        let set_sdk_status = self.config.set_sdk_status.clone();
        let replay_user_messages = self.config.replay_user_messages;
        let _include_partial_messages = self.config.include_partial_messages;

        self.discovered_skill_names.clear();
        self.permission_denials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        set_cwd_state(&cwd);
        let _persist_session = !is_session_persistence_disabled();
        let started_at = Instant::now();
        let session_id = get_session_id().0;

        // ---- 包装 canUseTool：追踪权限拒绝（SDK 上报用）----
        let inner_can_use_tool = self.config.can_use_tool.clone();
        let denials = self.permission_denials.clone();
        let wrapped_can_use_tool: CanUseToolFn = Arc::new(move |req: CanUseToolRequest<'_>| {
            let inner = inner_can_use_tool.clone();
            let denials = denials.clone();
            Box::pin(async move {
                let tool_name = req.tool.name().to_string();
                let tool_use_id = req.tool_use_id.to_string();
                let tool_input = req.input.clone();
                let result = inner(req).await;
                // result.behavior !== 'allow' → 记录拒绝
                if !matches!(result, PermissionResult::Allow(_)) {
                    let mut guard = denials.lock().unwrap_or_else(|p| p.into_inner());
                    guard.push(SdkPermissionDenial {
                        typ: "permission_denial".to_string(),
                        tool_name,
                        tool_use_id,
                        tool_input,
                    });
                }
                result
            })
        });

        // ---- 模型 / thinking 初始值 ----
        // TS: userSpecifiedModel ? parseUserSpecifiedModel(...) : getMainLoopModel()
        // parseUserSpecifiedModel 未转换 → 原样透传（占位清单 #2）。
        let initial_main_loop_model =
            user_specified_model.unwrap_or_else(get_main_loop_model);
        // TS: shouldEnableThinkingByDefault() !== false ? {type:'adaptive'} : {type:'disabled'}
        // thinking.ts 未转换 → 按默认启用取 adaptive（见占位清单 #3）。
        let initial_thinking_config: Value =
            cfg_thinking_config.unwrap_or_else(|| json!({"type": "adaptive"}));

        // ---- 系统提示 / 上下文（fetchSystemPromptParts 未转换 → 最小内联）----
        // custom prompt 优先；defaultSystemPrompt / userContext / systemContext
        // 待 utils/queryContext.ts + context.ts 转换后接入（占位清单 #4）。
        let default_system_prompt: Vec<String> = Vec::new();
        let mut user_context: HashMap<String, String> = HashMap::new();
        let system_context: HashMap<String, String> = HashMap::new();
        // COORDINATOR_MODE feature 与 coordinator 模块均未转换 → 不合并额外上下文。
        // 自定义 prompt + CLAUDE_COWORK_MEMORY_PATH_OVERRIDE 的 memory mechanics
        // 提示注入（memdir 未转换）→ 跳过（占位清单 #4）。
        let system_prompt: SystemPrompt = {
            let mut parts: Vec<String> = match &custom_system_prompt {
                Some(custom) => vec![custom.clone()],
                None => default_system_prompt,
            };
            if let Some(append) = &append_system_prompt {
                parts.push(append.clone());
            }
            parts
        };
        let _ = &mut user_context; // 后续 coordinator/memory 扩展点

        // ---- 结构化输出 enforcement（hooks/registerStructuredOutputEnforcement
        // 未转换）→ 跳过（占位清单 #5）----

        // ---- processUserInput 前的 ToolUseContext ----
        let ctx_first = build_tool_use_context(
            &tools,
            &commands,
            &mcp_clients,
            &agents,
            &initial_main_loop_model,
            &initial_thinking_config,
            verbose,
            max_budget_usd,
            &custom_system_prompt,
            &append_system_prompt,
            &self.mutable_messages,
            &get_app_state,
            &set_app_state,
            &self.abort_controller,
            &self.read_file_state,
            &handle_elicitation,
            &set_sdk_status,
            &self.loaded_nested_memory_paths,
            &self.discovered_skill_names,
        );

        // ---- COMPACTION_V2 空闲维护：默认关闭，门控与 compaction2/orchestrator
        // 均未转换 → 更新时钟后跳过（占位清单 #6）----
        if feature("COMPACTION_V2")
            && std::env::var("CCH_COMPACTION_IDLE")
                .map(|v| v == "1")
                .unwrap_or(false)
        {
            // TODO(compaction2): runIdleCompactionV2({messages, model,
            // toolUseContext, cacheSafeParams, querySource:'sdk',
            // lastActivityAt}) 就绪后在此挂载。
        }
        self.last_activity_at = Some(now_ms());

        // ---- 孤儿权限处理（handleOrphanedPermission 未转换）→ 仅占位 ----
        if self.config.orphaned_permission.is_some() && !self.has_handled_orphaned_permission {
            self.has_handled_orphaned_permission = true;
            // TODO(queryHelpers): handleOrphanedPermission 逐条 yield 其产出消息。
        }

        // ---- processUserInput（utils/processUserInput/* 未转换 → 最小内联）----
        // 纯文本/内容块直接构造用户消息；斜杠命令、附件解析、模型开关等待对接。
        let content = match &prompt {
            Prompt::Text(text) => MessageContent::Text(text.clone()),
            Prompt::Blocks(blocks) => MessageContent::Blocks(blocks.clone()),
        };
        let user_msg = UserMessage {
            message: claude::UserMessageBody { content },
            typ: "user".to_string(),
            uuid: options
                .uuid
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            timestamp: now_ts(),
            is_meta: if options.is_meta.unwrap_or(false) {
                Some(true)
            } else {
                None
            },
        };
        let messages_from_user_input: Vec<Message> = vec![Message::User(user_msg.clone())];
        let should_query = true; // 斜杠命令分支未转换 → 恒发起查询
        let allowed_tools: Vec<String> = Vec::new();
        let model_from_user_input: Option<String> = None;
        let result_text: Option<String> = None;

        // ---- 2.12 grace notes（rateLimitGrace 未转换）→ 跳过（占位清单 #7）----

        // ---- 追加新消息并快照 ----
        self.mutable_messages
            .extend(messages_from_user_input.iter().cloned());
        let mut messages = self.mutable_messages.clone();

        // ---- transcript 持久化（sessionStorage 未转换）→ 跳过（占位清单 #8）----
        // TS 在进入 query 前先写 transcript 保证 --resume 可用；Rust 侧待对接。

        // ---- 需 ack 的回放消息过滤 ----
        // messageSelector 未转换 → selectableUserMessagesFilter 退化为保留全部。
        let mut messages_to_ack: Vec<UserMessage> = Vec::new();
        if replay_user_messages {
            for m in &messages_from_user_input {
                if let Message::User(u) = m {
                    if u.is_meta.unwrap_or(false) {
                        continue;
                    }
                    // 跳过 tool_result（query 内会另行 ack）
                    if let MessageContent::Blocks(blocks) = &u.message.content {
                        if blocks
                            .iter()
                            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                        {
                            continue;
                        }
                    }
                    messages_to_ack.push(u.clone());
                }
            }
        }

        // ---- 同步 alwaysAllowRules.command 到 ToolPermissionContext ----
        let allowed_rules = Value::Array(
            allowed_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        );
        let set_state = set_app_state.clone();
        set_state(Arc::new(move |mut prev: Value| {
            if let Some(obj) = prev.as_object_mut() {
                let tpc = obj
                    .entry("toolPermissionContext")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(tpc_obj) = tpc.as_object_mut() {
                    let rules = tpc_obj
                        .entry("alwaysAllowRules")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Some(rules_obj) = rules.as_object_mut() {
                        rules_obj.insert("command".to_string(), allowed_rules.clone());
                    }
                }
            }
            prev
        }));

        let main_loop_model = model_from_user_input.unwrap_or_else(|| initial_main_loop_model.clone());

        // ---- 处理完 prompt 后重建 ToolUseContext（TS 第二个 context，
        // setMessages 为 no-op：斜杠命令路径未转换）----
        let ctx = build_tool_use_context(
            &tools,
            &commands,
            &mcp_clients,
            &agents,
            &main_loop_model,
            &initial_thinking_config,
            verbose,
            max_budget_usd,
            &custom_system_prompt,
            &append_system_prompt,
            &messages,
            &get_app_state,
            &set_app_state,
            &self.abort_controller,
            &self.read_file_state,
            &handle_elicitation,
            &set_sdk_status,
            &self.loaded_nested_memory_paths,
            &self.discovered_skill_names,
        );
        // ctx_first 仅为 TS 对齐而构造（其 setMessages 写回路径未接线）。
        drop(ctx_first);

        // ---- skills / plugins（cache-only 加载未转换）→ 跳过（占位清单 #9）----

        // ---- buildSystemInitMessage（utils/messages/systemInit.ts 未转换）
        // → 不产出 system init（占位清单 #10）----

        // ---- 本地命令路径（shouldQuery === false）----
        if !should_query {
            for m in &messages_from_user_input {
                if let Message::User(u) = m {
                    if let MessageContent::Text(text) = &u.message.content {
                        if text.contains(&format!("<{LOCAL_COMMAND_STDOUT_TAG}>"))
                            || text.contains(&format!("<{LOCAL_COMMAND_STDERR_TAG}>"))
                        {
                            // stripAnsi 未转换 → 原样输出
                            sink.emit(SdkMessage::User(replay_user_message(
                                u, &session_id,
                            )));
                        }
                    }
                }
                // system local_command / compact_boundary：claude::Message 无
                // system 变体 → 跳过（占位清单 #11）。
            }
            // transcript 二次写入：未转换 → 跳过
            sink.emit(SdkMessage::Result(self.build_result(
                "success",
                false,
                messages.len().saturating_sub(1) as u32,
                result_text,
                None,
                None,
                Vec::new(),
                started_at,
                &session_id,
                self.total_usage.clone(),
            )));
            return Ok(());
        }

        // ---- fileHistory 快照（fileHistoryEnabled / makeSnapshot 未转换）
        // → 跳过（占位清单 #12）----

        // ---- turn 级簿记 ----
        let mut turn_count = 1u32;
        let mut has_acknowledged_initial_messages = false;
        let mut structured_output_from_tool: Option<Value> = None;
        let mut last_stop_reason: Option<String> = None;
        // 本 query 累计 usage（占位清单 #1：无流式事件，改由 AssistantMessage
        // 的 usage 字段按消息累计）。
        let mut query_usage = empty_usage();
        // error_during_execution 的 errors[] 按 TS 语义做 turn 级截取：
        // 环形缓冲移出后 lastIndexOf 返回 -1 → 全量（安全回退）。
        let error_log_watermark = get_in_memory_errors().last().cloned();
        let initial_structured_output_calls = if json_schema.is_some() {
            count_tool_calls(&self.mutable_messages, SYNTHETIC_OUTPUT_TOOL_NAME)
        } else {
            0
        };

        // ---- 驱动 query()（事件先收集后映射）----
        let params = QueryParams {
            messages: messages.clone(),
            system_prompt: system_prompt.clone(),
            user_context: user_context.clone(),
            system_context: system_context.clone(),
            can_use_tool: wrapped_can_use_tool.clone(),
            tool_use_context: ctx,
            fallback_model: fallback_model.clone(),
            query_source: "sdk".to_string(),
            max_output_tokens_override: None,
            max_turns,
            skip_cache_write: false,
            task_budget,
            deps: None,
        };
        let mut collected = CollectSink::default();
        let terminal: Terminal = query(params, &mut collected).await?;

        let mut early_result: Option<SdkResult> = None;
        for event in collected.events {
            if early_result.is_some() {
                break; // 提前终止：丢弃剩余事件（对齐 TS generator return）
            }
            match &event {
                QueryEvent::AssistantMessage(am) => {
                    messages.push(Message::Assistant(am.clone()));
                    self.mutable_messages.push(Message::Assistant(am.clone()));
                    if let Some(u) = &am.message.usage {
                        query_usage = accumulate_usage(&query_usage, u);
                    }
                    if let Some(sr) = &am.message.stop_reason {
                        last_stop_reason = Some(sr.clone());
                    }
                    if !has_acknowledged_initial_messages && !messages_to_ack.is_empty() {
                        has_acknowledged_initial_messages = true;
                        for ack in &messages_to_ack {
                            sink.emit(SdkMessage::User(replay_user_message(
                                ack,
                                &session_id,
                            )));
                        }
                    }
                    sink.emit(SdkMessage::Assistant(sdk_assistant_message(
                        am, &session_id,
                    )));
                }
                QueryEvent::UserMessage(um) => {
                    messages.push(Message::User(um.clone()));
                    self.mutable_messages.push(Message::User(um.clone()));
                    turn_count = turn_count.saturating_add(1);
                    if !has_acknowledged_initial_messages && !messages_to_ack.is_empty() {
                        has_acknowledged_initial_messages = true;
                        for ack in &messages_to_ack {
                            sink.emit(SdkMessage::User(replay_user_message(
                                ack,
                                &session_id,
                            )));
                        }
                    }
                    sink.emit(SdkMessage::User(sdk_user_message(um, &session_id)));
                }
                QueryEvent::ToolResult(um) => {
                    // tool_result 用户消息：与 TS 'user' 分支同处理（入栈 + 计数）。
                    messages.push(Message::User(um.clone()));
                    self.mutable_messages.push(Message::User(um.clone()));
                    turn_count = turn_count.saturating_add(1);
                    sink.emit(SdkMessage::User(sdk_user_message(um, &session_id)));
                }
                QueryEvent::Tombstone(_) => {
                    // 墓碑是移除信号，不入栈、不产出（对齐 TS）
                }
                QueryEvent::SystemMessage(sn) => {
                    // snip 边界：回调就地重放 store；边界本身不入栈（对齐 TS）
                    if let Some(snip_replay) = &self.config.snip_replay {
                        let _executed = snip_replay(sn, &mut self.mutable_messages);
                    }
                    // compact_boundary 截断（释放压缩前消息）：SystemNotice 无
                    // compactMetadata → 无法判定，暂不截断（占位清单 #11）；
                    // api_error → api_retry 事件同理。
                }
                QueryEvent::Attachment(AttachmentKind::MaxTurnsReached {
                    max_turns,
                    turn_count: attachment_turn_count,
                }) => {
                    early_result = Some(self.build_result(
                        "error_max_turns",
                        true,
                        *attachment_turn_count,
                        None,
                        last_stop_reason.clone(),
                        None,
                        vec![format!("Reached maximum number of turns ({max_turns})")],
                        started_at,
                        &session_id,
                        query_usage.clone(),
                    ));
                }
                QueryEvent::Attachment(AttachmentKind::HookStoppedContinuation) => {
                    // TS attachment 分支仅处理 structured_output / max_turns /
                    // queued_command → hook 停止附件不产出 SDK 事件。
                }
                QueryEvent::StreamRequestStart => {
                    // 不产出（对齐 TS 'stream_request_start' case）
                }
            }

            // ---- USD 预算检查（每条消息后）----
            if early_result.is_none() {
                if let Some(budget) = max_budget_usd {
                    if get_total_cost_usd() >= budget {
                        early_result = Some(self.build_result(
                            "error_max_budget_usd",
                            true,
                            turn_count,
                            None,
                            last_stop_reason.clone(),
                            None,
                            vec![format!(
                                "Reached maximum budget (${budget}). Increase the limit with \
                                 --max-budget-usd or start a new session."
                            )],
                            started_at,
                            &session_id,
                            query_usage.clone(),
                        ));
                    }
                }
            }

            // ---- 结构化输出重试上限（仅 user 消息后检查）----
            if early_result.is_none() && json_schema.is_some()
                && matches!(&event, QueryEvent::UserMessage(_) | QueryEvent::ToolResult(_))
            {
                let max_retries = match std::env::var("MAX_STRUCTURED_OUTPUT_RETRIES") {
                    // TS parseInt 失败 → NaN → 永不触发；Rust 以 u64::MAX 表达。
                    Ok(raw) => raw.parse::<u64>().unwrap_or(u64::MAX),
                    Err(_) => 5,
                };
                let current_calls =
                    count_tool_calls(&self.mutable_messages, SYNTHETIC_OUTPUT_TOOL_NAME);
                if current_calls.saturating_sub(initial_structured_output_calls) >= max_retries {
                    early_result = Some(self.build_result(
                        "error_max_structured_output_retries",
                        true,
                        turn_count,
                        None,
                        last_stop_reason.clone(),
                        None,
                        vec![format!(
                            "Failed to provide valid structured output after {max_retries} attempts"
                        )],
                        started_at,
                        &session_id,
                        query_usage.clone(),
                    ));
                }
            }
        }

        if let Some(result) = early_result {
            // 提前终止路径：只能拿到已累计的 query_usage（无 Terminal）。
            self.total_usage = accumulate_usage(&self.total_usage, &query_usage);
            sink.emit(SdkMessage::Result(result));
            return Ok(());
        }

        // 自然结束：Terminal.usage 已含本 query 全部 usage。
        self.total_usage = accumulate_usage(&self.total_usage, &terminal.usage);

        // flush buffered transcript（sessionStorage 未转换）→ 跳过

        // ---- 结果消息 ----
        // stop hooks 可能产出 progress/attachment 收尾消息 → 与 TS 同样只取
        // 最后的 assistant|user 作为结果消息。
        let result_msg = messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::Assistant(_) | Message::User(_)));
        let ede_result_type = match result_msg {
            Some(Message::Assistant(_)) => "assistant",
            Some(Message::User(_)) => "user",
            None => "undefined",
        };
        let ede_last_content_type = match result_msg {
            Some(Message::Assistant(a)) => last_content_type(a),
            _ => "n/a",
        };

        if !is_result_successful(result_msg, &last_stop_reason) {
            let mut errors = vec![format!(
                "[ede_diagnostic] result_type={ede_result_type} \
                 last_content_type={ede_last_content_type} stop_reason={last_stop_reason:?}"
            )];
            let all = get_in_memory_errors();
            let start = match &error_log_watermark {
                Some(watermark) => all
                    .iter()
                    .rposition(|e| {
                        e.error == watermark.error && e.timestamp == watermark.timestamp
                    })
                    .map(|i| i + 1)
                    .unwrap_or(0),
                None => 0,
            };
            errors.extend(all.into_iter().skip(start).map(|e| e.error));
            sink.emit(SdkMessage::Result(self.build_result(
                "error_during_execution",
                true,
                turn_count,
                None,
                last_stop_reason.clone(),
                None,
                errors,
                started_at,
                &session_id,
                self.total_usage.clone(),
            )));
            return Ok(());
        }

        // 提取文本结果（SYNTHETIC_MESSAGES 集合未转换 → 不做合成文本过滤，
        // 见占位清单 #13）
        let (text_result, is_api_error) = match result_msg {
            Some(Message::Assistant(a)) => {
                let last_block = match &a.message.content {
                    MessageContent::Blocks(blocks) => blocks.last(),
                    MessageContent::Text(_) => None,
                };
                let text = last_block
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (text.unwrap_or_default(), false /* isApiErrorMessage 无对应字段 */)
            }
            _ => (String::new(), false),
        };

        sink.emit(SdkMessage::Result(self.build_result(
            "success",
            is_api_error,
            turn_count,
            Some(text_result),
            last_stop_reason.clone(),
            structured_output_from_tool.take(),
            Vec::new(),
            started_at,
            &session_id,
            self.total_usage.clone(),
        )));
        Ok(())
    }

    // ---- 控制与访问器（对齐 TS interrupt / resetAbortController /
    // getAbortSignal / getMessages / getReadFileState / getSessionId /
    // setModel）----

    pub fn interrupt(&self) {
        self.abort_controller.abort(None);
    }

    /// interrupt() 后重置令牌，使下一次 submit_message() 拿到未取消信号。
    pub fn reset_abort_controller(&mut self) {
        self.abort_controller = Arc::new(CancellationToken::new());
    }

    /// 暴露当前取消令牌（外部消费者用，如 ACP bridge）。
    pub fn abort_token(&self) -> Arc<CancellationToken> {
        self.abort_controller.clone()
    }

    pub fn get_messages(&self) -> Vec<Message> {
        self.mutable_messages.clone()
    }

    pub fn get_read_file_state(&self) -> FileStateCache {
        self.read_file_state.clone()
    }

    pub fn get_session_id(&self) -> String {
        crate::bootstrap::state::get_session_id().0
    }

    pub fn set_model(&mut self, model: String) {
        self.config.user_specified_model = Some(model);
    }

    // ---- 结果消息组装 ----

    #[allow(clippy::too_many_arguments)]
    fn build_result(
        &self,
        subtype: &str,
        is_error: bool,
        num_turns: u32,
        result: Option<String>,
        stop_reason: Option<String>,
        structured_output: Option<Value>,
        errors: Vec<String>,
        started_at: Instant,
        session_id: &str,
        usage: NonNullableUsage,
    ) -> SdkResult {
        let permission_denials = self
            .permission_denials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        SdkResult {
            typ: "result".to_string(),
            subtype: subtype.to_string(),
            is_error,
            duration_ms: started_at.elapsed().as_millis(),
            duration_api_ms: get_total_api_duration(),
            num_turns,
            result,
            stop_reason,
            session_id: session_id.to_string(),
            total_cost_usd: get_total_cost_usd(),
            usage,
            model_usage: model_usage_value(),
            permission_denials,
            uuid: Uuid::new_v4().to_string(),
            errors,
            structured_output,
            // getFastModeState 未转换 → None（占位清单 #14）
            fast_mode_state: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ask() — 一次性便捷包装（对齐 TS `ask()`）
// ---------------------------------------------------------------------------

/// `ask()` 的全部入参（对齐 TS `ask({...})` 的参数对象）。
pub struct AskParams {
    pub prompt: Prompt,
    pub prompt_uuid: Option<String>,
    pub is_meta: Option<bool>,
    pub cwd: String,
    pub tools: Tools,
    pub commands: Vec<Command>,
    pub mcp_clients: Vec<McpServerConnection>,
    pub verbose: bool,
    pub thinking_config: Option<ThinkingConfig>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub task_budget: Option<i64>,
    pub can_use_tool: CanUseToolFn,
    pub mutable_messages: Vec<Message>,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub user_specified_model: Option<String>,
    pub fallback_model: Option<String>,
    pub json_schema: Option<Value>,
    pub get_app_state: GetAppStateFn,
    pub set_app_state: SetAppStateFn,
    pub get_read_file_cache: Arc<dyn Fn() -> FileStateCache + Send + Sync>,
    pub set_read_file_cache: Arc<dyn Fn(FileStateCache) + Send + Sync>,
    pub abort_controller: Option<Arc<CancellationToken>>,
    pub replay_user_messages: bool,
    pub include_partial_messages: bool,
    pub handle_elicitation: Option<HandleElicitationFn>,
    pub agents: Vec<AgentDefinition>,
    pub set_sdk_status: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub orphaned_permission: Option<Value>,
}

/// 单次 prompt 便捷入口：构 Engine、跑一次 submit_message、写回 readFileCache
/// （对齐 TS `ask()` 的 finally 语义）。
pub async fn ask(
    params: AskParams,
    sink: &mut dyn SdkSink,
) -> Result<(), QueryEngineError> {
    let set_read_file_cache = params.set_read_file_cache.clone();
    let get_read_file_cache = params.get_read_file_cache.clone();
    let mut engine = QueryEngine::new(QueryEngineConfig {
        cwd: params.cwd,
        tools: params.tools,
        commands: params.commands,
        mcp_clients: params.mcp_clients,
        agents: params.agents,
        can_use_tool: params.can_use_tool,
        get_app_state: params.get_app_state,
        set_app_state: params.set_app_state,
        initial_messages: Some(params.mutable_messages),
        // cloneFileStateCache（fileStateCache.ts 未转换）→ 直接 clone。
        read_file_cache: get_read_file_cache(),
        custom_system_prompt: params.custom_system_prompt,
        append_system_prompt: params.append_system_prompt,
        user_specified_model: params.user_specified_model,
        fallback_model: params.fallback_model,
        thinking_config: params.thinking_config,
        max_turns: params.max_turns,
        max_budget_usd: params.max_budget_usd,
        task_budget: params.task_budget,
        json_schema: params.json_schema,
        verbose: params.verbose,
        replay_user_messages: params.replay_user_messages,
        handle_elicitation: params.handle_elicitation,
        include_partial_messages: params.include_partial_messages,
        set_sdk_status: params.set_sdk_status,
        abort_controller: params.abort_controller,
        orphaned_permission: params.orphaned_permission,
        // HISTORY_SNIP 模块未转换 → 不注入 snipReplay（占位清单 #15）
        snip_replay: None,
    });
    let outcome = engine
        .submit_message(
            params.prompt,
            &SubmitMessageOptions {
                uuid: params.prompt_uuid,
                is_meta: params.is_meta,
            },
            sink,
        )
        .await;
    set_read_file_cache(engine.get_read_file_state());
    outcome
}

// ---------------------------------------------------------------------------
// 内部助手
// ---------------------------------------------------------------------------

/// query 事件收集 sink（流式透传未就绪前先缓冲，见占位清单 #1）。
#[derive(Default)]
struct CollectSink {
    events: Vec<QueryEvent>,
}

impl QuerySink for CollectSink {
    fn emit(&mut self, event: QueryEvent) {
        self.events.push(event);
    }
}

fn model_usage_value() -> Value {
    Value::Object(
        get_model_usage()
            .into_iter()
            .map(|(model, usage): (String, ModelUsage)| {
                (
                    model,
                    json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cache_read_input_tokens": usage.cache_read_input_tokens,
                        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        "web_search_requests": usage.web_search_requests,
                        "cost_usd": usage.cost_usd,
                        "context_window": usage.context_window,
                        "max_output_tokens": usage.max_output_tokens,
                    }),
                )
            })
            .collect(),
    )
}

/// 构造 ToolUseContext（对齐 TS 两个 processUserInputContext 的公共字段）。
#[allow(clippy::too_many_arguments)]
fn build_tool_use_context(
    tools: &Tools,
    commands: &[Command],
    mcp_clients: &[McpServerConnection],
    agents: &[AgentDefinition],
    main_loop_model: &str,
    thinking_config: &Value,
    verbose: bool,
    max_budget_usd: Option<f64>,
    custom_system_prompt: &Option<String>,
    append_system_prompt: &Option<String>,
    messages: &[Message],
    get_app_state: &GetAppStateFn,
    set_app_state: &SetAppStateFn,
    abort_controller: &Arc<CancellationToken>,
    read_file_state: &Value,
    handle_elicitation: &Option<HandleElicitationFn>,
    set_sdk_status: &Option<Arc<dyn Fn(String) + Send + Sync>>,
    loaded_nested_memory_paths: &HashSet<String>,
    discovered_skill_names: &HashSet<String>,
) -> ToolUseContext {
    // fileHistory / attribution 更新器：透传到 AppState（对齐 TS 的
    // setAppState(prev => ({...prev, fileHistory: updater(prev.fileHistory)}))）。
    let update_file_history_state: UpdateStateFn = {
        let set_state = Arc::clone(set_app_state);
        Arc::new(move |updater: StateUpdater| {
            let set_state = Arc::clone(&set_state);
            set_state(Arc::new(move |mut prev: Value| {
                if let Some(obj) = prev.as_object_mut() {
                    let prev_val = obj.get("fileHistory").cloned().unwrap_or(Value::Null);
                    obj.insert("fileHistory".to_string(), updater(prev_val));
                }
                prev
            }));
        })
    };
    let update_attribution_state: UpdateStateFn = {
        let set_state = Arc::clone(set_app_state);
        Arc::new(move |updater: StateUpdater| {
            let set_state = Arc::clone(&set_state);
            set_state(Arc::new(move |mut prev: Value| {
                if let Some(obj) = prev.as_object_mut() {
                    let prev_val = obj.get("attribution").cloned().unwrap_or(Value::Null);
                    obj.insert("attribution".to_string(), updater(prev_val));
                }
                prev
            }));
        })
    };

    ToolUseContext {
        options: ToolUseOptions {
            commands: commands.to_vec(),
            debug: false, // 走 stdout，避免污染（对齐 TS 注释）
            main_loop_model: main_loop_model.to_string(),
            tools: tools.clone(),
            verbose,
            thinking_config: thinking_config.clone(),
            mcp_clients: mcp_clients.to_vec(),
            mcp_resources: HashMap::new(),
            is_non_interactive_session: true,
            agent_definitions: json!({
                "activeAgents": agents.to_vec(),
                "allAgents": Vec::<Value>::new(),
            }),
            max_budget_usd,
            custom_system_prompt: custom_system_prompt.clone(),
            append_system_prompt: append_system_prompt.clone(),
            query_source: Some("sdk".to_string()),
            refresh_tools: None,
            allow_background_forked_slash_commands: None,
        },
        abort_controller: Arc::clone(abort_controller),
        read_file_state: read_file_state.clone(),
        get_app_state: Arc::clone(get_app_state),
        set_app_state: Arc::clone(set_app_state),
        set_app_state_for_tasks: None,
        handle_elicitation: handle_elicitation.clone(),
        set_tool_jsx: None,
        add_notification: None,
        append_system_message: None,
        send_os_notification: None,
        nested_memory_attachment_triggers: Some(HashSet::new()),
        loaded_nested_memory_paths: Some(loaded_nested_memory_paths.clone()),
        dynamic_skill_dir_triggers: Some(HashSet::new()),
        discovered_skill_names: Some(discovered_skill_names.clone()),
        user_modified: None,
        set_in_progress_tool_use_ids: Arc::new(|_ids: StringSetUpdater| ()),
        set_has_interruptible_tool_in_progress: None,
        set_response_length: None,
        push_api_metrics_entry: None,
        set_stream_mode: None,
        on_compact_progress: None,
        set_sdk_status: set_sdk_status.clone(),
        open_message_selector: None,
        update_file_history_state,
        update_attribution_state,
        set_conversation_id: None,
        agent_id: None,
        agent_type: None,
        require_can_use_tool: None,
        messages: messages_to_values(messages),
        file_reading_limits: None,
        glob_limits: None,
        tool_decisions: None,
        query_tracking: None,
        request_prompt: None,
        tool_use_id: None,
        critical_system_reminder_experimental: None,
        langfuse_trace: None,
        langfuse_root_trace: None,
        langfuse_batch_span: None,
        preserve_tool_use_results: None,
        local_denial_tracking: None,
        content_replacement_state: None,
        rendered_system_prompt: None,
    }
}

/// claude::Message → Value（ToolUseContext.messages 的占位类型是 Value）。
fn messages_to_values(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m {
            Message::User(u) => serde_json::to_value(u).unwrap_or(Value::Null),
            Message::Assistant(a) => serde_json::to_value(a).unwrap_or(Value::Null),
        })
        .collect()
}

fn content_value(content: &MessageContent) -> Value {
    serde_json::to_value(content).unwrap_or(Value::Null)
}

/// 回放用户消息（ack 队列 / 本地命令输出，isReplay = true）。
fn replay_user_message(msg: &UserMessage, session_id: &str) -> SdkUserMessage {
    SdkUserMessage {
        typ: "user".to_string(),
        message: json!({
            "role": "user",
            "content": content_value(&msg.message.content),
        }),
        session_id: session_id.to_string(),
        parent_tool_use_id: None,
        uuid: msg.uuid.clone(),
        timestamp: msg.timestamp.clone(),
        is_replay: true,
        is_synthetic: None,
    }
}

/// normalizeMessage（queryHelpers.ts 未转换）的最小等价：用户消息。
fn sdk_user_message(msg: &UserMessage, session_id: &str) -> SdkUserMessage {
    SdkUserMessage {
        typ: "user".to_string(),
        message: json!({
            "role": "user",
            "content": content_value(&msg.message.content),
        }),
        session_id: session_id.to_string(),
        parent_tool_use_id: None,
        uuid: msg.uuid.clone(),
        timestamp: msg.timestamp.clone(),
        is_replay: false,
        is_synthetic: msg.is_meta,
    }
}

/// normalizeMessage 的最小等价：助手消息。
fn sdk_assistant_message(msg: &AssistantMessage, session_id: &str) -> SdkAssistantMessage {
    SdkAssistantMessage {
        typ: "assistant".to_string(),
        message: json!({
            "role": "assistant",
            "content": content_value(&msg.message.content),
        }),
        session_id: session_id.to_string(),
        parent_tool_use_id: None,
        uuid: msg.uuid.clone(),
        timestamp: msg.timestamp.clone(),
        request_id: msg.request_id.clone(),
    }
}

/// isResultSuccessful（queryHelpers.ts 未转换）的最小等价：
/// 最后一条 assistant 的末块为 text/thinking、或最后一条 user 含 tool_result、
/// 或 stop_reason 为 end_turn，视为成功。
fn is_result_successful(result: Option<&Message>, last_stop_reason: &Option<String>) -> bool {
    if last_stop_reason.as_deref() == Some("end_turn") {
        return true;
    }
    match result {
        Some(Message::Assistant(a)) => match &a.message.content {
            MessageContent::Text(_) => true,
            MessageContent::Blocks(blocks) => blocks
                .last()
                .and_then(|b| b.get("type"))
                .and_then(Value::as_str)
                .map_or(false, |t| t == "text" || t == "thinking"),
        },
        Some(Message::User(u)) => match &u.message.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")),
            MessageContent::Text(_) => false,
        },
        None => false,
    }
}

/// ede 诊断用的最后内容块类型。
fn last_content_type(msg: &AssistantMessage) -> &'static str {
    match &msg.message.content {
        MessageContent::Text(_) => "text",
        MessageContent::Blocks(blocks) => match blocks
            .last()
            .and_then(|b| b.get("type"))
            .and_then(Value::as_str)
        {
            Some("text") => "text",
            Some("thinking") => "thinking",
            Some("tool_use") => "tool_use",
            Some(_) => "other",
            None => "none",
        },
    }
}

/// countToolCalls（utils/messages.ts 未转换）的最小等价：统计指定工具的
/// tool_use 块数量。
fn count_tool_calls(messages: &[Message], tool_name: &str) -> u64 {
    messages
        .iter()
        .map(|m| match m {
            Message::Assistant(a) => match &a.message.content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) == Some("tool_use")
                            && b.get("name").and_then(Value::as_str) == Some(tool_name)
                    })
                    .count() as u64,
                MessageContent::Text(_) => 0,
            },
            Message::User(_) => 0,
        })
        .sum()
}

// ---------------------------------------------------------------------------
// 占位清单（未转换依赖 → 当前行为偏差）
// ---------------------------------------------------------------------------
// 1. 流式透传：Rust `QueryEvent` 无 message_start/message_delta 等流式事件，
//    submit_message 先收集 query 全部事件再映射；usage 按 AssistantMessage
//    的 usage 字段累计（TS 按流内 message_stop 累计，近似等价）。
//    `include_partial_messages` 因此暂不产出 StreamEvent。
// 2. parseUserSpecifiedModel：未转换 → userSpecifiedModel 原样透传。
// 3. shouldEnableThinkingByDefault：未转换 → 默认 adaptive；注意
//    `claude::ThinkingConfig` 无 Adaptive 变体，query.rs 反序列化失败回退
//    Disabled，故实际效果为关闭思考。
// 4. fetchSystemPromptParts / buildContext / memdir：未转换 → 默认系统提示
//    为空、userContext/systemContext 为空 map、memory mechanics 提示不注入。
// 5. registerStructuredOutputEnforcement：hooks 模块未转换 → 不注册。
// 6. COMPACTION_V2 空闲压缩 + HISTORY_SNIP 边界压缩：services/compact/*
//    未转换 → 仅保留触发时钟与接口（snip_replay）；无压缩让位。
// 7. rateLimitGrace（grace notes / 2.12 主回合提示）：未转换 → 不产出。
// 8. sessionStorage（recordTranscript / flushSessionStorage）：未转换 →
//    不落盘；kill 后 --resume 不可用（TS 注释强调的关键路径差异）。
// 9. getSlashCommandToolSkills / loadAllPluginsCacheOnly：未转换 → 空集。
// 10. buildSystemInitMessage：未转换 → 不产出 system init SDK 消息。
// 11. system 消息映射：claude::Message 无 system 变体、SystemNotice 无
//     subtype/compactMetadata → compact_boundary 截断（TS splice 释放压缩
//     前消息）与 api_retry 事件均不可达；system 消息不入 mutableMessages。
// 12. fileHistory 快照、commitAttribution 更新器：透传接口已接线，但
//     快照生产侧（fileHistoryMakeSnapshot）未转换 → 不拍照。
// 13. SYNTHETIC_MESSAGES（utils/messages.ts）与 isApiErrorMessage：
//     未转换 → 文本结果不做合成文本过滤、success 的 is_error 恒 false。
// 14. getFastModeState / currentGraceLimitsObservation / claudeAiLimits：
//     未转换 → result.fast_mode_state 恒 None。
// 15. handleOrphanedPermission：未转换 → orphaned_permission 仅标记已处理，
//     不产出消息；ask() 不注入 snipReplay。
// 16. processUserInput：未转换 → 斜杠命令/附件/模型开关全部退化为
//     shouldQuery=true 的纯文本消息；allowed_tools 恒为空数组。
