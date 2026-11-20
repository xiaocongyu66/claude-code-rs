//! types.rs — settings.json 的 schema、类型与校验（SettingsSchema 的 Rust 对应物）。
//!
//! 重写自 `src/utils/settings/types.ts`。TS 用 zod v4 + lazySchema；Rust 无
//! zod，schema 改为「手写校验器 + serde struct」：
//! - [`parse_settings_json`] 对应非严格 `SettingsSchema().safeParse(data)`
//!   （passthrough：未知键保留在 [`SettingsJson::extra`]）；
//! - [`parse_settings_json_strict`] 对应 `.strict()`（未知键 → unrecognized_keys
//!   issue）；
//! - [`SchemaIssue`] 只建模 [`crate::utils::settings::validation`] 的
//!   `formatZodError` 会消费的 zod v4 issue 形状（invalid_type / invalid_value /
//!   unrecognized_keys / too_small / too_big / custom），错误文案由
//!   format_zod_error 统一组装。
//!
//! feature 门控的处理：TS 在 schema 构造期用 `feature('X')` / `USER_TYPE === 'ant'`
//! / `isEnvTruthy(CLAUDE_CODE_ENABLE_XAA)` 展开 schema 字段；Rust 侧在解析时调用
//! [`feature_enabled`]（环境变量 `FEATURE_<NAME>` 覆盖，缺省取 build.ts 的
//! `DEFAULT_BUILD_FEATURES`）。门控关闭时该字段视同未知键：保留在 extra、不
//! 产生 issue（对齐 TS 里字段不存在于 schema 的行为）。
//!
//! 已知简化（对应 TS 模块尚未转 Rust，待下一波消除）：
//! - `MarketplaceSourceSchema`（plugins/schemas.ts）按判别联合做形状校验，
//!   `z.string().url()` 用 `contains("://")` 近似；
//! - sandbox / hooks / marketplace 的 describe 文本不进校验器（JSON Schema
//!   输出由 schema_output.rs 单独承载）。

use serde_json::{Map, Value};

use crate::bootstrap::state::HookEvent;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::permissions::permission_mode::PERMISSION_MODES;
use crate::utils::settings::constants::CLAUDE_CODE_SETTINGS_SCHEMA_URL;
use crate::utils::settings::permission_validation::permission_rule_schema_validate;

// ---------------------------------------------------------------------------
// 小型字符串枚举（对齐 TS 的 z.enum 字面量联合）
// ---------------------------------------------------------------------------

/// 声明一个「字符串字面量枚举」：单元变体 + camelCase serde 重命名 +
/// from_str/as_str + 合法值常量。
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $values:ident,
        { $($variant:ident = $lit:literal),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name {
            $($variant,)+
        }

        impl $name {
            pub fn from_str(s: &str) -> Option<Self> {
                $(if s == $lit { return Some($name::$variant); })+
                None
            }

            pub fn as_str(&self) -> &'static str {
                match self { $( $name::$variant => $lit, )+ }
            }
        }

        pub const $values: &[&str] = &[$($lit),+];
    };
}

string_enum!(
    /// 模型 API 提供方类型（`modelType`）。
    ModelTypeSetting, MODEL_TYPE_VALUES,
    { Anthropic = "anthropic", Openai = "openai", Gemini = "gemini", Grok = "grok" }
);
string_enum!(
    /// 输入框 `!` 命令的默认 shell（`defaultShell`）。
    DefaultShellSetting, DEFAULT_SHELL_VALUES,
    { Bash = "bash", Powershell = "powershell" }
);
string_enum!(
    /// WebSearch 后端适配器（`webSearchAdapter`）。
    WebSearchAdapter, WEB_SEARCH_ADAPTER_VALUES,
    { Api = "api", Bing = "bing", Brave = "brave", Exa = "exa", Tavily = "tavily" }
);
string_enum!(
    /// WebFetch 后端适配器（`webFetchAdapter`）。
    WebFetchAdapter, WEB_FETCH_ADAPTER_VALUES,
    { Tavily = "tavily", Http = "http" }
);
string_enum!(
    /// 强制登录方式（`forceLoginMethod`）。
    ForceLoginMethod, FORCE_LOGIN_METHOD_VALUES,
    { Claudeai = "claudeai", Console = "console" }
);
string_enum!(
    /// 自动更新渠道（`autoUpdatesChannel`）。
    AutoUpdatesChannel, AUTO_UPDATES_CHANNEL_VALUES,
    { Latest = "latest", Stable = "stable" }
);
string_enum!(
    /// 语音 STT 后端（`voiceProvider`，VOICE_MODE）。
    VoiceProvider, VOICE_PROVIDER_VALUES,
    { Anthropic = "anthropic", Doubao = "doubao" }
);
string_enum!(
    /// 默认 transcript 视图（`defaultView`，KAIROS/KAIROS_BRIEF）。
    DefaultView, DEFAULT_VIEW_VALUES,
    { Chat = "chat", Transcript = "transcript" }
);

/// 努力级别（`effortLevel`）。ant 用户额外接受 `max`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    /// 仅 USER_TYPE=ant 可用。
    Max,
}

impl EffortLevel {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "low" => Some(EffortLevel::Low),
            "medium" => Some(EffortLevel::Medium),
            "high" => Some(EffortLevel::High),
            "xhigh" => Some(EffortLevel::Xhigh),
            "max" if is_ant_user() => Some(EffortLevel::Max),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }

    /// 当前用户接受的枚举值（ant 含 `max`）。
    pub fn allowed_values() -> Vec<&'static str> {
        let mut v = vec!["low", "medium", "high", "xhigh"];
        if is_ant_user() {
            v.push("max");
        }
        v
    }
}

/// `permissions.defaultMode` 等权限模式枚举值。
pub const PERMISSION_MODE_VALUES: &[&str] = PERMISSION_MODES;

/// hooks schema 接受的事件键（bootstrap::state::HookEvent 的 27 项名字）。
const HOOK_EVENT_NAMES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "PermissionRequest",
    "PermissionDenied",
    "Setup",
    "TeammateIdle",
    "TaskCreated",
    "TaskCompleted",
    "Elicitation",
    "ElicitationResult",
    "ConfigChange",
    "WorktreeCreate",
    "WorktreeRemove",
    "InstructionsLoaded",
    "CwdChanged",
    "FileChanged",
];

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// V2 compaction 方法名（镜像 services/compaction2/contracts.ts）。
pub const COMPACTION_METHOD_NAMES: &[&str] =
    &["remote", "snapcompact", "handoff", "shake", "soft"];

/// `strictPluginOnlyCustomization` 可锁定的定制面。
pub const CUSTOMIZATION_SURFACES: &[&str] = &["skills", "agents", "hooks", "mcp"];

/// 预留官方 marketplace 名（MarketplaceNameSchema 的保留名集合）。
const ALLOWED_OFFICIAL_MARKETPLACE_NAMES: &[&str] = &[
    "claude-code-marketplace",
    "claude-code-plugins",
    "claude-plugins-official",
    "anthropic-marketplace",
    "anthropic-plugins",
    "agent-skills",
    "life-sciences",
    "knowledge-work-plugins",
];

/// 判别 official 名仿冒的正则（plugins/schemas.ts BLOCKED_OFFICIAL_NAME_PATTERN）。
static BLOCKED_OFFICIAL_NAME_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r"(?i)(official[^a-z0-9]*(anthropic|claude)|(?:anthropic|claude)[^a-z0-9]*official|^(?:anthropic|claude)[^a-z0-9]*(marketplace|plugins|official))",
        )
        .expect("BLOCKED_OFFICIAL_NAME_RE is a constant regex")
    });

/// npm 包名正则（scoped 与普通两种形式）。
static NPM_PACKAGE_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^(@[a-z0-9][a-z0-9-._]*/[a-z0-9][a-z0-9-._]*|[a-z0-9][a-z0-9-._]*)$")
            .expect("NPM_PACKAGE_RE is a constant regex")
    });

fn is_ant_user() -> bool {
    std::env::var("USER_TYPE").as_deref() == Ok("ant")
}

/// build.ts `DEFAULT_BUILD_FEATURES` 的默认开启项（环境变量 `FEATURE_<NAME>`
/// 缺省时取此列表；`feature()` 在此求值——对齐 TS 在 schema 构造期求值）。
const DEFAULT_BUILD_FEATURES: &[&str] = &[
    "BUDDY",
    "TRANSCRIPT_CLASSIFIER",
    "BRIDGE_MODE",
    "AGENT_TRIGGERS_REMOTE",
    "CHICAGO_MCP",
    "VOICE_MODE",
    "SHOT_STATS",
    "PROMPT_CACHE_BREAK_DETECTION",
    "TOKEN_BUDGET",
    "AGENT_TRIGGERS",
    "ULTRATHINK",
    "BUILTIN_EXPLORE_PLAN_AGENTS",
    "LODESTONE",
    "EXTRACT_MEMORIES",
    "VERIFICATION_AGENT",
    "KAIROS_BRIEF",
    "AWAY_SUMMARY",
    "ULTRAPLAN",
    "DAEMON",
    "ACP",
    "WORKFLOW_SCRIPTS",
    "HISTORY_SNIP",
    "MONITOR_TOOL",
    "KAIROS",
    "COORDINATOR_MODE",
    "BG_SESSIONS",
    "TEMPLATES",
    "CONNECTOR_TEXT",
    "COMMIT_ATTRIBUTION",
    "DIRECT_CONNECT",
    "EXPERIMENTAL_SKILL_SEARCH",
    "EXPERIMENTAL_SEARCH_EXTRA_TOOLS",
    "POOR",
    "SSH_REMOTE",
];

/// `feature('NAME')` 的运行时对应物：`FEATURE_<NAME>` 环境变量覆盖，缺省取
/// 默认构建启用集合。
pub fn feature_enabled(name: &str) -> bool {
    let var = format!("FEATURE_{name}");
    if let Ok(value) = std::env::var(&var) {
        return is_env_truthy(Some(&value));
    }
    DEFAULT_BUILD_FEATURES.contains(&name)
}

fn xaa_idp_enabled() -> bool {
    is_env_truthy(std::env::var("CLAUDE_CODE_ENABLE_XAA").ok().as_deref())
}

// ---------------------------------------------------------------------------
// zod issue 对应物
// ---------------------------------------------------------------------------

/// format_zod_error 会消费的 zod v4 issue code 子集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueCode {
    /// z.number().int() 失败、非 string/bool/数组/对象等类型不符。
    InvalidType,
    /// z.enum / z.literal 失败。
    InvalidValue,
    /// .strict() 模式下的未知键。
    UnrecognizedKeys,
    /// z.number().min()/nonnegative()/gt(0) 失败。
    TooSmall,
    /// z.number().max() 失败。
    TooBig,
    /// superRefine / refine 自定义失败（params.received）。
    Custom,
    /// 其它 code（只携带 message，如 invalid_string）。
    Other(String),
}

/// 一条校验 issue（对齐 zod v4 ZodIssue 的相关字段）。
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaIssue {
    pub code: IssueCode,
    /// 点分路径的段（format 时用 `.` 连接）。
    pub path: Vec<String>,
    pub message: String,
    /// invalid_type 的期望类型 / too_small 的最小值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// invalid_type 的实际输入。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// invalid_value 的合法枚举值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Value>>,
    /// unrecognized_keys 的键列表。
    pub keys: Vec<String>,
    /// too_small/too_big 的数值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    /// custom issue 的 params（`{ received: ... }`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Map<String, Value>>,
}

impl SchemaIssue {
    pub fn invalid_type(path: Vec<String>, expected: &str, input: &Value) -> Self {
        Self {
            code: IssueCode::InvalidType,
            path,
            message: format!("Expected {expected}, but received {}", received_type(input)),
            expected: Some(expected.to_string()),
            input: Some(input.clone()),
            values: None,
            keys: Vec::new(),
            minimum: None,
            maximum: None,
            params: None,
        }
    }

    pub fn invalid_value(path: Vec<String>, values: &[&str]) -> Self {
        Self {
            code: IssueCode::InvalidValue,
            path,
            message: "Invalid value".to_string(),
            expected: None,
            input: None,
            values: Some(values.iter().map(|v| Value::String((*v).to_string())).collect()),
            keys: Vec::new(),
            minimum: None,
            maximum: None,
            params: None,
        }
    }

    pub fn unrecognized_keys(path: Vec<String>, keys: Vec<String>) -> Self {
        Self {
            code: IssueCode::UnrecognizedKeys,
            path,
            message: "Unrecognized key(s)".to_string(),
            expected: None,
            input: None,
            values: None,
            keys,
            minimum: None,
            maximum: None,
            params: None,
        }
    }

    pub fn too_small(path: Vec<String>, minimum: f64, input: &Value) -> Self {
        Self {
            code: IssueCode::TooSmall,
            path,
            message: format!(
                "Number must be greater than or equal to {}",
                format_number(minimum)
            ),
            expected: Some(format_number(minimum)),
            input: Some(input.clone()),
            values: None,
            keys: Vec::new(),
            minimum: Some(minimum),
            maximum: None,
            params: None,
        }
    }

    pub fn too_big(path: Vec<String>, maximum: f64, input: &Value) -> Self {
        Self {
            code: IssueCode::TooBig,
            path,
            message: format!(
                "Number must be less than or equal to {}",
                format_number(maximum)
            ),
            expected: Some(format_number(maximum)),
            input: Some(input.clone()),
            values: None,
            keys: Vec::new(),
            minimum: None,
            maximum: Some(maximum),
            params: None,
        }
    }

    pub fn custom(path: Vec<String>, message: String, received: Option<&Value>) -> Self {
        Self {
            code: IssueCode::Custom,
            path,
            message,
            expected: None,
            input: None,
            values: None,
            keys: Vec::new(),
            minimum: None,
            maximum: None,
            params: received.map(|v| {
                let mut m = Map::new();
                m.insert("received".to_string(), v.clone());
                m
            }),
        }
    }

    pub fn other(path: Vec<String>, code: &str, message: String) -> Self {
        Self {
            code: IssueCode::Other(code.to_string()),
            path,
            message,
            expected: None,
            input: None,
            values: None,
            keys: Vec::new(),
            minimum: None,
            maximum: None,
            params: None,
        }
    }
}

fn received_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

// ---------------------------------------------------------------------------
// 嵌套 settings 类型
// ---------------------------------------------------------------------------

/// `permissions` 段（PermissionsSchema，passthrough）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_bypass_permissions_mode: Option<String>,
    /// TRANSCRIPT_CLASSIFIER 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_auto_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// MCP 企业 allowlist/denylist 条目（三者互斥，refine 检查）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

impl McpServerEntry {
    /// 类型守卫：条目是否带 serverName（`isMcpServerNameEntry`）。
    pub fn is_name_entry(&self) -> bool {
        self.server_name.is_some()
    }

    /// 类型守卫：条目是否带 serverCommand（`isMcpServerCommandEntry`）。
    pub fn is_command_entry(&self) -> bool {
        self.server_command.is_some()
    }

    /// 类型守卫：条目是否带 serverUrl（`isMcpServerUrlEntry`）。
    pub fn is_url_entry(&self) -> bool {
        self.server_url.is_some()
    }

    /// refine：三者恰好有一个。
    pub fn exactly_one(&self) -> bool {
        let defined = [
            self.server_name.is_some(),
            self.server_command.is_some(),
            self.server_url.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        defined == 1
    }
}

/// V2 compaction 策略（CompactionSettingsSchema，每字段 catch，整体 catch）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// transform 已过滤为合法方法名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method_order: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_percent: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_turn_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_continue: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_threshold_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `sandbox` 的 network 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_managed_domains_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `sandbox` 的 filesystem 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxFilesystemConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_write: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_write: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_read: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_read: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_managed_read_paths_only: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `sandbox.ripgrep` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRipgrepConfig {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `sandbox`（SandboxSettingsSchema，passthrough）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_if_unavailable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<SandboxFilesystemConfig>,
    /// record<string, array<string>>，保留原始 JSON。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<Map<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_weaker_network_isolation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_commands: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_sandbox_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsandboxed_commands_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ripgrep: Option<SandboxRipgrepConfig>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `hooks` 条目的命令（HookCommandSchema 判别联合）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum HookCommand {
    Command(BashCommandHook),
    Prompt(PromptHook),
    Agent(AgentHook),
    Http(HttpHook),
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashCommandHook {
    pub command: String,
    #[serde(rename = "if")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "once")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    #[serde(rename = "async")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_rewake: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptHook {
    pub prompt: String,
    #[serde(rename = "if")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "once")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHook {
    pub prompt: String,
    #[serde(rename = "if")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "once")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpHook {
    pub url: String,
    #[serde(rename = "if")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_env_vars: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "once")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `hooks.<event>` 条目（HookMatcherSchema）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `hooks`（HooksSchema = partialRecord(HOOK_EVENTS, HookMatcher[])）。
pub type HooksSettings = Map<String, Vec<HookMatcher>>;

/// 插件 hook matcher（内部类型，含插件上下文；非 zod schema）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginHookMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
    pub plugin_root: String,
    pub plugin_name: String,
    /// 格式 "pluginName@marketplaceName"。
    pub plugin_id: String,
}

/// skill hook matcher（内部类型；非 zod schema）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillHookMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
    pub skill_root: String,
    pub skill_name: String,
}

/// `attribution` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttributionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `fileSuggestion` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSuggestionSettings {
    /// 固定字面量 "command"。
    pub suggestion_type: String,
    pub command: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `statusLine` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineSettings {
    /// 固定字面量 "command"。
    pub status_type: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_interval: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `worktree` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_directories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_paths: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `spinnerVerbs` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpinnerVerbsSettings {
    pub mode: String,
    pub verbs: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `spinnerTipsOverride` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpinnerTipsOverrideSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_default: Option<bool>,
    pub tips: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `remote` 段。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_environment_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `autoMode` 段（TRANSCRIPT_CLASSIFIER）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoModeSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_deny: Option<Vec<String>>,
    /// ant 用户的兼容别名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `sshConfigs` 数组元素。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfigEntry {
    pub id: String,
    pub name: String,
    pub ssh_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_identity_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_directory: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `allowedChannelPlugins` 数组元素。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPluginEntry {
    pub marketplace: String,
    pub plugin: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `strictPluginOnlyCustomization`：`true` 锁全部面，数组锁列出的面。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum StrictPluginOnlyCustomization {
    All(bool),
    Surfaces(Vec<String>),
}

/// `enabledPlugins` 的值：`array<string> | boolean`（JSON null 非法）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum EnabledPluginValue {
    Bool(bool),
    List(Vec<String>),
}

/// 用户配置值（pluginConfigs 的值元素）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum UserConfigValue {
    Str(String),
    Bool(bool),
    Num(f64),
    List(Vec<String>),
}

/// plugin 配置（`pluginConfigs` 的值）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Map<String, Map<String, UserConfigValue>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Map<String, UserConfigValue>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// SettingsJson（SettingsSchema 推断类型）
// ---------------------------------------------------------------------------

/// settings.json 的统一类型（对齐 TS `SettingsJson`）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsJson {
    /// `$schema`，必须等于 CLAUDE_CODE_SETTINGS_SCHEMA_URL。
    #[serde(rename = "$schema")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_credential_export: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_auth_refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcp_auth_refresh: Option<String>,
    /// 仅 CLAUDE_CODE_ENABLE_XAA 开启时校验；passthrough 始终保留原值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xaa_idp: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_suggestion: Option<FileSuggestionSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respect_gitignore: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_period_days: Option<i64>,
    /// z.record(string, coerce.string())：数字/布尔会被转成字符串。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<AttributionSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_co_authored_by: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_git_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_aliases: Option<Map<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_type: Option<ModelTypeSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<Map<String, String>>,
    /// record<string, 正整数>。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_overrides: Option<Map<String, i64>>,
    /// 整段 catch：非法值整体丢弃。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_all_project_mcp_servers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_mcpjson_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_mcpjson_servers: Option<Vec<String>>,
    /// 企业 MCP allowlist。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mcp_servers: Option<Vec<McpServerEntry>>,
    /// 企业 MCP denylist。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_mcp_servers: Option<Vec<McpServerEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_all_hooks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_shell: Option<DefaultShellSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_managed_hooks_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_http_hook_urls: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_hook_allowed_env_vars: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_managed_permission_rules_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_managed_mcp_servers_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_plugin_only_customization: Option<StrictPluginOnlyCustomization>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLineSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<Map<String, EnabledPluginValue>>,
    /// record<string, ExtraKnownMarketplace>。条目以 Value 保留，校验在
    /// parse 时按形状进行（key===source.name 检查等）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_known_marketplaces: Option<Map<String, Value>>,
    /// MarketplaceSource 数组（策略 settings 专用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_known_marketplaces: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_marketplaces: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_login_method: Option<ForceLoginMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_login_org_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_headers_helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_web_fetch_preflight: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_adapter: Option<WebSearchAdapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch_adapter: Option<WebFetchAdapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tavily_endpoint_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brave_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch_http_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exa_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exa_endpoint_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_survey_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spinner_tips_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spinner_verbs: Option<SpinnerVerbsSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spinner_tips_override: Option<SpinnerTipsOverrideSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax_highlighting_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_title_from_rename: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub always_thinking_enabled: Option<bool>,
    /// 枚举 + catch(undefined)：非法值静默丢弃。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advisor_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_mode_per_session_opt_in: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_suggestion_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poor_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_clear_context_on_plan_accept: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company_announcements: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_configs: Option<Map<String, PluginConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_updates_channel: Option<AutoUpdatesChannel>,
    /// `enum(['disable'])`，LODESTONE 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_deep_link_registration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plans_directory: Option<String>,
    /// ant-only（USER_TYPE=ant）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_permissions_enabled: Option<bool>,
    /// PROACTIVE || KAIROS 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_sleep_duration_ms: Option<i64>,
    /// PROACTIVE || KAIROS 门控；允许 -1（不限）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sleep_duration_ms: Option<i64>,
    /// VOICE_MODE 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_enabled: Option<bool>,
    /// VOICE_MODE 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_provider: Option<VoiceProvider>,
    /// KAIROS 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant: Option<bool>,
    /// KAIROS 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_channel_plugins: Option<Vec<ChannelPluginEntry>>,
    /// KAIROS || KAIROS_BRIEF 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_view: Option<DefaultView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefers_reduced_motion: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_memory_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_memory_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_dream_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_thinking_summaries: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_dangerous_mode_permission_prompt: Option<bool>,
    /// TRANSCRIPT_CLASSIFIER 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_auto_permission_prompt: Option<bool>,
    /// TRANSCRIPT_CLASSIFIER 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_auto_mode_during_plan: Option<bool>,
    /// TRANSCRIPT_CLASSIFIER 门控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<AutoModeSettings>,
    /// `enum(['disable'])`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_auto_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_configs: Option<Vec<SshConfigEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_md_excludes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_threshold: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_warning_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_trust_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_api_key: Option<String>,
    /// passthrough：未知键原样保留（对齐 `.passthrough()`）。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}



/// 已知的顶层 settings 键（含门控字段——门控关闭时该键落入 extra）。
pub fn known_top_level_keys() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = vec![
        "$schema",
        "apiKeyHelper",
        "awsCredentialExport",
        "awsAuthRefresh",
        "gcpAuthRefresh",
        "fileSuggestion",
        "respectGitignore",
        "cleanupPeriodDays",
        "env",
        "attribution",
        "includeCoAuthoredBy",
        "includeGitInstructions",
        "permissions",
        "toolAliases",
        "modelType",
        "model",
        "availableModels",
        "modelOverrides",
        "contextWindowOverrides",
        "compaction",
        "enableAllProjectMcpServers",
        "enabledMcpjsonServers",
        "disabledMcpjsonServers",
        "allowedMcpServers",
        "deniedMcpServers",
        "hooks",
        "worktree",
        "disableAllHooks",
        "defaultShell",
        "allowManagedHooksOnly",
        "allowedHttpHookUrls",
        "httpHookAllowedEnvVars",
        "allowManagedPermissionRulesOnly",
        "allowManagedMcpServersOnly",
        "strictPluginOnlyCustomization",
        "statusLine",
        "statusLineEnabled",
        "enabledPlugins",
        "extraKnownMarketplaces",
        "strictKnownMarketplaces",
        "blockedMarketplaces",
        "forceLoginMethod",
        "forceLoginOrgUUID",
        "otelHeadersHelper",
        "outputStyle",
        "language",
        "skipWebFetchPreflight",
        "webSearchAdapter",
        "webFetchAdapter",
        "tavilyEndpointUrl",
        "braveApiKey",
        "webFetchHttpTimeoutMs",
        "exaApiKey",
        "exaEndpointUrl",
        "sandbox",
        "feedbackSurveyRate",
        "spinnerTipsEnabled",
        "uiLocale",
        "spinnerVerbs",
        "spinnerTipsOverride",
        "syntaxHighlightingDisabled",
        "terminalTitleFromRename",
        "alwaysThinkingEnabled",
        "effortLevel",
        "advisorModel",
        "fastMode",
        "fastModePerSessionOptIn",
        "promptSuggestionEnabled",
        "poorMode",
        "showClearContextOnPlanAccept",
        "agent",
        "companyAnnouncements",
        "pluginConfigs",
        "remote",
        "autoUpdatesChannel",
        "minimumVersion",
        "plansDirectory",
        "channelsEnabled",
        "allowedChannelPlugins",
        "prefersReducedMotion",
        "autoMemoryEnabled",
        "autoMemoryDirectory",
        "autoDreamEnabled",
        "showThinkingSummaries",
        "skipDangerousModePermissionPrompt",
        "disableAutoMode",
        "sshConfigs",
        "claudeMdExcludes",
        "cacheThreshold",
        "cacheWarningEnabled",
        "pluginTrustMessage",
        "workspaceApiKey",
    ];
    if xaa_idp_enabled() {
        keys.push("xaaIdp");
    }
    if feature_enabled("LODESTONE") {
        keys.push("disableDeepLinkRegistration");
    }
    if is_ant_user() {
        keys.push("classifierPermissionsEnabled");
    }
    if feature_enabled("PROACTIVE") || feature_enabled("KAIROS") {
        keys.push("minSleepDurationMs");
        keys.push("maxSleepDurationMs");
    }
    if feature_enabled("VOICE_MODE") {
        keys.push("voiceEnabled");
        keys.push("voiceProvider");
    }
    if feature_enabled("KAIROS") {
        keys.push("assistant");
        keys.push("assistantName");
    }
    if feature_enabled("KAIROS") || feature_enabled("KAIROS_BRIEF") {
        keys.push("defaultView");
    }
    if feature_enabled("TRANSCRIPT_CLASSIFIER") {
        keys.push("skipAutoPermissionPrompt");
        keys.push("useAutoModeDuringPlan");
        keys.push("autoMode");
    }
    keys
}

// ---------------------------------------------------------------------------
// 校验器
// ---------------------------------------------------------------------------

fn child_path(parent: &[String], key: &str) -> Vec<String> {
    let mut p = parent.to_vec();
    p.push(key.to_string());
    p
}

fn index_path(parent: &[String], index: usize) -> Vec<String> {
    let mut p = parent.to_vec();
    p.push(index.to_string());
    p
}

/// 期望字符串（z.string()）。非字符串 → invalid_type issue。
fn need_string(v: &Value, path: Vec<String>, issues: &mut Vec<SchemaIssue>) -> Option<String> {
    match v.as_str() {
        Some(s) => Some(s.to_string()),
        None => {
            issues.push(SchemaIssue::invalid_type(path, "string", v));
            None
        }
    }
}

/// 期望布尔（z.boolean()）。
fn need_bool(v: &Value, path: Vec<String>, issues: &mut Vec<SchemaIssue>) -> Option<bool> {
    match v.as_bool() {
        Some(b) => Some(b),
        None => {
            issues.push(SchemaIssue::invalid_type(path, "boolean", v));
            None
        }
    }
}

/// 值是否为（z.number() 接受的）JSON 数字。
fn as_zod_number(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// 期望 number + int（z.number().int()）。
fn need_int(v: &Value, path: Vec<String>, issues: &mut Vec<SchemaIssue>) -> Option<i64> {
    match as_zod_number(v) {
        Some(f) if f.fract() == 0.0 && f.abs() < 9.007e15 => Some(f as i64),
        Some(_) => {
            issues.push(SchemaIssue::invalid_type(path, "integer", v));
            None
        }
        None => {
            issues.push(SchemaIssue::invalid_type(path, "number", v));
            None
        }
    }
}

/// 期望 number（z.number()，允许小数）。
fn need_float(v: &Value, path: Vec<String>, issues: &mut Vec<SchemaIssue>) -> Option<f64> {
    match as_zod_number(v) {
        Some(f) => Some(f),
        None => {
            issues.push(SchemaIssue::invalid_type(path, "number", v));
            None
        }
    }
}

/// 期望数组（z.array(...) 的容器）。
fn need_array<'a>(
    v: &'a Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<&'a Vec<Value>> {
    match v.as_array() {
        Some(a) => Some(a),
        None => {
            issues.push(SchemaIssue::invalid_type(path, "array", v));
            None
        }
    }
}

/// 期望对象（z.object(...) / z.record(...) 的容器）。
fn need_object<'a>(
    v: &'a Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<&'a Map<String, Value>> {
    match v.as_object() {
        Some(o) => Some(o),
        None => {
            issues.push(SchemaIssue::invalid_type(path, "object", v));
            None
        }
    }
}

/// 期望枚举字符串（z.enum(vals)）。非法 → invalid_value issue。
fn need_enum(
    v: &Value,
    path: Vec<String>,
    values: &[&str],
    issues: &mut Vec<SchemaIssue>,
) -> Option<String> {
    match v.as_str() {
        Some(s) if values.contains(&s) => Some(s.to_string()),
        _ => {
            issues.push(SchemaIssue::invalid_value(path, values));
            None
        }
    }
}

/// 期望字面量（z.literal(lit)）。
fn need_literal(
    v: &Value,
    path: Vec<String>,
    lit: &str,
    issues: &mut Vec<SchemaIssue>,
) -> Option<String> {
    match v.as_str() {
        Some(s) if s == lit => Some(s.to_string()),
        _ => {
            issues.push(SchemaIssue::invalid_value(path, &[lit]));
            None
        }
    }
}

/// record<string, string>。
fn need_record_string(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Map<String, String>> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = Map::new();
    for (k, val) in obj {
        if let Some(s) = need_string(val, child_path(&path, k), issues) {
            out.insert(k.clone(), s);
        }
    }
    Some(out)
}

/// z.record(string, coerce.string())：数字/布尔/数组等转字符串（env 专用）。
fn need_record_coerced_string(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Map<String, String>> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = Map::new();
    for (k, val) in obj {
        out.insert(k.clone(), coerce_to_string(val));
    }
    Some(out)
}

/// z.coerce.string() 的 JSON 端近似：number/bool/null → 字符串。
fn coerce_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items
            .iter()
            .map(coerce_to_string)
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// record<string, int positive>（contextWindowOverrides）。
fn need_record_positive_int(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Map<String, i64>> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = Map::new();
    for (k, val) in obj {
        let p = child_path(&path, k);
        if let Some(n) = need_int(val, p.clone(), issues) {
            if n > 0 {
                out.insert(k.clone(), n);
            } else {
                issues.push(SchemaIssue::too_small(p, 0.0, val));
            }
        }
    }
    Some(out)
}

/// 对象内必需的字符串字段（MarketplaceSource 变体字段校验）。
fn require_obj_string(
    obj: &Map<String, Value>,
    issues: &mut Vec<SchemaIssue>,
    path: &[String],
    key: &str,
) -> bool {
    let p = child_path(path, key);
    match obj.get(key) {
        Some(v) => need_string(v, p, issues).is_some(),
        None => {
            issues.push(SchemaIssue::invalid_type(p, "string", &Value::Null));
            false
        }
    }
}

/// z.string() 数组。
fn need_string_array(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Vec<String>> {
    let arr = need_array(v, path.clone(), issues)?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        if let Some(s) = need_string(item, index_path(&path, i), issues) {
            out.push(s);
        }
    }
    Some(out)
}

/// 权限规则数组（z.string().superRefine(PermissionRuleSchema)）。
fn need_permission_rule_array(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Vec<String>> {
    let arr = need_array(v, path.clone(), issues)?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let p = index_path(&path, i);
        if let Some(s) = need_string(item, p.clone(), issues) {
            if let Err(message) = permission_rule_schema_validate(&s) {
                issues.push(SchemaIssue::custom(p, message, Some(item)));
            } else {
                out.push(s);
            }
        }
    }
    Some(out)
}

/// 提取对象字段（存在时调用提取器），返回 Some 仅当字段存在。
fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key)
}

// --- MCP 条目 ---------------------------------------------------------------

fn parse_mcp_entry(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<McpServerEntry> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut entry = McpServerEntry {
        server_name: None,
        server_command: None,
        server_url: None,
    };
    if let Some(s) = field(obj, "serverName") {
        // 自带 regex 校验（^[a-zA-Z0-9_-]+$）——失败用 invalid_string 近似。
        let p = child_path(&path, "serverName");
        if let Some(name) = need_string(s, p.clone(), issues) {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                issues.push(SchemaIssue::other(
                    p,
                    "invalid_string",
                    "Server name can only contain letters, numbers, hyphens, and underscores"
                        .to_string(),
                ));
            } else {
                entry.server_name = Some(name);
            }
        }
    }
    if let Some(c) = field(obj, "serverCommand") {
        let p = child_path(&path, "serverCommand");
        let arr = need_array(c, p.clone(), issues)?;
        if arr.is_empty() {
            issues.push(SchemaIssue::other(
                p,
                "too_small",
                "Server command must have at least one element (the command)".to_string(),
            ));
        } else {
            let mut cmd = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                if let Some(s) = need_string(item, index_path(&p, i), issues) {
                    cmd.push(s);
                }
            }
            if cmd.len() == arr.len() {
                entry.server_command = Some(cmd);
            }
        }
    }
    if let Some(u) = field(obj, "serverUrl") {
        entry.server_url = need_string(u, child_path(&path, "serverUrl"), issues);
    }
    // refine：恰好一个字段
    if !entry.exactly_one() {
        issues.push(SchemaIssue::custom(
            path,
            "Entry must have exactly one of \"serverName\", \"serverCommand\", or \"serverUrl\""
                .to_string(),
            Some(v),
        ));
        return None;
    }
    Some(entry)
}

// --- permissions ------------------------------------------------------------

fn parse_permissions(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<PermissionsSettings> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = PermissionsSettings {
        allow: None,
        deny: None,
        ask: None,
        default_mode: None,
        disable_bypass_permissions_mode: None,
        disable_auto_mode: None,
        additional_directories: None,
        extra: Map::new(),
    };
    let known: Vec<&str> = if feature_enabled("TRANSCRIPT_CLASSIFIER") {
        vec![
            "allow",
            "deny",
            "ask",
            "defaultMode",
            "disableBypassPermissionsMode",
            "disableAutoMode",
            "additionalDirectories",
        ]
    } else {
        vec![
            "allow",
            "deny",
            "ask",
            "defaultMode",
            "disableBypassPermissionsMode",
            "additionalDirectories",
        ]
    };
    for (k, val) in obj {
        if !known.contains(&k.as_str()) {
            out.extra.insert(k.clone(), val.clone());
            continue;
        }
        match k.as_str() {
            "allow" => {
                out.allow = need_permission_rule_array(val, child_path(&path, k), issues);
            }
            "deny" => {
                out.deny = need_permission_rule_array(val, child_path(&path, k), issues);
            }
            "ask" => {
                out.ask = need_permission_rule_array(val, child_path(&path, k), issues);
            }
            "defaultMode" => {
                out.default_mode = need_enum(
                    val,
                    child_path(&path, k),
                    PERMISSION_MODE_VALUES,
                    issues,
                );
            }
            "disableBypassPermissionsMode" => {
                out.disable_bypass_permissions_mode =
                    need_literal(val, child_path(&path, k), "disable", issues);
            }
            "disableAutoMode" => {
                out.disable_auto_mode =
                    need_literal(val, child_path(&path, k), "disable", issues);
            }
            "additionalDirectories" => {
                out.additional_directories =
                    need_string_array(val, child_path(&path, k), issues);
            }
            _ => {}
        }
    }
    Some(out)
}

// --- compaction -------------------------------------------------------------

/// CompactionSettingsSchema：每字段 `.catch(undefined)`，整体对象 `.catch(undefined)`。
/// 任何非法输入都静默丢弃，不产生 issue。
fn parse_compaction(v: &Value) -> Option<CompactionSettings> {
    let obj = v.as_object()?;
    let mut out = CompactionSettings {
        enabled: None,
        method_order: None,
        threshold_tokens: None,
        threshold_percent: None,
        reserve_tokens: None,
        keep_recent_tokens: None,
        mid_turn_enabled: None,
        auto_continue: None,
        async_enabled: None,
        idle_enabled: None,
        idle_threshold_tokens: None,
        idle_timeout_seconds: None,
        extra: Map::new(),
    };
    for (k, val) in obj {
        match k.as_str() {
            "enabled" => out.enabled = val.as_bool(),
            "methodOrder" => {
                // z.array(z.unknown()).transform(过滤合法方法名)
                if let Some(arr) = val.as_array() {
                    let filtered: Vec<String> = arr
                        .iter()
                        .filter_map(|x| {
                            x.as_str()
                                .filter(|s| COMPACTION_METHOD_NAMES.contains(s))
                                .map(str::to_string)
                        })
                        .collect();
                    out.method_order = Some(filtered);
                }
            }
            "thresholdTokens" => out.threshold_tokens = catch_int_positive(val),
            "thresholdPercent" => out.threshold_percent = catch_int_range(val, 1, 99),
            "reserveTokens" => out.reserve_tokens = catch_int_positive(val),
            "keepRecentTokens" => out.keep_recent_tokens = catch_int_nonnegative(val),
            "midTurnEnabled" => out.mid_turn_enabled = val.as_bool(),
            "autoContinue" => out.auto_continue = val.as_bool(),
            "asyncEnabled" => out.async_enabled = val.as_bool(),
            "idleEnabled" => out.idle_enabled = val.as_bool(),
            "idleThresholdTokens" => out.idle_threshold_tokens = catch_int_positive(val),
            "idleTimeoutSeconds" => out.idle_timeout_seconds = catch_int_min(val, 60),
            _ => {
                out.extra.insert(k.clone(), val.clone());
            }
        }
    }
    Some(out)
}

fn catch_int(val: &Value) -> Option<i64> {
    match val.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 9.007e15 => Some(f as i64),
        _ => None,
    }
}

fn catch_int_positive(val: &Value) -> Option<i64> {
    catch_int(val).filter(|&n| n > 0)
}

fn catch_int_nonnegative(val: &Value) -> Option<i64> {
    catch_int(val).filter(|&n| n >= 0)
}

fn catch_int_range(val: &Value, min: i64, max: i64) -> Option<i64> {
    catch_int(val).filter(|&n| n >= min && n <= max)
}

fn catch_int_min(val: &Value, min: i64) -> Option<i64> {
    catch_int(val).filter(|&n| n >= min)
}

// --- sandbox ----------------------------------------------------------------

fn parse_sandbox_network(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<SandboxNetworkConfig> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = SandboxNetworkConfig {
        allowed_domains: None,
        allow_managed_domains_only: None,
        allow_unix_sockets: None,
        allow_all_unix_sockets: None,
        allow_local_binding: None,
        http_proxy_port: None,
        socks_proxy_port: None,
        extra: Map::new(),
    };
    for (k, val) in obj {
        match k.as_str() {
            "allowedDomains" => {
                out.allowed_domains = need_string_array(val, child_path(&path, k), issues);
            }
            "allowManagedDomainsOnly" => {
                out.allow_managed_domains_only = need_bool(val, child_path(&path, k), issues);
            }
            "allowUnixSockets" => {
                out.allow_unix_sockets = need_string_array(val, child_path(&path, k), issues);
            }
            "allowAllUnixSockets" => {
                out.allow_all_unix_sockets = need_bool(val, child_path(&path, k), issues);
            }
            "allowLocalBinding" => {
                out.allow_local_binding = need_bool(val, child_path(&path, k), issues);
            }
            "httpProxyPort" => {
                out.http_proxy_port = need_float(val, child_path(&path, k), issues);
            }
            "socksProxyPort" => {
                out.socks_proxy_port = need_float(val, child_path(&path, k), issues);
            }
            _ => {
                out.extra.insert(k.clone(), val.clone());
            }
        }
    }
    Some(out)
}

fn parse_sandbox_filesystem(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<SandboxFilesystemConfig> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = SandboxFilesystemConfig {
        allow_write: None,
        deny_write: None,
        deny_read: None,
        allow_read: None,
        allow_managed_read_paths_only: None,
        extra: Map::new(),
    };
    for (k, val) in obj {
        match k.as_str() {
            "allowWrite" => out.allow_write = need_string_array(val, child_path(&path, k), issues),
            "denyWrite" => out.deny_write = need_string_array(val, child_path(&path, k), issues),
            "denyRead" => out.deny_read = need_string_array(val, child_path(&path, k), issues),
            "allowRead" => out.allow_read = need_string_array(val, child_path(&path, k), issues),
            "allowManagedReadPathsOnly" => {
                out.allow_managed_read_paths_only = need_bool(val, child_path(&path, k), issues);
            }
            _ => {
                out.extra.insert(k.clone(), val.clone());
            }
        }
    }
    Some(out)
}

fn parse_sandbox(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<SandboxSettings> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = SandboxSettings {
        enabled: None,
        fail_if_unavailable: None,
        auto_allow_bash_if_sandboxed: None,
        allow_unsandboxed_commands: None,
        network: None,
        filesystem: None,
        ignore_violations: None,
        enable_weaker_nested_sandbox: None,
        enable_weaker_network_isolation: None,
        excluded_commands: None,
        strict_sandbox_mode: None,
        unsandboxed_commands_disabled: None,
        ripgrep: None,
        extra: Map::new(),
    };
    let known: &[&str] = &[
        "enabled",
        "failIfUnavailable",
        "autoAllowBashIfSandboxed",
        "allowUnsandboxedCommands",
        "network",
        "filesystem",
        "ignoreViolations",
        "enableWeakerNestedSandbox",
        "enableWeakerNetworkIsolation",
        "excludedCommands",
        "strictSandboxMode",
        "unsandboxedCommandsDisabled",
        "ripgrep",
    ];
    for (k, val) in obj {
        if !known.contains(&k.as_str()) {
            out.extra.insert(k.clone(), val.clone());
            continue;
        }
        match k.as_str() {
            "enabled" => out.enabled = need_bool(val, child_path(&path, k), issues),
            "failIfUnavailable" => {
                out.fail_if_unavailable = need_bool(val, child_path(&path, k), issues);
            }
            "autoAllowBashIfSandboxed" => {
                out.auto_allow_bash_if_sandboxed = need_bool(val, child_path(&path, k), issues);
            }
            "allowUnsandboxedCommands" => {
                out.allow_unsandboxed_commands = need_bool(val, child_path(&path, k), issues);
            }
            "network" => {
                out.network = parse_sandbox_network(val, child_path(&path, k), issues);
            }
            "filesystem" => {
                out.filesystem = parse_sandbox_filesystem(val, child_path(&path, k), issues);
            }
            "ignoreViolations" => {
                // record<string, array<string>>
                let p = child_path(&path, k);
                if let Some(o) = need_object(val, p.clone(), issues) {
                    let mut map = Map::new();
                    for (ik, iv) in o {
                        if let Some(arr) = need_string_array(iv, child_path(&p, ik), issues) {
                            map.insert(ik.clone(), arr);
                        }
                    }
                    out.ignore_violations = Some(map);
                }
            }
            "enableWeakerNestedSandbox" => {
                out.enable_weaker_nested_sandbox = need_bool(val, child_path(&path, k), issues);
            }
            "enableWeakerNetworkIsolation" => {
                out.enable_weaker_network_isolation = need_bool(val, child_path(&path, k), issues);
            }
            "excludedCommands" => {
                out.excluded_commands = need_string_array(val, child_path(&path, k), issues);
            }
            "strictSandboxMode" => {
                out.strict_sandbox_mode = need_bool(val, child_path(&path, k), issues);
            }
            "unsandboxedCommandsDisabled" => {
                out.unsandboxed_commands_disabled = need_bool(val, child_path(&path, k), issues);
            }
            "ripgrep" => {
                let p = child_path(&path, k);
                if let Some(o) = need_object(val, p.clone(), issues) {
                    let command = need_string(
                        o.get("command").unwrap_or(&Value::Null),
                        child_path(&p, "command"),
                        issues,
                    );
                    let args =
                        o.get("args").map(|a| need_string_array(a, child_path(&p, "args"), issues));
                    if let Some(command) = command {
                        out.ripgrep = Some(SandboxRipgrepConfig {
                            command,
                            args: args.flatten(),
                            extra: Map::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Some(out)
}

// --- hooks ------------------------------------------------------------------

fn parse_hook_command(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<HookCommand> {
    let obj = need_object(v, path.clone(), issues)?;
    let hook_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let p = child_path(&path, "type");
    // 必需字符串字段（type 判别器之外的 command/prompt/url）
    let mut required_string = |issues: &mut Vec<SchemaIssue>, field: &str| {
        let fv = obj.get(field).unwrap_or(&Value::Null);
        need_string(fv, child_path(&path, field), issues)
    };
    match hook_type {
        "command" => {
            let command = required_string(&mut *issues, "command")?;
            let timeout = obj
                .get("timeout")
                .map(|t| need_positive_float(t, child_path(&path, "timeout"), issues));
            Some(HookCommand::Command(BashCommandHook {
                command,
                if_condition: obj.get("if").and_then(Value::as_str).map(str::to_string),
                shell: obj
                    .get("shell")
                    .map(|s| need_enum(s, child_path(&path, "shell"), &["bash", "powershell"], issues)),
                timeout,
                status_message: obj.get("statusMessage").and_then(Value::as_str).map(str::to_string),
                once: obj.get("once").and_then(Value::as_bool),
                is_async: obj.get("async").and_then(Value::as_bool),
                async_rewake: obj.get("asyncRewake").and_then(Value::as_bool),
                extra: extra_of(obj, &[
                    "type",
                    "command",
                    "if",
                    "shell",
                    "timeout",
                    "statusMessage",
                    "once",
                    "async",
                    "asyncRewake",
                ]),
            }))
        }
        "prompt" => {
            let prompt = required_string(&mut *issues, "prompt")?;
            let timeout = obj
                .get("timeout")
                .map(|t| need_positive_float(t, child_path(&path, "timeout"), issues));
            Some(HookCommand::Prompt(PromptHook {
                prompt,
                if_condition: obj.get("if").and_then(Value::as_str).map(str::to_string),
                timeout,
                model: obj.get("model").and_then(Value::as_str).map(str::to_string),
                status_message: obj.get("statusMessage").and_then(Value::as_str).map(str::to_string),
                once: obj.get("once").and_then(Value::as_bool),
                extra: extra_of(obj, &["type", "prompt", "if", "timeout", "model", "statusMessage", "once"]),
            }))
        }
        "agent" => {
            let prompt = required_string(&mut *issues, "prompt")?;
            let timeout = obj
                .get("timeout")
                .map(|t| need_positive_float(t, child_path(&path, "timeout"), issues));
            Some(HookCommand::Agent(AgentHook {
                prompt,
                if_condition: obj.get("if").and_then(Value::as_str).map(str::to_string),
                timeout,
                model: obj.get("model").and_then(Value::as_str).map(str::to_string),
                status_message: obj.get("statusMessage").and_then(Value::as_str).map(str::to_string),
                once: obj.get("once").and_then(Value::as_bool),
                extra: extra_of(obj, &["type", "prompt", "if", "timeout", "model", "statusMessage", "once"]),
            }))
        }
        "http" => {
            let url = required_string(&mut *issues, "url")?;
            // z.string().url() → 近似校验
            if !is_approximate_url(&url) {
                issues.push(SchemaIssue::other(
                    child_path(&path, "url"),
                    "invalid_string",
                    "Invalid url".to_string(),
                ));
                return None;
            }
            let timeout = obj
                .get("timeout")
                .map(|t| need_positive_float(t, child_path(&path, "timeout"), issues));
            let headers = obj.get("headers").map(|h| {
                need_record_string(h, child_path(&path, "headers"), issues)
            });
            Some(HookCommand::Http(HttpHook {
                url,
                if_condition: obj.get("if").and_then(Value::as_str).map(str::to_string),
                timeout,
                headers,
                allowed_env_vars: obj
                    .get("allowedEnvVars")
                    .map(|a| need_string_array(a, child_path(&path, "allowedEnvVars"), issues)),
                status_message: obj.get("statusMessage").and_then(Value::as_str).map(str::to_string),
                once: obj.get("once").and_then(Value::as_bool),
                extra: extra_of(obj, &[
                    "type",
                    "url",
                    "if",
                    "timeout",
                    "headers",
                    "allowedEnvVars",
                    "statusMessage",
                    "once",
                ]),
            }))
        }
        _ => {
            issues.push(SchemaIssue::invalid_value(p, &["command", "prompt", "agent", "http"]));
            None
        }
    }
}

fn extra_of(obj: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    obj.iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn need_positive_float(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<f64> {
    match need_float(v, path.clone(), issues) {
        Some(f) if f > 0.0 => Some(f),
        Some(_) => {
            issues.push(SchemaIssue::too_small(path, 0.0, v));
            None
        }
        None => None,
    }
}

fn is_approximate_url(s: &str) -> bool {
    s.contains("://") || s.starts_with("mailto:")
}

fn parse_hooks(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<HooksSettings> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out: HooksSettings = Map::new();
    for (k, val) in obj {
        let p = child_path(&path, k);
        // partialRecord 的 key 必须是合法 HookEvent
        if HookEvent::from_name(k).is_none() {
            issues.push(SchemaIssue::invalid_value(p.clone(), HOOK_EVENT_NAMES));
            continue;
        }
        let Some(arr) = need_array(val, p.clone(), issues) else {
            continue;
        };
        let mut matchers = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let mp = index_path(&p, i);
            let Some(mobj) = need_object(item, mp.clone(), issues) else {
                continue;
            };
            let matcher = mobj
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            let hooks_v = mobj.get("hooks").unwrap_or(&Value::Null);
            let hpath = child_path(&mp, "hooks");
            let Some(harr) = need_array(hooks_v, hpath.clone(), issues) else {
                continue;
            };
            let mut hooks = Vec::with_capacity(harr.len());
            for (hi, hv) in harr.iter().enumerate() {
                if let Some(h) = parse_hook_command(hv, index_path(&hpath, hi), issues) {
                    hooks.push(h);
                }
            }
            // hooks 为必需数组；保留校验通过形状的 matcher（元素全无效时保留空
            // 数组的近似：只要 hooks 键的类型正确即保留）
            matchers.push(HookMatcher {
                matcher,
                hooks,
                extra: extra_of(mobj, &["matcher", "hooks"]),
            });
        }
        if !matchers.is_empty() {
            out.insert(k.clone(), matchers);
        }
    }
    Some(out)
}

// --- marketplace ------------------------------------------------------------
//
// MarketplaceSourceSchema（plugins/schemas.ts）为 8 变体判别联合；此处按
// 形状校验。settings 变体额外要求 name 满足 MarketplaceNameSchema 且 plugins
// 为远程来源数组。

fn is_valid_marketplace_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(' ')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name != "."
        && !name.eq_ignore_ascii_case("inline")
        && !name.eq_ignore_ascii_case("builtin")
        && !is_blocked_official_name(name)
}

fn is_blocked_official_name(name: &str) -> bool {
    if ALLOWED_OFFICIAL_MARKETPLACE_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return false;
    }
    // 非 ASCII（同形字攻击）阻断
    if !name.is_ascii() {
        return true;
    }
    BLOCKED_OFFICIAL_NAME_RE.is_match(name)
}

/// 校验 MarketplaceSource（返回 true=通过）。unknown 键按 zod 默认剥离（不报错）。
fn validate_marketplace_source(v: &Value, path: Vec<String>, issues: &mut Vec<SchemaIssue>) -> bool {
    let Some(obj) = v.as_object() else {
        issues.push(SchemaIssue::invalid_type(path, "object", v));
        return false;
    };
    let Some(source) = obj.get("source").and_then(Value::as_str) else {
        issues.push(SchemaIssue::invalid_value(
            child_path(&path, "source"),
            &[
                "url", "github", "git", "npm", "file", "directory", "hostPattern", "pathPattern",
                "settings",
            ],
        ));
        return false;
    };
    let mut ok = true;
    match source {
        "url" => {
            ok &= require_obj_string(obj, issues, &path, "url");
            if let Some(url) = obj.get("url").and_then(Value::as_str) {
                if !is_approximate_url(url) {
                    issues.push(SchemaIssue::other(
                        child_path(&path, "url"),
                        "invalid_string",
                        "Invalid url".to_string(),
                    ));
                    ok = false;
                }
            }
        }
        "github" | "git" => {
            ok &= require_obj_string(
                obj,
                issues,
                &path,
                if source == "github" { "repo" } else { "url" },
            );
        }
        "npm" => {
            let p = child_path(&path, "package");
            match obj.get("package").and_then(Value::as_str) {
                Some(pkg)
                    if !pkg.contains("..")
                        && !pkg.contains("//")
                        && NPM_PACKAGE_RE.is_match(pkg) => {}
                _ => {
                    issues.push(SchemaIssue::other(
                        p,
                        "custom",
                        "Invalid npm package name format".to_string(),
                    ));
                    ok = false;
                }
            }
        }
        "file" | "directory" => {
            ok &= require_obj_string(obj, issues, &path, "path");
        }
        "hostPattern" => {
            ok &= require_obj_string(obj, issues, &path, "hostPattern");
        }
        "pathPattern" => {
            ok &= require_obj_string(obj, issues, &path, "pathPattern");
        }
        "settings" => {
            // name: MarketplaceNameSchema
            let p = child_path(&path, "name");
            match obj.get("name").and_then(Value::as_str) {
                Some(name) if is_valid_marketplace_name(name) => {
                    if ALLOWED_OFFICIAL_MARKETPLACE_NAMES.contains(&name.to_ascii_lowercase().as_str())
                    {
                        issues.push(SchemaIssue::other(
                            p,
                            "custom",
                            "Reserved official marketplace names cannot be used with settings sources."
                                .to_string(),
                        ));
                        ok = false;
                    }
                }
                _ => {
                    issues.push(SchemaIssue::invalid_type(p, "string", &Value::Null));
                    ok = false;
                }
            }
            // plugins: SettingsMarketplacePluginSchema 数组
            let pp = child_path(&path, "plugins");
            match obj.get("plugins") {
                Some(Value::Array(arr)) => {
                    for (i, item) in arr.iter().enumerate() {
                        let ip = index_path(&pp, i);
                        let Some(pobj) = item.as_object() else {
                            issues.push(SchemaIssue::invalid_type(ip, "object", item));
                            ok = false;
                            continue;
                        };
                        // name: 非空、无空格
                        match pobj.get("name").and_then(Value::as_str) {
                            Some(n) if !n.is_empty() && !n.contains(' ') => {}
                            _ => {
                                issues.push(SchemaIssue::other(
                                    child_path(&ip, "name"),
                                    "custom",
                                    "Invalid plugin name".to_string(),
                                ));
                                ok = false;
                            }
                        }
                        // source 必须是对象（远程来源），不能是 "./xxx" 字符串
                        match pobj.get("source") {
                            Some(Value::Object(_)) => {}
                            _ => {
                                issues.push(SchemaIssue::other(
                                    child_path(&ip, "source"),
                                    "custom",
                                    "Plugins in a settings-sourced marketplace must use remote sources \
                                     (github, git-subdir, npm, url, pip)."
                                        .to_string(),
                                ));
                                ok = false;
                            }
                        }
                    }
                }
                _ => {
                    issues.push(SchemaIssue::invalid_type(pp, "array", &Value::Null));
                    ok = false;
                }
            }
        }
        _ => unreachable!(),
    }
    ok
}

/// extraKnownMarketplaces：record<string, {source, installLocation?, autoUpdate?}>
/// + key===source.name（settings 变体）检查。条目保留为 Value。
fn parse_extra_known_marketplaces(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Map<String, Value>> {
    let obj = need_object(v, path.clone(), issues)?;
    let mut out = Map::new();
    for (k, val) in obj {
        let p = child_path(&path, k);
        let Some(entry) = val.as_object() else {
            issues.push(SchemaIssue::invalid_type(p.clone(), "object", val));
            continue;
        };
        let source_v = entry.get("source").unwrap_or(&Value::Null);
        if !validate_marketplace_source(source_v, child_path(&p, "source"), issues) {
            continue;
        }
        // installLocation?: string；autoUpdate?: boolean
        if let Some(il) = entry.get("installLocation") {
            need_string(il, child_path(&p, "installLocation"), issues);
        }
        if let Some(au) = entry.get("autoUpdate") {
            need_bool(au, child_path(&p, "autoUpdate"), issues);
        }
        // check：settings 变体的 source.name 必须等于 dict key
        if entry
            .get("source")
            .and_then(|s| s.as_object())
            .and_then(|s| s.get("source"))
            .and_then(Value::as_str)
            == Some("settings")
        {
            let entry_name = entry
                .get("source")
                .and_then(|s| s.as_object())
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str);
            let key_str: &str = k;
            if entry_name != Some(key_str) {
                issues.push(SchemaIssue::custom(
                    child_path(&child_path(&p, "source"), "name"),
                    format!(
                        "Settings-sourced marketplace name must match its extraKnownMarketplaces key \
                         (got key \"{}\" but source.name \"{}\")",
                        k,
                        entry_name.unwrap_or("")
                    ),
                    entry_name.map(Value::String).as_ref(),
                ));
            }
        }
        // 非透传 schema：剥离未知键后保留（对齐 zod 默认 strip 行为）
        let mut stripped = Map::new();
        if let Some(source) = entry.get("source") {
            stripped.insert("source".to_string(), strip_marketplace_source(source));
        }
        for key in ["installLocation", "autoUpdate"] {
            if let Some(v) = entry.get(key) {
                stripped.insert(key.to_string(), v.clone());
            }
        }
        out.insert(k.clone(), Value::Object(stripped));
    }
    Some(out)
}

/// strictKnownMarketplaces / blockedMarketplaces：MarketplaceSource 数组。
fn parse_marketplace_source_array(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<Vec<Value>> {
    let arr = need_array(v, path.clone(), issues)?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        if validate_marketplace_source(item, index_path(&path, i), issues) {
            out.push(strip_marketplace_source(item));
        }
    }
    Some(out)
}

/// 按 zod 默认 strip 语义剥离 MarketplaceSource 的未知键（各变体只保留
/// schema 声明的字段）。
fn strip_marketplace_source(v: &Value) -> Value {
    let Some(obj) = v.as_object() else {
        return v.clone();
    };
    let source = obj.get("source").and_then(Value::as_str).unwrap_or("");
    let keep: &[&str] = match source {
        "url" => &["source", "url", "headers"],
        "github" | "git" => &["source", "url", "repo", "ref", "path", "sparsePaths"],
        "npm" => &["source", "package"],
        "file" | "directory" => &["source", "path"],
        "hostPattern" => &["source", "hostPattern"],
        "pathPattern" => &["source", "pathPattern"],
        "settings" => &["source", "name", "plugins", "owner"],
        _ => return v.clone(),
    };
    let mut stripped = Map::new();
    for key in keep {
        if let Some(v) = obj.get(*key) {
            stripped.insert((*key).to_string(), v.clone());
        }
    }
    Value::Object(stripped)
}

// --- 顶层解析 ---------------------------------------------------------------

/// `SettingsSchema().safeParse(data)`（passthrough）。
pub fn parse_settings_json(data: &Value) -> Result<SettingsJson, Vec<SchemaIssue>> {
    parse_settings_inner(data, false)
}

/// `SettingsSchema().strict().safeParse(data)`（未知键报错）。
pub fn parse_settings_json_strict(data: &Value) -> Result<SettingsJson, Vec<SchemaIssue>> {
    parse_settings_inner(data, true)
}

fn parse_settings_inner(data: &Value, strict: bool) -> Result<SettingsJson, Vec<SchemaIssue>> {
    let mut issues: Vec<SchemaIssue> = Vec::new();
    let mut settings = SettingsJson::default();

    let Some(obj) = data.as_object() else {
        // z.object 在根上的 invalid_type（null 等会经 format_zod_error 显示为
        // 'Invalid or malformed JSON'）
        issues.push(SchemaIssue::invalid_type(Vec::new(), "object", data));
        return Err(issues);
    };

    let known = known_top_level_keys();
    let is_known = |k: &str| known.contains(&k);

    for (key, value) in obj {
        let path = vec![key.clone()];
        if !is_known(key) {
            if strict {
                issues.push(SchemaIssue::unrecognized_keys(
                    Vec::new(),
                    vec![key.clone()],
                ));
            } else {
                settings.extra.insert(key.clone(), value.clone());
            }
            continue;
        }
        match key.as_str() {
            "$schema" => {
                settings.schema = need_literal(value, path, CLAUDE_CODE_SETTINGS_SCHEMA_URL, &mut issues);
            }
            "apiKeyHelper" => {
                settings.api_key_helper = need_string(value, path, &mut issues);
            }
            "awsCredentialExport" => {
                settings.aws_credential_export = need_string(value, path, &mut issues);
            }
            "awsAuthRefresh" => {
                settings.aws_auth_refresh = need_string(value, path, &mut issues);
            }
            "gcpAuthRefresh" => {
                settings.gcp_auth_refresh = need_string(value, path, &mut issues);
            }
            "xaaIdp" => {
                // 门控字段：env 关闭时不进 known → 走 extra 分支，不会到此处。
                // issuer: string().url()；clientId: string；callbackPort?: int 正
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let issuer = need_string(
                        o.get("issuer").unwrap_or(&Value::Null),
                        child_path(&path, "issuer"),
                        &mut issues,
                    );
                    let client_id = need_string(
                        o.get("clientId").unwrap_or(&Value::Null),
                        child_path(&path, "clientId"),
                        &mut issues,
                    );
                    let callback_port = o.get("callbackPort").map(|p| {
                        let np = child_path(&path, "callbackPort");
                        match need_int(p, np.clone(), &mut issues) {
                            Some(n) if n > 0 => Some(n),
                            Some(_) => {
                                issues.push(SchemaIssue::too_small(np, 0.0, p));
                                None
                            }
                            None => None,
                        }
                    });
                    if let (Some(issuer), Some(client_id)) = (issuer, client_id) {
                        let mut m = Map::new();
                        m.insert("issuer".to_string(), Value::String(issuer));
                        m.insert("clientId".to_string(), Value::String(client_id));
                        if let Some(p) = callback_port.flatten() {
                            m.insert("callbackPort".to_string(), Value::from(p));
                        }
                        settings.xaa_idp = Some(m);
                    }
                }
            }
            "fileSuggestion" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let t = need_literal(
                        o.get("type").unwrap_or(&Value::Null),
                        child_path(&path, "type"),
                        "command",
                        &mut issues,
                    );
                    let command = need_string(
                        o.get("command").unwrap_or(&Value::Null),
                        child_path(&path, "command"),
                        &mut issues,
                    );
                    if let (Some(t), Some(command)) = (t, command) {
                        settings.file_suggestion = Some(FileSuggestionSettings {
                            suggestion_type: t,
                            command,
                            extra: extra_of(o, &["type", "command"]),
                        });
                    }
                }
            }
            "respectGitignore" => {
                settings.respect_gitignore = need_bool(value, path, &mut issues);
            }
            "cleanupPeriodDays" => {
                let n = need_int(value, path.clone(), &mut issues);
                if let Some(n) = n {
                    if n < 0 {
                        issues.push(SchemaIssue::too_small(path, 0.0, value));
                    } else {
                        settings.cleanup_period_days = Some(n);
                    }
                }
            }
            "env" => {
                settings.env = need_record_coerced_string(value, path, &mut issues);
            }
            "attribution" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let commit = o
                        .get("commit")
                        .map(|c| need_string(c, child_path(&path, "commit"), &mut issues));
                    let pr = o
                        .get("pr")
                        .map(|c| need_string(c, child_path(&path, "pr"), &mut issues));
                    settings.attribution = Some(AttributionSettings {
                        commit: commit.flatten(),
                        pr: pr.flatten(),
                        extra: extra_of(o, &["commit", "pr"]),
                    });
                }
            }
            "includeCoAuthoredBy" => {
                settings.include_co_authored_by = need_bool(value, path, &mut issues);
            }
            "includeGitInstructions" => {
                settings.include_git_instructions = need_bool(value, path, &mut issues);
            }
            "permissions" => {
                settings.permissions = parse_permissions(value, path, &mut issues);
            }
            "toolAliases" => {
                settings.tool_aliases = need_record_string(value, path, &mut issues);
            }
            "modelType" => {
                settings.model_type = need_enum(value, path, MODEL_TYPE_VALUES, &mut issues)
                    .and_then(|s| ModelTypeSetting::from_str(&s));
            }
            "model" => {
                settings.model = need_string(value, path, &mut issues);
            }
            "availableModels" => {
                settings.available_models = need_string_array(value, path, &mut issues);
            }
            "modelOverrides" => {
                settings.model_overrides = need_record_string(value, path, &mut issues);
            }
            "contextWindowOverrides" => {
                settings.context_window_overrides =
                    need_record_positive_int(value, path, &mut issues);
            }
            "compaction" => {
                // 整段 catch：解析失败静默丢弃
                settings.compaction = parse_compaction(value);
            }
            "enableAllProjectMcpServers" => {
                settings.enable_all_project_mcp_servers = need_bool(value, path, &mut issues);
            }
            "enabledMcpjsonServers" => {
                settings.enabled_mcpjson_servers = need_string_array(value, path, &mut issues);
            }
            "disabledMcpjsonServers" => {
                settings.disabled_mcpjson_servers = need_string_array(value, path, &mut issues);
            }
            "allowedMcpServers" => {
                let arr = need_array(value, path.clone(), &mut issues);
                if let Some(arr) = arr {
                    let mut out = Vec::with_capacity(arr.len());
                    for (i, item) in arr.iter().enumerate() {
                        if let Some(e) = parse_mcp_entry(item, index_path(&path, i), &mut issues) {
                            out.push(e);
                        }
                    }
                    settings.allowed_mcp_servers = Some(out);
                }
            }
            "deniedMcpServers" => {
                let arr = need_array(value, path.clone(), &mut issues);
                if let Some(arr) = arr {
                    let mut out = Vec::with_capacity(arr.len());
                    for (i, item) in arr.iter().enumerate() {
                        if let Some(e) = parse_mcp_entry(item, index_path(&path, i), &mut issues) {
                            out.push(e);
                        }
                    }
                    settings.denied_mcp_servers = Some(out);
                }
            }
            "hooks" => {
                settings.hooks = parse_hooks(value, path, &mut issues);
            }
            "worktree" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let symlink_dirs = o
                        .get("symlinkDirectories")
                        .map(|v| need_string_array(v, child_path(&path, "symlinkDirectories"), &mut issues));
                    let sparse = o
                        .get("sparsePaths")
                        .map(|v| need_string_array(v, child_path(&path, "sparsePaths"), &mut issues));
                    settings.worktree = Some(WorktreeSettings {
                        symlink_directories: symlink_dirs.flatten(),
                        sparse_paths: sparse.flatten(),
                        extra: extra_of(o, &["symlinkDirectories", "sparsePaths"]),
                    });
                }
            }
            "disableAllHooks" => {
                settings.disable_all_hooks = need_bool(value, path, &mut issues);
            }
            "defaultShell" => {
                settings.default_shell = need_enum(value, path, DEFAULT_SHELL_VALUES, &mut issues)
                    .and_then(|s| DefaultShellSetting::from_str(&s));
            }
            "allowManagedHooksOnly" => {
                settings.allow_managed_hooks_only = need_bool(value, path, &mut issues);
            }
            "allowedHttpHookUrls" => {
                settings.allowed_http_hook_urls = need_string_array(value, path, &mut issues);
            }
            "httpHookAllowedEnvVars" => {
                settings.http_hook_allowed_env_vars = need_string_array(value, path, &mut issues);
            }
            "allowManagedPermissionRulesOnly" => {
                settings.allow_managed_permission_rules_only = need_bool(value, path, &mut issues);
            }
            "allowManagedMcpServersOnly" => {
                settings.allow_managed_mcp_servers_only = need_bool(value, path, &mut issues);
            }
            "strictPluginOnlyCustomization" => {
                // preprocess（数组过滤未知面）+ union(bool, array<enum>) + catch(undefined)
                settings.strict_plugin_only_customization = parse_strict_plugin_only_customization(value);
            }
            "statusLine" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let t = need_literal(
                        o.get("type").unwrap_or(&Value::Null),
                        child_path(&path, "type"),
                        "command",
                        &mut issues,
                    );
                    let command = need_string(
                        o.get("command").unwrap_or(&Value::Null),
                        child_path(&path, "command"),
                        &mut issues,
                    );
                    let padding = o
                        .get("padding")
                        .map(|v| need_float(v, child_path(&path, "padding"), &mut issues));
                    let refresh = o
                        .get("refreshInterval")
                        .map(|v| need_float(v, child_path(&path, "refreshInterval"), &mut issues));
                    if let (Some(t), Some(command)) = (t, command) {
                        settings.status_line = Some(StatusLineSettings {
                            status_type: t,
                            command,
                            padding: padding.flatten(),
                            refresh_interval: refresh.flatten(),
                            extra: extra_of(o, &["type", "command", "padding", "refreshInterval"]),
                        });
                    }
                }
            }
            "statusLineEnabled" => {
                settings.status_line_enabled = need_bool(value, path, &mut issues);
            }
            "enabledPlugins" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let mut map = Map::new();
                    for (k, v) in o {
                        let p = child_path(&path, k);
                        match v {
                            Value::Bool(b) => {
                                map.insert(k.clone(), EnabledPluginValue::Bool(*b));
                            }
                            Value::Array(_) => {
                                if let Some(arr) = need_string_array(v, p.clone(), &mut issues) {
                                    map.insert(k.clone(), EnabledPluginValue::List(arr));
                                }
                            }
                            // null 等不匹配联合的值 → invalid_union（zod v4 默认信息）
                            _ => {
                                issues.push(SchemaIssue::other(
                                    p,
                                    "invalid_union",
                                    "Invalid input".to_string(),
                                ));
                            }
                        }
                    }
                    settings.enabled_plugins = Some(map);
                }
            }
            "extraKnownMarketplaces" => {
                settings.extra_known_marketplaces =
                    parse_extra_known_marketplaces(value, path, &mut issues);
            }
            "strictKnownMarketplaces" => {
                settings.strict_known_marketplaces =
                    parse_marketplace_source_array(value, path, &mut issues);
            }
            "blockedMarketplaces" => {
                settings.blocked_marketplaces =
                    parse_marketplace_source_array(value, path, &mut issues);
            }
            "forceLoginMethod" => {
                settings.force_login_method = need_enum(value, path, FORCE_LOGIN_METHOD_VALUES, &mut issues)
                    .and_then(|s| ForceLoginMethod::from_str(&s));
            }
            "forceLoginOrgUUID" => {
                settings.force_login_org_uuid = need_string(value, path, &mut issues);
            }
            "otelHeadersHelper" => {
                settings.otel_headers_helper = need_string(value, path, &mut issues);
            }
            "outputStyle" => {
                settings.output_style = need_string(value, path, &mut issues);
            }
            "language" => {
                settings.language = need_string(value, path, &mut issues);
            }
            "skipWebFetchPreflight" => {
                settings.skip_web_fetch_preflight = need_bool(value, path, &mut issues);
            }
            "webSearchAdapter" => {
                settings.web_search_adapter = need_enum(value, path, WEB_SEARCH_ADAPTER_VALUES, &mut issues)
                    .and_then(|s| WebSearchAdapter::from_str(&s));
            }
            "webFetchAdapter" => {
                settings.web_fetch_adapter = need_enum(value, path, WEB_FETCH_ADAPTER_VALUES, &mut issues)
                    .and_then(|s| WebFetchAdapter::from_str(&s));
            }
            "tavilyEndpointUrl" => {
                settings.tavily_endpoint_url = need_string(value, path, &mut issues);
            }
            "braveApiKey" => {
                settings.brave_api_key = need_string(value, path, &mut issues);
            }
            "webFetchHttpTimeoutMs" => {
                let n = need_int(value, path.clone(), &mut issues);
                if let Some(n) = n {
                    if n <= 0 {
                        issues.push(SchemaIssue::too_small(path, 1.0, value));
                    } else {
                        settings.web_fetch_http_timeout_ms = Some(n);
                    }
                }
            }
            "exaApiKey" => {
                settings.exa_api_key = need_string(value, path, &mut issues);
            }
            "exaEndpointUrl" => {
                settings.exa_endpoint_url = need_string(value, path, &mut issues);
            }
            "sandbox" => {
                settings.sandbox = parse_sandbox(value, path, &mut issues);
            }
            "feedbackSurveyRate" => {
                let f = need_float(value, path.clone(), &mut issues);
                if let Some(f) = f {
                    if f < 0.0 {
                        issues.push(SchemaIssue::too_small(path.clone(), 0.0, value));
                    } else if f > 1.0 {
                        issues.push(SchemaIssue::too_big(path, 1.0, value));
                    } else {
                        settings.feedback_survey_rate = Some(f);
                    }
                }
            }
            "spinnerTipsEnabled" => {
                settings.spinner_tips_enabled = need_bool(value, path, &mut issues);
            }
            "uiLocale" => {
                settings.ui_locale = need_string(value, path, &mut issues);
            }
            "spinnerVerbs" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let mode = need_enum(
                        o.get("mode").unwrap_or(&Value::Null),
                        child_path(&path, "mode"),
                        &["append", "replace"],
                        &mut issues,
                    );
                    let verbs = need_string_array(
                        o.get("verbs").unwrap_or(&Value::Null),
                        child_path(&path, "verbs"),
                        &mut issues,
                    );
                    if let (Some(mode), Some(verbs)) = (mode, verbs) {
                        settings.spinner_verbs = Some(SpinnerVerbsSettings {
                            mode,
                            verbs,
                            extra: extra_of(o, &["mode", "verbs"]),
                        });
                    }
                }
            }
            "spinnerTipsOverride" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let exclude = o
                        .get("excludeDefault")
                        .map(|v| need_bool(v, child_path(&path, "excludeDefault"), &mut issues));
                    let tips = need_string_array(
                        o.get("tips").unwrap_or(&Value::Null),
                        child_path(&path, "tips"),
                        &mut issues,
                    );
                    if let Some(tips) = tips {
                        settings.spinner_tips_override = Some(SpinnerTipsOverrideSettings {
                            exclude_default: exclude.flatten(),
                            tips,
                            extra: extra_of(o, &["excludeDefault", "tips"]),
                        });
                    }
                }
            }
            "syntaxHighlightingDisabled" => {
                settings.syntax_highlighting_disabled = need_bool(value, path, &mut issues);
            }
            "terminalTitleFromRename" => {
                settings.terminal_title_from_rename = need_bool(value, path, &mut issues);
            }
            "alwaysThinkingEnabled" => {
                settings.always_thinking_enabled = need_bool(value, path, &mut issues);
            }
            "effortLevel" => {
                // 枚举 + catch(undefined)
                if let Some(v) = value.as_str().and_then(EffortLevel::from_str) {
                    settings.effort_level = Some(v);
                }
            }
            "advisorModel" => {
                settings.advisor_model = need_string(value, path, &mut issues);
            }
            "fastMode" => {
                settings.fast_mode = need_bool(value, path, &mut issues);
            }
            "fastModePerSessionOptIn" => {
                settings.fast_mode_per_session_opt_in = need_bool(value, path, &mut issues);
            }
            "promptSuggestionEnabled" => {
                settings.prompt_suggestion_enabled = need_bool(value, path, &mut issues);
            }
            "poorMode" => {
                settings.poor_mode = need_bool(value, path, &mut issues);
            }
            "showClearContextOnPlanAccept" => {
                settings.show_clear_context_on_plan_accept = need_bool(value, path, &mut issues);
            }
            "agent" => {
                settings.agent = need_string(value, path, &mut issues);
            }
            "companyAnnouncements" => {
                settings.company_announcements = need_string_array(value, path, &mut issues);
            }
            "pluginConfigs" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let mut map = Map::new();
                    for (k, v) in o {
                        let p = child_path(&path, k);
                        if let Some(pc) = parse_plugin_config(v, p, &mut issues) {
                            map.insert(k.clone(), pc);
                        }
                    }
                    settings.plugin_configs = Some(map);
                }
            }
            "remote" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let id = o
                        .get("defaultEnvironmentId")
                        .map(|v| need_string(v, child_path(&path, "defaultEnvironmentId"), &mut issues));
                    settings.remote = Some(RemoteSettings {
                        default_environment_id: id.flatten(),
                        extra: extra_of(o, &["defaultEnvironmentId"]),
                    });
                }
            }
            "autoUpdatesChannel" => {
                settings.auto_updates_channel = need_enum(value, path, AUTO_UPDATES_CHANNEL_VALUES, &mut issues)
                    .and_then(|s| AutoUpdatesChannel::from_str(&s));
            }
            "disableDeepLinkRegistration" => {
                // LODESTONE 门控字段
                settings.disable_deep_link_registration =
                    need_literal(value, path, "disable", &mut issues);
            }
            "minimumVersion" => {
                settings.minimum_version = need_string(value, path, &mut issues);
            }
            "plansDirectory" => {
                settings.plans_directory = need_string(value, path, &mut issues);
            }
            "classifierPermissionsEnabled" => {
                // ant 门控字段
                settings.classifier_permissions_enabled = need_bool(value, path, &mut issues);
            }
            "minSleepDurationMs" => {
                let n = need_int(value, path.clone(), &mut issues);
                if let Some(n) = n {
                    if n < 0 {
                        issues.push(SchemaIssue::too_small(path, 0.0, value));
                    } else {
                        settings.min_sleep_duration_ms = Some(n);
                    }
                }
            }
            "maxSleepDurationMs" => {
                let n = need_int(value, path.clone(), &mut issues);
                if let Some(n) = n {
                    if n < -1 {
                        issues.push(SchemaIssue::too_small(path, -1.0, value));
                    } else {
                        settings.max_sleep_duration_ms = Some(n);
                    }
                }
            }
            "voiceEnabled" => {
                settings.voice_enabled = need_bool(value, path, &mut issues);
            }
            "voiceProvider" => {
                settings.voice_provider = need_enum(value, path, VOICE_PROVIDER_VALUES, &mut issues)
                    .and_then(|s| VoiceProvider::from_str(&s));
            }
            "assistant" => {
                settings.assistant = need_bool(value, path, &mut issues);
            }
            "assistantName" => {
                settings.assistant_name = need_string(value, path, &mut issues);
            }
            "channelsEnabled" => {
                settings.channels_enabled = need_bool(value, path, &mut issues);
            }
            "allowedChannelPlugins" => {
                let arr = need_array(value, path.clone(), &mut issues);
                if let Some(arr) = arr {
                    let mut out = Vec::with_capacity(arr.len());
                    for (i, item) in arr.iter().enumerate() {
                        let p = index_path(&path, i);
                        let Some(o) = need_object(item, p.clone(), &mut issues) else {
                            continue;
                        };
                        let marketplace = need_string(
                            o.get("marketplace").unwrap_or(&Value::Null),
                            child_path(&p, "marketplace"),
                            &mut issues,
                        );
                        let plugin = need_string(
                            o.get("plugin").unwrap_or(&Value::Null),
                            child_path(&p, "plugin"),
                            &mut issues,
                        );
                        if let (Some(marketplace), Some(plugin)) = (marketplace, plugin) {
                            out.push(ChannelPluginEntry {
                                marketplace,
                                plugin,
                                extra: extra_of(o, &["marketplace", "plugin"]),
                            });
                        }
                    }
                    settings.allowed_channel_plugins = Some(out);
                }
            }
            "defaultView" => {
                settings.default_view = need_enum(value, path, DEFAULT_VIEW_VALUES, &mut issues)
                    .and_then(|s| DefaultView::from_str(&s));
            }
            "prefersReducedMotion" => {
                settings.prefers_reduced_motion = need_bool(value, path, &mut issues);
            }
            "autoMemoryEnabled" => {
                settings.auto_memory_enabled = need_bool(value, path, &mut issues);
            }
            "autoMemoryDirectory" => {
                settings.auto_memory_directory = need_string(value, path, &mut issues);
            }
            "autoDreamEnabled" => {
                settings.auto_dream_enabled = need_bool(value, path, &mut issues);
            }
            "showThinkingSummaries" => {
                settings.show_thinking_summaries = need_bool(value, path, &mut issues);
            }
            "skipDangerousModePermissionPrompt" => {
                settings.skip_dangerous_mode_permission_prompt =
                    need_bool(value, path, &mut issues);
            }
            "skipAutoPermissionPrompt" => {
                settings.skip_auto_permission_prompt = need_bool(value, path, &mut issues);
            }
            "useAutoModeDuringPlan" => {
                settings.use_auto_mode_during_plan = need_bool(value, path, &mut issues);
            }
            "autoMode" => {
                let o = need_object(value, path.clone(), &mut issues);
                if let Some(o) = o {
                    let allow = o
                        .get("allow")
                        .map(|v| need_string_array(v, child_path(&path, "allow"), &mut issues));
                    let soft_deny = o
                        .get("soft_deny")
                        .map(|v| need_string_array(v, child_path(&path, "soft_deny"), &mut issues));
                    let deny = if is_ant_user() {
                        o.get("deny")
                            .map(|v| need_string_array(v, child_path(&path, "deny"), &mut issues))
                    } else {
                        None
                    };
                    let environment = o
                        .get("environment")
                        .map(|v| need_string_array(v, child_path(&path, "environment"), &mut issues));
                    let mut extra = extra_of(o, &["allow", "soft_deny", "deny", "environment"]);
                    if !is_ant_user() {
                        if let Some(v) = o.get("deny") {
                            extra.insert("deny".to_string(), v.clone());
                        }
                    }
                    settings.auto_mode = Some(AutoModeSettings {
                        allow: allow.flatten(),
                        soft_deny: soft_deny.flatten(),
                        deny: deny.flatten(),
                        environment: environment.flatten(),
                        extra,
                    });
                }
            }
            "disableAutoMode" => {
                settings.disable_auto_mode = need_literal(value, path, "disable", &mut issues);
            }
            "sshConfigs" => {
                let arr = need_array(value, path.clone(), &mut issues);
                if let Some(arr) = arr {
                    let mut out = Vec::with_capacity(arr.len());
                    for (i, item) in arr.iter().enumerate() {
                        let p = index_path(&path, i);
                        let Some(o) = need_object(item, p.clone(), &mut issues) else {
                            continue;
                        };
                        let id = need_string(
                            o.get("id").unwrap_or(&Value::Null),
                            child_path(&p, "id"),
                            &mut issues,
                        );
                        let name = need_string(
                            o.get("name").unwrap_or(&Value::Null),
                            child_path(&p, "name"),
                            &mut issues,
                        );
                        let ssh_host = need_string(
                            o.get("sshHost").unwrap_or(&Value::Null),
                            child_path(&p, "sshHost"),
                            &mut issues,
                        );
                        let ssh_port = o
                            .get("sshPort")
                            .map(|v| need_int(v, child_path(&p, "sshPort"), &mut issues));
                        let ssh_identity_file = o
                            .get("sshIdentityFile")
                            .map(|v| need_string(v, child_path(&p, "sshIdentityFile"), &mut issues));
                        let start_directory = o
                            .get("startDirectory")
                            .map(|v| need_string(v, child_path(&p, "startDirectory"), &mut issues));
                        if let (Some(id), Some(name), Some(ssh_host)) = (id, name, ssh_host) {
                            out.push(SshConfigEntry {
                                id,
                                name,
                                ssh_host,
                                ssh_port: ssh_port.flatten(),
                                ssh_identity_file: ssh_identity_file.flatten(),
                                start_directory: start_directory.flatten(),
                                extra: extra_of(
                                    o,
                                    &[
                                        "id",
                                        "name",
                                        "sshHost",
                                        "sshPort",
                                        "sshIdentityFile",
                                        "startDirectory",
                                    ],
                                ),
                            });
                        }
                    }
                    settings.ssh_configs = Some(out);
                }
            }
            "claudeMdExcludes" => {
                settings.claude_md_excludes = need_string_array(value, path, &mut issues);
            }
            "cacheThreshold" => {
                let n = need_int(value, path.clone(), &mut issues);
                if let Some(n) = n {
                    if n < 0 {
                        issues.push(SchemaIssue::too_small(path.clone(), 0.0, value));
                    } else if n > 100 {
                        issues.push(SchemaIssue::too_big(path, 100.0, value));
                    } else {
                        settings.cache_threshold = Some(n);
                    }
                }
            }
            "cacheWarningEnabled" => {
                settings.cache_warning_enabled = need_bool(value, path, &mut issues);
            }
            "pluginTrustMessage" => {
                settings.plugin_trust_message = need_string(value, path, &mut issues);
            }
            "workspaceApiKey" => {
                settings.workspace_api_key = need_string(value, path, &mut issues);
            }
            _ => {}
        }
    }

    if issues.is_empty() {
        Ok(settings)
    } else {
        Err(issues)
    }
}

fn parse_strict_plugin_only_customization(value: &Value) -> Option<StrictPluginOnlyCustomization> {
    // preprocess：数组先过滤未知面（ forwards-compat 降级）
    if let Value::Array(arr) = value {
        let filtered: Vec<String> = arr
            .iter()
            .filter_map(|v| {
                v.as_str()
                    .filter(|s| CUSTOMIZATION_SURFACES.contains(s))
                    .map(str::to_string)
            })
            .collect();
        return Some(StrictPluginOnlyCustomization::Surfaces(filtered));
    }
    match value {
        Value::Bool(b) => Some(StrictPluginOnlyCustomization::All(*b)),
        // 非数组/非布尔（含 null）→ catch(undefined) 丢弃
        _ => None,
    }
}

fn parse_plugin_config(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<PluginConfig> {
    let obj = need_object(v, path.clone(), issues)?;
    let mcp_servers = obj.get("mcpServers").map(|m| {
        let p = child_path(&path, "mcpServers");
        let Some(mobj) = need_object(m, p.clone(), issues) else {
            return None;
        };
        let mut out = Map::new();
        for (server, cfg) in mobj {
            let sp = child_path(&p, server);
            let Some(cobj) = need_object(cfg, sp.clone(), issues) else {
                continue;
            };
            let mut server_cfg = Map::new();
            for (k, val) in cobj {
                let kp = child_path(&sp, k);
                if let Some(uv) = parse_user_config_value(val, kp, issues) {
                    server_cfg.insert(k.clone(), uv);
                }
            }
            out.insert(server.clone(), server_cfg);
        }
        Some(out)
    });
    let options = obj.get("options").map(|o| {
        let p = child_path(&path, "options");
        let Some(oobj) = need_object(o, p.clone(), issues) else {
            return None;
        };
        let mut out = Map::new();
        for (k, val) in oobj {
            let kp = child_path(&p, k);
            if let Some(uv) = parse_user_config_value(val, kp.clone(), issues) {
                out.insert(k.clone(), uv);
            }
        }
        Some(out)
    });
    Some(PluginConfig {
        mcp_servers: mcp_servers.flatten(),
        options: options.flatten(),
        extra: extra_of(obj, &["mcpServers", "options"]),
    })
}

fn parse_user_config_value(
    v: &Value,
    path: Vec<String>,
    issues: &mut Vec<SchemaIssue>,
) -> Option<UserConfigValue> {
    match v {
        Value::String(s) => Some(UserConfigValue::Str(s.clone())),
        Value::Bool(b) => Some(UserConfigValue::Bool(*b)),
        Value::Number(n) => n.as_f64().map(UserConfigValue::Num),
        Value::Array(_) => need_string_array(v, path, issues).map(UserConfigValue::List),
        _ => {
            issues.push(SchemaIssue::invalid_type(path, "string", v));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: &Value) -> Result<SettingsJson, Vec<SchemaIssue>> {
        parse_settings_json(v)
    }

    #[test]
    fn parses_minimal_object() {
        let s = parse(&json!({})).unwrap();
        assert_eq!(s.model, None);
        assert!(s.extra.is_empty());
    }

    #[test]
    fn root_must_be_object() {
        let err = parse(&json!(null)).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(matches!(err[0].code, IssueCode::InvalidType));
        assert_eq!(err[0].expected.as_deref(), Some("object"));
    }

    #[test]
    fn passthrough_keeps_unknown_keys() {
        let s = parse(&json!({"unknownKey": 42, "model": "claude-4"})).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-4"));
        assert_eq!(s.extra.get("unknownKey"), Some(&json!(42)));
    }

    #[test]
    fn strict_rejects_unknown_keys() {
        let err = parse_settings_json_strict(&json!({"unknownKey": 1})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::UnrecognizedKeys));
        assert_eq!(err[0].keys, vec!["unknownKey"]);
    }

    #[test]
    fn invalid_enum_produces_invalid_value() {
        let err = parse(&json!({"modelType": "bedrock"})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::InvalidValue));
        assert_eq!(err[0].path, vec!["modelType"]);
    }

    #[test]
    fn number_range_issues() {
        let err = parse(&json!({"cleanupPeriodDays": -1})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::TooSmall));

        let err = parse(&json!({"cacheThreshold": 101})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::TooBig));

        assert!(parse(&json!({"cleanupPeriodDays": 0})).is_ok());
        assert!(parse(&json!({"cacheThreshold": 100})).is_ok());
    }

    #[test]
    fn json_syntax_error_shape() {
        // 对齐 formatZodError 的特殊路径：根 invalid_type + null input
        let err = parse(&json!(null)).unwrap_err();
        assert!(err[0].path.is_empty());
    }

    #[test]
    fn env_coerces_numbers_and_bools() {
        let s = parse(&json!({"env": {"DEBUG": true, "PORT": 3000}})).unwrap();
        let env = s.env.unwrap();
        assert_eq!(env.get("DEBUG").unwrap(), "true");
        assert_eq!(env.get("PORT").unwrap(), "3000");
    }

    #[test]
    fn compaction_invalid_is_dropped_silently() {
        let s = parse(&json!({"compaction": "not-an-object"})).unwrap();
        assert_eq!(s.compaction, None);
        let s = parse(&json!({"compaction": {"thresholdTokens": -5, "enabled": true}})).unwrap();
        let c = s.compaction.unwrap();
        assert_eq!(c.enabled, Some(true));
        assert_eq!(c.threshold_tokens, None); // catch
    }

    #[test]
    fn compaction_method_order_filters_unknown() {
        let s = parse(&json!({"compaction": {"methodOrder": ["remote", "bogus", "shake"]}}))
            .unwrap();
        assert_eq!(
            s.compaction.unwrap().method_order.unwrap(),
            vec!["remote", "shake"]
        );
    }

    #[test]
    fn permissions_passthrough_and_rules() {
        let s = parse(&json!({
            "permissions": {
                "allow": ["Bash(npm install)"],
                "defaultMode": "plan",
                "customKey": 1
            }
        }))
        .unwrap();
        let perms = s.permissions.unwrap();
        assert_eq!(perms.allow.unwrap(), vec!["Bash(npm install)"]);
        assert_eq!(perms.default_mode.as_deref(), Some("plan"));
        assert_eq!(perms.extra.get("customKey"), Some(&json!(1)));

        let err = parse(&json!({"permissions": {"defaultMode": "bogus"}})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::InvalidValue));
    }

    #[test]
    fn mcp_entry_exactly_one() {
        let err = parse(&json!({
            "allowedMcpServers": [{"serverName": "a", "serverUrl": "https://x"}]
        }))
        .unwrap_err();
        assert!(matches!(err[0].code, IssueCode::Custom));

        let s = parse(&json!({"deniedMcpServers": [{"serverCommand": ["cmd"]}]})).unwrap();
        assert!(s.denied_mcp_servers.unwrap()[0].is_command_entry());

        let err = parse(&json!({"allowedMcpServers": [{"serverName": "bad name!"}]}))
            .unwrap_err();
        assert!(matches!(err[0].code, IssueCode::Other(_)));
    }

    #[test]
    fn hooks_validate() {
        let s = parse(&json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Edit|Write", "hooks": [
                    {"type": "command", "command": "echo done"}
                ]}]
            }
        }))
        .unwrap();
        let hooks = s.hooks.unwrap();
        assert_eq!(hooks.len(), 1);

        // 未知事件键
        let err = parse(&json!({"hooks": {"NotAnEvent": []}})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::InvalidValue));

        // hook type 字面量
        let err = parse(&json!({
            "hooks": {"PostToolUse": [{"hooks": [{"type": "bogus"}]}]}
        }))
        .unwrap_err();
        assert!(matches!(err[0].code, IssueCode::InvalidValue));
    }

    #[test]
    fn strict_plugin_only_customization_forms() {
        let s = parse(&json!({"strictPluginOnlyCustomization": true})).unwrap();
        assert_eq!(
            s.strict_plugin_only_customization,
            Some(StrictPluginOnlyCustomization::All(true))
        );
        let s = parse(&json!({
            "strictPluginOnlyCustomization": ["skills", "commands", "hooks"]
        }))
        .unwrap();
        // 未知面 "commands" 被 preprocess 过滤
        match s.strict_plugin_only_customization {
            Some(StrictPluginOnlyCustomization::Surfaces(v)) => assert_eq!(v, vec!["skills", "hooks"]),
            other => panic!("unexpected {other:?}"),
        }
        // 非法值 catch → None
        let s = parse(&json!({"strictPluginOnlyCustomization": "skills"})).unwrap();
        assert_eq!(s.strict_plugin_only_customization, None);
    }

    #[test]
    fn extra_known_marketplaces_key_check() {
        let err = parse(&json!({
            "extraKnownMarketplaces": {
                "my-market": {"source": {"source": "settings", "name": "other-name", "plugins": []}}
            }
        }))
        .unwrap_err();
        assert!(matches!(err[0].code, IssueCode::Custom));
        assert_eq!(err[0].path, vec!["extraKnownMarketplaces", "my-market", "source", "name"]);

        let s = parse(&json!({
            "extraKnownMarketplaces": {
                "my-market": {"source": {"source": "github", "repo": "org/repo"}}
            }
        }))
        .unwrap();
        assert!(s.extra_known_marketplaces.is_some());

        // github 缺 repo → 失败
        let err = parse(&json!({
            "extraKnownMarketplaces": {"k": {"source": {"source": "github"}}}
        }))
        .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn sandbox_shape() {
        let s = parse(&json!({
            "sandbox": {
                "enabled": true,
                "network": {"allowedDomains": ["example.com"], "httpProxyPort": 8080},
                "ignoreViolations": {"bash": ["ls"]},
                "custom": 1
            }
        }))
        .unwrap();
        let sb = s.sandbox.unwrap();
        assert_eq!(sb.enabled, Some(true));
        assert_eq!(sb.network.unwrap().allowed_domains.unwrap(), vec!["example.com"]);
        assert_eq!(sb.ignore_violations.unwrap().get("bash").unwrap(), &vec!["ls".to_string()]);
        assert!(sb.extra.contains_key("custom"));
    }

    #[test]
    fn plugin_configs_shape() {
        let s = parse(&json!({
            "pluginConfigs": {
                "p@market": {
                    "mcpServers": {"srv": {"timeout": 30, "flag": true}},
                    "options": {"opt": "value"}
                }
            }
        }))
        .unwrap();
        let pc = s.plugin_configs.unwrap().get("p@market").cloned().unwrap();
        let mcp = pc.mcp_servers.unwrap().get("srv").cloned().unwrap();
        assert!(matches!(mcp.get("timeout"), Some(UserConfigValue::Num(30.0))));
        assert!(matches!(mcp.get("flag"), Some(UserConfigValue::Bool(true))));
    }

    #[test]
    fn feature_flag_gating_defaults() {
        // TRANSCRIPT_CLASSIFIER 在默认构建 features 中
        assert!(feature_enabled("TRANSCRIPT_CLASSIFIER"));
        assert!(!feature_enabled("PROACTIVE"));
        assert!(known_top_level_keys().contains(&"autoMode"));
        assert!(!known_top_level_keys().contains(&"minSleepDurationMs"));
    }

    #[test]
    fn schema_literal() {
        let err = parse(&json!({"$schema": "https://bogus.example/x.json"})).unwrap_err();
        assert!(matches!(err[0].code, IssueCode::InvalidValue));

        let s = parse(&json!({"$schema": CLAUDE_CODE_SETTINGS_SCHEMA_URL})).unwrap();
        assert_eq!(s.schema.as_deref(), Some(CLAUDE_CODE_SETTINGS_SCHEMA_URL));
    }
}
