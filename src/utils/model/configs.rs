//! configs.rs — 各模型的 provider 专属 ID 配置表。
//!
//! 重写自 `src/utils/model/configs.ts`。表内容逐字对齐 TS；
//! `ALL_MODEL_CONFIGS` / `CANONICAL_MODEL_IDS` / `CANONICAL_ID_TO_KEY`
//! 以 Rust 等价结构表达。

use std::collections::HashMap;

use once_cell::sync::Lazy;

use crate::utils::model::provider_classification::ApiProvider;

/// `ModelName` / `ModelShortName`：模型 ID 即字符串。
pub type ModelName = &'static str;

/// `ModelConfig = Record<APIProvider, ModelName>`：每个 provider 的模型 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelConfig {
    pub first_party: ModelName,
    pub bedrock: ModelName,
    pub vertex: ModelName,
    pub foundry: ModelName,
    pub openai: ModelName,
    pub gemini: ModelName,
    pub grok: ModelName,
}

impl ModelConfig {
    /// `config[provider]`：按 provider 取模型 ID。
    pub fn for_provider(&self, provider: ApiProvider) -> ModelName {
        match provider {
            ApiProvider::FirstParty => self.first_party,
            ApiProvider::Bedrock => self.bedrock,
            ApiProvider::Vertex => self.vertex,
            ApiProvider::Foundry => self.foundry,
            ApiProvider::Openai => self.openai,
            ApiProvider::Gemini => self.gemini,
            ApiProvider::Grok => self.grok,
        }
    }
}

// @[MODEL LAUNCH]: 新增 CLAUDE_*_CONFIG 常量时在此登记。

pub const CLAUDE_3_7_SONNET_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-3-7-sonnet-20250219",
    bedrock: "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
    vertex: "claude-3-7-sonnet@20250219",
    foundry: "claude-3-7-sonnet",
    openai: "claude-3-7-sonnet-20250219",
    gemini: "claude-3-7-sonnet-20250219",
    grok: "claude-3-7-sonnet-20250219",
};

pub const CLAUDE_3_5_V2_SONNET_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-3-5-sonnet-20241022",
    bedrock: "anthropic.claude-3-5-sonnet-20241022-v2:0",
    vertex: "claude-3-5-sonnet-v2@20241022",
    foundry: "claude-3-5-sonnet",
    openai: "claude-3-5-sonnet-20241022",
    gemini: "claude-3-5-sonnet-20241022",
    grok: "claude-3-5-sonnet-20241022",
};

pub const CLAUDE_3_5_HAIKU_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-3-5-haiku-20241022",
    bedrock: "us.anthropic.claude-3-5-haiku-20241022-v1:0",
    vertex: "claude-3-5-haiku@20241022",
    foundry: "claude-3-5-haiku",
    openai: "claude-3-5-haiku-20241022",
    gemini: "claude-3-5-haiku-20241022",
    grok: "claude-3-5-haiku-20241022",
};

pub const CLAUDE_HAIKU_4_5_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-haiku-4-5-20251001",
    bedrock: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    vertex: "claude-haiku-4-5@20251001",
    foundry: "claude-haiku-4-5",
    openai: "claude-haiku-4-5-20251001",
    gemini: "claude-haiku-4-5-20251001",
    grok: "claude-haiku-4-5-20251001",
};

pub const CLAUDE_SONNET_4_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-sonnet-4-20250514",
    bedrock: "us.anthropic.claude-sonnet-4-20250514-v1:0",
    vertex: "claude-sonnet-4@20250514",
    foundry: "claude-sonnet-4",
    openai: "claude-sonnet-4-20250514",
    gemini: "claude-sonnet-4-20250514",
    grok: "claude-sonnet-4-20250514",
};

pub const CLAUDE_SONNET_4_5_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-sonnet-4-5-20250929",
    bedrock: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    vertex: "claude-sonnet-4-5@20250929",
    foundry: "claude-sonnet-4-5",
    openai: "claude-sonnet-4-5-20250929",
    gemini: "claude-sonnet-4-5-20250929",
    grok: "claude-sonnet-4-5-20250929",
};

pub const CLAUDE_OPUS_4_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-4-20250514",
    bedrock: "us.anthropic.claude-opus-4-20250514-v1:0",
    vertex: "claude-opus-4@20250514",
    foundry: "claude-opus-4",
    openai: "claude-opus-4-20250514",
    gemini: "claude-opus-4-20250514",
    grok: "claude-opus-4-20250514",
};

pub const CLAUDE_OPUS_4_1_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-4-1-20250805",
    bedrock: "us.anthropic.claude-opus-4-1-20250805-v1:0",
    vertex: "claude-opus-4-1@20250805",
    foundry: "claude-opus-4-1",
    openai: "claude-opus-4-1-20250805",
    gemini: "claude-opus-4-1-20250805",
    grok: "claude-opus-4-1-20250805",
};

pub const CLAUDE_OPUS_4_5_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-4-5-20251101",
    bedrock: "us.anthropic.claude-opus-4-5-20251101-v1:0",
    vertex: "claude-opus-4-5@20251101",
    foundry: "claude-opus-4-5",
    openai: "claude-opus-4-5-20251101",
    gemini: "claude-opus-4-5-20251101",
    grok: "claude-opus-4-5-20251101",
};

pub const CLAUDE_OPUS_4_6_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-4-6",
    bedrock: "us.anthropic.claude-opus-4-6-v1",
    vertex: "claude-opus-4-6",
    foundry: "claude-opus-4-6",
    openai: "claude-opus-4-6",
    gemini: "claude-opus-4-6",
    grok: "claude-opus-4-6",
};

pub const CLAUDE_OPUS_4_7_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-4-7",
    bedrock: "us.anthropic.claude-opus-4-7-v1",
    vertex: "claude-opus-4-7",
    foundry: "claude-opus-4-7",
    openai: "claude-opus-4-7",
    gemini: "claude-opus-4-7",
    grok: "claude-opus-4-7",
};

pub const CLAUDE_OPUS_5_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-opus-5",
    bedrock: "us.anthropic.claude-opus-5-v1",
    vertex: "claude-opus-5",
    foundry: "claude-opus-5",
    openai: "claude-opus-5",
    gemini: "claude-opus-5",
    grok: "claude-opus-5",
};

pub const CLAUDE_SONNET_4_6_CONFIG: ModelConfig = ModelConfig {
    first_party: "claude-sonnet-4-6",
    bedrock: "us.anthropic.claude-sonnet-4-6",
    vertex: "claude-sonnet-4-6",
    foundry: "claude-sonnet-4-6",
    openai: "claude-sonnet-4-6",
    gemini: "claude-sonnet-4-6",
    grok: "claude-sonnet-4-6",
};

/// `ModelKey`：ALL_MODEL_CONFIGS 的键（TS 字符串联合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelKey {
    Haiku35,
    Haiku45,
    Sonnet35,
    Sonnet37,
    Sonnet40,
    Sonnet45,
    Sonnet46,
    Opus40,
    Opus41,
    Opus45,
    Opus46,
    Opus47,
    Opus5,
}

impl ModelKey {
    /// TS 中 ALL_MODEL_CONFIGS 的字符串键。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Haiku35 => "haiku35",
            Self::Haiku45 => "haiku45",
            Self::Sonnet35 => "sonnet35",
            Self::Sonnet37 => "sonnet37",
            Self::Sonnet40 => "sonnet40",
            Self::Sonnet45 => "sonnet45",
            Self::Sonnet46 => "sonnet46",
            Self::Opus40 => "opus40",
            Self::Opus41 => "opus41",
            Self::Opus45 => "opus45",
            Self::Opus46 => "opus46",
            Self::Opus47 => "opus47",
            Self::Opus5 => "opus5",
        }
    }

    pub fn from_str(key: &str) -> Option<Self> {
        match key {
            "haiku35" => Some(Self::Haiku35),
            "haiku45" => Some(Self::Haiku45),
            "sonnet35" => Some(Self::Sonnet35),
            "sonnet37" => Some(Self::Sonnet37),
            "sonnet40" => Some(Self::Sonnet40),
            "sonnet45" => Some(Self::Sonnet45),
            "sonnet46" => Some(Self::Sonnet46),
            "opus40" => Some(Self::Opus40),
            "opus41" => Some(Self::Opus41),
            "opus45" => Some(Self::Opus45),
            "opus46" => Some(Self::Opus46),
            "opus47" => Some(Self::Opus47),
            "opus5" => Some(Self::Opus5),
            _ => None,
        }
    }
}

/// `ALL_MODEL_CONFIGS`：内部短键 → 模型配置。保持 TS 的枚举顺序。
pub const ALL_MODEL_CONFIGS: &[(ModelKey, &ModelConfig)] = &[
    (ModelKey::Haiku35, &CLAUDE_3_5_HAIKU_CONFIG),
    (ModelKey::Haiku45, &CLAUDE_HAIKU_4_5_CONFIG),
    (ModelKey::Sonnet35, &CLAUDE_3_5_V2_SONNET_CONFIG),
    (ModelKey::Sonnet37, &CLAUDE_3_7_SONNET_CONFIG),
    (ModelKey::Sonnet40, &CLAUDE_SONNET_4_CONFIG),
    (ModelKey::Sonnet45, &CLAUDE_SONNET_4_5_CONFIG),
    (ModelKey::Sonnet46, &CLAUDE_SONNET_4_6_CONFIG),
    (ModelKey::Opus40, &CLAUDE_OPUS_4_CONFIG),
    (ModelKey::Opus41, &CLAUDE_OPUS_4_1_CONFIG),
    (ModelKey::Opus45, &CLAUDE_OPUS_4_5_CONFIG),
    (ModelKey::Opus46, &CLAUDE_OPUS_4_6_CONFIG),
    (ModelKey::Opus47, &CLAUDE_OPUS_4_7_CONFIG),
    (ModelKey::Opus5, &CLAUDE_OPUS_5_CONFIG),
];

/// `CANONICAL_MODEL_IDS`：所有 canonical first-party 模型 ID
/// （完备性测试使用）。
pub static CANONICAL_MODEL_IDS: Lazy<Vec<ModelName>> = Lazy::new(|| {
    ALL_MODEL_CONFIGS
        .iter()
        .map(|(_, config)| config.first_party)
        .collect()
});

/// `CANONICAL_ID_TO_KEY`：canonical ID → 内部短键，
/// 用于应用 settings 中的 modelOverrides。
pub static CANONICAL_ID_TO_KEY: Lazy<HashMap<ModelName, ModelKey>> = Lazy::new(|| {
    ALL_MODEL_CONFIGS
        .iter()
        .map(|(key, config)| (config.first_party, *key))
        .collect()
});
