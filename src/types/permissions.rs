//! permissions.rs — 权限类型定义（打破循环依赖的纯类型提取）。
//!
//! 重写自 `src/types/permissions.ts`。语义对齐说明：
//! - 该文件只含类型定义与常量，无运行时依赖；
//! - TS 字符串字面量联合映射为带 `serde` rename 的枚举（wire 格式不变）；
//! - `Map<string, T>` 映射为 `HashMap<String, T>`；
//! - `Record<string, unknown>` 兜底映射为 `HashMap<String, Value>` / `Value`；
//! - SDK 的 `ContentBlockParam` 尚无 Rust 对应模块，暂以 `serde_json::Value`
//!   占位，待 messages 模块落地后替换。
//!
//! 判别联合使用 serde 内部标签（`behavior` / `type`），变体承载结构体中
']* 不再重复判别字段（与 TS 平铺结构 wire 兼容）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `@anthropic-ai/sdk` `ContentBlockParam` 的占位别名（messages 模块未落地前）。
pub type ContentBlockParam = serde_json::Value;

// ============================================================================
// Permission Modes
// ============================================================================

/// 外部可寻址的权限模式集合。
pub const EXTERNAL_PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExternalPermissionMode {
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "plan")]
    Plan,
}

impl ExternalPermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Default => "default",
            Self::DontAsk => "dontAsk",
            Self::Plan => "plan",
        }
    }
}

/// 穷举模式联合（用于类型检查）。用户可寻址的运行时集合是
/// `INTERNAL_PERMISSION_MODES`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InternalPermissionMode {
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "bubble")]
    Bubble,
}

impl InternalPermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Default => "default",
            Self::DontAsk => "dontAsk",
            Self::Plan => "plan",
            // 'auto' 在 TRANSCRIPT_CLASSIFIER 关闭时回退到提示。
            Self::Auto => "auto",
            Self::Bubble => "bubble",
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(
            self,
            Self::AcceptEdits
                | Self::BypassPermissions
                | Self::Default
                | Self::DontAsk
                | Self::Plan
        )
    }
}

impl From<ExternalPermissionMode> for InternalPermissionMode {
    fn from(mode: ExternalPermissionMode) -> Self {
        match mode {
            ExternalPermissionMode::AcceptEdits => Self::AcceptEdits,
            ExternalPermissionMode::BypassPermissions => Self::BypassPermissions,
            ExternalPermissionMode::Default => Self::Default,
            ExternalPermissionMode::DontAsk => Self::DontAsk,
            ExternalPermissionMode::Plan => Self::Plan,
        }
    }
}

pub type PermissionMode = InternalPermissionMode;

/// 运行时校验集合：用户可寻址的模式（settings.json defaultMode、
/// --permission-mode CLI flag、会话恢复）。
pub const INTERNAL_PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
    "auto",
];

pub const PERMISSION_MODES: &[&str] = INTERNAL_PERMISSION_MODES;

// ============================================================================
// Permission Behaviors
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

// ============================================================================
// Permission Rules
// ============================================================================

/// 权限规则的来源（含全部 `SettingSource` 值及规则专属来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionRuleSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    FlagSettings,
    PolicySettings,
    CliArg,
    Command,
    Session,
}

/// 权限规则的值：指定工具与可选内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRuleValue {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// 带来源与行为的权限规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub source: PermissionRuleSource,
    pub rule_behavior: PermissionBehavior,
    pub rule_value: PermissionRuleValue,
    /// 该规则同时匹配的替代工具名（上游 anchor 0023 module-007：
    /// `aliasSkillToolNames`）。从 settings 字符串解析的规则从不携带别名；
    /// 程序化产生的规则（SDK/会话规则、打包策略）可以携带。
    /// 由 `src/utils/permissions/permissions.ts: ruleTargetsToolName` 消费。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_skill_tool_names: Option<Vec<String>>,
}

// ============================================================================
// Permission Updates
// ============================================================================

/// 权限更新的持久化目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    Session,
    CliArg,
}

/// 权限配置更新操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PermissionUpdate {
    AddRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    ReplaceRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    RemoveRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    SetMode {
        destination: PermissionUpdateDestination,
        mode: ExternalPermissionMode,
    },
    AddDirectories {
        destination: PermissionUpdateDestination,
        directories: Vec<String>,
    },
    RemoveDirectories {
        destination: PermissionUpdateDestination,
        directories: Vec<String>,
    },
}

/// 附加工作目录权限的来源（当前与 `PermissionRuleSource` 相同，
/// 保留为独立类型以备语义分叉）。
pub type WorkingDirectorySource = PermissionRuleSource;

/// 纳入权限范围的附加目录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdditionalWorkingDirectory {
    pub path: String,
    pub source: WorkingDirectorySource,
}

// ============================================================================
// Permission Decisions & Results
// ============================================================================

/// 权限元数据所需的最小 command 形状（刻意只取子集以避免循环依赖）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionCommandMetadata {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 允许附加属性，保持前向兼容。
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// 附在权限决策上的元数据（TS 中为 `{ command } | undefined`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionMetadata {
    pub command: PermissionCommandMetadata,
}

/// 待执行的异步分类器检查（用于非阻塞式 allow 分类器评估）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingClassifierCheck {
    pub command: String,
    pub cwd: String,
    pub descriptions: Vec<String>,
}

/// 权限被允许时的结果。`updated_input` 对应 TS 泛型参数 `Input`
/// （默认 `Record<string, unknown>`），Rust 侧以 `serde_json::Value` 承载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAllowDecision {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_modified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<PermissionDecisionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_feedback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ContentBlockParam>>,
}

/// 需要询问用户时的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAskDecision {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<PermissionDecisionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<Vec<PermissionUpdate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PermissionMetadata>,
    /// 为 true 时表示该询问由 `bashCommandIsSafe_DEPRECATED` 安全检查触发，
    /// 针对 `splitCommand_DEPRECATED` 可能误解析的模式（行 continuation、
    /// shell-quote 变换等）。简单换行复合命令不置位。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_bash_security_check_for_misparsing: Option<bool>,
    /// 置位时应异步执行 allow 分类器检查，可能在用户响应前自动批准。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_classifier_check: Option<PendingClassifierCheck>,
    /// 附在拒绝消息旁发送给工具结果的内容块（如用户粘贴的图片反馈）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ContentBlockParam>>,
}

/// 权限被拒绝时的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDenyDecision {
    pub message: String,
    pub decision_reason: PermissionDecisionReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// 权限决策：allow / ask / deny。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior")]
pub enum PermissionDecision {
    #[serde(rename = "allow")]
    Allow(PermissionAllowDecision),
    #[serde(rename = "ask")]
    Ask(PermissionAskDecision),
    #[serde(rename = "deny")]
    Deny(PermissionDenyDecision),
}

/// 带 passthrough 选项的权限结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPassthroughDecision {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<PermissionDecisionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<Vec<PermissionUpdate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    /// 置位时应异步执行 allow 分类器检查，可能在用户响应前自动批准。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_classifier_check: Option<PendingClassifierCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior")]
pub enum PermissionResult {
    #[serde(rename = "allow")]
    Allow(PermissionAllowDecision),
    #[serde(rename = "ask")]
    Ask(PermissionAskDecision),
    #[serde(rename = "deny")]
    Deny(PermissionDenyDecision),
    #[serde(rename = "passthrough")]
    Passthrough(PermissionPassthroughDecision),
}

/// sandbox 越权的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SandboxOverrideReason {
    ExcludedCommand,
    DangerouslyDisableSandbox,
}

/// 权限决策的成因解释。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PermissionDecisionReason {
    Rule { rule: PermissionRule },
    Mode { mode: PermissionMode },
    SubcommandResults {
        reasons: HashMap<String, PermissionResult>,
    },
    PermissionPromptTool {
        permission_prompt_tool_name: String,
        tool_result: serde_json::Value,
    },
    Hook {
        hook_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hook_source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    AsyncAgent { reason: String },
    SandboxOverride { reason: SandboxOverrideReason },
    Classifier { classifier: String, reason: String },
    WorkingDir { reason: String },
    SafetyCheck {
        reason: String,
        /// 为 true 时 auto 模式让分类器评估而非强制提示。敏感路径
        /// （.claude/、.git/、shell 配置）为 true；Windows 路径绕过尝试与
        /// 跨机 bridge 消息为 false。
        classifier_approvable: bool,
    },
    Other { reason: String },
}

// ============================================================================
// Bash Classifier Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    pub matches: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_description: Option<String>,
    pub confidence: ClassifierConfidence,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierBehavior {
    Deny,
    Ask,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// 分类器调用提示各部分的字符长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierPromptLengths {
    pub system_prompt: u64,
    pub tool_calls: u64,
    pub user_prompts: u64,
}

/// 两阶段（fast/thinking）XML 分类的阶段标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierStage {
    Fast,
    Thinking,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoloClassifierResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    pub should_block: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<bool>,
    /// API 返回 "prompt is too long" —— 分类器 transcript 超出上下文窗口。
    /// 具确定性（相同 transcript → 相同错误），调用方应回退到正常提示，
    /// 而不是重试或 fail-closed。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_too_long: Option<bool>,
    /// 本次分类器调用使用的模型。
    pub model: String,
    /// 分类器 API 调用的 token 用量（开销遥测）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ClassifierUsage>,
    /// 分类器 API 调用耗时（毫秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 发送给分类器的提示各部分字符长度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_lengths: Option<ClassifierPromptLengths>,
    /// 错误提示的转储路径（仅在因 API 错误不可用时置位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_dump_path: Option<String>,
    /// 最终决策由哪个阶段产生（仅两阶段 XML）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<ClassifierStage>,
    /// 阶段 1（fast）的 token 用量（当阶段 2 也执行时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage1_usage: Option<ClassifierUsage>,
    /// 阶段 1 耗时（毫秒，当阶段 2 也执行时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage1_duration_ms: Option<u64>,
    /// 阶段 1 的 API request_id（req_xxx），用于对接 api_usage 日志的
    /// 缓存未命中/路由归因；单阶段（tool_use）分类器的唯一请求也放这里。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage1_request_id: Option<String>,
    /// 阶段 1 的 API message id（msg_xxx），用于把
    /// tengu_auto_mode_decision 分析事件与分类器实际 prompt/completion 关联。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage1_msg_id: Option<String>,
    /// 阶段 2（thinking）的 token 用量（当执行了阶段 2 时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage2_usage: Option<ClassifierUsage>,
    /// 阶段 2 耗时（毫秒，当执行了阶段 2 时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage2_duration_ms: Option<u64>,
    /// 阶段 2 的 API request_id（执行了阶段 2 时置位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage2_request_id: Option<String>,
    /// 阶段 2 的 API message id（执行了阶段 2 时置位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage2_msg_id: Option<String>,
}

// ============================================================================
// Permission Explainer Types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionExplanation {
    pub risk_level: RiskLevel,
    pub explanation: String,
    pub reasoning: String,
    pub risk: String,
}

// ============================================================================
// Tool Permission Context
// ============================================================================

/// 按来源分组的权限规则映射（TS 中的 mapped type，展开为具名字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPermissionRulesBySource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_settings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_settings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_settings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_settings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_settings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_arg: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Vec<String>>,
}

/// 工具权限检查所需的上下文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPermissionContext {
    pub mode: PermissionMode,
    pub additional_working_directories: HashMap<String, AdditionalWorkingDirectory>,
    pub always_allow_rules: ToolPermissionRulesBySource,
    pub always_deny_rules: ToolPermissionRulesBySource,
    pub always_ask_rules: ToolPermissionRulesBySource,
    pub is_bypass_permissions_mode_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stripped_dangerous_rules: Option<ToolPermissionRulesBySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub should_avoid_permission_prompts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub await_automated_checks_before_dialog: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_plan_mode: Option<PermissionMode>,
}
