//! user_prompt_keywords.rs — 用户输入中的关键词模式识别。
//!
//! 重写自 `src/utils/userPromptKeywords.ts`。原文件用两条 JS 正则
//! （`\b` 词边界 + 小写化）匹配负面情绪 / 继续工作关键词；本实现按
//! JS `\w = [A-Za-z0-9_]` 语义做词元级匹配还原：
//!
//! - 单词内多选（如 `shit(ty|tiest)?`）在词元内比较
//! - 跨词模式（如 `piece of shit`）要求相邻词元间恰好一个空格
//!   （正则中是字面量单个空格）

/// 判断输入是否匹配负面情绪关键词。
///
/// 对齐 TS：
/// ```text
/// /\b(wtf|wth|ffs|omfg|shit(ty|tiest)?|dumbass|horrible|awful|
///   piss(ed|ing)? off|piece of (shit|crap|junk)|what the (fuck|hell)|
///   fucking? (broken|useless|terrible|awful|horrible)|fuck you|
///   screw (this|you)|so frustrating|this sucks|damn it)\b/
/// ```
pub fn matches_negative_keyword(input: &str) -> bool {
    let text: Vec<char> = to_lower_ascii(input).chars().collect();

    for i in 0..text.len() {
        if !is_word_start(&text, i) {
            continue;
        }
        let first_end = word_end(&text, i);
        let first: String = text[i..first_end].iter().collect();

        match first.as_str() {
            "wtf" | "wth" | "ffs" | "omfg" | "dumbass" | "horrible" | "awful" | "shit"
            | "shitty" | "shittiest" => return true,
            "piss" | "pissed" | "pissing" => {
                if next_word(&text, first_end).as_deref() == Some("off") {
                    return true;
                }
            }
            "piece" => {
                if let Some((second, second_end)) = next_word_span(&text, first_end) {
                    if second == "of" {
                        if let Some((third, _)) = next_word_span(&text, second_end) {
                            if third == "shit" || third == "crap" || third == "junk" {
                                return true;
                            }
                        }
                    }
                }
            }
            "what" => {
                if let Some((second, second_end)) = next_word_span(&text, first_end) {
                    if second == "the" {
                        if let Some((third, _)) = next_word_span(&text, second_end) {
                            if third == "fuck" || third == "hell" {
                                return true;
                            }
                        }
                    }
                }
            }
            "fuck" => {
                if let Some((second, _)) = next_word_span(&text, first_end) {
                    if matches!(
                        second.as_str(),
                        "broken" | "useless" | "terrible" | "awful" | "horrible" | "you"
                    ) {
                        return true;
                    }
                }
            }
            "fucking" => {
                // 与 "fuck you" 不同：`fucking? (adj)` 只接形容词
                if let Some((second, _)) = next_word_span(&text, first_end) {
                    if matches!(
                        second.as_str(),
                        "broken" | "useless" | "terrible" | "awful" | "horrible"
                    ) {
                        return true;
                    }
                }
            }
            "screw" => {
                if let Some((second, _)) = next_word_span(&text, first_end) {
                    if second == "this" || second == "you" {
                        return true;
                    }
                }
            }
            "so" => {
                if next_word(&text, first_end).as_deref() == Some("frustrating") {
                    return true;
                }
            }
            "this" => {
                if next_word(&text, first_end).as_deref() == Some("sucks") {
                    return true;
                }
            }
            "damn" => {
                if next_word(&text, first_end).as_deref() == Some("it") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// 判断输入是否匹配"继续工作"关键词。
///
/// 对齐 TS：先 `toLowerCase().trim()`，仅当整个 prompt 为 "continue"
/// 时匹配；否则全文查 "keep going" / "go on" 词组。
pub fn matches_keep_going_keyword(input: &str) -> bool {
    let lower = to_lower_ascii(input);
    let trimmed = lower.trim();
    if trimmed == "continue" {
        return true;
    }
    let text: Vec<char> = trimmed.chars().collect();
    find_phrase(&text, "keep going") || find_phrase(&text, "go on")
}

fn is_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// 词元起始位置：当前字符是 \w，且前一字符不是 \w（等价 \b 模式）。
fn is_word_start(text: &[char], i: usize) -> bool {
    is_word(text[i]) && (i == 0 || !is_word(text[i - 1]))
}

/// 词元结束位置（词元为 \w 字符序列）。
fn word_end(text: &[char], start: usize) -> usize {
    let mut end = start;
    while end < text.len() && is_word(text[end]) {
        end += 1;
    }
    end
}

/// 位置 `after` 之后隔一个空格的下一个词元；返回词文本与其结束位置。
/// 与正则字面量空格一致：必须恰好一个空格（多个空格不匹配）。
fn next_word_span(text: &[char], after: usize) -> Option<(String, usize)> {
    if text.get(after) != Some(&' ') {
        return None;
    }
    let start = after + 1;
    if start >= text.len() || !is_word(text[start]) {
        return None;
    }
    let end = word_end(text, start);
    Some((text[start..end].iter().collect(), end))
}

/// 仅取下一词元文本。
fn next_word(text: &[char], after: usize) -> Option<String> {
    next_word_span(text, after).map(|(word, _)| word)
}

/// 词组匹配：`\bphrase\b`（词边界开始，词边界结束，组内为字面子串）。
fn find_phrase(text: &[char], phrase: &str) -> bool {
    let target: Vec<char> = phrase.chars().collect();
    if target.is_empty() || text.len() < target.len() {
        return false;
    }
    for i in 0..=text.len() - target.len() {
        if text[i..i + target.len()] != target[..] {
            continue;
        }
        if i > 0 && is_word(text[i - 1]) {
            continue; // 前边界失败
        }
        let after = i + target.len();
        if after < text.len() && is_word(text[after]) {
            continue; // 后边界失败
        }
        return true;
    }
    false
}

/// ASCII 小写化（关键词均为 ASCII；对齐 TS `toLowerCase` 在此输入域上等价）。
fn to_lower_ascii(input: &str) -> String {
    input.chars().map(|c| c.to_ascii_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_negative_keywords() {
        assert!(matches_negative_keyword("this is wtf territory"));
        assert!(matches_negative_keyword("WTF?!")); // 大小写不敏感
        assert!(matches_negative_keyword("That piece of junk"));
        assert!(matches_negative_keyword("what the hell"));
        assert!(matches_negative_keyword("this is so fucking broken"));
        assert!(matches_negative_keyword("damn it all"));
        assert!(matches_negative_keyword("you dumbass"));
        assert!(matches_negative_keyword("awful, just awful"));
    }

    #[test]
    fn ignores_substrings_without_word_boundaries() {
        assert!(!matches_negative_keyword("wtfish behavior")); // \b 后跟 \w 不算
        assert!(!matches_negative_keyword("awfully good")); // awfully ≠ awful
        assert!(!matches_negative_keyword("scRewed up")); // screwed ≠ screw + ed
    }

    #[test]
    fn detects_keep_going() {
        assert!(matches_keep_going_keyword(" continue "));
        assert!(matches_keep_going_keyword("please keep going"));
        assert!(matches_keep_going_keyword("okay, go on"));
        // "continue" 必须独占整个 prompt 才匹配
        assert!(!matches_keep_going_keyword("continue working"));
    }

    #[test]
    fn no_false_positives() {
        assert!(!matches_negative_keyword("great work"));
        assert!(!matches_keep_going_keyword("stop now"));
    }
}
