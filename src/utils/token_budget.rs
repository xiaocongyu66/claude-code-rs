//! token_budget.rs — 从用户文本中解析 token 预算。
//!
//! 重写自 `src/utils/tokenBudget.ts`。原文件用三条 JS 正则匹配
//! "+500k"（锚定开头/结尾）、"use/spend 2M tokens"（全文）等预算写法；
//! 本实现按 ECMAScript 语义手写扫描器逐一还原（`\b`/`\s` 语义与
//! matchAll 的 lastIndex 前进规则），避免引入 regex crate。
//!
//! 位置/长度一律按字符索引返回（对齐 TS 的 UTF-16 索引，BMP 文本等价）。

/// ECMAScript `\s` 字符集。
fn is_ws(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// ECMAScript `\w`：`[A-Za-z0-9_]`（注意下划线算单词字符）。
fn is_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_suffix(ch: char) -> bool {
    matches!(ch.to_ascii_lowercase(), 'k' | 'm' | 'b')
}

fn multiplier(suffix: char) -> f64 {
    match suffix.to_ascii_lowercase() {
        'k' => 1_000.0,
        'm' => 1_000_000.0,
        'b' => 1_000_000_000.0,
        _ => 0.0,
    }
}

/// 匹配 `\d+(\.\d+)?`；返回结束位置与数字字面量文本。
fn parse_number(text: &[char], pos: usize) -> Option<(usize, String)> {
    let start = pos;
    let mut i = pos;
    while i < text.len() && text[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let mut end = i;
    if i + 1 < text.len() && text[i] == '.' && text[i + 1].is_ascii_digit() {
        end = i + 1;
        while end < text.len() && text[end].is_ascii_digit() {
            end += 1;
        }
    }
    Some((end, text[start..end].iter().collect()))
}

fn parse_budget_match(value: &str, suffix: char) -> Option<f64> {
    let n: f64 = value.parse().ok()?;
    Some(n * multiplier(suffix))
}

struct ShorthandMatch {
    /// matched value digits
    value: String,
    suffix: char,
    /// 整段匹配的起止字符位置
    start: usize,
    end: usize,
}

/// `/^\s*\+(\d+(?:\.\d+)?)\s*(k|m|b)\b/i`
fn match_shorthand_start(text: &[char]) -> Option<ShorthandMatch> {
    let mut i = 0;
    while i < text.len() && is_ws(text[i]) {
        i += 1;
    }
    let plus_pos = i;
    if i >= text.len() || text[i] != '+' {
        return None;
    }
    i += 1;
    let (num_end, value) = parse_number(text, i)?;
    i = num_end;
    while i < text.len() && is_ws(text[i]) {
        i += 1;
    }
    if i >= text.len() || !is_suffix(text[i]) {
        return None;
    }
    let suffix = text[i];
    let after = i + 1;
    if after < text.len() && is_word(text[after]) {
        return None; // \b 失败
    }
    Some(ShorthandMatch {
        value,
        suffix,
        start: plus_pos,
        end: after,
    })
}

/// `/\s\+(\d+(?:\.\d+)?)\s*(k|m|b)\s*[.!?]?\s*$/i`，返回最左匹配。
fn match_shorthand_end(text: &[char]) -> Option<ShorthandMatch> {
    for i in 0..text.len() {
        if !is_ws(text[i]) {
            continue;
        }
        let Some(&'+') = text.get(i + 1) else {
            continue;
        };
        let Some((num_end, value)) = parse_number(text, i + 2) else {
            continue;
        };
        let mut j = num_end;
        while j < text.len() && is_ws(text[j]) {
            j += 1;
        }
        if j >= text.len() || !is_suffix(text[j]) {
            continue;
        }
        let suffix = text[j];
        // tail: \s*[.!?]?\s*$ — \s 与 [.!?] 类不相交，贪心 \s* 后
        // 最多吞一个标点再贪心 \s* 直至结尾
        let mut k = j + 1;
        while k < text.len() && is_ws(text[k]) {
            k += 1;
        }
        if k < text.len() && matches!(text[k], '.' | '!' | '?') {
            k += 1;
            while k < text.len() && is_ws(text[k]) {
                k += 1;
            }
        }
        if k == text.len() {
            return Some(ShorthandMatch {
                value,
                suffix,
                start: i + 1,
                end: k,
            });
        }
    }
    None
}

struct VerboseMatch {
    value: String,
    suffix: char,
    start: usize,
    end: usize,
}

fn eq_ignore_case(slice: &[char], word: &str) -> bool {
    let bytes: Vec<char> = word.chars().collect();
    slice.len() == bytes.len()
        && slice
            .iter()
            .zip(bytes.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// `/\b(?:use|spend)\s+(\d+(?:\.\d+)?)\s*(k|m|b)\s*tokens?\b/i` 在位置
/// `i` 是否匹配；`\b` 位置即 match.index。
fn match_verbose_at(text: &[char], i: usize) -> Option<VerboseMatch> {
    // \b：前一字符不得为单词字符（后一字符必为 'u'/'s'，是单词字符）
    if i > 0 && is_word(text[i - 1]) {
        return None;
    }
    let j = if i + 3 <= text.len() && eq_ignore_case(&text[i..i + 3], "use") {
        i + 3
    } else if i + 5 <= text.len() && eq_ignore_case(&text[i..i + 5], "spend") {
        i + 5
    } else {
        return None;
    };
    // \s+（至少一个空白）
    if j >= text.len() || !is_ws(text[j]) {
        return None;
    }
    let mut k = j + 1;
    while k < text.len() && is_ws(text[k]) {
        k += 1;
    }
    let (num_end, value) = parse_number(text, k)?;
    k = num_end;
    while k < text.len() && is_ws(text[k]) {
        k += 1;
    }
    if k >= text.len() || !is_suffix(text[k]) {
        return None;
    }
    let suffix = text[k];
    let mut e = k + 1;
    while e < text.len() && is_ws(text[e]) {
        e += 1;
    }
    if e + 5 > text.len() || !eq_ignore_case(&text[e..e + 5], "token") {
        return None;
    }
    e += 5;
    if e < text.len() && (text[e] == 's' || text[e] == 'S') {
        e += 1;
    }
    if e < text.len() && is_word(text[e]) {
        return None; // \b 失败
    }
    Some(VerboseMatch {
        value,
        suffix,
        start: i,
        end: e,
    })
}

fn find_verbose_matches(text: &[char]) -> Vec<VerboseMatch> {
    let mut matches = Vec::new();
    let mut cursor = 0; // 对齐 matchAll 的 lastIndex 前进规则
    while cursor < text.len() {
        match (cursor..text.len())
            .find_map(|i| match_verbose_at(text, i))
        {
            Some(m) => {
                cursor = m.end;
                matches.push(m);
            }
            None => break,
        }
    }
    matches
}

/// 从文本解析 token 预算（如 "+500k"、"use 2M tokens"）；无匹配返回 None。
pub fn parse_token_budget(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    if let Some(m) = match_shorthand_start(&chars) {
        return parse_budget_match(&m.value, m.suffix);
    }
    if let Some(m) = match_shorthand_end(&chars) {
        return parse_budget_match(&m.value, m.suffix);
    }
    if let Some(m) = (0..chars.len()).find_map(|i| match_verbose_at(&chars, i)) {
        return parse_budget_match(&m.value, m.suffix);
    }
    None
}

/// 找出文本中所有预算写法的字符位置区间（与 parseTokenBudget 同源三条
/// 规则，VERBOSE 全量非重叠扫描）。
pub fn find_token_budget_positions(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut positions = Vec::new();

    if let Some(m) = match_shorthand_start(&chars) {
        positions.push((m.start, m.end));
    }

    if let Some(m) = match_shorthand_end(&chars) {
        // 避免输入仅为 "+500k" 时重复计数：结尾简写需消耗一个前导空白，
        // +1 后若落在开头简写的区间内则跳过
        let already_covered = positions
            .iter()
            .any(|(start, end)| m.start >= *start && m.start < *end);
        if !already_covered {
            positions.push((m.start, m.end));
        }
    }

    for m in find_verbose_matches(&chars) {
        positions.push((m.start, m.end));
    }

    positions
}

/// Intl.NumberFormat('en-US') 的三位千分位格式化（默认最多 3 位小数）。
fn format_en_us(n: f64) -> String {
    let negative = n < 0.0;
    let n = n.abs();
    let int_part = n.trunc() as u64;
    let frac = n - n.trunc();
    let digits = int_part.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let mut out = if frac == 0.0 {
        grouped
    } else {
        let frac_digits = format!("{:.3}", frac);
        format!("{grouped}.{}", &frac_digits[2..])
    };
    if negative {
        out.insert(0, '-');
    }
    out
}

/// 预算到达后的"继续工作"提示。
pub fn get_budget_continuation_message(pct: f64, turn_tokens: f64, budget: f64) -> String {
    format!(
        "Stopped at {}% of token target ({} / {}). Keep working — do not summarize.",
        pct,
        format_en_us(turn_tokens),
        format_en_us(budget)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shorthand_at_start() {
        assert_eq!(parse_token_budget("+500k"), Some(500_000.0));
        assert_eq!(parse_token_budget("  +1.5m"), Some(1_500_000.0));
    }

    #[test]
    fn parses_shorthand_at_end() {
        assert_eq!(parse_token_budget("do stuff +500k."), Some(500_000.0));
        assert_eq!(parse_token_budget("work +2b !"), Some(2_000_000_000.0));
    }

    #[test]
    fn parses_verbose() {
        assert_eq!(parse_token_budget("use 2M tokens"), Some(2_000_000.0));
        assert_eq!(
            parse_token_budget("Please spend 10k tokens more"),
            Some(10_000.0)
        );
    }

    #[test]
    fn returns_none_when_no_budget() {
        assert_eq!(parse_token_budget("just some text 2M"), None);
        assert_eq!(parse_token_budget("500k without plus"), None);
        assert_eq!(parse_token_budget("use many tokens"), None);
    }

    #[test]
    fn start_takes_priority_over_end() {
        // "+500k" 只匹配开头简写（结尾简写需要前导空白）
        let positions = find_token_budget_positions("+500k");
        assert_eq!(positions, vec![(0, 5)]);
    }

    #[test]
    fn positions_cover_all_kinds() {
        // "+10k work, use 20m tokens" — 'u' 位于字符索引 11，match 结束于 25
        let positions = find_token_budget_positions("+10k work, use 20m tokens");
        let contains = |start, end| positions.contains(&(start, end));
        assert!(contains(0, 4)); // "+10k"
        assert!(contains(11, 25)); // "use 20m tokens"
    }

    #[test]
    fn end_positions_do_not_overlap_start() {
        // 结尾简写起点落在开头简写区间内时跳过
        let positions = find_token_budget_positions("+500k");
        assert_eq!(positions.len(), 1);
    }

    #[test]
    fn continuation_message_format() {
        let msg = get_budget_continuation_message(50.0, 1200.0, 50000.0);
        assert_eq!(
            msg,
            "Stopped at 50% of token target (1,200 / 50,000). Keep working — do not summarize."
        );
    }
}
