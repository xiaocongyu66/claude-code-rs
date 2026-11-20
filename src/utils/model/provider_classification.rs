//! provider_classification.rs — API provider 分类。
//!
//! 重写自 `src/utils/model/providerClassification.ts`。

use serde::{Deserialize, Serialize};

/// `APIProvider`：七种 API provider。serde 映射对齐 TS 的字符串联合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiProvider {
    #[serde(rename = "firstParty")]
    FirstParty,
    #[serde(rename = "bedrock")]
    Bedrock,
    #[serde(rename = "vertex")]
    Vertex,
    #[serde(rename = "foundry")]
    Foundry,
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "grok")]
    Grok,
}

impl ApiProvider {
    /// 与 TS 字符串联合一致的字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstParty => "firstParty",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
            Self::Foundry => "foundry",
            Self::Openai => "openai",
            Self::Gemini => "gemini",
            Self::Grok => "grok",
        }
    }
}

/// `isThirdPartyAPIProvider`：非 firstParty 即第三方。
pub fn is_third_party_api_provider(provider: ApiProvider) -> bool {
    provider != ApiProvider::FirstParty
}
