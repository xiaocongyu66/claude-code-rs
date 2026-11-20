//! attribution_email.rs — 模型归属邮箱映射。
//!
//! 重写自 `src/utils/attributionEmail.ts`。按模型名关键字匹配
//! Co-Authored-By 使用的邮箱；未命中时回退到 Anthropic 官方邮箱。

struct ModelEmailEntry {
    keywords: &'static [&'static str],
    email: &'static str,
}

const MODEL_EMAIL_MAP: &[ModelEmailEntry] = &[
    ModelEmailEntry {
        keywords: &["claude"],
        email: "noreply@anthropic.com",
    },
    // 由于找不到他们的邮箱和头像, 所以改为了使用我们的邮箱先记录, 后续官方有
    // github 能用的邮箱可以替换。github 组织是不能用 co author 的。
    ModelEmailEntry {
        keywords: &["gpt", "dall-e", "o1-", "o3-", "o4-"],
        email: "openai@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["gemini"],
        email: "google-gemini@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["grok"],
        email: "xai-org@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["glm"],
        email: "zai-org@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["deepseek"],
        email: "deepseek-ai@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["qwen"],
        email: "QwenLM@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["minimax"],
        email: "MiniMax-AI@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["mimo"],
        email: "XiaomiMiMo@claude-code-best.win",
    },
    ModelEmailEntry {
        keywords: &["kimi"],
        email: "MoonshotAI@claude-code-best.win",
    },
];

const DEFAULT_EMAIL: &str = "noreply@anthropic.com";

/// 按模型名（大小写不敏感）匹配第一个命中关键字的邮箱。
pub fn get_attribution_email(model_name: &str) -> &'static str {
    let lower = model_name.to_ascii_lowercase();
    for entry in MODEL_EMAIL_MAP {
        if entry.keywords.iter().any(|kw| lower.contains(kw)) {
            return entry.email;
        }
    }
    DEFAULT_EMAIL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_keywords_case_insensitively() {
        assert_eq!(
            get_attribution_email("claude-opus-4-5"),
            "noreply@anthropic.com"
        );
        assert_eq!(
            get_attribution_email("GPT-4o"),
            "openai@claude-code-best.win"
        );
        assert_eq!(
            get_attribution_email("o1-mini"),
            "openai@claude-code-best.win"
        );
        assert_eq!(
            get_attribution_email("gemini-2.5-pro"),
            "google-gemini@claude-code-best.win"
        );
        assert_eq!(
            get_attribution_email("grok-code"),
            "xai-org@claude-code-best.win"
        );
        assert_eq!(
            get_attribution_email("deepseek-v3"),
            "deepseek-ai@claude-code-best.win"
        );
    }

    #[test]
    fn falls_back_to_anthropic() {
        assert_eq!(get_attribution_email("unknown-model"), DEFAULT_EMAIL);
    }
}
