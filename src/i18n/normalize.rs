//! Locale tag normalization — pure logic, no dependencies.
//! Kept separate from index.rs so tests and lightweight callers avoid
//! importing the settings/auth dependency chain.
//! Rewrite of `src/i18n/normalize.ts`.

/// Locales with a translation pack available.
pub const AVAILABLE_LOCALES: [&str; 5] = ["en", "zh-CN", "zh-TW", "ja", "ko"];

/// A supported locale tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Locale {
    En,
    ZhCN,
    ZhTW,
    Ja,
    Ko,
}

impl Locale {
    /// BCP-47 style tag as used in locale pack file names.
    pub fn as_tag(&self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhCN => "zh-CN",
            Locale::ZhTW => "zh-TW",
            Locale::Ja => "ja",
            Locale::Ko => "ko",
        }
    }

    /// Case-insensitive tag lookup.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let lower = tag.trim().to_ascii_lowercase();
        AVAILABLE_LOCALES
            .iter()
            .find(|l| l.to_ascii_lowercase() == lower)
            .and_then(|l| match *l {
                "en" => Some(Locale::En),
                "zh-CN" => Some(Locale::ZhCN),
                "zh-TW" => Some(Locale::ZhTW),
                "ja" => Some(Locale::Ja),
                "ko" => Some(Locale::Ko),
                _ => None,
            })
    }

    /// First subtag of the tag (e.g. "zh_CN.UTF-8" → "zh").
    fn primary_subtag(lower: &str) -> &str {
        lower
            .split(['-', '_', '.'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("")
    }
}

/// Map a raw language tag (LANG value, BCP-47, etc.) to a supported locale.
/// Unknown languages fall back to `En`.
pub fn normalize_tag(tag: &str) -> Locale {
    let lower = tag.trim().to_ascii_lowercase();

    if let Some(locale) = Locale::from_tag(&lower) {
        return locale;
    }

    // Language subtag match: e.g. LANG=zh_CN.UTF-8 → zh-CN, ja_JP → ja
    let sub = Locale::primary_subtag(&lower);
    if sub == "zh" {
        // Prefer matching script/region: zh-tw/zh-hk/zh-hant → zh-TW
        return if ["tw", "hk", "mo", "hant"].iter().any(|s| lower.contains(s)) {
            Locale::ZhTW
        } else {
            Locale::ZhCN
        };
    }

    AVAILABLE_LOCALES
        .iter()
        .find(|l| Locale::primary_subtag(l) == sub)
        .and_then(|l| Locale::from_tag(l))
        .unwrap_or(Locale::En)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tags_match_case_insensitively() {
        assert_eq!(normalize_tag("zh-CN"), Locale::ZhCN);
        assert_eq!(normalize_tag("ZH-cn"), Locale::ZhCN);
        assert_eq!(normalize_tag("ja"), Locale::Ja);
        assert_eq!(normalize_tag("ko"), Locale::Ko);
        assert_eq!(normalize_tag("en"), Locale::En);
    }

    #[test]
    fn lang_env_values() {
        assert_eq!(normalize_tag("zh_CN.UTF-8"), Locale::ZhCN);
        assert_eq!(normalize_tag("zh_TW.UTF-8"), Locale::ZhTW);
        assert_eq!(normalize_tag("zh-Hant"), Locale::ZhTW);
        assert_eq!(normalize_tag("ja_JP"), Locale::Ja);
        assert_eq!(normalize_tag("en_US.UTF-8"), Locale::En);
    }

    #[test]
    fn unknown_falls_back_to_en() {
        assert_eq!(normalize_tag("fr-FR"), Locale::En);
        assert_eq!(normalize_tag(""), Locale::En);
        assert_eq!(normalize_tag("  "), Locale::En);
    }
}
