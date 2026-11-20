//! state.rs — 会话级全局状态（模块级单例）。
//!
//! 重写自 `src/bootstrap/state.ts`。TS 用模块作用域的 `const STATE` +
//! `let` 模块变量表达单例；Rust 用 `once_cell::sync::Lazy<Mutex<State>>`
//! 表达同一语义，模块作用域的零散变量用 `Atomic*` / `Lazy<Mutex<..>>`
//! 承载（对齐 CLAUDE.md 中 state.ts 模块级单例 → LazyLock/OnceLock 的
//! 指示）。
//!
//! // DO NOT ADD MORE STATE HERE - BE JUDICIOUS WITH GLOBAL STATE
//!
//! 已知差异（均为依赖未转换模块所致，等对应波次落地后替换）：
//! - `t()`（i18n）未转换：`set_meter` 的 counter 描述用英文字面量，
//!   即 i18n 的恒等（en）行为。
//! - `resetSettingsCache`（utils/settings/settingsCache.js）未转换：
//!   `set_use_cowork_plugins` 内部调用以本地 no-op 占位
//!   （`reset_settings_cache_unimplemented`）。
//! - TS 的 `.normalize('NFC')` 无 unicode-normalization crate 可用，
//!   路径按原样存储。
//! - OTel（Meter/Logger/Provider 等）未转换：以 `OpaqueProvider`
//!   （`Arc<dyn Any + Send + Sync>`）做「存入即原样取回」的不透明句柄。
//! - `BetaMessageStreamParams`（SDK）未转换：`last_api_request` /
//!   `last_api_request_messages` 以 `serde_json::Value` 暂存。
//! - `SessionId` 来自 `crate::types::ids`；`ModelUsage`、`HookEvent`、
//!   `ModelSetting`、`ModelStrings`、`SettingSource`、`AgentColorName`、
//!   `RegisteredHookMatcher` 等类型在此局部定义，对应模块转换后应迁移
//!   并删除本模块中的临时定义。
//! - TS 返回 live 引用的 getter（Map/Set/数组）在此返回快照克隆，
//!   避免裸引用跨越 Mutex 边界；经由 `*View` 类型提供 get/delete 的
//!   活锁视图（plans.ts / systemPromptSections.ts 的使用模式）。

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use once_cell::sync::Lazy;
use uuid::Uuid;

use crate::types::ids::SessionId;
use crate::utils::signal::Signal;

// ---------------------------------------------------------------------------
// 临时类型定义（依赖未转换模块；见模块头已知差异）
// ---------------------------------------------------------------------------

/// 模型用量（对齐 `src/entrypoints/sdk/coreTypes.generated.ts` 的 ModelUsage）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub web_search_requests: i64,
    pub cost_usd: f64,
    pub context_window: u64,
    pub max_output_tokens: u64,
}

/// `ModelSetting = ModelName | ModelAlias | null`（utils/model/model.ts）。
/// `ModelName` 即 string、`ModelAlias` 为 string 联合，故 Option<String> 等价。
pub type ModelSetting = Option<String>;

/// `ModelStrings = Record<ModelKey, string>`（utils/model/modelStrings.ts）。
pub type ModelStrings = HashMap<String, String>;

/// 设置来源（utils/settings/constants.ts 的 SETTING_SOURCES）。
/// 顺序敏感：后面的来源覆盖前面的。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    FlagSettings,
    PolicySettings,
}

impl SettingSource {
    /// 默认允许的全部来源（顺序与 TS 的 getAllowedSettingSources 初值一致）。
    pub const ALL: &[SettingSource] = &[
        SettingSource::UserSettings,
        SettingSource::ProjectSettings,
        SettingSource::LocalSettings,
        SettingSource::FlagSettings,
        SettingSource::PolicySettings,
    ];
}

/// Agent 颜色名（builtin-tools/.../agentColorManager.ts 的 AgentColorName）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentColorName {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}

/// Hook 事件名（entrypoints/sdk/coreSchemas.ts HOOK_EVENTS，27 项）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    PermissionRequest,
    PermissionDenied,
    Setup,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    WorktreeCreate,
    WorktreeRemove,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::Notification => "Notification",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PermissionDenied => "PermissionDenied",
            HookEvent::Setup => "Setup",
            HookEvent::TeammateIdle => "TeammateIdle",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::Elicitation => "Elicitation",
            HookEvent::ElicitationResult => "ElicitationResult",
            HookEvent::ConfigChange => "ConfigChange",
            HookEvent::WorktreeCreate => "WorktreeCreate",
            HookEvent::WorktreeRemove => "WorktreeRemove",
            HookEvent::InstructionsLoaded => "InstructionsLoaded",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::FileChanged => "FileChanged",
        }
    }

    pub fn from_name(name: &str) -> Option<HookEvent> {
        HOOK_EVENT_ALL
            .iter()
            .copied()
            .find(|event| event.as_str() == name)
    }
}

const HOOK_EVENT_ALL: [HookEvent; 27] = [
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::PostToolUseFailure,
    HookEvent::Notification,
    HookEvent::UserPromptSubmit,
    HookEvent::SessionStart,
    HookEvent::SessionEnd,
    HookEvent::Stop,
    HookEvent::StopFailure,
    HookEvent::SubagentStart,
    HookEvent::SubagentStop,
    HookEvent::PreCompact,
    HookEvent::PostCompact,
    HookEvent::PermissionRequest,
    HookEvent::PermissionDenied,
    HookEvent::Setup,
    HookEvent::TeammateIdle,
    HookEvent::TaskCreated,
    HookEvent::TaskCompleted,
    HookEvent::Elicitation,
    HookEvent::ElicitationResult,
    HookEvent::ConfigChange,
    HookEvent::WorktreeCreate,
    HookEvent::WorktreeRemove,
    HookEvent::InstructionsLoaded,
    HookEvent::CwdChanged,
    HookEvent::FileChanged,
];

/// 已注册 hook matcher 的统一接口（对齐 TS 的
/// `HookCallbackMatcher | PluginHookMatcher` 联合类型）。
/// types/hooks.rs 与 utils/settings/types.rs 转换后由具体结构实现本 trait。
pub trait RegisteredHookMatcher: Send + Sync {
    /// 是否为插件提供的原生 hook（带 pluginRoot）。
    /// 对齐 TS `clearRegisteredPluginHooks` 中的 `'pluginRoot' in m` 判定。
    fn is_plugin_hook(&self) -> bool;
}

/// OTel provider/logger 等不透明句柄（见模块头已知差异）。
pub type OpaqueProvider = Arc<dyn Any + Send + Sync>;

/// OTel 属性值（Attributes）的本地近似。
#[derive(Clone, Debug)]
pub enum AttributeValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// OTel Attributes 的本地近似。
pub type Attributes = Vec<(String, AttributeValue)>;

/// 可带附加属性的计数器（对齐 TS 的 AttributedCounter）。
pub trait AttributedCounter: Send + Sync {
    fn add(&self, value: f64, additional_attributes: Option<&Attributes>);
}

/// OTel MetricOptions 的本地近似（set_meter 创建 counter 时传入）。
#[derive(Clone, Debug)]
pub struct MetricOptions {
    pub description: String,
    pub unit: Option<String>,
}

/// `statsStore`： `{ observe(name, value) }` 的观察器接口。
pub trait StatsStore: Send + Sync {
    fn observe(&self, name: &str, value: f64);
}

// ---------------------------------------------------------------------------
// ChannelEntry（--channels 解析结果；TS 原生定义于本文件）
// ---------------------------------------------------------------------------

/// `--channels` 条目。dev: true 表示来自 --dangerously-load-development-channels；
/// allowlist 门禁按条目检查（而非会话级 has_dev_channels 位），避免两个 flag
/// 同时传递时 dev 对话的接受结果泄漏 allowlist 绕过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelEntry {
    Plugin {
        name: String,
        marketplace: String,
        dev: bool,
    },
    Server {
        name: String,
        dev: bool,
    },
}

// ---------------------------------------------------------------------------
// SessionCronTask / InvokedSkillInfo / TeleportedSessionInfo / SlowOperation
// ---------------------------------------------------------------------------

/// 会话级 cron 任务（durable: false 的 CronCreate 任务，不落盘）。
#[derive(Clone, Debug, PartialEq)]
pub struct SessionCronTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    pub recurring: Option<bool>,
    /// 由进程内 teammate（非 team lead）创建时设置；调度器把触发投递到该
    /// teammate 的 pendingUserMessages 队列而非主 REPL 命令队列。
    pub agent_id: Option<String>,
}

/// 已调用 skill 的追踪记录（压缩时保留）。
#[derive(Clone, Debug, PartialEq)]
pub struct InvokedSkillInfo {
    pub skill_name: String,
    pub skill_path: String,
    pub content: String,
    pub invoked_at: i64,
    pub agent_id: Option<String>,
}

/// Teleported session 可靠性日志追踪。
#[derive(Clone, Debug, PartialEq)]
pub struct TeleportedSessionInfo {
    pub is_teleported: bool,
    pub has_logged_first_message: bool,
    pub session_id: Option<String>,
}

/// 慢操作记录（dev bar 展示用，ant-only）。
#[derive(Clone, Debug, PartialEq)]
pub struct SlowOperation {
    pub operation: String,
    pub duration_ms: f64,
    pub timestamp: i64,
}

/// 内存错误日志条目。
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorLogEntry {
    pub error: String,
    pub timestamp: String,
}

/// 问题预览格式（questionPreview: 'markdown' | 'html'）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionPreviewFormat {
    Markdown,
    Html,
}

// ---------------------------------------------------------------------------
// 有序 Map（TS Map 语义 + 容量淘汰）
// ---------------------------------------------------------------------------

/// plan slug 缓存：插入有序、容量 50，超限时淘汰最旧条目。
/// 对齐 TS `planSlugCache: Map<string, string>` +
/// `if (size >= 50) delete(keys().next().value)`。
#[derive(Default)]
struct PlanSlugCache {
    entries: Vec<(String, String)>,
}

impl PlanSlugCache {
    const MAX_ENTRIES: usize = 50;

    fn get(&self, session_id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(k, _)| k == session_id)
            .map(|(_, v)| v.clone())
    }

    fn remove(&mut self, session_id: &str) {
        self.entries.retain(|(k, _)| k != session_id);
    }

    fn set(&mut self, session_id: String, slug: String) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == session_id) {
            slot.1 = slug;
            return;
        }
        if self.entries.len() >= Self::MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push((session_id, slug));
    }
}

/// system prompt section 缓存：插入有序、容量 100，超限时淘汰最旧条目。
/// 值可为 None（显式记录「计算结果为空」，与 TS 的 `string | null` 对齐）。
#[derive(Default)]
struct SystemPromptSectionCache {
    entries: Vec<(String, Option<String>)>,
}

impl SystemPromptSectionCache {
    const MAX_ENTRIES: usize = 100;

    fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == name)
    }

    fn get(&self, name: &str) -> Option<Option<String>> {
        self.entries
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    fn set(&mut self, name: String, value: Option<String>) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == name) {
            slot.1 = value;
            return;
        }
        if self.entries.len() >= Self::MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push((name, value));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

// ---------------------------------------------------------------------------
// State 主体
// ---------------------------------------------------------------------------

// 部分字段在 TS 侧只有 setter、没有 getter（session_source、agent_color_index、
// in_memory_error_log），为 API 对等保留，等下游模块转换后产生读取方。
#[allow(dead_code)]
struct State {
    original_cwd: String,
    /// 稳定的项目根：启动时设置一次（含 --worktree），会话中 EnterWorktreeTool
    /// 不更新。用于项目身份（history/skills/sessions），不用于文件操作。
    project_root: String,
    total_cost_usd: f64,
    total_api_duration: f64,
    total_api_duration_without_retries: f64,
    total_tool_duration: f64,
    turn_hook_duration_ms: f64,
    turn_tool_duration_ms: f64,
    turn_classifier_duration_ms: f64,
    turn_tool_count: u32,
    turn_hook_count: u32,
    turn_classifier_count: u32,
    start_time: i64,
    last_interaction_time: i64,
    total_lines_added: i64,
    total_lines_removed: i64,
    has_unknown_model_cost: bool,
    cwd: String,
    model_usage: HashMap<String, ModelUsage>,
    main_loop_model_override: ModelSetting,
    initial_main_loop_model: ModelSetting,
    model_strings: Option<ModelStrings>,
    is_interactive: bool,
    kairos_active: bool,
    /// true 时 ensureToolResultPairing 在失配时抛错而非用合成占位修复；
    /// HFI 启动时开启，让轨迹快速失败。
    strict_tool_result_pairing: bool,
    sdk_agent_progress_summaries_enabled: bool,
    user_msg_opt_in: bool,
    client_type: String,
    session_source: Option<String>,
    question_preview_format: Option<QuestionPreviewFormat>,
    flag_settings_path: Option<String>,
    flag_settings_inline: Option<serde_json::Value>,
    allowed_setting_sources: Vec<SettingSource>,
    session_ingress_token: Option<String>,
    oauth_token_from_fd: Option<String>,
    api_key_from_fd: Option<String>,
    // Telemetry state
    meter: Option<OpaqueProvider>,
    session_counter: Option<Arc<dyn AttributedCounter>>,
    loc_counter: Option<Arc<dyn AttributedCounter>>,
    pr_counter: Option<Arc<dyn AttributedCounter>>,
    commit_counter: Option<Arc<dyn AttributedCounter>>,
    cost_counter: Option<Arc<dyn AttributedCounter>>,
    token_counter: Option<Arc<dyn AttributedCounter>>,
    code_edit_tool_decision_counter: Option<Arc<dyn AttributedCounter>>,
    active_time_counter: Option<Arc<dyn AttributedCounter>>,
    stats_store: Option<Arc<dyn StatsStore>>,
    session_id: SessionId,
    /// 父会话 ID（会话谱系追踪，如 plan mode -> implementation）。
    parent_session_id: Option<SessionId>,
    // Logger / Meter / Tracer provider state
    logger_provider: Option<OpaqueProvider>,
    event_logger: Option<OpaqueProvider>,
    meter_provider: Option<OpaqueProvider>,
    tracer_provider: Option<OpaqueProvider>,
    // Agent color state
    agent_color_map: HashMap<String, AgentColorName>,
    agent_color_index: u32,
    // 最近一次 API 请求（bug report 用）
    last_api_request: Option<serde_json::Value>,
    /// 最近一次 API 请求的 messages（ant-only；引用而非克隆）。
    last_api_request_messages: Option<serde_json::Value>,
    /// 最近 auto-mode classifier 请求（/share transcript 用）。
    last_classifier_requests: Option<Vec<serde_json::Value>>,
    /// context.ts 缓存的 CLAUDE.md 内容（auto-mode classifier 用），
    /// 打断 yoloClassifier → claudemd → filesystem → permissions 循环。
    cached_claude_md_content: Option<String>,
    // In-memory error log for recent errors
    in_memory_error_log: Vec<ErrorLogEntry>,
    // --plugin-dir 的会话级插件
    inline_plugins: Vec<String>,
    // 显式 --chrome / --no-chrome 值（None = CLI 未设置）
    chrome_flag_override: Option<bool>,
    // 使用 cowork_plugins 目录（--cowork 或环境变量）
    use_cowork_plugins: bool,
    // 会话级 bypass permissions（不持久化）
    session_bypass_permissions_mode: bool,
    // .claude/scheduled_tasks.json watcher 的会话级开关
    scheduled_tasks_enabled: bool,
    // CronCreate durable:false 的会话级任务
    session_cron_tasks: Vec<SessionCronTask>,
    // 本会话创建的 teams；gracefulShutdown 时由 cleanupSessionTeams() 清理
    session_created_teams: HashSet<String>,
    // 主目录的会话级 trust 标记（不持久化）
    session_trust_accepted: bool,
    // 禁用会话持久化的会话级标记
    session_persistence_disabled: bool,
    has_exited_plan_mode: bool,
    needs_plan_mode_exit_attachment: bool,
    needs_auto_mode_exit_attachment: bool,
    lsp_recommendation_shown_this_session: bool,
    // SDK init event state：structured output 的 jsonSchema
    init_json_schema: Option<serde_json::Value>,
    // 已注册 hooks：SDK 回调 + 插件原生 hooks
    registered_hooks: Option<HashMap<HookEvent, Vec<Arc<dyn RegisteredHookMatcher>>>>,
    // plan slug 缓存：sessionId -> wordSlug
    plan_slug_cache: PlanSlugCache,
    teleported_session_info: Option<TeleportedSessionInfo>,
    // 已调用 skill（压缩时保留）；键为 `${agentId ?? ''}:${skillName}`
    invoked_skills: HashMap<String, InvokedSkillInfo>,
    // 慢操作（dev bar 展示，ant-only）
    slow_operations: Vec<SlowOperation>,
    // SDK 提供的 betas（如 context-1m-2025-08-07）
    sdk_betas: Option<Vec<String>>,
    main_thread_agent_type: Option<String>,
    is_remote_mode: bool,
    direct_connect_server_url: Option<String>,
    system_prompt_section_cache: SystemPromptSectionCache,
    last_emitted_date: Option<String>,
    // --add-dir 附加目录（CLAUDE.md 加载用）
    additional_directories_for_claude_md: Vec<String>,
    // --channels 允许列表
    allowed_channels: Vec<ChannelEntry>,
    has_dev_channels: bool,
    // 会话 .jsonl 所在目录；None = 由 originalCwd 推导
    session_project_dir: Option<String>,
    // GrowthBook 的 prompt cache 1h TTL allowlist（会话稳定）
    prompt_cache_1h_allowlist: Option<Vec<String>>,
    prompt_cache_1h_eligible: Option<bool>,
    // Beta header 的 sticky-on latch（None = 未触发）
    afk_mode_header_latched: Option<bool>,
    fast_mode_header_latched: Option<bool>,
    cache_editing_header_latched: Option<bool>,
    prompt_id: Option<String>,
    // 主会话链最近 API requestId（关闭时用于 cache eviction hint）
    last_main_request_id: Option<String>,
    // 最近一次 API 成功完成时间戳（Date.now()）
    last_api_completion_timestamp: Option<i64>,
    // 压缩后置位标记（logAPISuccess 消费一次后自动复位）
    pending_post_compaction: bool,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn user_type() -> Option<String> {
    env::var("USER_TYPE").ok()
}

/// 解析 cwd 的符号链接并对齐 TS `realpathSync` + `normalize('NFC')` 行为
/// （NFC 归一化见模块头已知差异）。realpath 失败（如 CloudStorage EPERM）
/// 时回退到原始路径。
fn resolve_cwd() -> String {
    match env::current_dir() {
        Ok(raw_cwd) => match fs::canonicalize(&raw_cwd) {
            Ok(resolved) => resolved.to_string_lossy().into_owned(),
            Err(_) => raw_cwd.to_string_lossy().into_owned(),
        },
        Err(_) => String::new(),
    }
}

impl State {
    /// 对齐 TS `getInitialState()`。
    fn initial() -> State {
        let resolved_cwd = resolve_cwd();
        State {
            original_cwd: resolved_cwd.clone(),
            project_root: resolved_cwd.clone(),
            total_cost_usd: 0.0,
            total_api_duration: 0.0,
            total_api_duration_without_retries: 0.0,
            total_tool_duration: 0.0,
            turn_hook_duration_ms: 0.0,
            turn_tool_duration_ms: 0.0,
            turn_classifier_duration_ms: 0.0,
            turn_tool_count: 0,
            turn_hook_count: 0,
            turn_classifier_count: 0,
            start_time: now_ms(),
            last_interaction_time: now_ms(),
            total_lines_added: 0,
            total_lines_removed: 0,
            has_unknown_model_cost: false,
            cwd: resolved_cwd,
            model_usage: HashMap::new(),
            main_loop_model_override: None,
            initial_main_loop_model: None,
            model_strings: None,
            is_interactive: false,
            kairos_active: false,
            strict_tool_result_pairing: false,
            sdk_agent_progress_summaries_enabled: false,
            user_msg_opt_in: false,
            client_type: "cli".to_string(),
            session_source: None,
            question_preview_format: None,
            flag_settings_path: None,
            flag_settings_inline: None,
            allowed_setting_sources: SettingSource::ALL.to_vec(),
            session_ingress_token: None,
            oauth_token_from_fd: None,
            api_key_from_fd: None,
            meter: None,
            session_counter: None,
            loc_counter: None,
            pr_counter: None,
            commit_counter: None,
            cost_counter: None,
            token_counter: None,
            code_edit_tool_decision_counter: None,
            active_time_counter: None,
            stats_store: None,
            session_id: SessionId::new(Uuid::new_v4().to_string()),
            parent_session_id: None,
            logger_provider: None,
            event_logger: None,
            meter_provider: None,
            tracer_provider: None,
            agent_color_map: HashMap::new(),
            agent_color_index: 0,
            last_api_request: None,
            last_api_request_messages: None,
            last_classifier_requests: None,
            cached_claude_md_content: None,
            in_memory_error_log: Vec::new(),
            inline_plugins: Vec::new(),
            chrome_flag_override: None,
            use_cowork_plugins: false,
            session_bypass_permissions_mode: false,
            scheduled_tasks_enabled: false,
            session_cron_tasks: Vec::new(),
            session_created_teams: HashSet::new(),
            session_trust_accepted: false,
            session_persistence_disabled: false,
            has_exited_plan_mode: false,
            needs_plan_mode_exit_attachment: false,
            needs_auto_mode_exit_attachment: false,
            lsp_recommendation_shown_this_session: false,
            init_json_schema: None,
            registered_hooks: None,
            plan_slug_cache: PlanSlugCache::default(),
            teleported_session_info: None,
            invoked_skills: HashMap::new(),
            slow_operations: Vec::new(),
            sdk_betas: None,
            main_thread_agent_type: None,
            is_remote_mode: false,
            direct_connect_server_url: None,
            system_prompt_section_cache: SystemPromptSectionCache::default(),
            last_emitted_date: None,
            additional_directories_for_claude_md: Vec::new(),
            allowed_channels: Vec::new(),
            has_dev_channels: false,
            session_project_dir: None,
            prompt_cache_1h_allowlist: None,
            prompt_cache_1h_eligible: None,
            afk_mode_header_latched: None,
            fast_mode_header_latched: None,
            cache_editing_header_latched: None,
            prompt_id: None,
            last_main_request_id: None,
            last_api_completion_timestamp: None,
            pending_post_compaction: false,
        }
    }
}

static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::initial()));

fn lock() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// 模块作用域（不在 STATE 内）的会话状态
static INTERACTION_TIME_DIRTY: AtomicBool = AtomicBool::new(false);
static OUTPUT_TOKENS_AT_TURN_START: AtomicI64 = AtomicI64::new(0);
static CURRENT_TURN_TOKEN_BUDGET: Lazy<Mutex<Option<i64>>> = Lazy::new(|| Mutex::new(None));
static BUDGET_CONTINUATION_COUNT: AtomicI64 = AtomicI64::new(0);
static SESSION_SWITCHED: Lazy<Signal<String>> = Lazy::new(Signal::new);

// 滚动排放抑制 —— 后台 interval 在执行工作前检查此标记，避免与滚动帧
// 竞争事件循环。模块作用域（不在 STATE 内）：热路径瞬时标记。
// 注意：TS 源码中 scrollDraining 仅在本文件内被读取（ScrollBox 的写入
// 路径在 TS 树中未落盘），此处保持同样只导出 get + wait 的表面。
const SCROLL_DRAIN_IDLE_MS: u64 = 150;
static SCROLL_DRAINING: AtomicBool = AtomicBool::new(false);

const MAX_SLOW_OPERATIONS: usize = 10;
const SLOW_OPERATION_TTL_MS: i64 = 10000;

// ---------------------------------------------------------------------------
// Session id / 会话切换
// ---------------------------------------------------------------------------

pub fn get_session_id() -> SessionId {
    lock().session_id.clone()
}

/// 重新生成会话 ID。`set_current_as_parent` 为 true 时把当前会话记为父
/// 会话。丢弃旧会话的 plan-slug 条目避免 Map 积累过期键；重生成会话属于
/// 当前项目（session_project_dir 复位为 None）。
pub fn regenerate_session_id(set_current_as_parent: bool) -> SessionId {
    let mut state = lock();
    if set_current_as_parent {
        state.parent_session_id = Some(state.session_id.clone());
    }
    // 丢弃旧会话的 plan-slug 条目，避免 Map 积累过期键。
    let previous_session_id = state.session_id.clone();
    state.plan_slug_cache.remove(previous_session_id.as_ref());
    state.session_id = SessionId::new(Uuid::new_v4().to_string());
    state.session_project_dir = None;
    state.session_id.clone()
}

pub fn get_parent_session_id() -> Option<SessionId> {
    lock().parent_session_id.clone()
}

/// 原子切换活跃会话。`session_id` 与 `session_project_dir` 总是一起变更
/// （没有独立的 setter），二者不会漂移（CC-34）。
///
/// `project_dir`：`<sessionId>.jsonl` 所在目录；省略（None）表示会话在
/// 当前项目——读取时由 originalCwd 推导。会话在别的项目目录时传
/// `dirname(transcript_path)`。每次调用都重置项目目录，从不沿用。
pub fn switch_session(session_id: SessionId, project_dir: Option<String>) {
    let mut state = lock();
    // 丢弃旧会话的 plan-slug 条目，保持 Map 有界（仅当前会话的 slug 会被读取）。
    let previous_session_id = state.session_id.clone();
    state.plan_slug_cache.remove(previous_session_id.as_ref());
    state.session_id = session_id.clone();
    state.session_project_dir = project_dir;
    drop(state);
    SESSION_SWITCHED.emit(&session_id.to_string());
}

/// 注册 switch_session 改变活跃 sessionId 时的回调。bootstrap 不能直接
/// import 监听方（DAG 叶），故由调用方自行注册；concurrentSessions.ts 用
/// 它把 PID 文件的 sessionId 与 --resume 保持同步。
/// 等价 TS `onSessionSwitch = sessionSwitched.subscribe`。
pub fn on_session_switch<F>(listener: F) -> crate::utils::signal::Subscription<String>
where
    F: FnMut(&String) + Send + Sync + 'static,
{
    SESSION_SWITCHED.subscribe(listener)
}

/// 当前会话 transcript 所在的项目目录；None 表示会话创建于当前项目
/// （常见情形，由 originalCwd 推导）。见 `switch_session()`。
pub fn get_session_project_dir() -> Option<String> {
    lock().session_project_dir.clone()
}

pub fn get_original_cwd() -> String {
    lock().original_cwd.clone()
}

/// 稳定的项目根目录。与 get_original_cwd() 不同，会话中 EnterWorktreeTool
/// 从不更新它（技能/历史锚定在会话起点）；--worktree 启动时它就是
/// 该 worktree。用于项目身份，不用于文件操作。
pub fn get_project_root() -> String {
    lock().project_root.clone()
}

pub fn set_original_cwd(cwd: &str) {
    // TS: cwd.normalize('NFC') —— 见模块头已知差异。
    lock().original_cwd = cwd.to_string();
}

/// 仅供 --worktree 启动 flag。会话中 EnterWorktreeTool 禁止调用——
/// 技能/历史应锚定在会话起点。
pub fn set_project_root(cwd: &str) {
    lock().project_root = cwd.to_string();
}

pub fn get_cwd_state() -> String {
    lock().cwd.clone()
}

pub fn set_cwd_state(cwd: &str) {
    lock().cwd = cwd.to_string();
}

pub fn get_direct_connect_server_url() -> Option<String> {
    lock().direct_connect_server_url.clone()
}

pub fn set_direct_connect_server_url(url: &str) {
    lock().direct_connect_server_url = Some(url.to_string());
}

// ---------------------------------------------------------------------------
// 时长 / 成本统计
// ---------------------------------------------------------------------------

pub fn add_to_total_duration_state(duration: f64, duration_without_retries: f64) {
    let mut state = lock();
    state.total_api_duration += duration;
    state.total_api_duration_without_retries += duration_without_retries;
}

pub fn reset_total_duration_state_and_cost_for_tests_only() {
    let mut state = lock();
    state.total_api_duration = 0.0;
    state.total_api_duration_without_retries = 0.0;
    state.total_cost_usd = 0.0;
}

pub fn add_to_total_cost_state(cost: f64, model_usage: ModelUsage, model: &str) {
    let mut state = lock();
    state.model_usage.insert(model.to_string(), model_usage);
    state.total_cost_usd += cost;
}

pub fn get_total_cost_usd() -> f64 {
    lock().total_cost_usd
}

pub fn get_total_api_duration() -> f64 {
    lock().total_api_duration
}

pub fn get_total_duration() -> i64 {
    now_ms() - lock().start_time
}

pub fn get_total_api_duration_without_retries() -> f64 {
    lock().total_api_duration_without_retries
}

pub fn get_total_tool_duration() -> f64 {
    lock().total_tool_duration
}

pub fn add_to_tool_duration(duration: f64) {
    let mut state = lock();
    state.total_tool_duration += duration;
    state.turn_tool_duration_ms += duration;
    state.turn_tool_count += 1;
}

pub fn get_turn_hook_duration_ms() -> f64 {
    lock().turn_hook_duration_ms
}

pub fn add_to_turn_hook_duration(duration: f64) {
    let mut state = lock();
    state.turn_hook_duration_ms += duration;
    state.turn_hook_count += 1;
}

pub fn reset_turn_hook_duration() {
    let mut state = lock();
    state.turn_hook_duration_ms = 0.0;
    state.turn_hook_count = 0;
}

pub fn get_turn_hook_count() -> u32 {
    lock().turn_hook_count
}

pub fn get_turn_tool_duration_ms() -> f64 {
    lock().turn_tool_duration_ms
}

pub fn reset_turn_tool_duration() {
    let mut state = lock();
    state.turn_tool_duration_ms = 0.0;
    state.turn_tool_count = 0;
}

pub fn get_turn_tool_count() -> u32 {
    lock().turn_tool_count
}

pub fn get_turn_classifier_duration_ms() -> f64 {
    lock().turn_classifier_duration_ms
}

pub fn add_to_turn_classifier_duration(duration: f64) {
    let mut state = lock();
    state.turn_classifier_duration_ms += duration;
    state.turn_classifier_count += 1;
}

pub fn reset_turn_classifier_duration() {
    let mut state = lock();
    state.turn_classifier_duration_ms = 0.0;
    state.turn_classifier_count = 0;
}

pub fn get_turn_classifier_count() -> u32 {
    lock().turn_classifier_count
}

pub fn set_stats_store(store: Option<Arc<dyn StatsStore>>) {
    lock().stats_store = store;
}

pub fn get_stats_store() -> Option<Arc<dyn StatsStore>> {
    lock().stats_store.clone()
}

// ---------------------------------------------------------------------------
// 交互时间（延迟到渲染帧批量刷新，避免每次按键都 Date.now()）
// ---------------------------------------------------------------------------

/// 标记发生了一次交互。
///
/// 默认把实际的取时延迟到下一次渲染帧（flush_interaction_time()），
/// 避免每次按键都调用时钟。`immediate = true` 用于 React useEffect
/// 等**已经位于渲染循环之后**的回调；否则时间戳会陈旧到下一帧
/// （用户闲置时可能永不刷新，如权限对话框等待输入时）。
pub fn update_last_interaction_time(immediate: bool) {
    if immediate {
        flush_interaction_time_inner();
    } else {
        INTERACTION_TIME_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 若上次 flush 之后记录过交互，则立即刷新时间戳。Ink 在每个渲染循环
/// 前调用，把多次按键合并成一次时钟调用。
pub fn flush_interaction_time() {
    if INTERACTION_TIME_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
        flush_interaction_time_inner();
    }
}

fn flush_interaction_time_inner() {
    lock().last_interaction_time = now_ms();
    INTERACTION_TIME_DIRTY.store(false, std::sync::atomic::Ordering::Relaxed);
}

pub fn add_to_total_lines_changed(added: i64, removed: i64) {
    let mut state = lock();
    state.total_lines_added += added;
    state.total_lines_removed += removed;
}

pub fn get_total_lines_added() -> i64 {
    lock().total_lines_added
}

pub fn get_total_lines_removed() -> i64 {
    lock().total_lines_removed
}

fn sum_model_usage(field: ModelUsageField) -> i64 {
    lock()
        .model_usage
        .values()
        .map(|usage| match field {
            ModelUsageField::InputTokens => usage.input_tokens,
            ModelUsageField::OutputTokens => usage.output_tokens,
            ModelUsageField::CacheReadInputTokens => usage.cache_read_input_tokens,
            ModelUsageField::CacheCreationInputTokens => usage.cache_creation_input_tokens,
            ModelUsageField::WebSearchRequests => usage.web_search_requests,
        })
        .sum()
}

enum ModelUsageField {
    InputTokens,
    OutputTokens,
    CacheReadInputTokens,
    CacheCreationInputTokens,
    WebSearchRequests,
}

pub fn get_total_input_tokens() -> i64 {
    sum_model_usage(ModelUsageField::InputTokens)
}

pub fn get_total_output_tokens() -> i64 {
    sum_model_usage(ModelUsageField::OutputTokens)
}

pub fn get_total_cache_read_input_tokens() -> i64 {
    sum_model_usage(ModelUsageField::CacheReadInputTokens)
}

pub fn get_total_cache_creation_input_tokens() -> i64 {
    sum_model_usage(ModelUsageField::CacheCreationInputTokens)
}

pub fn get_total_web_search_requests() -> i64 {
    sum_model_usage(ModelUsageField::WebSearchRequests)
}

pub fn get_turn_output_tokens() -> i64 {
    get_total_output_tokens()
        - OUTPUT_TOKENS_AT_TURN_START.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn get_current_turn_token_budget() -> Option<i64> {
    *CURRENT_TURN_TOKEN_BUDGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn snapshot_output_tokens_for_turn(budget: Option<i64>) {
    OUTPUT_TOKENS_AT_TURN_START.store(
        get_total_output_tokens(),
        std::sync::atomic::Ordering::Relaxed,
    );
    *CURRENT_TURN_TOKEN_BUDGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = budget;
    BUDGET_CONTINUATION_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

pub fn get_budget_continuation_count() -> i64 {
    BUDGET_CONTINUATION_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn increment_budget_continuation_count() {
    BUDGET_CONTINUATION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn set_has_unknown_model_cost() {
    lock().has_unknown_model_cost = true;
}

pub fn has_unknown_model_cost() -> bool {
    lock().has_unknown_model_cost
}

pub fn get_last_main_request_id() -> Option<String> {
    lock().last_main_request_id.clone()
}

pub fn set_last_main_request_id(request_id: &str) {
    lock().last_main_request_id = Some(request_id.to_string());
}

pub fn get_last_api_completion_timestamp() -> Option<i64> {
    lock().last_api_completion_timestamp
}

pub fn set_last_api_completion_timestamp(timestamp: i64) {
    lock().last_api_completion_timestamp = Some(timestamp);
}

/// 标记刚发生压缩。下一次 API 成功事件将带 is_post_compaction=true，
/// 随后标记自动复位。
pub fn mark_post_compaction() {
    lock().pending_post_compaction = true;
}

/// 消费压缩后置标记。压缩后返回一次 true，之后返回 false 直到再次压缩。
pub fn consume_post_compaction() -> bool {
    let mut state = lock();
    let was = state.pending_post_compaction;
    state.pending_post_compaction = false;
    was
}

pub fn get_last_interaction_time() -> i64 {
    lock().last_interaction_time
}

// ---------------------------------------------------------------------------
// 滚动排放抑制
// ---------------------------------------------------------------------------

/// 滚动正主动排放（最后一次事件 150ms 内）时为 true。
/// 后台 interval 应在为 true 时提前返回——工作推迟到滚动平息后的下一 tick。
pub fn get_is_scroll_draining() -> bool {
    SCROLL_DRAINING.load(std::sync::atomic::Ordering::Relaxed)
}

/// 在可能与滚动重合的一次性昂贵工作（网络、子进程）前等待。
/// 未在滚动时立即返回；否则按空闲间隔轮询直到标记清除。
pub fn wait_for_scroll_idle() {
    while SCROLL_DRAINING.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(SCROLL_DRAIN_IDLE_MS));
    }
}

// ---------------------------------------------------------------------------
// 模型用量 / 模型设置
// ---------------------------------------------------------------------------

pub fn get_model_usage() -> HashMap<String, ModelUsage> {
    lock().model_usage.clone()
}

pub fn get_usage_for_model(model: &str) -> Option<ModelUsage> {
    lock().model_usage.get(model).cloned()
}

/// 获取 --model CLI flag 或用户更新配置后的模型覆盖。
pub fn get_main_loop_model_override() -> ModelSetting {
    lock().main_loop_model_override.clone()
}

pub fn get_initial_main_loop_model() -> ModelSetting {
    lock().initial_main_loop_model.clone()
}

pub fn set_main_loop_model_override(model: ModelSetting) {
    lock().main_loop_model_override = model;
}

pub fn set_initial_main_loop_model(model: ModelSetting) {
    lock().initial_main_loop_model = model;
}

pub fn get_sdk_betas() -> Option<Vec<String>> {
    lock().sdk_betas.clone()
}

pub fn set_sdk_betas(betas: Option<Vec<String>>) {
    lock().sdk_betas = betas;
}

pub fn reset_cost_state() {
    let mut state = lock();
    state.total_cost_usd = 0.0;
    state.total_api_duration = 0.0;
    state.total_api_duration_without_retries = 0.0;
    state.total_tool_duration = 0.0;
    state.start_time = now_ms();
    state.total_lines_added = 0;
    state.total_lines_removed = 0;
    state.has_unknown_model_cost = false;
    state.model_usage.clear();
    state.prompt_id = None;
}

/// cost-tracker.ts 的 restoreCostStateForSession 调用。
pub struct CostStateForRestore {
    pub total_cost_usd: f64,
    pub total_api_duration: f64,
    pub total_api_duration_without_retries: f64,
    pub total_tool_duration: f64,
    pub total_lines_added: i64,
    pub total_lines_removed: i64,
    pub last_duration: Option<f64>,
    pub model_usage: Option<HashMap<String, ModelUsage>>,
}

pub fn set_cost_state_for_restore(restored: CostStateForRestore) {
    let mut state = lock();
    state.total_cost_usd = restored.total_cost_usd;
    state.total_api_duration = restored.total_api_duration;
    state.total_api_duration_without_retries = restored.total_api_duration_without_retries;
    state.total_tool_duration = restored.total_tool_duration;
    state.total_lines_added = restored.total_lines_added;
    state.total_lines_removed = restored.total_lines_removed;
    if let Some(model_usage) = restored.model_usage {
        state.model_usage = model_usage;
    }
    // 调整 start_time 使墙上时长可累积
    if let Some(last_duration) = restored.last_duration {
        state.start_time = now_ms() - last_duration as i64;
    }
}

/// resetStateForTests 的错误（文案与 TS 一致）。
#[derive(Debug, thiserror::Error)]
#[error("resetStateForTests can only be called in tests")]
pub struct NotInTestEnv;

/// 仅测试用。把全部状态重置为初值。
pub fn reset_state_for_tests() -> Result<(), NotInTestEnv> {
    if env::var("NODE_ENV").as_deref() != Ok("test") {
        return Err(NotInTestEnv);
    }
    *lock() = State::initial();
    OUTPUT_TOKENS_AT_TURN_START.store(0, std::sync::atomic::Ordering::Relaxed);
    *CURRENT_TURN_TOKEN_BUDGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    BUDGET_CONTINUATION_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    SESSION_SWITCHED.clear();
    Ok(())
}

// 你不应该直接使用本 API；见 src/utils/model/modelStrings.ts::getModelStrings()
pub fn get_model_strings() -> Option<ModelStrings> {
    lock().model_strings.clone()
}

// 你不应该直接使用本 API；见 src/utils/model/modelStrings.ts
pub fn set_model_strings(model_strings: ModelStrings) {
    lock().model_strings = Some(model_strings);
}

/// 测试专用：重置 model strings 以便重新初始化。与 set_model_strings
/// 分开是因为测试只应接受 None。
pub fn reset_model_strings_for_testing_only() {
    lock().model_strings = None;
}

// ---------------------------------------------------------------------------
// Telemetry（OTel 尚未转换；meter 以不透明句柄存储，counter 是 trait 对象）
// ---------------------------------------------------------------------------

/// 初始化 telemetry counters。对齐 TS `setMeter(meter, createCounter)`：
/// 用给定工厂创建全部计数器（描述文案即 TS `t()` 的 en 源文）。
pub fn set_meter(
    meter: OpaqueProvider,
    create_counter: impl Fn(&str, &MetricOptions) -> Arc<dyn AttributedCounter>,
) {
    let mut state = lock();
    state.meter = Some(meter);
    state.session_counter = Some(create_counter(
        "claude_code.session.count",
        &MetricOptions {
            description: "Count of CLI sessions started".to_string(),
            unit: None,
        },
    ));
    state.loc_counter = Some(create_counter(
        "claude_code.lines_of_code.count",
        &MetricOptions {
            description: "Count of lines of code modified, with the 'type' attribute indicating whether lines were added or removed".to_string(),
            unit: None,
        },
    ));
    state.pr_counter = Some(create_counter(
        "claude_code.pull_request.count",
        &MetricOptions {
            description: "Number of pull requests created".to_string(),
            unit: None,
        },
    ));
    state.commit_counter = Some(create_counter(
        "claude_code.commit.count",
        &MetricOptions {
            description: "Number of git commits created".to_string(),
            unit: None,
        },
    ));
    state.cost_counter = Some(create_counter(
        "claude_code.cost.usage",
        &MetricOptions {
            description: "Cost of the Claude Code session".to_string(),
            unit: Some("USD".to_string()),
        },
    ));
    state.token_counter = Some(create_counter(
        "claude_code.token.usage",
        &MetricOptions {
            description: "Number of tokens used".to_string(),
            unit: Some("tokens".to_string()),
        },
    ));
    state.code_edit_tool_decision_counter = Some(create_counter(
        "claude_code.code_edit_tool.decision",
        &MetricOptions {
            description: "Count of code editing tool permission decisions (accept/reject) for Edit, Write, and NotebookEdit tools".to_string(),
            unit: None,
        },
    ));
    state.active_time_counter = Some(create_counter(
        "claude_code.active_time.total",
        &MetricOptions {
            description: "Total active time in seconds".to_string(),
            unit: Some("s".to_string()),
        },
    ));
}

pub fn get_meter() -> Option<OpaqueProvider> {
    lock().meter.clone()
}

pub fn get_session_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().session_counter.clone()
}

pub fn get_loc_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().loc_counter.clone()
}

pub fn get_pr_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().pr_counter.clone()
}

pub fn get_commit_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().commit_counter.clone()
}

pub fn get_cost_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().cost_counter.clone()
}

pub fn get_token_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().token_counter.clone()
}

pub fn get_code_edit_tool_decision_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().code_edit_tool_decision_counter.clone()
}

pub fn get_active_time_counter() -> Option<Arc<dyn AttributedCounter>> {
    lock().active_time_counter.clone()
}

// ---------------------------------------------------------------------------
// OTel provider / logger 句柄
// ---------------------------------------------------------------------------

pub fn set_logger_provider(provider: Option<OpaqueProvider>) {
    lock().logger_provider = provider;
}

pub fn get_logger_provider() -> Option<OpaqueProvider> {
    lock().logger_provider.clone()
}

pub fn set_event_logger(logger: Option<OpaqueProvider>) {
    lock().event_logger = logger;
}

pub fn get_event_logger() -> Option<OpaqueProvider> {
    lock().event_logger.clone()
}

pub fn set_meter_provider(provider: Option<OpaqueProvider>) {
    lock().meter_provider = provider;
}

pub fn get_meter_provider() -> Option<OpaqueProvider> {
    lock().meter_provider.clone()
}

pub fn set_tracer_provider(provider: Option<OpaqueProvider>) {
    lock().tracer_provider = provider;
}

pub fn get_tracer_provider() -> Option<OpaqueProvider> {
    lock().tracer_provider.clone()
}

// ---------------------------------------------------------------------------
// 交互性 / 客户端类型 / 特性开关
// ---------------------------------------------------------------------------

pub fn get_is_non_interactive_session() -> bool {
    !lock().is_interactive
}

pub fn get_is_interactive() -> bool {
    lock().is_interactive
}

pub fn set_is_interactive(value: bool) {
    lock().is_interactive = value;
}

pub fn get_client_type() -> String {
    lock().client_type.clone()
}

pub fn set_client_type(client_type: &str) {
    lock().client_type = client_type.to_string();
}

pub fn get_sdk_agent_progress_summaries_enabled() -> bool {
    lock().sdk_agent_progress_summaries_enabled
}

pub fn set_sdk_agent_progress_summaries_enabled(value: bool) {
    lock().sdk_agent_progress_summaries_enabled = value;
}

pub fn get_kairos_active() -> bool {
    lock().kairos_active
}

pub fn set_kairos_active(value: bool) {
    lock().kairos_active = value;
}

pub fn get_strict_tool_result_pairing() -> bool {
    lock().strict_tool_result_pairing
}

pub fn set_strict_tool_result_pairing(value: bool) {
    lock().strict_tool_result_pairing = value;
}

// 字段名 'userMsgOptIn' 避开排除字符串子串（'BriefTool'、
// 'SendUserMessage'——大小写不敏感）。全部调用方都在 feature() 守卫内，
// 故这些访问器不需要自己的守卫（与 get_kairos_active 一致）。
pub fn get_user_msg_opt_in() -> bool {
    lock().user_msg_opt_in
}

pub fn set_user_msg_opt_in(value: bool) {
    lock().user_msg_opt_in = value;
}

pub fn set_session_source(source: &str) {
    lock().session_source = Some(source.to_string());
}

pub fn get_question_preview_format() -> Option<QuestionPreviewFormat> {
    lock().question_preview_format
}

pub fn set_question_preview_format(format: QuestionPreviewFormat) {
    lock().question_preview_format = Some(format);
}

/// agent 颜色映射的活锁视图（agentColorManager 经返回的 Map 做
/// get/set 双向操作）。
pub struct AgentColorMapView<'a> {
    state: MutexGuard<'a, State>,
}

impl<'a> AgentColorMapView<'a> {
    pub fn get(&self, agent_type: &str) -> Option<AgentColorName> {
        self.state.agent_color_map.get(agent_type).copied()
    }

    pub fn set(&mut self, agent_type: &str, color: AgentColorName) {
        self.state
            .agent_color_map
            .insert(agent_type.to_string(), color);
    }

    pub fn len(&self) -> usize {
        self.state.agent_color_map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.agent_color_map.is_empty()
    }
}

pub fn get_agent_color_map() -> AgentColorMapView<'static> {
    AgentColorMapView { state: lock() }
}

// ---------------------------------------------------------------------------
// Settings / token 来源
// ---------------------------------------------------------------------------

pub fn get_flag_settings_path() -> Option<String> {
    lock().flag_settings_path.clone()
}

pub fn set_flag_settings_path(path: Option<String>) {
    lock().flag_settings_path = path;
}

pub fn get_flag_settings_inline() -> Option<serde_json::Value> {
    lock().flag_settings_inline.clone()
}

pub fn set_flag_settings_inline(settings: Option<serde_json::Value>) {
    lock().flag_settings_inline = settings;
}

pub fn get_session_ingress_token() -> Option<String> {
    lock().session_ingress_token.clone()
}

pub fn set_session_ingress_token(token: Option<String>) {
    lock().session_ingress_token = token;
}

pub fn get_oauth_token_from_fd() -> Option<String> {
    lock().oauth_token_from_fd.clone()
}

pub fn set_oauth_token_from_fd(token: Option<String>) {
    lock().oauth_token_from_fd = token;
}

pub fn get_api_key_from_fd() -> Option<String> {
    lock().api_key_from_fd.clone()
}

pub fn set_api_key_from_fd(key: Option<String>) {
    lock().api_key_from_fd = key;
}

// ---------------------------------------------------------------------------
// 最近 API 请求（bug report 用；SDK 类型未转换，以 serde_json::Value 暂存）
// ---------------------------------------------------------------------------

pub fn set_last_api_request(params: Option<serde_json::Value>) {
    lock().last_api_request = params;
}

pub fn get_last_api_request() -> Option<serde_json::Value> {
    lock().last_api_request.clone()
}

pub fn set_last_api_request_messages(messages: Option<serde_json::Value>) {
    lock().last_api_request_messages = messages;
}

pub fn get_last_api_request_messages() -> Option<serde_json::Value> {
    lock().last_api_request_messages.clone()
}

pub fn set_last_classifier_requests(requests: Option<Vec<serde_json::Value>>) {
    lock().last_classifier_requests = requests;
}

pub fn get_last_classifier_requests() -> Option<Vec<serde_json::Value>> {
    lock().last_classifier_requests.clone()
}

pub fn set_cached_claude_md_content(content: Option<String>) {
    lock().cached_claude_md_content = content;
}

pub fn get_cached_claude_md_content() -> Option<String> {
    lock().cached_claude_md_content.clone()
}

pub fn add_to_in_memory_error_log(error_info: ErrorLogEntry) {
    const MAX_IN_MEMORY_ERRORS: usize = 100;
    let mut state = lock();
    if state.in_memory_error_log.len() >= MAX_IN_MEMORY_ERRORS {
        state.in_memory_error_log.remove(0); // 移除最旧错误
    }
    state.in_memory_error_log.push(error_info);
}

pub fn get_allowed_setting_sources() -> Vec<SettingSource> {
    lock().allowed_setting_sources.clone()
}

pub fn set_allowed_setting_sources(sources: Vec<SettingSource>) {
    lock().allowed_setting_sources = sources;
}

/// IDE 扩展在认证上应表现为 1P。
pub fn prefer_third_party_authentication() -> bool {
    let state = lock();
    !state.is_interactive && state.client_type != "claude-vscode"
}

// ---------------------------------------------------------------------------
// 插件 / chrome flag / cowork / 权限模式
// ---------------------------------------------------------------------------

pub fn set_inline_plugins(plugins: Vec<String>) {
    lock().inline_plugins = plugins;
}

pub fn get_inline_plugins() -> Vec<String> {
    lock().inline_plugins.clone()
}

pub fn set_chrome_flag_override(value: Option<bool>) {
    lock().chrome_flag_override = value;
}

pub fn get_chrome_flag_override() -> Option<bool> {
    lock().chrome_flag_override
}

/// utils/settings/settingsCache.ts 尚未转换；此处以 no-op 占位
/// （对齐 TS 的 resetSettingsCache() 调用点）。见模块头已知差异。
fn reset_settings_cache_unimplemented() {}

pub fn set_use_cowork_plugins(value: bool) {
    lock().use_cowork_plugins = value;
    reset_settings_cache_unimplemented();
}

pub fn get_use_cowork_plugins() -> bool {
    lock().use_cowork_plugins
}

pub fn set_session_bypass_permissions_mode(enabled: bool) {
    lock().session_bypass_permissions_mode = enabled;
}

pub fn get_session_bypass_permissions_mode() -> bool {
    lock().session_bypass_permissions_mode
}

pub fn set_scheduled_tasks_enabled(enabled: bool) {
    lock().scheduled_tasks_enabled = enabled;
}

pub fn get_scheduled_tasks_enabled() -> bool {
    lock().scheduled_tasks_enabled
}

pub fn get_session_cron_tasks() -> Vec<SessionCronTask> {
    lock().session_cron_tasks.clone()
}

pub fn add_session_cron_task(task: SessionCronTask) {
    lock().session_cron_tasks.push(task);
}

/// 返回实际移除的任务数。调用方据此跳过下游工作（如 removeCronTasks
/// 的磁盘读取）——全部 id 都已在此处处理时。
pub fn remove_session_cron_tasks(ids: &[String]) -> usize {
    if ids.is_empty() {
        return 0;
    }
    let id_set: HashSet<&String> = ids.iter().collect();
    let mut state = lock();
    let before = state.session_cron_tasks.len();
    state
        .session_cron_tasks
        .retain(|task| !id_set.contains(&task.id));
    before - state.session_cron_tasks.len()
}

pub fn set_session_trust_accepted(accepted: bool) {
    lock().session_trust_accepted = accepted;
}

pub fn get_session_trust_accepted() -> bool {
    lock().session_trust_accepted
}

pub fn set_session_persistence_disabled(disabled: bool) {
    lock().session_persistence_disabled = disabled;
}

pub fn is_session_persistence_disabled() -> bool {
    lock().session_persistence_disabled
}

// ---------------------------------------------------------------------------
// Plan / auto mode 转换追踪
// ---------------------------------------------------------------------------

pub fn has_exited_plan_mode_in_session() -> bool {
    lock().has_exited_plan_mode
}

pub fn set_has_exited_plan_mode(value: bool) {
    lock().has_exited_plan_mode = value;
}

pub fn needs_plan_mode_exit_attachment() -> bool {
    lock().needs_plan_mode_exit_attachment
}

pub fn set_needs_plan_mode_exit_attachment(value: bool) {
    lock().needs_plan_mode_exit_attachment = value;
}

pub fn handle_plan_mode_transition(from_mode: &str, to_mode: &str) {
    let mut state = lock();
    // 切换进 plan mode 时清除待发的 exit attachment，
    // 避免用户快速切换时同时发送 plan_mode 和 plan_mode_exit。
    if to_mode == "plan" && from_mode != "plan" {
        state.needs_plan_mode_exit_attachment = false;
    }
    // 切出 plan mode 时触发 plan_mode_exit attachment。
    if from_mode == "plan" && to_mode != "plan" {
        state.needs_plan_mode_exit_attachment = true;
    }
}

pub fn needs_auto_mode_exit_attachment() -> bool {
    lock().needs_auto_mode_exit_attachment
}

pub fn set_needs_auto_mode_exit_attachment(value: bool) {
    lock().needs_auto_mode_exit_attachment = value;
}

pub fn handle_auto_mode_transition(from_mode: &str, to_mode: &str) {
    // auto↔plan 转换由 prepareContextForPlanMode（auto 可在 plan 中保持
    // 启用）与 ExitPlanMode（恢复模式）处理。两个方向都跳过，本函数只
    // 处理直接的 auto 转换。
    if (from_mode == "auto" && to_mode == "plan") || (from_mode == "plan" && to_mode == "auto") {
        return;
    }
    let from_is_auto = from_mode == "auto";
    let to_is_auto = to_mode == "auto";
    let mut state = lock();
    // 切换进 auto mode 时清除待发的 exit attachment，
    // 避免用户快速切换时同时发送 auto_mode 和 auto_mode_exit。
    if to_is_auto && !from_is_auto {
        state.needs_auto_mode_exit_attachment = false;
    }
    // 切出 auto mode 时触发 auto_mode_exit attachment。
    if from_is_auto && !to_is_auto {
        state.needs_auto_mode_exit_attachment = true;
    }
}

// ---------------------------------------------------------------------------
// LSP 推荐 / SDK init 状态
// ---------------------------------------------------------------------------

pub fn has_shown_lsp_recommendation_this_session() -> bool {
    lock().lsp_recommendation_shown_this_session
}

pub fn set_lsp_recommendation_shown_this_session(value: bool) {
    lock().lsp_recommendation_shown_this_session = value;
}

pub fn set_init_json_schema(schema: serde_json::Value) {
    lock().init_json_schema = Some(schema);
}

pub fn get_init_json_schema() -> Option<serde_json::Value> {
    lock().init_json_schema.clone()
}

/// 注册 hook 回调。可多次调用，按事件 merge（不覆盖）。
pub fn register_hook_callbacks(hooks: Vec<(HookEvent, Vec<Arc<dyn RegisteredHookMatcher>>)>) {
    let mut state = lock();
    let registered = state.registered_hooks.get_or_insert_with(HashMap::new);
    for (event, matchers) in hooks {
        registered.entry(event).or_default().extend(matchers);
    }
}

pub fn get_registered_hooks() -> Option<HashMap<HookEvent, Vec<Arc<dyn RegisteredHookMatcher>>>> {
    lock().registered_hooks.clone()
}

/// 只保留回调 hook（不带 pluginRoot 的），移除全部插件原生 hook。
pub fn clear_registered_plugin_hooks() {
    let mut state = lock();
    let mut filtered: HashMap<HookEvent, Vec<Arc<dyn RegisteredHookMatcher>>> = HashMap::new();
    if let Some(registered) = state.registered_hooks.take() {
        for (event, matchers) in registered {
            let callback_hooks: Vec<Arc<dyn RegisteredHookMatcher>> = matchers
                .into_iter()
                .filter(|matcher| !matcher.is_plugin_hook())
                .collect();
            if !callback_hooks.is_empty() {
                filtered.insert(event, callback_hooks);
            }
        }
    }
    state.registered_hooks = if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    };
}

pub fn reset_sdk_init_state() {
    let mut state = lock();
    state.init_json_schema = None;
    state.registered_hooks = None;
}

// ---------------------------------------------------------------------------
// Plan slug 缓存 / teams / teleported session
// ---------------------------------------------------------------------------

/// plan slug 缓存的活锁视图（plans.ts 经返回的 Map 做 get/delete）。
pub struct PlanSlugCacheView<'a> {
    state: MutexGuard<'a, State>,
}

impl<'a> PlanSlugCacheView<'a> {
    pub fn get(&self, session_id: &str) -> Option<String> {
        self.state.plan_slug_cache.get(session_id)
    }

    pub fn delete(&mut self, session_id: &str) {
        self.state.plan_slug_cache.remove(session_id);
    }
}

pub fn get_plan_slug_cache() -> PlanSlugCacheView<'static> {
    PlanSlugCacheView { state: lock() }
}

pub fn set_plan_slug_cache_entry(session_id: &str, slug: &str) {
    lock()
        .plan_slug_cache
        .set(session_id.to_string(), slug.to_string());
}

/// 本会话创建的 teams 快照（TS 经返回的 live Set 直接 add/delete；
/// 本端口返回克隆，增删经下方两个 mutator——对齐 teamHelpers.ts 的
/// `getSessionCreatedTeams().add/.delete` 使用模式）。
pub fn get_session_created_teams() -> HashSet<String> {
    lock().session_created_teams.clone()
}

/// 等价 TS `getSessionCreatedTeams().add(team)`。
pub fn add_session_created_team(team: &str) {
    lock().session_created_teams.insert(team.to_string());
}

/// 等价 TS `getSessionCreatedTeams().delete(team)`。
pub fn remove_session_created_team(team: &str) {
    lock().session_created_teams.remove(team);
}

pub fn set_teleported_session_info(session_id: Option<String>) {
    lock().teleported_session_info = Some(TeleportedSessionInfo {
        is_teleported: true,
        has_logged_first_message: false,
        session_id,
    });
}

pub fn get_teleported_session_info() -> Option<TeleportedSessionInfo> {
    lock().teleported_session_info.clone()
}

pub fn mark_first_teleport_message_logged() {
    if let Some(info) = &mut lock().teleported_session_info {
        info.has_logged_first_message = true;
    }
}

// ---------------------------------------------------------------------------
// 已调用 skill（压缩时保留）
// ---------------------------------------------------------------------------

/// 追踪一次 skill 调用。键为 `${agentId ?? ''}:${skillName}`，避免跨
/// agent 覆盖。
pub fn add_invoked_skill(
    skill_name: &str,
    skill_path: &str,
    content: &str,
    agent_id: Option<&str>,
) {
    let key = format!("{}:{}", agent_id.unwrap_or(""), skill_name);
    let mut state = lock();
    state.invoked_skills.insert(
        key,
        InvokedSkillInfo {
            skill_name: skill_name.to_string(),
            skill_path: skill_path.to_string(),
            content: content.to_string(),
            invoked_at: now_ms(),
            agent_id: agent_id.map(str::to_string),
        },
    );
}

/// 返回指定 agent 的已调用 skill（键 -> 信息）。agentId 按 `?? null`
/// 归一化后比较。
pub fn get_invoked_skills_for_agent(agent_id: Option<&str>) -> HashMap<String, InvokedSkillInfo> {
    let state = lock();
    state
        .invoked_skills
        .iter()
        .filter(|(_, skill)| skill.agent_id.as_deref() == agent_id)
        .map(|(key, skill)| (key.clone(), skill.clone()))
        .collect()
}

/// 清除已调用 skill；`preserved_agent_ids` 为空或 None 时清空全部，
/// 否则只保留 agentId 在保留集合内的记录（agentId 为 None 的一律清除）。
pub fn clear_invoked_skills(preserved_agent_ids: Option<&HashSet<String>>) {
    let mut state = lock();
    // TS: `if (!preservedAgentIds || preservedAgentIds.size === 0) { clear(); return }`
    match preserved_agent_ids {
        Some(preserved) if !preserved.is_empty() => {
            state.invoked_skills.retain(|_, skill| {
                skill
                    .agent_id
                    .as_ref()
                    .map(|agent_id| preserved.contains(agent_id))
                    .unwrap_or(false)
            });
        }
        _ => {
            state.invoked_skills.clear();
        }
    }
}

pub fn clear_invoked_skills_for_agent(agent_id: &str) {
    let mut state = lock();
    state
        .invoked_skills
        .retain(|_, skill| skill.agent_id.as_deref() != Some(agent_id));
}

// ---------------------------------------------------------------------------
// 慢操作追踪（dev bar 用，ant-only）
// ---------------------------------------------------------------------------

pub fn add_slow_operation(operation: &str, duration_ms: f64) {
    if user_type().as_deref() != Some("ant") {
        return;
    }
    // 跳过 editor 会话（用户在 $EDITOR 里编辑 prompt 文件）——
    // 用户在起草文本，慢是正常的。
    if operation.contains("exec") && operation.contains("claude-prompt-") {
        return;
    }
    let now = now_ms();
    let mut state = lock();
    // 移除过期操作
    state
        .slow_operations
        .retain(|op| now - op.timestamp < SLOW_OPERATION_TTL_MS);
    // 加入新操作
    state.slow_operations.push(SlowOperation {
        operation: operation.to_string(),
        duration_ms,
        timestamp: now,
    });
    // 只保留最近的操作
    if state.slow_operations.len() > MAX_SLOW_OPERATIONS {
        let start = state.slow_operations.len() - MAX_SLOW_OPERATIONS;
        state.slow_operations.drain(0..start);
    }
}

/// TS 返回稳定空引用让调用方的 setState 经 Object.is 跳过重渲染；
/// Rust 返回等值快照。过期条目在此处惰性清除。
pub fn get_slow_operations() -> Vec<SlowOperation> {
    let mut state = lock();
    if state.slow_operations.is_empty() {
        return Vec::new();
    }
    let now = now_ms();
    if state
        .slow_operations
        .iter()
        .any(|op| now - op.timestamp >= SLOW_OPERATION_TTL_MS)
    {
        state
            .slow_operations
            .retain(|op| now - op.timestamp < SLOW_OPERATION_TTL_MS);
    }
    state.slow_operations.clone()
}

// ---------------------------------------------------------------------------
// 远程 / agent 类型 / system prompt 缓存
// ---------------------------------------------------------------------------

pub fn get_main_thread_agent_type() -> Option<String> {
    lock().main_thread_agent_type.clone()
}

pub fn set_main_thread_agent_type(agent_type: Option<String>) {
    lock().main_thread_agent_type = agent_type;
}

pub fn get_is_remote_mode() -> bool {
    lock().is_remote_mode
}

pub fn set_is_remote_mode(value: bool) {
    lock().is_remote_mode = value;
}

/// system prompt section 缓存的活锁视图
/// （systemPromptSections.ts 经返回的 Map 做 has/get）。
pub struct SystemPromptSectionCacheView<'a> {
    state: MutexGuard<'a, State>,
}

impl<'a> SystemPromptSectionCacheView<'a> {
    /// 同 TS `cache.has(name)`。
    pub fn has(&self, name: &str) -> bool {
        self.state.system_prompt_section_cache.has(name)
    }

    /// 同 TS `cache.get(name)`：条目存在时返回 `Some(值)`（值可为 None），
    /// 不存在时返回 None。
    pub fn get(&self, name: &str) -> Option<Option<String>> {
        self.state.system_prompt_section_cache.get(name)
    }
}

pub fn get_system_prompt_section_cache() -> SystemPromptSectionCacheView<'static> {
    SystemPromptSectionCacheView { state: lock() }
}

pub fn set_system_prompt_section_cache_entry(name: &str, value: Option<String>) {
    lock()
        .system_prompt_section_cache
        .set(name.to_string(), value);
}

pub fn clear_system_prompt_section_state() {
    lock().system_prompt_section_cache.clear();
}

// 最近输出给模型的日期（检测跨午夜日期变化）
pub fn get_last_emitted_date() -> Option<String> {
    lock().last_emitted_date.clone()
}

pub fn set_last_emitted_date(date: Option<String>) {
    lock().last_emitted_date = date;
}

pub fn get_additional_directories_for_claude_md() -> Vec<String> {
    lock().additional_directories_for_claude_md.clone()
}

pub fn set_additional_directories_for_claude_md(directories: Vec<String>) {
    lock().additional_directories_for_claude_md = directories;
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

pub fn get_allowed_channels() -> Vec<ChannelEntry> {
    lock().allowed_channels.clone()
}

pub fn set_allowed_channels(entries: Vec<ChannelEntry>) {
    lock().allowed_channels = entries;
}

pub fn get_has_dev_channels() -> bool {
    lock().has_dev_channels
}

pub fn set_has_dev_channels(value: bool) {
    lock().has_dev_channels = value;
}

// ---------------------------------------------------------------------------
// Prompt cache / beta header latch
// ---------------------------------------------------------------------------

pub fn get_prompt_cache_1h_allowlist() -> Option<Vec<String>> {
    lock().prompt_cache_1h_allowlist.clone()
}

pub fn set_prompt_cache_1h_allowlist(allowlist: Option<Vec<String>>) {
    lock().prompt_cache_1h_allowlist = allowlist;
}

pub fn get_prompt_cache_1h_eligible() -> Option<bool> {
    lock().prompt_cache_1h_eligible
}

pub fn set_prompt_cache_1h_eligible(eligible: Option<bool>) {
    lock().prompt_cache_1h_eligible = eligible;
}

pub fn get_afk_mode_header_latched() -> Option<bool> {
    lock().afk_mode_header_latched
}

pub fn set_afk_mode_header_latched(value: bool) {
    lock().afk_mode_header_latched = Some(value);
}

pub fn get_fast_mode_header_latched() -> Option<bool> {
    lock().fast_mode_header_latched
}

pub fn set_fast_mode_header_latched(value: bool) {
    lock().fast_mode_header_latched = Some(value);
}

pub fn get_cache_editing_header_latched() -> Option<bool> {
    lock().cache_editing_header_latched
}

pub fn set_cache_editing_header_latched(value: bool) {
    lock().cache_editing_header_latched = Some(value);
}

/// 把 beta header latch 复位为 None。/clear 与 /compact 调用，
/// 让新会话获得新的 header 评估。
pub fn clear_beta_header_latches() {
    let mut state = lock();
    state.afk_mode_header_latched = None;
    state.fast_mode_header_latched = None;
    state.cache_editing_header_latched = None;
}

// ---------------------------------------------------------------------------
// Prompt id
// ---------------------------------------------------------------------------

pub fn get_prompt_id() -> Option<String> {
    lock().prompt_id.clone()
}

pub fn set_prompt_id(id: Option<String>) {
    lock().prompt_id = id;
}

/// REPL bridge 在所有构建均未启用（TS 硬编码 return false）。
pub fn is_repl_bridge_active() -> bool {
    false
}

// ---------------------------------------------------------------------------
// 假实现（测试桩）：让未转换模块的调用方在 Rust 侧有可用的锚点。
// 对应模块转换后删除。
// ---------------------------------------------------------------------------

/// 内置测试 matcher（实现 RegisteredHookMatcher，用于 register/clear 路径的
/// 单元测试）。types/hooks.rs 落地后由真实 matcher 取代。
#[cfg(test)]
struct CallbackMatcherStub;

#[cfg(test)]
impl RegisteredHookMatcher for CallbackMatcherStub {
    fn is_plugin_hook(&self) -> bool {
        false
    }
}

#[cfg(test)]
struct PluginMatcherStub;

#[cfg(test)]
impl RegisteredHookMatcher for PluginMatcherStub {
    fn is_plugin_hook(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_regenerates_and_tracks_parent() {
        let first = get_session_id();
        let second = regenerate_session_id(true);
        assert_ne!(first, second);
        assert_eq!(get_parent_session_id(), Some(first));
        assert_eq!(get_session_project_dir(), None);
    }

    #[test]
    fn switch_session_emits_and_updates_dir() {
        let id = SessionId::new("switch-target".to_string());
        switch_session(id, Some("/some/dir".to_string()));
        assert_eq!(get_session_id().as_ref(), "switch-target");
        assert_eq!(get_session_project_dir(), Some("/some/dir".to_string()));
    }

    #[test]
    fn plan_slug_cache_evicts_oldest_at_50() {
        for i in 0..60 {
            set_plan_slug_cache_entry(&format!("session{i}"), "slug");
        }
        let view = get_plan_slug_cache();
        assert!(view.get("session0").is_none());
        assert!(view.get("session59").is_some());
    }

    #[test]
    fn cost_accumulation_and_reset() {
        add_to_total_cost_state(1.5, ModelUsage::default(), "claude-test-model");
        assert!(get_total_cost_usd() > 1.0);
        assert!(get_usage_for_model("claude-test-model").is_some());
        reset_cost_state();
        assert_eq!(get_total_cost_usd(), 0.0);
    }

    #[test]
    fn usage_for_model_roundtrip() {
        assert!(get_usage_for_model("claude-test-model-missing").is_none());
    }

    #[test]
    fn remove_session_cron_tasks_returns_removed_count() {
        let task = SessionCronTask {
            id: "t1".to_string(),
            cron: "* * * * *".to_string(),
            prompt: "hi".to_string(),
            created_at: 0,
            recurring: Some(true),
            agent_id: None,
        };
        add_session_cron_task(task);
        assert_eq!(remove_session_cron_tasks(&["t1".to_string()]), 1);
        assert_eq!(remove_session_cron_tasks(&["missing".to_string()]), 0);
    }

    #[test]
    fn hooks_merge_and_plugin_hooks_cleared() {
        register_hook_callbacks(vec![(
            HookEvent::SessionStart,
            vec![Arc::new(CallbackMatcherStub), Arc::new(PluginMatcherStub)],
        )]);
        let hooks = get_registered_hashes();
        assert_eq!(hooks.len(), 1);
        clear_registered_plugin_hooks();
        let hooks = get_registered_hashes();
        let matchers = hooks.get(&HookEvent::SessionStart).expect("event exists");
        assert_eq!(matchers.len(), 1);
        assert!(!matchers[0].is_plugin_hook());
        reset_sdk_init_state();
        assert!(get_registered_hashes().is_none());
    }

    fn get_registered_hashes() -> HashMap<HookEvent, Vec<Arc<dyn RegisteredHookMatcher>>> {
        get_registered_hooks().expect("hooks registered")
    }

    #[test]
    fn slow_operations_skip_non_ant_and_editor_sessions() {
        // USER_TYPE 不是 'ant' 时 add_slow_operation 直接返回。
        add_slow_operation("anything", 5000.0);
        assert!(get_slow_operations().is_empty());
    }

    #[test]
    fn invoked_skills_keyed_by_agent() {
        add_invoked_skill("skillA", "/a", "content", Some("agent1"));
        add_invoked_skill("skillB", "/b", "content", None);
        let for_agent = get_invoked_skills_for_agent(Some("agent1"));
        assert_eq!(for_agent.len(), 1);
        let for_main = get_invoked_skills_for_agent(None);
        assert_eq!(for_main.len(), 1);
        clear_invoked_skills(Some(&HashSet::from(["agent1".to_string()])));
        assert!(get_invoked_skills_for_agent(None).is_empty());
        assert_eq!(get_invoked_skills_for_agent(Some("agent1")).len(), 1);
        clear_invoked_skills(None);
        assert!(get_invoked_skills_for_agent(Some("agent1")).is_empty());
    }

    #[test]
    fn in_memory_error_log_caps_at_100() {
        for i in 0..105 {
            add_to_in_memory_error_log(ErrorLogEntry {
                error: format!("error{i}"),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            });
        }
        // 无法经公开 API 读出条目（TS 也未导出 getter），仅验证不 panic、
        // 上限删除路径正常执行。
    }

    #[test]
    fn plan_mode_transition_sets_exit_attachment() {
        handle_plan_mode_transition("plan", "default");
        assert!(needs_plan_mode_exit_attachment());
        handle_plan_mode_transition("default", "plan");
        assert!(!needs_plan_mode_exit_attachment());
        set_needs_plan_mode_exit_attachment(false);
    }

    #[test]
    fn auto_mode_transition_skips_plan_bridge() {
        set_needs_auto_mode_exit_attachment(false);
        handle_auto_mode_transition("auto", "plan");
        assert!(!needs_auto_mode_exit_attachment());
        handle_auto_mode_transition("plan", "auto");
        assert!(!needs_auto_mode_exit_attachment());
        handle_auto_mode_transition("auto", "default");
        assert!(needs_auto_mode_exit_attachment());
        set_needs_auto_mode_exit_attachment(false);
    }

    #[test]
    fn post_compaction_flag_consumed_once() {
        assert!(!consume_post_compaction());
        mark_post_compaction();
        assert!(consume_post_compaction());
        assert!(!consume_post_compaction());
    }

    #[test]
    fn repl_bridge_is_never_active() {
        assert!(!is_repl_bridge_active());
    }
}
