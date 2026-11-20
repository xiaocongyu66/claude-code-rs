//! `services/compact/apiMicrocompact.ts` — API 侧 context management 策略。
//!
//! 语义对齐：`getAPIContextManagement` 按 thinking/redact 状态与
//! `USE_API_CLEAR_TOOL_RESULTS` / `USE_API_CLEAR_TOOL_USES` /
//! `API_MAX_INPUT_TOKENS` / `API_TARGET_INPUT_TOKENS` 环境变量拼装
//! `clear_tool_uses_20250919` / `clear_thinking_20251015` 策略。

use serde::{Deserialize, Serialize};

use crate::utils::env_utils::is_env_truthy;

/// 默认触发阈值（对齐客户端 microcompact 取值）。
const DEFAULT_MAX_INPUT_TOKENS: i64 = 180_000;
/// 默认目标保留量。
const DEFAULT_TARGET_INPUT_TOKENS: i64 = 40_000;

/// TS `TOOLS_CLEARABLE_RESULTS` 的工具名清单。
pub const TOOLS_CLEARABLE_RESULTS: &[&str] = &[
    // SHELL_TOOL_NAMES（Bash + PowerShell）
    "Bash",
    "PowerShell",
    "Glob",
    "Grep",
    "Read",
    "WebFetch",
    "WebSearch",
];

/// TS `TOOLS_CLEARABLE_USES`。
pub const TOOLS_CLEARABLE_USES: &[&str] = &["Edit", "Write", "NotebookEdit"];

/// TS `ContextEditStrategy`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextEditStrategy {
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses {
        trigger: Option<Trigger>,
        keep: Option<Keep>,
        #[serde(rename = "clear_tool_inputs", skip_serializing_if = "Option::is_none")]
        clear_tool_inputs: Option<ClearToolInputs>,
        #[serde(rename = "exclude_tools", skip_serializing_if = "Option::is_none")]
        exclude_tools: Option<Vec<String>>,
        #[serde(rename = "clear_at_least", skip_serializing_if = "Option::is_none")]
        clear_at_least: Option<Trigger>,
    },
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking { keep: ThinkingKeep },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    #[serde(rename = "type")]
    pub typ: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keep {
    #[serde(rename = "type")]
    pub typ: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClearToolInputs {
    Bool(bool),
    Tools(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingKeep {
    All,
}

/// TS `ContextManagementConfig`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextManagementConfig {
    pub edits: Vec<ContextEditStrategy>,
}

fn parse_int_or(raw: Option<String>, default: i64) -> i64 {
    // TS `parseInt` 对非法串得到 NaN，回落默认；Rust 用 parse 失败等价。
    raw.and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

/// TS `getAPIContextManagement({ hasThinking?, isRedactThinkingActive? })`。
pub fn get_api_context_management(
    has_thinking: bool,
    is_redact_thinking_active: bool,
) -> Option<ContextManagementConfig> {
    let mut strategies: Vec<ContextEditStrategy> = Vec::new();

    // 保留 thinking 块；redact-thinking 激活时跳过（无模型可见内容）。
    // CCH 注记：`clearAllThinking / thinking_turns:1` 变体已移除（upstream
    // 2.1.284 无此变体，唯一调用方传 false），回退为 keep:'all'。
    if has_thinking && !is_redact_thinking_active {
        strategies.push(ContextEditStrategy::ClearThinking {
            keep: ThinkingKeep::All,
        });
    }

    let use_clear_tool_results = match std::env::var("USE_API_CLEAR_TOOL_RESULTS") {
        Ok(value) => is_env_truthy(Some(&value)),
        Err(_) => true, // 默认开启
    };
    let use_clear_tool_uses = std::env::var("USE_API_CLEAR_TOOL_USES")
        .map(|v| is_env_truthy(Some(&v)))
        .unwrap_or(false);

    if !use_clear_tool_results && !use_clear_tool_uses {
        return if strategies.is_empty() {
            None
        } else {
            Some(ContextManagementConfig { edits: strategies })
        };
    }

    let trigger_threshold = parse_int_or(
        std::env::var("API_MAX_INPUT_TOKENS").ok(),
        DEFAULT_MAX_INPUT_TOKENS,
    );
    let keep_target = parse_int_or(
        std::env::var("API_TARGET_INPUT_TOKENS").ok(),
        DEFAULT_TARGET_INPUT_TOKENS,
    );

    if use_clear_tool_results {
        strategies.push(ContextEditStrategy::ClearToolUses {
            trigger: Some(Trigger {
                typ: "input_tokens".to_string(),
                value: trigger_threshold,
            }),
            keep: None,
            clear_tool_inputs: Some(ClearToolInputs::Tools(
                TOOLS_CLEARABLE_RESULTS.iter().map(|s| s.to_string()).collect(),
            )),
            exclude_tools: None,
            clear_at_least: Some(Trigger {
                typ: "input_tokens".to_string(),
                value: trigger_threshold - keep_target,
            }),
        });
    }

    if use_clear_tool_uses {
        strategies.push(ContextEditStrategy::ClearToolUses {
            trigger: Some(Trigger {
                typ: "input_tokens".to_string(),
                value: trigger_threshold,
            }),
            keep: None,
            clear_tool_inputs: None,
            exclude_tools: Some(
                TOOLS_CLEARABLE_USES.iter().map(|s| s.to_string()).collect(),
            ),
            clear_at_least: Some(Trigger {
                typ: "input_tokens".to_string(),
                value: trigger_threshold - keep_target,
            }),
        });
    }

    if strategies.is_empty() {
        None
    } else {
        Some(ContextManagementConfig { edits: strategies })
    }
}
