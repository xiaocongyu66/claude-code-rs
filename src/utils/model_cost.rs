//! model_cost.rs — 模型计费表与 USD 成本计算。
//!
//! 重写自 `src/utils/modelCost.ts`。计费层级常量与 `MODEL_COSTS` 表逐条
//! 忠实搬运（键等价于 TS 运行时的
//! `firstPartyNameToCanonical(CLAUDE_*_CONFIG.firstParty)` 结果）；
//! 货币格式化规则（整数不带小数、其余两位小数）与 TS 对齐。
//!
//! 已知差异（依赖未转换模块）：
//! - `model/model.ts` 未转换（`getCanonicalName` /
//!   `firstPartyNameToCanonical` / `resolveOverriddenModel`）：本模块内置
//!   最小等价实现（仅供内部使用），待 model.rs 落地后切换为共享实现。
//! - `fastMode.ts` 未转换（`isFastModeEnabled`）：`get_opus46_cost_tier`
//!   内联相同的 `CLAUDE_CODE_DISABLE_FAST_MODE` 环境变量检查（语义一致）。
//! - `services/analytics` 未转换（`logEvent`）：未知模型计费只调用
//!   `bootstrap::state::set_has_unknown_model_cost()`，事件上报省略。
//! - `model/model.ts` 的 `getDefaultMainLoopModelSetting` 未转换：
//!   未知模型回退采用 `DEFAULT_UNKNOWN_MODEL_COST`（TS 会回退到默认
//!   主循环模型的费率）。
//! - 货币小数舍入：JS `toFixed(2)` 四舍五入，Rust `{:.2}` 为银行家
//!   舍入；现有计费表取值（0.8 等）两种规则结果一致，不受影响。

use std::env;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::bootstrap::state::set_has_unknown_model_cost;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::settings::settings::get_initial_settings;

/// `ModelCosts`：每 Mtok 的输入/输出/缓存写入/缓存读取单价，以及每次
/// web 搜索请求的价格（USD）。
/// @see https://platform.claude.com/docs/en/about-claude/pricing
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCosts {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub prompt_cache_write_tokens: f64,
    pub prompt_cache_read_tokens: f64,
    pub web_search_requests: f64,
}

/// Sonnet 标准计费层：$3 输入 / $15 输出每 Mtok。
pub const COST_TIER_3_15: ModelCosts = ModelCosts {
    input_tokens: 3.0,
    output_tokens: 15.0,
    prompt_cache_write_tokens: 3.75,
    prompt_cache_read_tokens: 0.3,
    web_search_requests: 0.01,
};

/// Opus 4 / 4.1 计费层：$15 输入 / $75 输出每 Mtok。
pub const COST_TIER_15_75: ModelCosts = ModelCosts {
    input_tokens: 15.0,
    output_tokens: 75.0,
    prompt_cache_write_tokens: 18.75,
    prompt_cache_read_tokens: 1.5,
    web_search_requests: 0.01,
};

/// Opus 4.5 计费层：$5 输入 / $25 输出每 Mtok。
pub const COST_TIER_5_25: ModelCosts = ModelCosts {
    input_tokens: 5.0,
    output_tokens: 25.0,
    prompt_cache_write_tokens: 6.25,
    prompt_cache_read_tokens: 0.5,
    web_search_requests: 0.01,
};

/// Opus 4.6 fast mode 计费：$30 输入 / $150 输出每 Mtok。
pub const COST_TIER_30_150: ModelCosts = ModelCosts {
    input_tokens: 30.0,
    output_tokens: 150.0,
    prompt_cache_write_tokens: 37.5,
    prompt_cache_read_tokens: 3.0,
    web_search_requests: 0.01,
};

/// Haiku 3.5 计费：$0.80 输入 / $4 输出每 Mtok。
pub const COST_HAIKU_35: ModelCosts = ModelCosts {
    input_tokens: 0.8,
    output_tokens: 4.0,
    prompt_cache_write_tokens: 1.0,
    prompt_cache_read_tokens: 0.08,
    web_search_requests: 0.01,
};

/// Haiku 4.5 计费：$1 输入 / $5 输出每 Mtok。
pub const COST_HAIKU_45: ModelCosts = ModelCosts {
    input_tokens: 1.0,
    output_tokens: 5.0,
    prompt_cache_write_tokens: 1.25,
    prompt_cache_read_tokens: 0.1,
    web_search_requests: 0.01,
};

/// 未知模型的默认费率。
const DEFAULT_UNKNOWN_MODEL_COST: ModelCosts = COST_TIER_5_25;

/// `getOpus46CostTier`：按 fast mode 取 Opus 4.6 的计费层。
pub fn get_opus46_cost_tier(fast_mode: bool) -> ModelCosts {
    if is_fast_mode_enabled() && fast_mode {
        COST_TIER_30_150
    } else {
        COST_TIER_5_25
    }
}

/// `isFastModeEnabled`（fastMode.ts 未转换，内置等价实现）：
/// `CLAUDE_CODE_DISABLE_FAST_MODE` 未置真值时 fast mode 可用。
fn is_fast_mode_enabled() -> bool {
    !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_FAST_MODE").ok().as_deref())
}

/// `MODEL_COSTS`：canonical 短名 → 计费表。
///
/// @[MODEL LAUNCH]: 新模型在此登记计费条目。
/// 计费来源 https://platform.claude.com/docs/en/about-claude/pricing
/// web 搜索：每 1000 次请求 $10 = 每次请求 $0.01
pub static MODEL_COSTS: &[(&str, ModelCosts)] = &[
    // firstPartyNameToCanonical(CLAUDE_3_5_HAIKU_CONFIG.firstParty)
    ("claude-3-5-haiku", COST_HAIKU_35),
    // firstPartyNameToCanonical(CLAUDE_HAIKU_4_5_CONFIG.firstParty)
    ("claude-haiku-4-5", COST_HAIKU_45),
    // firstPartyNameToCanonical(CLAUDE_3_5_V2_SONNET_CONFIG.firstParty)
    ("claude-3-5-sonnet", COST_TIER_3_15),
    // firstPartyNameToCanonical(CLAUDE_3_7_SONNET_CONFIG.firstParty)
    ("claude-3-7-sonnet", COST_TIER_3_15),
    // firstPartyNameToCanonical(CLAUDE_SONNET_4_CONFIG.firstParty)
    ("claude-sonnet-4", COST_TIER_3_15),
    // firstPartyNameToCanonical(CLAUDE_SONNET_4_5_CONFIG.firstParty)
    ("claude-sonnet-4-5", COST_TIER_3_15),
    // firstPartyNameToCanonical(CLAUDE_SONNET_4_6_CONFIG.firstParty)
    ("claude-sonnet-4-6", COST_TIER_3_15),
    // firstPartyNameToCanonical(CLAUDE_OPUS_4_CONFIG.firstParty)
    ("claude-opus-4", COST_TIER_15_75),
    // firstPartyNameToCanonical(CLAUDE_OPUS_4_1_CONFIG.firstParty)
    ("claude-opus-4-1", COST_TIER_15_75),
    // firstPartyNameToCanonical(CLAUDE_OPUS_4_5_CONFIG.firstParty)
    ("claude-opus-4-5", COST_TIER_5_25),
    // firstPartyNameToCanonical(CLAUDE_OPUS_4_6_CONFIG.firstParty)
    ("claude-opus-4-6", COST_TIER_5_25),
];

/// 按 canonical 短名查计费表。
fn cost_by_canonical(short_name: &str) -> Option<ModelCosts> {
    MODEL_COSTS
        .iter()
        .find(|(key, _)| *key == short_name)
        .map(|(_, costs)| *costs)
}

/// 计费用量（TS `BetaUsage` 的最小等价子集）：token 原始计数，缺省字段
/// 按 0 计；`speed` 为 fast mode 速度标记（"fast"）。
#[derive(Debug, Clone, PartialEq)]
pub struct CostUsage {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cache_read_input_tokens: Option<f64>,
    pub cache_creation_input_tokens: Option<f64>,
    pub web_search_requests: Option<f64>,
    pub speed: Option<String>,
}

/// `tokensToUSDCost`：按计费表把 token 用量换算为 USD。
fn tokens_to_usd_cost(model_costs: &ModelCosts, usage: &CostUsage) -> f64 {
    (usage.input_tokens / 1_000_000.0) * model_costs.input_tokens
        + (usage.output_tokens / 1_000_000.0) * model_costs.output_tokens
        + (usage.cache_read_input_tokens.unwrap_or(0.0) / 1_000_000.0)
            * model_costs.prompt_cache_read_tokens
        + (usage.cache_creation_input_tokens.unwrap_or(0.0) / 1_000_000.0)
            * model_costs.prompt_cache_write_tokens
        + usage.web_search_requests.unwrap_or(0.0) * model_costs.web_search_requests
}

/// `getModelCosts`：取模型的计费表。
///
/// Opus 4.6 按 `usage.speed` 走 fast 层；未知模型标记
/// `has_unknown_model_cost` 并回退默认费率。
pub fn get_model_costs(model: &str, usage: &CostUsage) -> ModelCosts {
    let short_name = get_canonical_name(model);

    // Opus 4.6 + fast mode：short_name ===
    // firstPartyNameToCanonical(CLAUDE_OPUS_4_6_CONFIG.firstParty)
    if short_name == "claude-opus-4-6" {
        let is_fast_mode = usage.speed.as_deref() == Some("fast");
        return get_opus46_cost_tier(is_fast_mode);
    }

    if let Some(costs) = cost_by_canonical(&short_name) {
        return costs;
    }
    track_unknown_model_cost();
    // TS 回退：MODEL_COSTS[getCanonicalName(getDefaultMainLoopModelSetting())]
    // ?? DEFAULT_UNKNOWN_MODEL_COST。getDefaultMainLoopModelSetting 依赖的
    // auth/ant 模块未转换 → 回退 DEFAULT_UNKNOWN_MODEL_COST（见模块头）。
    DEFAULT_UNKNOWN_MODEL_COST
}

/// `trackUnknownModelCost`：标记未知模型计费。
/// logEvent('tengu_unknown_model_cost', ...) 上报省略（analytics 未转换）。
fn track_unknown_model_cost() {
    set_has_unknown_model_cost();
}

/// `calculateUSDCost`：按解析后的模型与用量计算 USD 成本；
/// 模型无计费条目时用默认费率。
pub fn calculate_usd_cost(resolved_model: &str, usage: &CostUsage) -> f64 {
    tokens_to_usd_cost(&get_model_costs(resolved_model, usage), usage)
}

/// 不经完整 BetaUsage 的原始 token 计数（分类器等侧链查询独立记账）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenCounts {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cache_read_input_tokens: f64,
    pub cache_creation_input_tokens: f64,
}

/// `calculateCostFromTokens`：由原始 token 计数算成本。
pub fn calculate_cost_from_tokens(model: &str, tokens: &TokenCounts) -> f64 {
    let usage = CostUsage {
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        cache_read_input_tokens: Some(tokens.cache_read_input_tokens),
        cache_creation_input_tokens: Some(tokens.cache_creation_input_tokens),
        web_search_requests: None,
        speed: None,
    };
    calculate_usd_cost(model, &usage)
}

/// `formatPrice`：整数不带小数，其余两位小数
/// （3 → "$3"，0.8 → "$0.80"，22.5 → "$22.50"）。
fn format_price(price: f64) -> String {
    if price.fract() == 0.0 {
        format!("${price:.0}")
    } else {
        format!("${price:.2}")
    }
}

/// `formatModelPricing`：计费表的展示字符串，如 "$3/$15 per Mtok"。
pub fn format_model_pricing(costs: &ModelCosts) -> String {
    format!(
        "{}/{} per Mtok",
        format_price(costs.input_tokens),
        format_price(costs.output_tokens)
    )
}

/// `getModelPricingString`：模型的展示计费字符串（接受短名或全名）；
/// 无计费条目返回 None。
pub fn get_model_pricing_string(model: &str) -> Option<String> {
    let short_name = get_canonical_name(model);
    cost_by_canonical(&short_name).map(|costs| format_model_pricing(&costs))
}

// --- model/model.ts 最小等价实现（未转换，见模块头） ---

static CANONICAL_FALLBACK_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"claude-(?-u:(\d+-\d+-)?\w+)").ok());

/// `resolveOverriddenModel`：被 settings `modelOverrides` 覆盖的模型 ID
/// （如 Bedrock ARN）反解回 canonical ID；无覆盖时原样返回。
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
/// 后缀（纯字符串匹配，不读 settings，可在模块顶层使用）。
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
