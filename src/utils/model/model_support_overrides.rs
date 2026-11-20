//! model_support_overrides.rs — 第三方 provider 的能力覆盖（capability override）。
//!
//! 重写自 `src/utils/model/modelSupportOverrides.ts`。tier 表、环境变量
//! 匹配语义与 lodash `memoize` 缓存（resolver = `model.toLowerCase():capability`，
//! 进程级、不失效）逐条对齐。

use std::collections::HashMap;
use std::env;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::utils::model::provider_classification::ApiProvider;
use crate::utils::model::providers::get_api_provider;

/// `ModelCapabilityOverride`：TS 的字符串字面量联合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelCapabilityOverride {
    Effort,
    MaxEffort,
    XhighEffort,
    Thinking,
    AdaptiveThinking,
    InterleavedThinking,
}

impl ModelCapabilityOverride {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Effort => "effort",
            Self::MaxEffort => "max_effort",
            Self::XhighEffort => "xhigh_effort",
            Self::Thinking => "thinking",
            Self::AdaptiveThinking => "adaptive_thinking",
            Self::InterleavedThinking => "interleaved_thinking",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "effort" => Some(Self::Effort),
            "max_effort" => Some(Self::MaxEffort),
            "xhigh_effort" => Some(Self::XhighEffort),
            "thinking" => Some(Self::Thinking),
            "adaptive_thinking" => Some(Self::AdaptiveThinking),
            "interleaved_thinking" => Some(Self::InterleavedThinking),
            _ => None,
        }
    }
}

/// `ANTHROPIC_TIERS` / `OPENAI_TIERS` 的单条 tier：pinned 模型环境变量 +
/// 支持能力环境变量。
struct ModelTier {
    model_env_var: &'static str,
    capabilities_env_var: &'static str,
}

const ANTHROPIC_TIERS: &[ModelTier] = &[
    ModelTier {
        model_env_var: "ANTHROPIC_DEFAULT_OPUS_MODEL",
        capabilities_env_var: "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
    },
    ModelTier {
        model_env_var: "ANTHROPIC_DEFAULT_SONNET_MODEL",
        capabilities_env_var: "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
    },
    ModelTier {
        model_env_var: "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        capabilities_env_var: "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
    },
];

const OPENAI_TIERS: &[ModelTier] = &[
    ModelTier {
        model_env_var: "OPENAI_DEFAULT_OPUS_MODEL",
        capabilities_env_var: "OPENAI_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
    },
    ModelTier {
        model_env_var: "OPENAI_DEFAULT_SONNET_MODEL",
        capabilities_env_var: "OPENAI_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
    },
    ModelTier {
        model_env_var: "OPENAI_DEFAULT_HAIKU_MODEL",
        capabilities_env_var: "OPENAI_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
    },
];

/// memoize 缓存：键为 resolver `${model.toLowerCase()}:${capability}`，
/// 值为查询结果（`undefined` 也缓存，对齐 memoize 行为）。
static CACHE: Lazy<Mutex<HashMap<(String, &'static str), Option<bool>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// `get3PModelCapabilityOverride`：检查是否为匹配 pinned `*_DEFAULT_*_MODEL`
/// 环境变量的第三方模型设置了能力覆盖。
///
/// firstParty provider 一律返回 None；provider 为 openai 时查 OPENAI_TIERS，
/// 其余第三方 provider 查 ANTHROPIC_TIERS。tier 的 pinned 模型未设置、或能力
/// 环境变量未定义（`undefined`，空串不算）时跳过；模型不匹配时跳过；
/// 均不命中返回 None。结果按 memoize resolver 键缓存。
pub fn get_3p_model_capability_override(
    model: &str,
    capability: ModelCapabilityOverride,
) -> Option<bool> {
    let cache_key = (model.to_ascii_lowercase(), capability.as_str());

    if let Ok(cache) = CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return *cached;
        }
    }

    let computed = compute_3p_model_capability_override(&cache_key.0, capability);

    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(cache_key, computed);
    }

    computed
}

fn compute_3p_model_capability_override(
    lowercase_model: &str,
    capability: ModelCapabilityOverride,
) -> Option<bool> {
    let provider = get_api_provider();
    if provider == ApiProvider::FirstParty {
        return None;
    }

    let tiers = if provider == ApiProvider::Openai {
        OPENAI_TIERS
    } else {
        ANTHROPIC_TIERS
    };

    for tier in tiers {
        let Ok(pinned) = env::var(tier.model_env_var) else {
            continue;
        };
        let capabilities = env::var(tier.capabilities_env_var);
        // TS: `capabilities === undefined` 才跳过；空串参与解析。
        let Ok(capabilities) = capabilities else {
            continue;
        };
        if pinned.is_empty() || lowercase_model != pinned.to_ascii_lowercase() {
            continue;
        }

        let matched = capabilities
            .to_ascii_lowercase()
            .split(',')
            .any(|s| s.trim() == capability.as_str());
        return Some(matched);
    }

    None
}
