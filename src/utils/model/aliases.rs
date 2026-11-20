//! aliases.rs — 模型别名表。
//!
//! 重写自 `src/utils/model/aliases.ts`。别名集合与匹配语义逐字对齐。

/// 用户可指定的模型别名（MODEL_ALIASES）。大小写敏感、精确匹配。
pub const MODEL_ALIASES: &[&str] = &[
    "sonnet",
    "opus",
    "haiku",
    "best",
    "sonnet[1m]",
    "opus[1m]",
    "opusplan",
];

/// TS 的 `ModelAlias` 字符串联合类型；Rust 端以 `&str` 表达。
pub type ModelAlias = &'static str;

/// `isModelAlias`：输入是否为已知模型别名（精确匹配，不裁剪空格、不忽略大小写）。
pub fn is_model_alias(model_input: &str) -> bool {
    MODEL_ALIASES.contains(&model_input)
}

/// 充当 availableModels 白名单通配符的裸模型家族别名。
/// 白名单含 "opus" 时任意 opus 模型（4.5、4.6 等）都放行；
/// 含具体模型 ID 时只放行该版本。
pub const MODEL_FAMILY_ALIASES: &[&str] = &["sonnet", "opus", "haiku"];

/// `isModelFamilyAlias`：输入是否为裸模型家族别名。
pub fn is_model_family_alias(model: &str) -> bool {
    MODEL_FAMILY_ALIASES.contains(&model)
}
