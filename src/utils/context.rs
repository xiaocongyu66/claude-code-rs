//! context.rs — 模型上下文窗口与最大输出 token 判定。
//!
//! 重写自 `src/utils/context.ts`。常量、`[1m]` 判定、上下文窗口解析
//! 优先级（用户覆盖 → ant 环境变量 → `[1m]` 后缀 → ChatGPT 表 → 能力表
//! → 1M beta → sonnet 实验 → ant 模型表 → 默认 200k）与最大输出 token
//! 阶梯逐条对齐 TS。
//!
//! 已知差异（依赖未转换模块）：
//! - `model/model.ts` 未转换（`getCanonicalName` /
//!   `firstPartyNameToCanonical` / `resolveOverriddenModel`）：本模块内置
//!   最小等价实现（仅供内部使用），待 model.rs 落地后切换为共享实现。
//! - `model/modelCapabilities.ts` 未转换（`getModelCapability`）：
//!   `get_context_window_for_model` 的 `max_input_tokens` 分支与
//!   `get_model_max_output_tokens` 的 `max_tokens` 上限分支按「能力表
//!   为空」语义跳过（其余检测分支与顺序保持不变）。
//! - `model/antModels.ts` 未转换（`resolveAntModel`）：`USER_TYPE ===
//!   'ant'` 的 ant 专用分支跳过（等价于 `resolveAntModel` 返回 None）。
//! - `config.ts` 未转换（`getGlobalConfig`）：
//!   `get_sonnet_1m_exp_treatment_enabled` 的
//!   `clientDataCache.coral_reef_sonnet` 检查按未启用返回 false。
//! - 环境变量数字解析使用严格 `parse::<i64>`（TS `parseInt` 对
//!   "123abc" 返回 123、对 "1e3" 返回 1；非法输入在此被整体忽略）。

use std::env;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::constants::betas::CONTEXT_1M_BETA_HEADER;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::model::chatgpt_models::{
    get_chatgpt_model_context_window, CHATGPT_CODEX_MAX_OUTPUT_TOKENS,
};
use crate::utils::settings::settings::get_initial_settings;

/// 模型上下文窗口默认值（当前所有模型 200k tokens）。
pub const MODEL_CONTEXT_WINDOW_DEFAULT: u32 = 200_000;

/// compact 操作的最大输出 token 数。
pub const COMPACT_MAX_OUTPUT_TOKENS: u32 = 20_000;

/// slot 预留优化的封顶默认值。BQ p99 输出 = 4,911 tokens，32k/64k 默认
/// 超额预留 8-16× slot 容量；启用封顶后 <1% 请求触顶，触顶时在 64k
/// 干净重试一次（见 query.ts 的 max_output_tokens_escalate）。封顶在
/// claude.ts:getMaxOutputTokensForModel 应用，以绕开
/// growthbook→betas→context 的导入环。
pub const CAPPED_DEFAULT_MAX_TOKENS: u32 = 8_000;

/// 触顶降级重试时的最大输出 token 数。
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

/// 默认最大输出 token 数。
const MAX_OUTPUT_TOKENS_DEFAULT: u32 = 32_000;

/// 最大输出 token 数上限。
const MAX_OUTPUT_TOKENS_UPPER_LIMIT: u32 = 64_000;

/// `is1mContextDisabled`：C4E 管理员可通过
/// `CLAUDE_CODE_DISABLE_1M_CONTEXT`（HIPAA 合规）禁用 1M 上下文。
pub fn is_1m_context_disabled() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT").ok().as_deref())
}

/// `has1mContext`：模型 ID 是否带显式 `[1m]` 后缀（不区分大小写）。
pub fn has_1m_context(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    model.to_ascii_lowercase().contains("[1m]")
}

/// `modelSupports1M`：模型家族是否支持 1M 上下文。
// @[MODEL LAUNCH]: 新模型支持 1M 上下文时更新此判定。
pub fn model_supports_1m(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    let canonical = get_canonical_name(model);
    canonical.contains("claude-sonnet-4")
        || canonical.contains("opus-4-6")
        || canonical.contains("opus-4-7")
}

/// `getContextWindowForModel`：解析模型的有效上下文窗口。
///
/// 优先级逐条对齐 TS：
/// 1. settings 的 `contextWindowOverrides`（direct → bare → canonical →
///    canonical-bare 键查找；direct 键存在但 ≤0 时直接落入后续检测）
/// 2. ant 用户的 `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 环境变量
/// 3. `[1m]` 显式后缀 → 1,000,000
/// 4. ChatGPT/GPT-5.6 家族表（1M 禁用时封顶 200k）
/// 5. 模型能力表 `max_input_tokens`（未转换 → 跳过）
/// 6. `context-1m` beta + 家族支持 → 1,000,000
/// 7. sonnet-4-6 实验处置（config 未转换 → 恒 false）
/// 8. ant 模型表（未转换 → 跳过）
/// 9. 默认 200,000
pub fn get_context_window_for_model(model: &str, betas: Option<&[String]>) -> u64 {
    // 用户配置的 per-model 覆盖（via /login）优先级最高——它表达用户对
    // 模型真实上下文窗口的明确意图，内置检测无法知晓三方/代理模型的
    // 真实窗口。
    let overrides = get_initial_settings().context_window_overrides;
    if let Some(overrides) = overrides.as_ref() {
        // 自定义模型可能经运行时别名带 "[1m]" 后缀
        // （parseUserSpecifiedModel 会重新附加），而 /login 存的是裸
        // ID——按 suffixed、bare、canonical 顺序查找。
        let bare = strip_1m_suffix(model);
        // DIRECT model-key 条目无论取值都是权威的：正数生效；0/负数表示
        // 「显式无覆盖」——bare/canonical 兄弟键不得复活，直接落入检测。
        if let Some(direct) = overrides.get(model) {
            if *direct > 0 {
                return *direct as u64;
            }
        } else {
            if let Some(via_bare) = overrides.get(bare) {
                if *via_bare > 0 {
                    return *via_bare as u64;
                }
            }
            let canonical = get_canonical_name(model);
            if canonical != model {
                if let Some(via_canonical) = overrides.get(&canonical) {
                    if *via_canonical > 0 {
                        return *via_canonical as u64;
                    }
                }
            }
            if bare != model {
                let canonical_bare = get_canonical_name(bare);
                if canonical_bare != bare {
                    if let Some(via_canonical_bare) = overrides.get(&canonical_bare) {
                        if *via_canonical_bare > 0 {
                            return *via_canonical_bare as u64;
                        }
                    }
                }
            }
        }
    }

    // 环境变量覆盖（ant 专用）：优先于其余所有窗口解析（含 1M 检测），
    // 让用户在仍使用 1M 端点的同时限制本地决策（auto-compact 等）的窗口。
    if env::var("USER_TYPE").as_deref() == Ok("ant") {
        if let Some(raw) = env::var("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
            .ok()
            .filter(|value| !value.is_empty())
        {
            if let Ok(override_value) = raw.parse::<i64>() {
                if override_value > 0 {
                    return override_value as u64;
                }
            }
        }
    }

    // [1m] 后缀——显式客户端选择，优先于一切检测。
    if has_1m_context(model) {
        return 1_000_000;
    }

    // GPT-5.6 家族：OAuth/Codex ≈ 272k；API key 路径 ≈ 1.05M（model
    // card）。用于 UI 百分比、auto-compact 阈值与本地预算——不作为
    // 请求字段发送（Codex Responses 不接受 max_input_tokens）。
    if let Some(chatgpt_context_window) =
        get_chatgpt_model_context_window(model).map(|window| window as u64)
    {
        if is_1m_context_disabled()
            && chatgpt_context_window > MODEL_CONTEXT_WINDOW_DEFAULT as u64
        {
            return MODEL_CONTEXT_WINDOW_DEFAULT as u64;
        }
        return chatgpt_context_window;
    }

    // 模型能力表（modelCapabilities.ts 未转换 → 跳过）：
    // const cap = getModelCapability(model);
    // if (cap?.max_input_tokens && cap.max_input_tokens >= 100_000) {
    //     if (cap.max_input_tokens > DEFAULT && is1mContextDisabled()) {
    //         return MODEL_CONTEXT_WINDOW_DEFAULT;
    //     }
    //     return cap.max_input_tokens;
    // }

    if let Some(betas) = betas {
        if betas.iter().any(|beta| beta.as_str() == CONTEXT_1M_BETA_HEADER)
            && model_supports_1m(model)
        {
            return 1_000_000;
        }
    }
    if get_sonnet_1m_exp_treatment_enabled(model) {
        return 1_000_000;
    }
    // ant 模型表（antModels.ts 未转换 → 跳过）：
    // if (process.env.USER_TYPE === 'ant') {
    //     const antModel = resolveAntModel(model);
    //     if (antModel?.contextWindow) return antModel.contextWindow;
    // }
    MODEL_CONTEXT_WINDOW_DEFAULT as u64
}

/// `getSonnet1mExpTreatmentEnabled`：sonnet-4-6（无显式 `[1m]` 后缀）
/// 的实验性 1M 上下文处置。
///
/// `getGlobalConfig().clientDataCache?.['coral_reef_sonnet'] === 'true'`
/// 依赖 config.ts（未转换），此处按未启用返回 false（见模块头）。
pub fn get_sonnet_1m_exp_treatment_enabled(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    // 仅对无显式 [1m] 后缀的 sonnet-4-6 生效
    if has_1m_context(model) {
        return false;
    }
    if !get_canonical_name(model).contains("sonnet-4-6") {
        return false;
    }
    false
}

/// 单次请求的输入 token 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// `calculateContextPercentages` 的返回：无用量数据时两项均为 None。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPercentages {
    pub used: Option<u32>,
    pub remaining: Option<u32>,
}

/// `calculateContextPercentages`：由 token 用量计算上下文窗口占用
/// 百分比。零输入 token 与无用量数据同义——避免第三方 API 在
/// message_start 省略 usage 时闪烁 "ctx:0%"。
pub fn calculate_context_percentages(
    current_usage: Option<ContextUsage>,
    context_window_size: u64,
) -> ContextPercentages {
    let Some(usage) = current_usage else {
        return ContextPercentages {
            used: None,
            remaining: None,
        };
    };

    let total_input_tokens =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;

    if total_input_tokens == 0 {
        return ContextPercentages {
            used: None,
            remaining: None,
        };
    }

    let used_percentage = (total_input_tokens as f64 / context_window_size as f64) * 100.0;
    let clamped_used = used_percentage.clamp(0.0, 100.0) as u32;

    ContextPercentages {
        used: Some(clamped_used),
        remaining: Some(100 - clamped_used),
    }
}

/// `getModelMaxOutputTokens` 的返回：默认值与上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMaxOutputTokens {
    pub default: u32,
    pub upper_limit: u32,
}

/// `getModelMaxOutputTokens`：返回模型的默认与上限最大输出 token 数。
pub fn get_model_max_output_tokens(model: &str) -> ModelMaxOutputTokens {
    // ant 分支（antModels.ts 未转换 → 跳过）：
    // if (process.env.USER_TYPE === 'ant') {
    //     const antModel = resolveAntModel(model.toLowerCase());
    //     if (antModel) {
    //         return { default: antModel.defaultMaxTokens ?? DEFAULT,
    //                  upperLimit: antModel.upperMaxTokensLimit ?? UPPER };
    //     }
    // }

    let m = get_canonical_name(model);

    let (default_tokens, upper_limit) =
        // GPT-5.6 家族：官方 128k 最大输出（OpenAI model card）。
        if get_chatgpt_model_context_window(model).is_some() {
            (32_000, CHATGPT_CODEX_MAX_OUTPUT_TOKENS)
        } else if m.contains("opus-4-7") {
            (64_000, 128_000)
        } else if m.contains("opus-4-6") {
            (64_000, 128_000)
        } else if m.contains("sonnet-4-6") {
            (32_000, 128_000)
        } else if m.contains("opus-4-5") || m.contains("sonnet-4") || m.contains("haiku-4") {
            (32_000, 64_000)
        } else if m.contains("opus-4-1") || m.contains("opus-4") {
            (32_000, 32_000)
        } else if m.contains("claude-3-opus") {
            (4_096, 4_096)
        } else if m.contains("claude-3-sonnet") {
            (8_192, 8_192)
        } else if m.contains("claude-3-haiku") {
            (4_096, 4_096)
        } else if m.contains("3-5-sonnet") || m.contains("3-5-haiku") {
            (8_192, 8_192)
        } else if m.contains("3-7-sonnet") {
            (32_000, 64_000)
        } else {
            (MAX_OUTPUT_TOKENS_DEFAULT, MAX_OUTPUT_TOKENS_UPPER_LIMIT)
        };

    // 模型能力表覆盖（modelCapabilities.ts 未转换 → 跳过）：
    // const cap = getModelCapability(model);
    // if (cap?.max_tokens && cap.max_tokens >= 4_096) {
    //     upper_limit = cap.max_tokens;
    //     default_tokens = default_tokens.min(upper_limit);
    // }

    ModelMaxOutputTokens {
        default: default_tokens,
        upper_limit,
    }
}

/// `getMaxThinkingTokensForModel`：最大思考 token 预算——严格小于最大
/// 输出 token 数。较新模型已改用自适应思考而非固定预算，已废弃。
pub fn get_max_thinking_tokens_for_model(model: &str) -> u32 {
    get_model_max_output_tokens(model)
        .upper_limit
        .saturating_sub(1)
}

// --- model/model.ts 最小等价实现（未转换，见模块头） ---

static CANONICAL_FALLBACK_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"claude-(?-u:(\d+-\d+-)?\w+)").ok());

/// 剥离模型 ID 结尾的 `[1m]` 后缀（不区分大小写），
/// 等价 TS `model.replace(/\[1m\]$/i, '')`。
fn strip_1m_suffix(model: &str) -> &str {
    let tail = &model[model.len().saturating_sub(4)..];
    if tail.to_ascii_lowercase() == "[1m]" {
        &model[..model.len() - 4]
    } else {
        model
    }
}

/// `resolveOverriddenModel`：被 settings `modelOverrides` 覆盖的模型 ID
/// （如 Bedrock ARN）反解回 canonical ID；无覆盖时原样返回。
/// 等价 TS 的 try/catch 回退（settings 未就绪时原样返回）。
fn resolve_overridden_model(model_id: &str) -> String {
    if let Some(overrides) = get_initial_settings().model_overrides.as_ref() {
        for (canonical_id, override_id) in overrides {
            if override_id == model_id {
                return canonical_id.clone();
            }
        }
    }
    model_id.to_string()
}

/// `firstPartyNameToCanonical`：剥离 first-party 模型 ID 的日期/provider
/// 后缀（纯字符串匹配，不读 settings，可在模块顶层使用）。输入须为
/// 1P 格式 ID（如 'claude-3-7-sonnet-20250219'、
/// 'us.anthropic.claude-opus-4-6-v1:0'）。
fn first_party_name_to_canonical(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    // Claude 4+ 特例以区分版本；顺序敏感（4-5 先于 4，4-6/4-7 更先）
    if name.contains("claude-opus-4-7") {
        return "claude-opus-4-7".to_string();
    }
    if name.contains("claude-opus-4-6") {
        return "claude-opus-4-6".to_string();
    }
    if name.contains("claude-opus-4-5") {
        return "claude-opus-4-5".to_string();
    }
    if name.contains("claude-opus-4-1") {
        return "claude-opus-4-1".to_string();
    }
    if name.contains("claude-opus-4") {
        return "claude-opus-4".to_string();
    }
    if name.contains("claude-sonnet-4-6") {
        return "claude-sonnet-4-6".to_string();
    }
    if name.contains("claude-sonnet-4-5") {
        return "claude-sonnet-4-5".to_string();
    }
    if name.contains("claude-sonnet-4") {
        return "claude-sonnet-4".to_string();
    }
    if name.contains("claude-haiku-4-5") {
        return "claude-haiku-4-5".to_string();
    }
    // Claude 3.x 使用不同命名方案（claude-3-{family}）
    if name.contains("claude-3-7-sonnet") {
        return "claude-3-7-sonnet".to_string();
    }
    if name.contains("claude-3-5-sonnet") {
        return "claude-3-5-sonnet".to_string();
    }
    if name.contains("claude-3-5-haiku") {
        return "claude-3-5-haiku".to_string();
    }
    if name.contains("claude-3-opus") {
        return "claude-3-opus".to_string();
    }
    if name.contains("claude-3-sonnet") {
        return "claude-3-sonnet".to_string();
    }
    if name.contains("claude-3-haiku") {
        return "claude-3-haiku".to_string();
    }
    if let Some(found) = CANONICAL_FALLBACK_RE
        .as_ref()
        .and_then(|re| re.find(&name))
    {
        return found.as_str().to_string();
    }
    // 无模式匹配时回退原名称
    name
}

/// `getCanonicalName`：全模型 ID → 跨 provider 统一的 canonical 短名。
/// 例如 'claude-3-5-haiku-20241022' 与
/// 'us.anthropic.claude-3-5-haiku-20241022-v1:0' 都映射到
/// 'claude-3-5-haiku'。
fn get_canonical_name(full_model_name: &str) -> String {
    first_party_name_to_canonical(&resolve_overridden_model(full_model_name))
}
