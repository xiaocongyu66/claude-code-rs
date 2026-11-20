//! deprecation.rs — 模型退役信息与警告文案。
//!
//! 重写自 `src/utils/model/deprecation.ts`。`DEPRECATED_MODELS` 表、
//! 大小写不敏感的子串匹配、provider 维度的退休日期逐字对齐 TS。

use crate::utils::model::provider_classification::ApiProvider;
use crate::utils::model::providers::get_api_provider;

/// `ModelCapabilityOverride` 等的 `Partial<Record<APIProvider, string | null>>`：
/// `None` 表示该 provider 下不退役。
struct DeprecationEntry {
    /// 匹配 model ID（小写）的子串键。
    key: &'static str,
    /// 人类可读模型名。
    model_name: &'static str,
    first_party: Option<&'static str>,
    bedrock: Option<&'static str>,
    vertex: Option<&'static str>,
    foundry: Option<&'static str>,
}

impl DeprecationEntry {
    /// `value.retirementDates[provider]`：按 provider 取退休日期。
    fn retirement_date(&self, provider: ApiProvider) -> Option<&'static str> {
        match provider {
            ApiProvider::FirstParty => self.first_party,
            ApiProvider::Bedrock => self.bedrock,
            ApiProvider::Vertex => self.vertex,
            ApiProvider::Foundry => self.foundry,
            // TS 中未列出的 provider 取值为 undefined → 不视为退役。
            ApiProvider::Openai | ApiProvider::Gemini | ApiProvider::Grok => None,
        }
    }
}

/// 已退役模型及其按 provider 的退休日期。
///
/// 键为匹配 model ID 的（小写）子串；新增退役模型时在此追加一条。
/// 表顺序即 TS `Object.entries` 的声明顺序，匹配取首个命中。
const DEPRECATED_MODELS: &[DeprecationEntry] = &[
    DeprecationEntry {
        key: "claude-3-opus",
        model_name: "Claude 3 Opus",
        first_party: Some("January 5, 2026"),
        bedrock: Some("January 15, 2026"),
        vertex: Some("January 5, 2026"),
        foundry: Some("January 5, 2026"),
    },
    DeprecationEntry {
        key: "claude-3-7-sonnet",
        model_name: "Claude 3.7 Sonnet",
        first_party: Some("February 19, 2026"),
        bedrock: Some("April 28, 2026"),
        vertex: Some("May 11, 2026"),
        foundry: Some("February 19, 2026"),
    },
    DeprecationEntry {
        key: "claude-3-5-haiku",
        model_name: "Claude 3.5 Haiku",
        first_party: Some("February 19, 2026"),
        bedrock: None,
        vertex: None,
        foundry: None,
    },
];

/// `getDeprecatedModelInfo(modelId)`（TS 内部）：模型是否在当前 provider 下退役。
fn get_deprecated_model_info(model_id: &str) -> Option<(&'static str, &'static str)> {
    let lowercase_model_id = model_id.to_ascii_lowercase();
    let provider = get_api_provider();

    for entry in DEPRECATED_MODELS {
        if !lowercase_model_id.contains(entry.key) {
            continue;
        }
        if let Some(retirement_date) = entry.retirement_date(provider) {
            return Some((entry.model_name, retirement_date));
        }
    }

    None
}

/// `getModelDeprecationWarning(modelId)`：返回退役警告文案，未退役/无 modelId 返回 None。
pub fn get_model_deprecation_warning(model_id: Option<&str>) -> Option<String> {
    let model_id = model_id?;

    let (model_name, retirement_date) = get_deprecated_model_info(model_id)?;

    Some(format!(
        "⚠ {model_name} will be retired on {retirement_date}. Consider switching to a newer model."
    ))
}
