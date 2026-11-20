//! chatgpt_models.rs — ChatGPT / Codex 模型表与 tier 解析。
//!
//! 重写自 `src/utils/model/chatgptModels.ts`。模型常量、tier 解析优先级、
//! 上下文窗口判定逐条对齐 TS。

use std::env;

use once_cell::sync::Lazy;

/// ChatGPT / Codex 模型选项（picker 展示用）。
pub struct ChatGptCodexModelOption {
    pub value: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// 默认 ChatGPT/Codex 模型（最新前沿）。
pub const CHATGPT_CODEX_DEFAULT_MODEL: &str = "gpt-5.6-sol";
pub const CHATGPT_CODEX_BALANCED_MODEL: &str = "gpt-5.6-terra";
/// 轻量任务的快速/小模型默认。
pub const CHATGPT_CODEX_FAST_MODEL: &str = "gpt-5.6-luna";

/// `ChatGPTCodexModelTier`：opus / sonnet / haiku 三档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatGptCodexModelTier {
    Opus,
    Sonnet,
    Haiku,
}

impl ChatGptCodexModelTier {
    /// `CHATGPT_CODEX_MODELS_BY_TIER`：tier → 默认模型。
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Opus => CHATGPT_CODEX_DEFAULT_MODEL,
            Self::Sonnet => CHATGPT_CODEX_BALANCED_MODEL,
            Self::Haiku => CHATGPT_CODEX_FAST_MODEL,
        }
    }
}

/// `resolveChatGPTCodexModelForTier`：taskOverride ?? tierOverride ?? (isChatGPTAuth ? 默认 : undefined)。
///
/// 注意 TS 的 `??` 不把空串当 null：`process.env.X` 为 `''` 时 `'' ?? y` 返回 `''`，
/// 故 `Some("")` 也直接返回（调用方传 env 原值即可）。
pub fn resolve_chatgpt_codex_model_for_tier(
    tier: ChatGptCodexModelTier,
    is_chatgpt_auth: bool,
    tier_override: Option<&str>,
    task_override: Option<&str>,
) -> Option<String> {
    task_override
        .map(str::to_string)
        .or_else(|| tier_override.map(str::to_string))
        .or_else(|| {
            if is_chatgpt_auth {
                Some(tier.default_model().to_string())
            } else {
                None
            }
        })
}

/// ChatGPT OAuth / Codex 订阅的实际上下文窗口。
/// Codex with ChatGPT login 受产品限制为 ~272k（非完整 API 1.05M）。
pub const CHATGPT_OAUTH_CONTEXT_WINDOW: u32 = 272_000;

/// OpenAI API model card 上 GPT-5.6 家族的上下文窗口（API key 路径）。
/// 超过 272k 输入 token 适用长上下文计价。
pub const CHATGPT_API_CONTEXT_WINDOW: u32 = 1_050_000;

/// @deprecated 使用 CHATGPT_OAUTH_CONTEXT_WINDOW 或 CHATGPT_API_CONTEXT_WINDOW。
pub const CHATGPT_CODEX_CONTEXT_WINDOW: u32 = CHATGPT_OAUTH_CONTEXT_WINDOW;

/// GPT-5.6 家族官方最大输出 token（OpenAI model card）。
pub const CHATGPT_CODEX_MAX_OUTPUT_TOKENS: u32 = 128_000;

/// ChatGPT OAuth / Codex 模型 picker 选项。最新 GPT-5.6 家族在前，
/// 上一代模型为仍在使用这些 ID 的用户保留。
pub static CHATGPT_CODEX_MODEL_OPTIONS: Lazy<Vec<ChatGptCodexModelOption>> = Lazy::new(|| {
    vec![
        ChatGptCodexModelOption {
            value: "gpt-5.6-sol",
            label: "gpt-5.6-sol",
            description: "Frontier model for complex coding, research, and real-world work",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.6-terra",
            label: "gpt-5.6-terra",
            description: "Strong model for everyday coding",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.6-luna",
            label: "gpt-5.6-luna",
            description: "Small, fast, and cost-efficient model for simpler coding tasks",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.5",
            label: "GPT-5.5",
            description: "Frontier model for complex coding, research, and real-world work",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.4",
            label: "GPT-5.4",
            description: "Strong model for everyday coding",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.4-mini",
            label: "GPT-5.4-Mini",
            description: "Small, fast, and cost-efficient model for simpler coding tasks",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.3-codex",
            label: "GPT-5.3-Codex",
            description: "Coding-optimized model",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.3-codex-spark",
            label: "GPT-5.3-Codex-Spark",
            description: "Ultra-fast coding model",
        },
        ChatGptCodexModelOption {
            value: "gpt-5.2",
            label: "GPT-5.2",
            description: "Optimized for professional work and long-running agents",
        },
    ]
});

/// `isChatGPTAuthMode`：OPENAI_AUTH_MODE === 'chatgpt'。
pub fn is_chatgpt_auth_mode() -> bool {
    env::var("OPENAI_AUTH_MODE").as_deref() == Ok("chatgpt")
}

/// `normalizeChatGPTModelId`：小写化并剥离结尾的 `[1m]`。
fn normalize_chatgpt_model_id(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    lower
        .strip_suffix("[1m]")
        .map(str::to_string)
        .unwrap_or(lower)
}

/// 是否为 GPT-5.6 家族模型 ID（Sol/Terra/Luna 或裸 `gpt-5.6`）。
pub fn is_gpt56_family_model(model: &str) -> bool {
    let normalized = normalize_chatgpt_model_id(model);
    normalized == "gpt-5.6" || normalized.starts_with("gpt-5.6-")
}

/// `isChatGPTCodexReasoningModel`：GPT-5.6 家族或已知选项表中的模型。
pub fn is_chatgpt_codex_reasoning_model(model: &str) -> bool {
    let normalized = normalize_chatgpt_model_id(model);
    if is_gpt56_family_model(model) {
        return true;
    }
    CHATGPT_CODEX_MODEL_OPTIONS
        .iter()
        .any(|option| option.value.to_ascii_lowercase() == normalized)
}

/// CCB 本地预算（状态栏百分比、auto-compact 阈值）使用的 GPT-5.6 上下文窗口。
/// 不作为请求字段发送。
///
/// - ChatGPT OAuth / Codex 后端：272k（订阅产品限制）
/// - API key / OpenAI 兼容端点使用 gpt-5.6-*：1.05M（model card）
pub fn get_chatgpt_model_context_window(model: &str) -> Option<u32> {
    if !is_gpt56_family_model(model) {
        return None;
    }
    Some(if is_chatgpt_auth_mode() {
        CHATGPT_OAUTH_CONTEXT_WINDOW
    } else {
        CHATGPT_API_CONTEXT_WINDOW
    })
}
