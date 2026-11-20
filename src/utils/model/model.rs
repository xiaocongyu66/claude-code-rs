//! model.rs — 模型映射主解析器（model 层根节点）。
//!
//! 重写自 `src/utils/model/model.ts`。模型 ID 映射表、`getMainLoopModel`
//! 优先级链（会话内 override > --model flag > `ANTHROPIC_MODEL` > settings >
//! 内置默认）、`parseUserSpecifiedModel` 的 alias/legacy/ant 分支与
//! `[1m]` 后缀处理逐条对齐 TS。
//!
//! ## 占位函数（依赖模块未转换）
//! - `antModels.ts`（growthbook flag `tengu_ant_model_override`）未转换：
//!   [`get_ant_model_override_config`] / [`resolve_ant_model`] 为占位，
//!   恒返回 `None`。`USER_TYPE === 'ant'` 的 ant 模型表分支等价于
//!   `resolveAntModel` 返回 `undefined`（parseUserSpecifiedModel /
//!   renderModelName / getDefaultMainLoopModelSetting 中的 ant 分支保留
//!   结构，但走 fall-through）。
//!
//! ## 内联实现的未转模块逻辑
//! - `modelStrings.ts` 未转换：`getModelStrings` / `resolveOverriddenModel`
//!   按 TS 语义内联（builtin configs + settings `modelOverrides` 覆盖 +
//!   bootstrap state 缓存）。**已知偏差**：TS 在 Bedrock 下会后台拉取
//!   inference profiles（`bedrock.ts` 未转换）并覆盖缓存，本端口一律
//!   使用硬编码的 bedrock fallback ID。
//! - `modelAllowlist.ts` 未转换：[`is_model_allowed`] 及其辅助函数按
//!   TS 语义内联完整实现（本模块的 `parse_user_specified_model` 与
//!   `resolve_overridden_model` 可覆盖其全部依赖）。
//!
//! ## 其他已知偏差
//! - 类型映射：`ModelName` / `ModelShortName` = `String`（`configs.rs` 中
//!   的同名类型为 `&'static str`，供静态配置表使用，两者路径独立、
//!   结构等价）；`ModelSetting` = `Option<String>`，与
//!   `bootstrap::state::ModelSetting` 对齐。
//! - TS 的 `process.env.X` 真值判定（`if` / `||`）只接受非空字符串，
//!   相应分支使用 `env_nonempty`；`??` 语义处保留空串（见
//!   `chatgpt_models.rs` 的 `??` 说明）。
//! - TS `try { getInitialSettings() } catch` 的容错在 Rust 侧不需要：
//!   `get_initial_settings` 解析失败时返回默认值，不抛错。
//! - `formatCtxTokens` 的 `toFixed(1)` 与 Rust `{:.1}` 在二进制浮点
//!   边界值上理论上可能相差一个 ulp 的舍入（实践中 token 为整数、
//!   除数为 10 的幂，两者一致）。

use std::env;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::bootstrap::state::{
    get_main_loop_model_override, get_model_strings as get_model_strings_state, set_model_strings,
    ModelStrings,
};
use crate::constants::figures::LIGHTNING_BOLT;
use crate::types::permissions::PermissionMode;
use crate::utils::auth::{
    get_subscription_type, is_claude_ai_subscriber, is_max_subscriber, is_pro_subscriber,
    is_team_premium_subscriber,
};
use crate::utils::context::{has_1m_context, is_1m_context_disabled, model_supports_1m};
use crate::utils::env_utils::is_env_truthy;
use crate::utils::model::aliases::{is_model_alias, is_model_family_alias};
use crate::utils::model::chatgpt_models::{
    is_chatgpt_auth_mode, resolve_chatgpt_codex_model_for_tier, ChatGptCodexModelTier,
};
use crate::utils::model::configs::{ALL_MODEL_CONFIGS, CANONICAL_ID_TO_KEY, ModelKey};
use crate::utils::model::provider_classification::ApiProvider;
use crate::utils::model::providers::{get_api_provider, is_first_party_anthropic_base_url};
use crate::utils::model_cost::{format_model_pricing, get_opus46_cost_tier};
use crate::utils::settings::settings::get_initial_settings;
use crate::utils::string_utils::capitalize;

/// `ModelShortName = string`：跨 provider 统一的 canonical 短名。
pub type ModelShortName = String;

/// `ModelName = string`：完整模型 ID。
pub type ModelName = String;

/// `ModelSetting = ModelName | ModelAlias | null`：`None` 表示显式默认，
/// 外层 `Option<Option<String>>` 的 `None` 表示「未配置」（见
/// [`get_user_specified_model_setting`]）。
pub type ModelSetting = Option<String>;

// ---------------------------------------------------------------------------
// antModels.ts 占位（growthbook 未转换）
// ---------------------------------------------------------------------------

/// `AntModelOverrideConfig` 最小等价结构（antModels.ts 未转换）。
#[derive(Debug, Clone, Default)]
pub struct AntModelOverrideConfig {
    /// TS `defaultModel?: string`。
    pub default_model: Option<String>,
}

/// `AntModel` 最小等价结构（antModels.ts 未转换）。
#[derive(Debug, Clone)]
pub struct AntModel {
    /// TS `model: string`。
    pub model: String,
}

/// TS `getAntModelOverrideConfig`：`USER_TYPE === 'ant'` 时读取 growthbook
/// flag `tengu_ant_model_override`。**占位**——growthbook 未转换，恒返回
/// `None`（ant 行为降级，见模块头）。
pub fn get_ant_model_override_config() -> Option<AntModelOverrideConfig> {
    None
}

/// TS `resolveAntModel`：按 alias / 模型子串匹配 ant 模型表。
/// **占位**——同上，恒返回 `None`。
pub fn resolve_ant_model(_model: Option<&str>) -> Option<AntModel> {
    None
}

// ---------------------------------------------------------------------------
// 环境变量辅助
// ---------------------------------------------------------------------------

/// JS 真值语义的环境变量读取：仅当已设置且非空时返回 `Some`。
///
/// 对齐 TS 中 `if (process.env.X)` / `process.env.X || fallback` 的判定
/// （空字符串为 falsy）；需要 `??` 语义（空串有效）的调用点直接用
/// `env::var(key).ok()`。
fn env_nonempty(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn is_ant_user() -> bool {
    env::var("USER_TYPE").as_deref() == Ok("ant")
}

/// 剥离模型 ID 结尾的 `[1m]` 后缀（不区分大小写），
/// 等价 TS `model.replace(/\[1m\]$/i, '')`。
fn strip_1m_suffix(model: &str) -> String {
    let tail = &model[model.len().saturating_sub(4)..];
    if tail.eq_ignore_ascii_case("[1m]") {
        model[..model.len() - 4].to_string()
    } else {
        model.to_string()
    }
}

// ---------------------------------------------------------------------------
// modelStrings.ts 内联实现（未转换）
// ---------------------------------------------------------------------------

/// `getBuiltinModelStrings(provider)`：ALL_MODEL_CONFIGS 按 provider
/// 取专属 ID。
fn builtin_model_strings(provider: ApiProvider) -> ModelStrings {
    ALL_MODEL_CONFIGS
        .iter()
        .map(|(key, config)| {
            (
                key.as_str().to_string(),
                config.for_provider(provider).to_string(),
            )
        })
        .collect()
}

/// `applyModelOverrides`：把 settings `modelOverrides`（键为 canonical
/// first-party ID）叠加到 provider 派生的模型字符串上。空串覆盖按 TS
/// 的 `if (key && override)` 跳过。
fn apply_model_overrides(ms: &ModelStrings) -> ModelStrings {
    let Some(overrides) = get_initial_settings().model_overrides else {
        return ms.clone();
    };
    let mut out = ms.clone();
    for (canonical_id, override_id) in &overrides {
        if override_id.is_empty() {
            continue;
        }
        if let Some(key) = CANONICAL_ID_TO_KEY.get(canonical_id.as_str()) {
            out.insert(key.as_str().to_string(), override_id.clone());
        }
    }
    out
}

/// `getModelStrings`（modelStrings.ts 未转换，内联）：provider 专属模型
/// ID 表，带 settings 覆盖与 bootstrap state 缓存。
///
/// 已知偏差：TS 在 Bedrock 下后台拉取 inference profiles 覆盖缓存
/// （`bedrock.ts` 未转换），本端口始终使用硬编码 fallback ID。
pub fn get_model_strings() -> ModelStrings {
    if let Some(cached) = get_model_strings_state() {
        return apply_model_overrides(&cached);
    }
    let builtin = builtin_model_strings(get_api_provider());
    // 非 Bedrock 同步落缓存；Bedrock 在 TS 端靠后台 profile 拉取覆盖，
    // 此处无该能力，直接落 fallback 缓存（值与 TS 的 interim 返回一致）。
    set_model_strings(builtin.clone());
    apply_model_overrides(&builtin)
}

/// `resolveOverriddenModel`：被 settings `modelOverrides` 覆盖的模型 ID
/// （如 Bedrock ARN）反解回 canonical ID；无覆盖时原样返回。
pub fn resolve_overridden_model(model_id: &str) -> String {
    if let Some(overrides) = get_initial_settings().model_overrides {
        for (canonical_id, override_id) in &overrides {
            if override_id == model_id {
                return canonical_id.clone();
            }
        }
    }
    model_id.to_string()
}

/// 模型字符串表取键值（缺失键时返回空串；builtin 表保证 13 键齐全）。
fn ms_id(ms: &ModelStrings, key: ModelKey) -> String {
    ms.get(key.as_str()).cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// tier → 默认模型解析
// ---------------------------------------------------------------------------

/// `OPENAI_DEFAULT_MODEL_ENV_BY_TIER`：tier → 环境变量名。
fn openai_default_model_env(tier: ChatGptCodexModelTier) -> &'static str {
    match tier {
        ChatGptCodexModelTier::Opus => "OPENAI_DEFAULT_OPUS_MODEL",
        ChatGptCodexModelTier::Sonnet => "OPENAI_DEFAULT_SONNET_MODEL",
        ChatGptCodexModelTier::Haiku => "OPENAI_DEFAULT_HAIKU_MODEL",
    }
}

/// `getOpenAIModelForTier`：OpenAI provider 的 tier 默认模型。
/// 调用方以真值判定消费结果，故空串过滤为 `None`（对齐 `if (openAIModel)`）。
fn get_openai_model_for_tier(provider: ApiProvider, tier: ChatGptCodexModelTier) -> Option<String> {
    if provider != ApiProvider::Openai {
        return None;
    }
    let tier_override = env::var(openai_default_model_env(tier)).ok();
    resolve_chatgpt_codex_model_for_tier(
        tier,
        is_chatgpt_auth_mode(),
        tier_override.as_deref(),
        None,
    )
    .filter(|model| !model.is_empty())
}

/// `getSmallFastModel`：后台/轻量任务用的小模型。
pub fn get_small_fast_model() -> ModelName {
    let provider = get_api_provider();
    if provider == ApiProvider::Openai && is_chatgpt_auth_mode() {
        let tier_override = env::var("OPENAI_DEFAULT_HAIKU_MODEL").ok();
        let task_override = env::var("OPENAI_SMALL_FAST_MODEL").ok();
        let chatgpt_model = resolve_chatgpt_codex_model_for_tier(
            ChatGptCodexModelTier::Haiku,
            true,
            tier_override.as_deref(),
            task_override.as_deref(),
        )
        .filter(|model| !model.is_empty());
        if let Some(model) = chatgpt_model {
            return model;
        }
    }
    // Provider 专属 small fast 模型
    if provider == ApiProvider::Openai {
        if let Some(model) = env_nonempty("OPENAI_SMALL_FAST_MODEL") {
            return model;
        }
    }
    if provider == ApiProvider::Gemini {
        if let Some(model) = env_nonempty("GEMINI_SMALL_FAST_MODEL") {
            return model;
        }
    }
    // Anthropic 专属或兜底
    env_nonempty("ANTHROPIC_SMALL_FAST_MODEL").unwrap_or_else(|_| get_default_haiku_model())
}

/// `isNonCustomOpusModel`：是否为内置 Opus 模型（非自定义 ID）。
pub fn is_non_custom_opus_model(model: &str) -> bool {
    let ms = get_model_strings();
    [
        ModelKey::Opus40,
        ModelKey::Opus41,
        ModelKey::Opus45,
        ModelKey::Opus46,
        ModelKey::Opus47,
    ]
    .into_iter()
    .any(|key| ms_id(&ms, key) == model)
}

// ---------------------------------------------------------------------------
// 用户指定模型与主循环模型
// ---------------------------------------------------------------------------

/// TS `getUserSpecifiedModelSetting`：读取用户指定（可能为 alias）。
///
/// 优先级：会话内 `/model` override（最高）→ settings 中的
/// `ANTHROPIC_MODEL` → settings.model。`None` 表示未配置或被
/// `availableModels` 白名单拒绝，`Some(None)` 表示显式默认。
pub fn get_user_specified_model_setting() -> Option<ModelSetting> {
    let specified: ModelSetting = match get_main_loop_model_override() {
        Some(model_override) => Some(model_override),
        None => env_nonempty("ANTHROPIC_MODEL")
            .or_else(|| get_initial_settings().model.filter(|m| !m.is_empty())),
    };

    // 用户指定模型不在 availableModels 白名单内时忽略（modelAllowlist.ts
    // 未转换，is_model_allowed 为本模块内联实现）。
    match specified {
        Some(ref model) if !model.is_empty() && !is_model_allowed(model) => None,
        other => Some(other),
    }
}

/// `getMainLoopModel`：当前会话使用的主循环模型。
///
/// 优先级：会话内 override → `--model` flag → `ANTHROPIC_MODEL` →
/// settings → 内置默认。
pub fn get_main_loop_model() -> ModelName {
    match get_user_specified_model_setting() {
        Some(Some(model)) => parse_user_specified_model(&model),
        Some(None) | None => get_default_main_loop_model(),
    }
}

/// `getBestModel`：当前最优模型（默认 Opus）。
pub fn get_best_model() -> ModelName {
    get_default_opus_model()
}

/// `getProviderPrimaryModel`：provider 的主模型环境变量
/// （`OPENAI_MODEL` / `GEMINI_MODEL` / `GROK_MODEL`）。
///
/// 调用方以真值消费，空串按未设置处理（`env_nonempty`）。Bedrock /
/// Vertex / Foundry / firstParty 无主模型环境变量，返回 `None`。
fn get_provider_primary_model() -> Option<ModelName> {
    match get_api_provider() {
        ApiProvider::Openai => env_nonempty("OPENAI_MODEL"),
        ApiProvider::Gemini => env_nonempty("GEMINI_MODEL"),
        ApiProvider::Grok => env_nonempty("GROK_MODEL"),
        _ => None,
    }
}

// @[MODEL LAUNCH]: 新模型发布时更新默认 Opus ID（3P provider 可能滞后，
// 故默认值不变）。
/// `getDefaultOpusModel`：默认 Opus 模型。
pub fn get_default_opus_model() -> ModelName {
    let provider = get_api_provider();
    if let Some(model) = get_openai_model_for_tier(provider, ChatGptCodexModelTier::Opus) {
        return model;
    }
    // Gemini provider 的 GEMINI_DEFAULT_OPUS_MODEL
    if provider == ApiProvider::Gemini {
        if let Some(model) = env_nonempty("GEMINI_DEFAULT_OPUS_MODEL") {
            return model;
        }
    }
    // Anthropic 专属覆盖（first-party 与其余 3P provider 共用）
    if let Some(model) = env_nonempty("ANTHROPIC_DEFAULT_OPUS_MODEL") {
        return model;
    }
    // 3P provider：用户已设主模型（如 OPENAI_MODEL=glm-5.1）时回退到它，
    // 避免 sideQuery / 后台任务打到 Anthropic API。
    if let Some(model) = get_provider_primary_model() {
        return model;
    }
    // TS 两个分支返回值相同（3P 与 firstParty 均为 opus47）。
    ms_id(&get_model_strings(), ModelKey::Opus47)
}

// @[MODEL LAUNCH]: 新模型发布时更新默认 Sonnet ID（3P provider 可能滞后）。
/// `getDefaultSonnetModel`：默认 Sonnet 模型（firstParty 为 4.6，
/// 其余 provider 回退 4.5）。
pub fn get_default_sonnet_model() -> ModelName {
    let provider = get_api_provider();
    if let Some(model) = get_openai_model_for_tier(provider, ChatGptCodexModelTier::Sonnet) {
        return model;
    }
    if provider == ApiProvider::Gemini {
        if let Some(model) = env_nonempty("GEMINI_DEFAULT_SONNET_MODEL") {
            return model;
        }
    }
    if let Some(model) = env_nonempty("ANTHROPIC_DEFAULT_SONNET_MODEL") {
        return model;
    }
    if let Some(model) = get_provider_primary_model() {
        return model;
    }
    let ms = get_model_strings();
    if provider != ApiProvider::FirstParty {
        return ms_id(&ms, ModelKey::Sonnet45);
    }
    ms_id(&ms, ModelKey::Sonnet46)
}

// @[MODEL LAUNCH]: 新模型发布时更新默认 Haiku ID（3P provider 可能滞后）。
/// `getDefaultHaikuModel`：默认 Haiku 模型（Haiku 4.5 全平台可用）。
pub fn get_default_haiku_model() -> ModelName {
    let provider = get_api_provider();
    if let Some(model) = get_openai_model_for_tier(provider, ChatGptCodexModelTier::Haiku) {
        return model;
    }
    if provider == ApiProvider::Gemini {
        if let Some(model) = env_nonempty("GEMINI_DEFAULT_HAIKU_MODEL") {
            return model;
        }
    }
    if let Some(model) = env_nonempty("ANTHROPIC_DEFAULT_HAIKU_MODEL") {
        return model;
    }
    if let Some(model) = get_provider_primary_model() {
        return model;
    }
    ms_id(&get_model_strings(), ModelKey::Haiku45)
}

/// `getRuntimeMainLoopModel`：按运行时上下文决定主循环模型。
///
/// - `opusplan` + plan 模式 + 未超 200k token → 默认 Opus（无 `[1m]` 后缀）
/// - `haiku` alias + plan 模式 → 默认 Sonnet
/// - 其余 → 传入的 mainLoopModel
///
/// TS 的 `exceeds200kTokens` 默认 `false`，Rust 侧由调用方显式传入。
pub fn get_runtime_main_loop_model(
    permission_mode: PermissionMode,
    main_loop_model: &str,
    exceeds_200k_tokens: bool,
) -> ModelName {
    let user_setting = get_user_specified_model_setting();

    if matches!(user_setting, Some(Some(ref m)) if m == "opusplan")
        && permission_mode == PermissionMode::Plan
        && !exceeds_200k_tokens
    {
        return get_default_opus_model();
    }

    if matches!(user_setting, Some(Some(ref m)) if m == "haiku")
        && permission_mode == PermissionMode::Plan
    {
        return get_default_sonnet_model();
    }

    main_loop_model.to_string()
}

/// `getDefaultMainLoopModelSetting`：内置默认模型。
///
/// - Max / Team Premium → 默认 Opus（`[1m]` 合并启用时附后缀）
/// - 其余（PAYG / Enterprise / Team Standard / Pro）→ 默认 Sonnet
/// - ant 用户取 flag 配置的 `defaultModel`，未配置时为默认 Opus + `[1m]`
///   （antModels.ts 未转换 → 恒走 fallback，见模块头）
pub fn get_default_main_loop_model_setting() -> ModelName {
    if is_ant_user() {
        // TS: `(getAntModelOverrideConfig()?.defaultModel as string) ?? getDefaultOpusModel() + '[1m]'`
        return format!("{}[1m]", get_default_opus_model());
    }

    if is_max_subscriber() || is_team_premium_subscriber() {
        let suffix = if is_opus_1m_merge_enabled() { "[1m]" } else { "" };
        return format!("{}{}", get_default_opus_model(), suffix);
    }

    get_default_sonnet_model()
}

/// `getDefaultMainLoopModel`：绕过用户指定的同步默认主循环模型。
pub fn get_default_main_loop_model() -> ModelName {
    parse_user_specified_model(&get_default_main_loop_model_setting())
}

// ---------------------------------------------------------------------------
// canonical 名解析
// ---------------------------------------------------------------------------

static CANONICAL_FALLBACK_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"claude-(?-u:(\d+-\d+-)?\w+)").ok());

// @[MODEL LAUNCH]: 新模型的 canonical 名映射登记在下方。
/// `firstPartyNameToCanonical`：剥离 first-party 模型 ID 的日期/provider
/// 后缀（纯字符串匹配，不读 settings，可在模块顶层使用）。输入须为
/// 1P 格式 ID（如 'claude-3-7-sonnet-20250219'、
/// 'us.anthropic.claude-opus-4-6-v1:0'）。
pub fn first_party_name_to_canonical(name: &str) -> ModelShortName {
    let name = name.to_ascii_lowercase();
    // Claude 4+ 特例以区分版本；顺序敏感（更具体的版本先判）
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
    if let Some(found) = CANONICAL_FALLBACK_RE.as_ref().and_then(|re| re.find(&name)) {
        return found.as_str().to_string();
    }
    // 无模式匹配时回退原名称
    name
}

/// `getCanonicalName`：全模型 ID → 跨 provider 统一的 canonical 短名。
/// 例如 'claude-3-5-haiku-20241022' 与
/// 'us.anthropic.claude-3-5-haiku-20241022-v1:0' 都映射到
/// 'claude-3-5-haiku'。
pub fn get_canonical_name(full_model_name: &str) -> ModelShortName {
    // 先把被覆盖的 ID（如 Bedrock ARN）反解回 1P 格式，
    // firstPartyNameToCanonical 即可处理。
    first_party_name_to_canonical(&resolve_overridden_model(full_model_name))
}

// ---------------------------------------------------------------------------
// 展示用文案
// ---------------------------------------------------------------------------

// @[MODEL LAUNCH]: 新模型的用户可见描述串登记在下方。
/// `getClaudeAiUserDefaultModelDescription`：模型选择器中的默认模型描述。
pub fn get_claude_ai_user_default_model_description(fast_mode: bool) -> String {
    if is_max_subscriber() || is_team_premium_subscriber() {
        let suffix = if fast_mode { get_opus_pricing_suffix(true) } else { String::new() };
        if is_opus_1m_merge_enabled() {
            return format!("Opus 4.7 with 1M context · Most capable for complex work{suffix}");
        }
        return format!("Opus 4.7 · Most capable for complex work{suffix}");
    }
    "Sonnet 4.6 · Best for everyday tasks".to_string()
}

/// `renderDefaultModelSetting`：默认模型设置的展示串。
pub fn render_default_model_setting(setting: &str) -> String {
    if setting == "opusplan" {
        return "Opus 4.7 in plan mode, else Sonnet 4.6".to_string();
    }
    render_model_name(&parse_user_specified_model(setting))
}

/// `getOpusPricingSuffix`：firstParty 下 Opus 计费信息后缀，fast mode
/// 附闪电标记；其余 provider 返回空串。
pub fn get_opus_pricing_suffix(fast_mode: bool) -> String {
    if get_api_provider() != ApiProvider::FirstParty {
        return String::new();
    }
    let pricing = format_model_pricing(&get_opus46_cost_tier(fast_mode));
    let fast_mode_indicator = if fast_mode {
        format!(" ({})", LIGHTNING_BOLT)
    } else {
        String::new()
    };
    format!(" ·{} {}", fast_mode_indicator, pricing)
}

/// `isOpus1mMergeEnabled`：Opus `[1m]` 合并是否启用。
///
/// 1M 禁用 / Pro 订阅 / 非 firstParty provider / 非官方 base URL 时关闭；
/// 订阅类型未知的 Claude.ai 订阅者同样 fail-closed（VS Code 配置加载
/// 子进程可能拿到缺 subscriptionType 的过期 token，此时 isProSubscriber
/// 误判为 false 会让 opus[1m] 泄漏进下拉列表，API 以误导性的「限流」
/// 报错拒绝）。
pub fn is_opus_1m_merge_enabled() -> bool {
    if is_1m_context_disabled()
        || is_pro_subscriber()
        || get_api_provider() != ApiProvider::FirstParty
        || !is_first_party_anthropic_base_url()
    {
        return false;
    }
    if is_claude_ai_subscriber() && get_subscription_type().is_none() {
        return false;
    }
    true
}

/// `renderModelSetting`：模型设置（alias 或 ID）的展示串。
pub fn render_model_setting(setting: &str) -> String {
    if setting == "opusplan" {
        return "Opus Plan".to_string();
    }
    if is_model_alias(setting) {
        return capitalize(setting);
    }
    render_model_name(setting)
}

// @[MODEL LAUNCH]: 新模型的展示名（base + [1m] 变体）登记在下方。
/// `getPublicModelDisplayName`：已知公开模型的人类可读展示名；
/// 未识别为公开模型时返回 `None`。
pub fn get_public_model_display_name(model: &str) -> Option<String> {
    let ms = get_model_strings();

    let opus47 = ms_id(&ms, ModelKey::Opus47);
    if model == opus47 {
        return Some("Opus 4.7".to_string());
    }
    if model == format!("{opus47}[1m]") {
        return Some("Opus 4.7 (1M context)".to_string());
    }
    let opus46 = ms_id(&ms, ModelKey::Opus46);
    if model == opus46 {
        return Some("Opus 4.6".to_string());
    }
    if model == format!("{opus46}[1m]") {
        return Some("Opus 4.6 (1M context)".to_string());
    }
    let opus45 = ms_id(&ms, ModelKey::Opus45);
    if model == opus45 {
        return Some("Opus 4.5".to_string());
    }
    let opus41 = ms_id(&ms, ModelKey::Opus41);
    if model == opus41 {
        return Some("Opus 4.1".to_string());
    }
    let opus40 = ms_id(&ms, ModelKey::Opus40);
    if model == opus40 {
        return Some("Opus 4".to_string());
    }
    let sonnet46 = ms_id(&ms, ModelKey::Sonnet46);
    if model == format!("{sonnet46}[1m]") {
        return Some("Sonnet 4.6 (1M context)".to_string());
    }
    if model == sonnet46 {
        return Some("Sonnet 4.6".to_string());
    }
    let sonnet45 = ms_id(&ms, ModelKey::Sonnet45);
    if model == format!("{sonnet45}[1m]") {
        return Some("Sonnet 4.5 (1M context)".to_string());
    }
    if model == sonnet45 {
        return Some("Sonnet 4.5".to_string());
    }
    let sonnet40 = ms_id(&ms, ModelKey::Sonnet40);
    if model == sonnet40 {
        return Some("Sonnet 4".to_string());
    }
    if model == format!("{sonnet40}[1m]") {
        return Some("Sonnet 4 (1M context)".to_string());
    }
    let sonnet37 = ms_id(&ms, ModelKey::Sonnet37);
    if model == sonnet37 {
        return Some("Sonnet 3.7".to_string());
    }
    let sonnet35 = ms_id(&ms, ModelKey::Sonnet35);
    if model == sonnet35 {
        return Some("Sonnet 3.5".to_string());
    }
    let haiku45 = ms_id(&ms, ModelKey::Haiku45);
    if model == haiku45 {
        return Some("Haiku 4.5".to_string());
    }
    let haiku35 = ms_id(&ms, ModelKey::Haiku35);
    if model == haiku35 {
        return Some("Haiku 3.5".to_string());
    }
    None
}

/// `maskModelCodename`：只遮蔽首个 dash 段（codename），保留其余，
/// 如 `capybara-v2-fast` → `cap*****-v2-fast`。
fn mask_model_codename(base_name: &str) -> String {
    let mut parts = base_name.split('-');
    let codename = parts.next().unwrap_or("");
    let chars: Vec<char> = codename.chars().collect();
    let masked: String = chars.iter().take(3).collect();
    let stars = "*".repeat(chars.len().saturating_sub(3));
    let masked = format!("{masked}{stars}");
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        masked
    } else {
        format!("{masked}-{}", rest.join("-"))
    }
}

/// `formatCtxTokens`：token 数的展示格式化：1000000 → "1m"、
/// 512000 → "512k"、512100 → "512.1k"（最多一位小数，对齐 /login
/// 平台表单中的 Ctx 输入简写）。
pub fn format_ctx_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let m = tokens as f64 / 1_000_000.0;
        return if m.fract() == 0.0 {
            format!("{}m", m as u64)
        } else {
            format!("{:.1}m", m)
        };
    }
    if tokens >= 1_000 {
        let k = tokens as f64 / 1_000.0;
        return if k.fract() == 0.0 {
            format!("{}k", k as u64)
        } else {
            format!("{:.1}k", k)
        };
    }
    tokens.to_string()
}

/// `getEffectiveContextTokens`：`[1m]` 后缀模型的有效上下文窗口——
/// settings `contextWindowOverrides` 优先，否则 `[1m]` 默认 1M。
pub fn get_effective_context_tokens(model: &str) -> i64 {
    if let Some(overrides) = get_initial_settings().context_window_overrides {
        if let Some(override_value) = overrides.get(model) {
            if *override_value > 0 {
                return *override_value;
            }
        }
    }
    1_000_000
}

/// `getEffectiveContextSuffix`：`[1m]` 后缀模型的展示后缀
/// （如 "[512k]"）；应保持 "[1m]" 展示时返回 `None`。
pub fn get_effective_context_suffix(model: &str) -> Option<String> {
    if !model.to_ascii_lowercase().ends_with("[1m]") {
        return None;
    }
    let rendered = format_ctx_tokens(get_effective_context_tokens(model) as u64);
    if rendered == "1m" {
        None
    } else {
        Some(format!("[{rendered}]"))
    }
}

/// `renderModelIdForDisplay`：UI 展示用模型 ID——用户覆盖改变
/// `[1m]` 默认上下文时替换后缀（如 `gpt-5.6[1m]` → `gpt-5.6[512k]`）。
pub fn render_model_id_for_display(model: &str) -> String {
    if let Some(suffix) = get_effective_context_suffix(model) {
        format!("{}{suffix}", strip_1m_suffix(model))
    } else {
        model.to_string()
    }
}

/// `renderModelName`：模型展示名。
pub fn render_model_name(model: &str) -> String {
    if let Some(public_name) = get_public_model_display_name(model) {
        return public_name;
    }
    if is_ant_user() {
        let resolved = parse_user_specified_model(model);
        // antModels.ts 未转换 → resolveAntModel 恒 None，codename 打码
        // 分支保留结构但不触发（见模块头）。
        if let Some(ant_model) = resolve_ant_model(Some(model)) {
            let base_name = strip_1m_suffix(&ant_model.model);
            let masked = mask_model_codename(&base_name);
            let suffix = if has_1m_context(&resolved) { "[1m]" } else { "" };
            return format!("{masked}{suffix}");
        }
        if resolved != model {
            return format!("{model} ({resolved})");
        }
        return resolved;
    }
    render_model_id_for_display(model)
}

/// `getPublicModelName`：公开的安全署名（如 git commit trailer）。
/// 已知公开模型返回 "Claude {展示名}"，否则 "Claude ({model})" 保留原 ID。
pub fn get_public_model_name(model: &str) -> String {
    match get_public_model_display_name(model) {
        Some(public_name) => format!("Claude {public_name}"),
        None => format!("Claude ({model})"),
    }
}

// ---------------------------------------------------------------------------
// 用户输入解析
// ---------------------------------------------------------------------------

/// `parseUserSpecifiedModel`：把用户指定的模型名/alias 解析为本次会话
/// 使用的完整模型名。故意不支持版本号，与模型切换器对齐。
///
/// 任意 alias 都可带 `[1m]` 后缀（如 `haiku[1m]`、`sonnet[1m]`），
/// 无需为每个变体登记 MODEL_ALIASES。自定义模型名保留原始大小写
/// （如 Azure Foundry 部署 ID），仅剥离结尾 `[1m]`。
pub fn parse_user_specified_model(model_input: &str) -> ModelName {
    let model_input_trimmed = model_input.trim();
    let normalized_model = model_input_trimmed.to_ascii_lowercase();

    let has_1m_tag = has_1m_context(&normalized_model);
    let model_string = if has_1m_tag {
        strip_1m_suffix(&normalized_model).trim().to_string()
    } else {
        normalized_model
    };

    if is_model_alias(&model_string) {
        let suffix = if has_1m_tag { "[1m]" } else { "" };
        match model_string.as_str() {
            // opusplan：默认 Sonnet，plan 模式下用 Opus
            // （见 getRuntimeMainLoopModel）
            "opusplan" | "sonnet" => {
                return format!("{}{}", get_default_sonnet_model(), suffix);
            }
            "haiku" => {
                return format!("{}{}", get_default_haiku_model(), suffix);
            }
            "opus" => {
                return format!("{}{}", get_default_opus_model(), suffix);
            }
            "best" => return get_best_model(),
            _ => {}
        }
    }

    // Opus 4/4.1 已在 first-party 下线（同 Claude.ai）——静默重映射到
    // 当前 Opus 默认。'opus' alias 本就解析到当前默认（4.7），显式写死
    // 这些字符串的用户是在 4.5 发布前固定在 settings/env/--model/SDK
    // 中的。3P provider 可能尚无 4.6/4.7 容量，原样放行。
    if get_api_provider() == ApiProvider::FirstParty
        && is_legacy_opus_first_party(&model_string)
        && is_legacy_model_remap_enabled()
    {
        let suffix = if has_1m_tag { "[1m]" } else { "" };
        return format!("{}{}", get_default_opus_model(), suffix);
    }

    if is_ant_user() {
        // antModels.ts 未转换 → resolveAntModel 恒 None，ant 模型表分支
        // 走 fall-through（TS 注释：解析失败时沿用 alias 字符串，API 会
        // 失败但 feedback 通道会暴露，提示用户重启等待 flag cache 刷新）。
        let base_ant_model = strip_1m_suffix(&normalized_model).trim().to_string();
        if let Some(ant_model) = resolve_ant_model(Some(&base_ant_model)) {
            let suffix = if has_1m_tag { "[1m]" } else { "" };
            return format!("{}{}", ant_model.model, suffix);
        }
    }

    // 保留自定义模型名的原始大小写（如 Azure Foundry 部署 ID），
    // 仅剥离结尾 [1m] 后缀再重新附上。
    if has_1m_tag {
        return format!("{}[1m]", strip_1m_suffix(model_input_trimmed).trim());
    }
    model_input_trimmed.to_string()
}

/// `resolveSkillModelOverride`：解析 skill `model:` frontmatter，
/// 在目标家族支持时携带 `[1m]` 后缀。
///
/// skill 作者写 `model: opus` 意为「opus 级推理」而非「降级到 200K」：
/// 用户在 opus[1m] 上 230k token 时若把裸 alias 直接放行，有效窗口会
/// 从 1M 掉到 200K，23% apparent usage 就触发 autocompact，出现其实
/// 并未溢出的 "Context limit reached"。
///
/// 仅当目标确实支持 1M（sonnet/opus）时携带；`model: haiku` 在 1M
/// 会话上仍降级（haiku 无 1M 变体，随之而来的 autocompact 是正确的）。
/// 已显式带 `[1m]` 的 skill 原样返回。
pub fn resolve_skill_model_override(skill_model: &str, current_model: &str) -> String {
    if has_1m_context(skill_model) || !has_1m_context(current_model) {
        return skill_model.to_string();
    }
    // modelSupports1M 按 canonical ID 匹配（'claude-opus-4-6'、
    // 'claude-sonnet-4'）；裸 'opus' alias 经 getCanonicalName 不命中，
    // 故先解析。
    if model_supports_1m(&parse_user_specified_model(skill_model)) {
        return format!("{skill_model}[1m]");
    }
    skill_model.to_string()
}

/// `LEGACY_OPUS_FIRSTPARTY`：first-party 已下线的 Opus 4/4.1 ID。
const LEGACY_OPUS_FIRSTPARTY: [&str; 4] = [
    "claude-opus-4-20250514",
    "claude-opus-4-1-20250805",
    "claude-opus-4-0",
    "claude-opus-4-1",
];

fn is_legacy_opus_first_party(model: &str) -> bool {
    LEGACY_OPUS_FIRSTPARTY.contains(&model)
}

/// `isLegacyModelRemapEnabled`：legacy Opus 4.0/4.1 → 当前 Opus 重映射
/// 的 opt-out（`CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP` 置真值时关闭）。
pub fn is_legacy_model_remap_enabled() -> bool {
    !is_env_truthy(env::var("CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP").ok().as_deref())
}

// ---------------------------------------------------------------------------
// modelAllowlist.ts 内联实现（未转换）
// ---------------------------------------------------------------------------

/// `modelBelongsToFamily`：模型名（或解析后的名字）是否包含家族标识。
fn model_belongs_to_family(model: &str, family: &str) -> bool {
    if model.contains(family) {
        return true;
    }
    // 解析 "best" → "claude-opus-4-6" 一类的 alias 后再判家族归属
    if is_model_alias(model) {
        return parse_user_specified_model(model)
            .to_ascii_lowercase()
            .contains(family);
    }
    false
}

/// `prefixMatchesModel`：模型名是否在段边界匹配前缀——前缀须匹配到
/// 名字结尾或 '-' 分隔符，如 "claude-opus-4-5" 匹配
/// "claude-opus-4-5-20251101" 但不匹配 "claude-opus-4-50"。
fn prefix_matches_model(model_name: &str, prefix: &str) -> bool {
    if !model_name.starts_with(prefix) {
        return false;
    }
    model_name.len() == prefix.len() || model_name.as_bytes().get(prefix.len()) == Some(&b'-')
}

/// `modelMatchesVersionPrefix`：allowlist 版本前缀条目匹配
/// （"opus-4-5" 视作 "claude-opus-4-5" 的简写；输入 alias 先解析）。
fn model_matches_version_prefix(model: &str, entry: &str) -> bool {
    let resolved_model = if is_model_alias(model) {
        parse_user_specified_model(model).to_ascii_lowercase()
    } else {
        model.to_string()
    };

    if prefix_matches_model(&resolved_model, entry) {
        return true;
    }
    if !entry.starts_with("claude-")
        && prefix_matches_model(&resolved_model, &format!("claude-{entry}"))
    {
        return true;
    }
    false
}

/// `familyHasSpecificEntries`：allowlist 中是否存在收窄该家族通配的更
/// 具体条目。同时含 "opus" 与 "opus-4-5" 时具体条目优先——"opus" 不再
/// 作为通配，仅 opus 4.5 放行。
fn family_has_specific_entries(family: &str, allowlist: &[String]) -> bool {
    for entry in allowlist {
        if is_model_family_alias(entry) {
            continue;
        }
        // 条目须是家族的版本限定变体（如 "opus-4-5"、
        // "claude-opus-4-5-20251101"），且在段边界后跟 '-' 或结尾，
        // 避免 "opusplan" 误匹配 "opus"。
        if let Some(idx) = entry.find(family) {
            let after_family = idx + family.len();
            if after_family == entry.len() || entry.as_bytes().get(after_family) == Some(&b'-') {
                return true;
            }
        }
    }
    false
}

/// `isModelAllowed`（modelAllowlist.ts 未转换，内联完整实现）：
/// settings `availableModels` 白名单判定；未设置时全部放行。
///
/// 匹配层级：
/// 1. 家族别名（"opus"/"sonnet"/"haiku"）通配整个家族——除非白名单
///    同时含该家族的更具体条目（如 "opus-4-5"），此时通配失效；
/// 2. 版本前缀（"opus-4-5"、"claude-opus-4-5"）匹配该版本任意构建；
/// 3. 完整模型 ID（"claude-opus-4-5-20251101"）精确匹配。
pub fn is_model_allowed(model: &str) -> bool {
    let Some(allowlist) = get_initial_settings().available_models else {
        return true; // 无限制
    };
    if allowlist.is_empty() {
        return false; // 空白名单拦截所有用户指定模型
    }

    let normalized_model = resolve_overridden_model(model).trim().to_ascii_lowercase();
    let normalized_allowlist: Vec<String> = allowlist
        .iter()
        .map(|entry| entry.trim().to_ascii_lowercase())
        .collect();

    // 直接匹配（别名对别名 / 全名对全名）。被具体条目收窄的家族别名
    // 跳过——["opus", "opus-4-5"] 中的 "opus" 不应直接命中，
    // 管理员意图是仅限 opus 4.5。
    if normalized_allowlist.iter().any(|entry| entry == &normalized_model)
        && !is_model_family_alias(&normalized_model)
        && !family_has_specific_entries(&normalized_model, &normalized_allowlist)
    {
        return true;
    }

    // 家族级别名在无更具体条目时通配整个家族。
    for entry in &normalized_allowlist {
        if is_model_family_alias(entry)
            && !family_has_specific_entries(entry, &normalized_allowlist)
            && model_belongs_to_family(&normalized_model, entry)
        {
            return true;
        }
    }

    // 非家族条目走双向 alias 解析：输入为 alias 时解析后匹配
    if is_model_alias(&normalized_model) {
        let resolved = parse_user_specified_model(&normalized_model).to_ascii_lowercase();
        if normalized_allowlist.iter().any(|entry| entry == &resolved) {
            return true;
        }
    }

    // 白名单内任意非家族 alias 解析后等于输入模型
    for entry in &normalized_allowlist {
        if !is_model_family_alias(entry) && is_model_alias(entry) {
            let resolved = parse_user_specified_model(entry).to_ascii_lowercase();
            if resolved == normalized_model {
                return true;
            }
        }
    }

    // 版本前缀匹配："opus-4-5" / "claude-opus-4-5" 在段边界匹配
    // "claude-opus-4-5-20251101"
    for entry in &normalized_allowlist {
        if !is_model_family_alias(entry)
            && !is_model_alias(entry)
            && model_matches_version_prefix(&normalized_model, entry)
        {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// 其余展示/工具函数
// ---------------------------------------------------------------------------

/// `modelDisplayString`：用户设置的模型展示串。
pub fn model_display_string(model: ModelSetting) -> String {
    match model {
        None => {
            if is_ant_user() {
                format!(
                    "Default for Ants ({})",
                    render_default_model_setting(&get_default_main_loop_model_setting())
                )
            } else if is_claude_ai_subscriber() {
                format!("Default ({})", get_claude_ai_user_default_model_description(false))
            } else {
                format!("Default ({})", get_main_loop_model())
            }
        }
        Some(model_str) => {
            let resolved = parse_user_specified_model(&model_str);
            if model_str == resolved {
                resolved
            } else {
                format!("{model_str} ({resolved})")
            }
        }
    }
}

// @[MODEL LAUNCH]: 新模型的 marketing 名映射登记在下方。
/// `getMarketingNameForModel`：模型 ID 的营销名；Foundry 的部署 ID
/// 由用户自定义、可能与实际模型无关，返回 `None`。
pub fn get_marketing_name_for_model(model_id: &str) -> Option<String> {
    if get_api_provider() == ApiProvider::Foundry {
        return None;
    }

    let has_1m = model_id.to_ascii_lowercase().contains("[1m]");
    let canonical = get_canonical_name(model_id);

    if canonical.contains("claude-opus-4-7") {
        return Some(if has_1m { "Opus 4.7 (with 1M context)" } else { "Opus 4.7" }.to_string());
    }
    if canonical.contains("claude-opus-4-6") {
        return Some(if has_1m { "Opus 4.6 (with 1M context)" } else { "Opus 4.6" }.to_string());
    }
    if canonical.contains("claude-opus-4-5") {
        return Some("Opus 4.5".to_string());
    }
    if canonical.contains("claude-opus-4-1") {
        return Some("Opus 4.1".to_string());
    }
    if canonical.contains("claude-opus-4") {
        return Some("Opus 4".to_string());
    }
    if canonical.contains("claude-sonnet-4-6") {
        return Some(
            if has_1m { "Sonnet 4.6 (with 1M context)" } else { "Sonnet 4.6" }.to_string(),
        );
    }
    if canonical.contains("claude-sonnet-4-5") {
        return Some(
            if has_1m { "Sonnet 4.5 (with 1M context)" } else { "Sonnet 4.5" }.to_string(),
        );
    }
    if canonical.contains("claude-sonnet-4") {
        return Some(if has_1m { "Sonnet 4 (with 1M context)" } else { "Sonnet 4" }.to_string());
    }
    if canonical.contains("claude-3-7-sonnet") {
        return Some("Claude 3.7 Sonnet".to_string());
    }
    if canonical.contains("claude-3-5-sonnet") {
        return Some("Claude 3.5 Sonnet".to_string());
    }
    if canonical.contains("claude-haiku-4-5") {
        return Some("Haiku 4.5".to_string());
    }
    if canonical.contains("claude-3-5-haiku") {
        return Some("Claude 3.5 Haiku".to_string());
    }

    None
}

static NORMALIZE_API_RE: Lazy<Option<Regex>> = Lazy::new(|| Regex::new(r"(?i)\[(1|2)m\]").ok());

/// `normalizeModelStringForAPI`：剥离发往 API 的模型 ID 中的
/// `[1m]` / `[2m]` 后缀（全局、不区分大小写）。
pub fn normalize_model_string_for_api(model: &str) -> String {
    match NORMALIZE_API_RE.as_ref() {
        Some(re) => re.replace_all(model, "").into_owned(),
        None => model.to_string(),
    }
}
