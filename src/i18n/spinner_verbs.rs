//! Localized spinner verb lists, keyed by locale tag.
//! English defaults live in `src/constants/spinner_verbs.rs` (SPINNER_VERBS);
//! other locales fall back to those when their list is absent — `En` here
//! returns an empty slice and callers resolve the fallback.
//! Tone: playful, short, progress-oriented.
//! Rewrite of `src/i18n/spinnerVerbs.ts`.

use super::normalize::Locale;

/// Verbs for the given locale; empty for `En` (fallback handled by caller).
pub fn locale_spinner_verbs(locale: Locale) -> &'static [&'static str] {
    match locale {
        Locale::ZhCN => &[
            "分析中", "构思中", "计算中", "编译中", "调试中", "部署中", "推理中", "沉思中",
            "酝酿中", "打磨中", "琢磨中", "敲键中", "翻阅中", "检索中", "绘制中", "重构中",
            "梳理中", "推演中", "蒸馏中", "对齐中", "拆解中", "搭建中", "试运行中", "收尾中",
        ],
        Locale::ZhTW => &[
            "分析中", "構思中", "計算中", "編譯中", "除錯中", "部署中", "推理中", "沉思中",
            "醞釀中", "打磨中", "琢磨中", "敲鍵中", "翻閱中", "檢索中", "繪製中", "重構中",
            "梳理中", "推演中", "蒸餾中", "對齊中", "拆解中", "搭建中", "試運行中", "收尾中",
        ],
        Locale::Ja => &[
            "分析中", "構想中", "計算中", "コンパイル中", "デバッグ中", "デプロイ中", "推論中",
            "熟考中", "醸成中", "研磨中", "吟味中", "タイピング中", "調査中", "検索中",
            "作図中", "リファクタ中", "整理中", "検証中", "蒸留中", "調整中", "分解中",
            "構築中", "テスト中", "仕上げ中",
        ],
        Locale::Ko => &[
            "분석 중", "구상 중", "계산 중", "컴파일 중", "디버깅 중", "배포 중", "추론 중",
            "사고 중", "숙성 중", "다듬는 중", "고민 중", "타이핑 중", "훑는 중", "검색 중",
            "그리는 중", "리팩터링 중", "정리 중", "검증 중", "증류 중", "맞추는 중", "분해 중",
            "조립 중", "테스트 중", "마무리 중",
        ],
        Locale::En => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_locale_has_24_verbs() {
        assert_eq!(locale_spinner_verbs(Locale::ZhCN).len(), 24);
        assert_eq!(locale_spinner_verbs(Locale::ZhTW).len(), 24);
        assert_eq!(locale_spinner_verbs(Locale::Ja).len(), 24);
        assert_eq!(locale_spinner_verbs(Locale::Ko).len(), 24);
    }

    #[test]
    fn en_is_the_fallback_marker() {
        assert!(locale_spinner_verbs(Locale::En).is_empty());
    }
}
