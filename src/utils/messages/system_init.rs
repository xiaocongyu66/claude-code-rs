//! system_init.rs — 构建 SDK 流首条 `system/init` 消息。
//!
//! 重写自 `src/utils/messages/systemInit.ts`。该消息承载会话元数据
//! （cwd、tools、model、slash commands 等），远程客户端用它渲染
//! 选择器与门控 UI。两条调用路径（QueryEngine 的 spawn-bridge /
//! print-mode / SDK 流，以及 REPL Remote Control 的 writeSdkMessages）
//! 必须产出相同结构。
//!
//! 类型说明（TS→Rust 鸭子类型）：
//! - `src/entrypoints/agentSdkTypes.ts`（重导出 `sdk/coreTypes.ts`）未
//!   转换；`SDKMessage` 是 `{ type: string; [key: string]: unknown }` 的
//!   开放结构，故返回 `serde_json::Value`，键名与 TS 逐一对齐。
//! - `PermissionMode` / `ApiKeySource` 在生成的 SDK 类型里均为 string
//!   别名，本模块对应使用 `&str`。
//!
//! 依赖未转换模块的本地复刻 / 已知差异：
//! - `MACRO.VERSION`（构建期 define，TS 端读 package.json）→ 本地常量
//!   [`MACRO_VERSION`]（与 Cargo.toml / package.json 的 version 对齐，
//!   升版时需同步）。
//! - `constants/outputStyles.ts`（`DEFAULT_OUTPUT_STYLE_NAME`）未转换 →
//!   本地常量 `"default"`。
//! - builtin-tools 的 `AGENT_TOOL_NAME` / `LEGACY_AGENT_TOOL_NAME` 未
//!   转换 → 本地常量。
//! - `feature('UDS_INBOX')`（bun:bundle 构建期开关）→ 环境变量
//!   `FEATURE_UDS_INBOX` 真值判定（与 `log.rs` 的 `feature()` 处理一致）。
//!     开启时 TS 会从 `../udsMessaging.ts` 取 `messaging_socket_path`，
//!     该模块未转换，本端口在开启时仅记录偏差并不落该键（UDS_INBOX
//!     在默认构建中禁用，该路径处于休眠状态）。
//! - `getFastModeState` 来自未转换的 `fastMode.ts` → 见私有
//!   [`get_fast_mode_state`] 的简化说明。
//! - serde_json 默认按字典序排列对象键（无 `preserve_order` feature）；
//!   JSON 对象键序无语义。

use std::env;

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::bootstrap::state::{get_sdk_betas, get_session_id};
use crate::utils::auth::{get_anthropic_api_key_with_source, GetApiKeyOpts};
use crate::utils::cwd::get_cwd;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::settings::settings::get_initial_settings;

/// `MACRO.VERSION` 的本地复刻（scripts/defines.ts 从 package.json 注入）。
const MACRO_VERSION: &str = "2.8.4";

/// `src/constants/outputStyles.ts` 的 `DEFAULT_OUTPUT_STYLE_NAME`（未转换）。
const DEFAULT_OUTPUT_STYLE_NAME: &str = "default";

/// builtin-tools AgentTool/constants.ts（未转换）。
const AGENT_TOOL_NAME: &str = "Agent";
const LEGACY_AGENT_TOOL_NAME: &str = "Task";

// TODO(next-minor): 与 TS 源码同义——SDK 消费方迁移到 'Agent' 工具名后
// 移除该翻译（wire 名在 #19647 由 Task 改为 Agent，但直接发出新名会破坏
// patch 级 SDK 消费方）。
/// SDK 兼容工具名：`Agent` → `Task`，其余原样返回。
pub fn sdk_compat_tool_name(name: &str) -> &str {
    if name == AGENT_TOOL_NAME {
        LEGACY_AGENT_TOOL_NAME
    } else {
        name
    }
}

/// TS `CommandLike`：斜杠命令 / skill 的最小描述。
#[derive(Debug, Clone)]
pub struct CommandLike {
    pub name: String,
    /// `userInvocable` 缺省时视为可调用。
    pub user_invocable: Option<bool>,
}

/// [`build_system_init_message`] 的输入集。
pub struct SystemInitInputs<'a> {
    pub tools: &'a [ToolName],
    pub mcp_clients: &'a [McpClientRef],
    pub model: &'a str,
    /// SDK `PermissionMode`（string 别名）。
    pub permission_mode: &'a str,
    pub commands: &'a [CommandLike],
    pub agents: &'a [AgentRef],
    pub skills: &'a [CommandLike],
    pub plugins: &'a [PluginRef],
    /// `fastMode`（undefined 表示未设置）。
    pub fast_mode: Option<bool>,
}

/// `{ name: string }` 的最小工具引用。
#[derive(Debug, Clone)]
pub struct ToolName {
    pub name: String,
}

/// `{ name: string; type: string }`。
#[derive(Debug, Clone)]
pub struct McpClientRef {
    pub name: String,
    /// SDK 侧映射为 `status`。
    pub r#type: String,
}

/// `{ agentType: string }`。
#[derive(Debug, Clone)]
pub struct AgentRef {
    pub agent_type: String,
}

/// `{ name: string; path: string; source: string }`。
#[derive(Debug, Clone)]
pub struct PluginRef {
    pub name: String,
    pub path: String,
    pub source: String,
}

// ---------------------------------------------------------------------------
// fastMode（fastMode.ts 未转换 → 私有简化复刻）
// ---------------------------------------------------------------------------

/// `CLAUDE_CODE_DISABLE_FAST_MODE` 真值时禁用（对齐 `isFastModeEnabled`）。
fn fast_mode_enabled() -> bool {
    !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_FAST_MODE").ok().as_deref())
}

/// `isFastModeSupportedByModel` 的简化复刻：模型名（小写）含
/// `opus-4-7` 或 `opus-4-6` 即支持。
/// 与 TS 的差异：TS 经 `parseUserSpecifiedModel` 解析、None 时回退
/// `getDefaultMainLoopModelSetting()`（`model/model.ts` 未转换），本端口
/// model 为 None 时按不支持处理。
fn fast_mode_supported_by_model(model: Option<&str>) -> bool {
    if !fast_mode_enabled() {
        return false;
    }
    let Some(model) = model else {
        return false;
    };
    let lower = model.to_ascii_lowercase();
    lower.contains("opus-4-7") || lower.contains("opus-4-6")
}

/// `getFastModeState` 的简化复刻（fastMode.ts 未转换）：仅保留可本地
/// 计算的三因子——env 开关、用户开关、模型名匹配。缺失 GrowthBook 服务端
/// 可用性判定（`isFastModeAvailable`）与运行时冷却态
/// （`isFastModeCooldown`，TS 模块级 `runtimeState` 初值为
/// `{ status: 'active' }`，即全新进程不冷却，本端口对齐该初值）。
/// fastMode.ts 转换后应替换为真实实现。
fn get_fast_mode_state(model: Option<&str>, fast_mode_user_enabled: Option<bool>) -> &'static str {
    if fast_mode_user_enabled != Some(true) {
        return "off";
    }
    if !fast_mode_supported_by_model(model) {
        return "off";
    }
    "on"
}

// ---------------------------------------------------------------------------
// feature('UDS_INBOX')
// ---------------------------------------------------------------------------

/// `feature('UDS_INBOX')` 的运行时等价（对齐 log.rs 的 feature 处理）：
/// 环境变量 `FEATURE_UDS_INBOX` 为真值。
fn uds_inbox_feature() -> bool {
    is_env_truthy(env::var("FEATURE_UDS_INBOX").ok().as_deref())
}

// ---------------------------------------------------------------------------
// buildSystemInitMessage
// ---------------------------------------------------------------------------

/// 构建 `system/init` SDKMessage。tools 经 [`sdk_compat_tool_name`] 映射；
/// `userInvocable !== false` 的 commands / skills 进 `slash_commands` /
/// `skills`；betas / outputStyle 取会话状态与 settings（缺省落 default）。
pub fn build_system_init_message(inputs: &SystemInitInputs) -> Value {
    let settings = get_initial_settings();
    let output_style = settings
        .output_style
        .as_deref()
        .unwrap_or(DEFAULT_OUTPUT_STYLE_NAME);

    let tools: Vec<Value> = inputs
        .tools
        .iter()
        .map(|tool| json!(sdk_compat_tool_name(&tool.name)))
        .collect();
    let mcp_servers: Vec<Value> = inputs
        .mcp_clients
        .iter()
        .map(|client| {
            json!({
                "name": client.name,
                "status": client.r#type,
            })
        })
        .collect();
    let slash_commands: Vec<&str> = inputs
        .commands
        .iter()
        .filter(|c| c.user_invocable != Some(false))
        .map(|c| c.name.as_str())
        .collect();
    let agents: Vec<&str> = inputs.agents.iter().map(|a| a.agent_type.as_str()).collect();
    let skills: Vec<&str> = inputs
        .skills
        .iter()
        .filter(|s| s.user_invocable != Some(false))
        .map(|s| s.name.as_str())
        .collect();
    let plugins: Vec<Value> = inputs
        .plugins
        .iter()
        .map(|plugin| {
            json!({
                "name": plugin.name,
                "path": plugin.path,
                "source": plugin.source,
            })
        })
        .collect();

    let api_key_source =
        get_anthropic_api_key_with_source(GetApiKeyOpts::default()).1.as_str().to_string();

    let mut init_message = Map::new();
    init_message.insert("type".to_string(), json!("system"));
    init_message.insert("subtype".to_string(), json!("init"));
    init_message.insert("cwd".to_string(), json!(get_cwd()));
    init_message.insert("session_id".to_string(), json!(get_session_id().to_string()));
    init_message.insert("tools".to_string(), Value::Array(tools));
    init_message.insert("mcp_servers".to_string(), Value::Array(mcp_servers));
    init_message.insert("model".to_string(), json!(inputs.model));
    init_message.insert("permissionMode".to_string(), json!(inputs.permission_mode));
    init_message.insert("slash_commands".to_string(), json!(slash_commands));
    init_message.insert("apiKeySource".to_string(), json!(api_key_source));
    if let Some(betas) = get_sdk_betas() {
        init_message.insert("betas".to_string(), json!(betas));
    }
    init_message.insert("claude_code_version".to_string(), json!(MACRO_VERSION));
    init_message.insert("output_style".to_string(), json!(output_style));
    init_message.insert("agents".to_string(), json!(agents));
    init_message.insert("skills".to_string(), json!(skills));
    init_message.insert("plugins".to_string(), Value::Array(plugins));
    init_message.insert("uuid".to_string(), json!(Uuid::new_v4().to_string()));

    // 隐藏于公开 SDK 类型：ant-only UDS 消息套接字路径。
    // udsMessaging.ts 未转换 → 开启时不落 messaging_socket_path 键（见模块
    // 头部已知差异）。
    if uds_inbox_feature() {
        // Intentionally no-op beyond the feature gate evaluation: TS 在此
        // 写入 messaging_socket_path，Rust 端依赖未就绪。
    }

    init_message.insert(
        "fast_mode_state".to_string(),
        json!(get_fast_mode_state(Some(inputs.model), inputs.fast_mode)),
    );

    Value::Object(init_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_inputs<'a>(
        tools: &'a [ToolName],
        mcp_clients: &'a [McpClientRef],
        commands: &'a [CommandLike],
        agents: &'a [AgentRef],
        skills: &'a [CommandLike],
        plugins: &'a [PluginRef],
        model: &'a str,
        fast_mode: Option<bool>,
    ) -> SystemInitInputs<'a> {
        SystemInitInputs {
            tools,
            mcp_clients,
            model,
            permission_mode: "default",
            commands,
            agents,
            skills,
            plugins,
            fast_mode,
        }
    }

    #[test]
    fn builds_init_message_shape() {
        let tools = vec![
            ToolName { name: "Agent".to_string() },
            ToolName { name: "Bash".to_string() },
        ];
        let mcp_clients = vec![McpClientRef {
            name: "github".to_string(),
            r#type: "stdio".to_string(),
        }];
        let commands = vec![
            CommandLike { name: "init".to_string(), user_invocable: Some(true) },
            CommandLike { name: "internal".to_string(), user_invocable: Some(false) },
        ];
        let agents = vec![AgentRef { agent_type: "general".to_string() }];
        let skills = vec![CommandLike { name: "skill-a".to_string(), user_invocable: None }];
        let plugins = vec![PluginRef {
            name: "p".to_string(),
            path: "/tmp/p".to_string(),
            source: "marketplace".to_string(),
        }];
        let inputs = make_inputs(
            &tools,
            &mcp_clients,
            &commands,
            &agents,
            &skills,
            &plugins,
            "opus-4-7",
            Some(true),
        );
        let msg = build_system_init_message(&inputs);
        assert_eq!(msg["type"], "system");
        assert_eq!(msg["subtype"], "init");
        assert_eq!(msg["cwd"], get_cwd());
        assert_eq!(msg["session_id"], get_session_id().to_string());
        // sdkCompatToolName: Agent -> Task
        assert_eq!(msg["tools"][0], "Task");
        assert_eq!(msg["tools"][1], "Bash");
        assert_eq!(msg["mcp_servers"][0]["status"], "stdio");
        // userInvocable === false 的命令被过滤
        let cmds = msg["slash_commands"].as_array().unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], "init");
        // userInvocable 缺省（None）视为可调用
        let skills_out = msg["skills"].as_array().unwrap();
        assert_eq!(skills_out.len(), 1);
        assert_eq!(msg["plugins"][0]["source"], "marketplace");
        assert_eq!(msg["claude_code_version"], MACRO_VERSION);
        assert_eq!(msg["output_style"], DEFAULT_OUTPUT_STYLE_NAME);
        assert!(!msg["uuid"].as_str().unwrap().is_empty());
        assert!(msg.get("messaging_socket_path").is_none());
    }

    #[test]
    fn sdk_compat_tool_name_remaps_agent_only() {
        assert_eq!(sdk_compat_tool_name("Agent"), "Task");
        assert_eq!(sdk_compat_tool_name("Task"), "Task");
        assert_eq!(sdk_compat_tool_name("Bash"), "Bash");
    }

    #[test]
    fn fast_mode_state_off_for_unsupported_model() {
        assert_eq!(get_fast_mode_state(Some("sonnet-4"), Some(true)), "off");
        assert_eq!(get_fast_mode_state(Some("opus-4-7"), None), "off");
        assert_eq!(get_fast_mode_state(None, Some(true)), "off");
        // 大小写不敏感匹配（对齐 toLowerCase）
        assert_eq!(get_fast_mode_state(Some("Opus-4-7"), Some(true)), "on");
    }

    #[test]
    fn output_style_defaults_when_unset() {
        let tools: Vec<ToolName> = vec![];
        let mcp_clients: Vec<McpClientRef> = vec![];
        let commands: Vec<CommandLike> = vec![];
        let agents: Vec<AgentRef> = vec![];
        let skills: Vec<CommandLike> = vec![];
        let plugins: Vec<PluginRef> = vec![];
        let inputs = make_inputs(
            &tools,
            &mcp_clients,
            &commands,
            &agents,
            &skills,
            &plugins,
            "sonnet",
            None,
        );
        let msg = build_system_init_message(&inputs);
        assert_eq!(msg["output_style"], "default");
        // fastMode 未启用
        assert_eq!(msg["fast_mode_state"], "off");
    }
}
