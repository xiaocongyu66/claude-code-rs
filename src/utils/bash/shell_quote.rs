//! shell_quote.rs — shell-quote (npm 1.12.0) 的纯 Rust 实现及 TS 安全包装。
//!
//! 重写自 `src/utils/bash/shellQuote.ts`。npm 包 `shell-quote` 无 Rust 等价物，
//! 其 `quote` / `parse` 按 1.12.0 源码语义手写：
//!
//! - `quote`：四级分支（同时含 `'` 与 `!` → 单引号转义；含 `"`/空白/`\` 且无 `'`
//!   → 原样单引号；含 `"`/`'`/空白 → 双引号转义 `"\\$``；其余 → 逐字符反斜杠
//!   转义，含 Windows 盘符前缀 `C:` 豁免），对象 token（glob/op/comment）按
//!   quote.js 处理。
//! - `parse`：chunker 正则（CONTROL | (ANSI_C|BAREWORD|DOUBLE_QUOTE|SINGLE_QUOTE)+）
//!   用手工扫描实现；逐字符状态机做引号/转义/`$` 展开/注释处理；ANSI-C `$'...'`
//!   展开与 env 函数的 TOKEN JSON 往返逐条对齐。
//!
//! 已知差异（均不影响 claude-code 现有调用方）：
//! - TS 的 ParseEntry 是联合类型；Rust 用枚举。env 函数返回对象经 TOKEN 往返
//!   后仅重建 op / glob / comment 三种已知形状，未知 JSON 形状报错
//!   （claude-code 唯一的函数 env 用法只返回字符串，该路径不被触发）。
//! - `parse` 的 opts（escape / splitUnquoted）未实现：claude-code 无调用方使用，
//!   相应的 pendingNw / flushRun / words 分支一并省略。
//! - TS 的 `logError(error)` 对齐为 `log::log_error(io::Error::new(Other, msg))`；抛错改为
//!   `ShellParseResult::Failed` / `ShellQuoteResult::Failed`。
//! - JS 字符串下标按 UTF-16 code unit 计，Rust 按标量值（char）扫描；BMP 字符
//!   完全一致，astral 字符（emoji 等）在 `parseEnvVar` 的 `i+1` 推进处有细微差异。
//! - TS 的 `quote()` 在严格校验失败时先 `jsonStringify` 再二次引号，失败则抛错；
//!   Rust 无 panic：二次失败时退化为逐参数单引号兜底（对 `[String]` 输入
//!   严格路径恒成功，该分支实际不可达）。

use std::collections::HashMap;

use rand::Rng;
use serde_json::Value;

use crate::utils::log::log_error;
use crate::utils::slow_operations::json_stringify;

// ────────────────────────────────────────────────────────────────────────────
// 类型（对齐 shell-quote 的 ParseEntry 与 TS 的包装类型）
// ────────────────────────────────────────────────────────────────────────────

/// shell-quote 解析条目：字符串、控制算符、glob 模式、注释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseEntry {
    Str(String),
    Control { op: String },
    Glob { pattern: String },
    Comment { comment: String },
}

/// env 查找返回值：字符串，或（函数 env 返回对象时）JSON 值。
#[derive(Debug, Clone, PartialEq)]
pub enum EnvValue {
    Str(String),
    Json(Value),
}

/// parse 的 env 来源：对象映射或函数（对齐 TS 的
/// `Record<string, string | undefined> | ((key) => value)`）。
pub enum ShellEnv<'a> {
    Map(&'a HashMap<String, String>),
    Function(&'a dyn Fn(&str) -> Option<EnvValue>),
}

/// `tryParseShellCommand` 结果（对齐 TS ShellParseResult）。
pub enum ShellParseResult {
    Success { tokens: Vec<ParseEntry> },
    Failed { error: String },
}

/// `tryQuoteShellArgs` 结果（对齐 TS ShellQuoteResult）。
pub enum ShellQuoteResult {
    Success { quoted: String },
    Failed { error: String },
}

// ────────────────────────────────────────────────────────────────────────────
// shell-quote 常量（quote.js / parse.js）
// ────────────────────────────────────────────────────────────────────────────

/// quote.js 的 OPS 列表——控制算符白名单。
const OPS: &[&str] = &[
    "||",
    "&&",
    ";;&",
    ";;",
    ";&",
    "|&",
    "<(",
    ">(",
    "<<<",
    "<<-",
    "<<",
    ">>",
    ">&",
    ">|",
    "&>>",
    "&>",
    "<&",
    "<>",
    "&",
    ";",
    "(",
    ")",
    "|",
    "<",
    ">",
];

/// parse.js 的 META：`|&;()<> \t`。
fn is_meta(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>' | ' ' | '\t')
}

/// JS `\s`：WhiteSpace + LineTerminator 的精确字符集。
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// 单字符是否为控制算符（controlRE.test(c)）。
fn is_control_char(c: char) -> bool {
    matches!(c, '&' | ';' | '(' | ')' | '|' | '<' | '>')
}

/// 整段文本是否恰为某个控制算符（controlRE.test(s)）。
fn is_control_op(s: &str) -> bool {
    OPS.contains(&s)
}

/// glob 模式转义字符集（quote.js GLOB_SHELL_SPECIAL）：
/// `\s # ! " $ & ' ( ) : ; < = > @ \ ^ ~`（注意不含 `* ?`）。
fn is_glob_special(c: char) -> bool {
    is_js_whitespace(c)
        || matches!(
            c,
            '#' | '!' | '"' | '$' | '&' | '\'' | '(' | ')' | ':' | ';' | '<' | '=' | '>' | '@'
                | '\\'
                | '^'
                | '`'
                | '|'
                | '~'
        )
}

/// quote.js 的 `["'\s]`：空白、`"` 或 `'`。
fn has_space_or_quote(s: &str) -> bool {
    s.chars().any(|c| is_js_whitespace(c) || c == '"' || c == '\'')
}

/// 逐字符反斜杠转义分支的 special 类（quote.js 第 4 分支）：
/// `#!"$&'()*,:;<=>?@[\]^`{|}~`
fn is_quote_special(c: char) -> bool {
    matches!(
        c,
        '#'
            | '!'
            | '"'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | ','
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '['
            | '\\'
            | ']'
            | '^'
            | '`'
            | '{'
            | '|'
            | '}'
            | '~'
    )
}

/// 行终止符（quote.js LINE_TERMINATORS，用于 comment/glob 的 throw 校验）。
fn has_line_terminator(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
}

/// 生成 TOKEN：32 个十六进制字符（parse.js 用 4 段随机 hex 拼接）。
fn generate_token() -> String {
    let bytes: [u8; 16] = rand::thread_rng().gen();
    let mut s = String::with_capacity(32);
    for b in &bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ────────────────────────────────────────────────────────────────────────────
// quote（quote.js）
// ────────────────────────────────────────────────────────────────────────────

/// shell-quote `quote`。输入可含对象 token（ParseEntry）或字符串。
/// 失败返回错误消息（对齐 quote.js 的 TypeError）。
pub fn quote_tokens(xs: &[ParseEntry]) -> Result<String, String> {
    let mut saw_comment = false;
    let mut parts: Vec<String> = Vec::with_capacity(xs.len());
    for entry in xs {
        match entry {
            ParseEntry::Str(s) => {
                if saw_comment && has_line_terminator(s) {
                    return Err(
                        "a token after a `comment` must not contain line terminators".into(),
                    );
                }
                if s.is_empty() {
                    parts.push("''".into());
                } else if s.contains('\'') && s.contains('!') {
                    // 分支 1：同时含 ' 与 ! → 单引号包裹，内嵌 ' 用 '"'"' 转义。
                    let escaped = s.replace('\'', "'\"'\"'");
                    parts.push(format!("'{}'", escaped));
                } else if (s.contains('"')
                    || s.chars().any(is_js_whitespace)
                    || s.contains('\\'))
                    && !s.contains('\'')
                {
                    // 分支 2：含 " / 空白 / \ 且不含 ' → 原样单引号（不转义）。
                    parts.push(format!("'{}'", s));
                } else if has_space_or_quote(s) {
                    // 分支 3：含 " / ' / 空白 → 双引号，转义 " \ $ `（不转义 !）。
                    // 对齐 quote.js 的 `s.replace(/(["\\$`])/g, '\\$1')`：逐字符加反斜杠。
                    let mut escaped = String::with_capacity(s.len() * 2);
                    for c in s.chars() {
                        if matches!(c, '"' | '\\' | '$' | '`') {
                            escaped.push('\\');
                        }
                        escaped.push(c);
                    }
                    parts.push(format!("\"{}\"", escaped));
                } else {
                    // 分支 4：逐字符转义；Windows 盘符 `C:` 前缀豁免（保留原样）。
                    parts.push(quote_escape_specials(s));
                }
            }
            ParseEntry::Control { op } => {
                if !OPS.contains(&op.as_str()) {
                    return Err(format!("invalid `op` value: {}", op));
                }
                // op.replace(/[\s\S]/g, '\\$&')：每个字符前加反斜杠。
                let mut escaped = String::with_capacity(op.len() * 2);
                for c in op.chars() {
                    escaped.push('\\');
                    escaped.push(c);
                }
                parts.push(escaped);
            }
            ParseEntry::Glob { pattern } => {
                if has_line_terminator(pattern) {
                    return Err("glob `pattern` must not contain line terminators".into());
                }
                if pattern.is_empty() {
                    parts.push("''".into());
                } else {
                    let mut escaped = String::with_capacity(pattern.len() * 2);
                    for c in pattern.chars() {
                        if is_glob_special(c) {
                            escaped.push('\\');
                        }
                        escaped.push(c);
                    }
                    parts.push(escaped);
                }
            }
            ParseEntry::Comment { comment } => {
                if has_line_terminator(comment) {
                    return Err("`comment` must not contain line terminators".into());
                }
                saw_comment = true;
                parts.push(format!("#{}", comment));
            }
        }
    }
    Ok(parts.join(" "))
}

/// quote 第 4 分支：逐字符反斜杠转义，`([A-Za-z]:)?` 前缀保持不转义。
fn quote_escape_specials(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() * 2);
    let mut i = 0usize;
    while i < chars.len() {
        // `([A-Za-z]:)?(special)`：盘符 + 冒号 + 特殊字符 → 仅转义特殊字符。
        if i + 2 < chars.len()
            && chars[i].is_ascii_alphabetic()
            && chars[i + 1] == ':'
            && is_quote_special(chars[i + 2])
        {
            out.push(chars[i]);
            out.push(':');
            out.push('\\');
            out.push(chars[i + 2]);
            i += 3;
        } else if is_quote_special(chars[i]) {
            out.push('\\');
            out.push(chars[i]);
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// parse（parse.js）—— chunker
// ────────────────────────────────────────────────────────────────────────────

/// chunker 的 CONTROL 组：在位置 `i` 尝试匹配控制算符。
/// 返回 (算符文本, 消费字节数)。交替顺序对齐 parse.js。
fn scan_control(s: &str, i: usize) -> Option<(&'static str, usize)> {
    let rest = &s[i..];
    if rest.starts_with("||") {
        return Some(("||", 2));
    }
    if rest.starts_with("&&") {
        return Some(("&&", 2));
    }
    if rest.starts_with("&>>") {
        return Some(("&>>", 3));
    }
    if rest.starts_with("&>") {
        return Some(("&>", 2));
    }
    if rest.starts_with(";;&") {
        return Some((";;&", 3));
    }
    if rest.starts_with(";;") {
        return Some((";;", 2));
    }
    if rest.starts_with(";&") {
        return Some((";&", 2));
    }
    if rest.starts_with("|&") {
        return Some(("|&", 2));
    }
    if rest.starts_with("<(") {
        return Some(("<(", 2));
    }
    if rest.starts_with("<<<") {
        return Some(("<<<", 3));
    }
    if rest.starts_with("<<-") {
        return Some(("<<-", 3));
    }
    // `<<(?!()`：`<<(` 拆成 `<` + `(`（zsh 读法）。
    if rest.starts_with("<<") && !rest[2..].starts_with('(') {
        return Some(("<<", 2));
    }
    if rest.starts_with(">>") {
        return Some((">>", 2));
    }
    if rest.starts_with(">&") {
        return Some((">&", 2));
    }
    if rest.starts_with(">|") {
        return Some((">|", 2));
    }
    if rest.starts_with(">(") {
        return Some((">(", 2));
    }
    if rest.starts_with("<&") {
        return Some(("<&", 2));
    }
    if rest.starts_with("<>") {
        return Some(("<>", 2));
    }
    match rest.chars().next() {
        Some(c) if is_control_char(c) => {
            let op = match c {
                '&' => "&",
                ';' => ";",
                '(' => "(",
                ')' => ")",
                '|' => "|",
                '<' => "<",
                '>' => ">",
                _ => return None,
            };
            Some((op, c.len_utf8()))
        }
        _ => None,
    }
}

/// 从 `"` 开始扫描双引号串，返回闭合后的字节位置（不含闭合引号则 None）。
/// DOUBLE_QUOTE = `"(?:\\[\s\S]|[^"\\])*"`。
fn scan_double_quote(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut p = i + 1;
    while p < bytes.len() {
        let b = bytes[p];
        if b == b'"' {
            return Some(p + 1);
        }
        if b == b'\\' && p + 1 < bytes.len() {
            p += 2;
        } else {
            p += 1;
        }
    }
    None
}

/// 从 `'` 开始扫描单引号串，返回闭合后的字节位置。
/// SINGLE_QUOTE = `'([^']*?)'`。
fn scan_single_quote(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut p = i + 1;
    while p < bytes.len() {
        if bytes[p] == b'\'' {
            return Some(p + 1);
        }
        p += 1;
    }
    None
}

/// ANSI-C 引用 `$'...'`：从 `$` 开始，要求闭合 `'`。
/// ANSI_C_QUOTE = `\$'(?:\\[\s\S]|[^\\'])*?'`。
fn scan_ansi_c_quote(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if i + 1 >= bytes.len() || bytes[i] != b'$' || bytes[i + 1] != b'\'' {
        return None;
    }
    let mut p = i + 2;
    while p < bytes.len() {
        let b = bytes[p];
        if b == b'\'' {
            return Some(p + 1);
        }
        if b == b'\\' && p + 1 < bytes.len() {
            p += 2;
        } else {
            p += 1;
        }
    }
    None
}

/// BAREWORD 在位置 `i` 的单次消费（返回消费字节数，0 表示不匹配）。
/// `\\['"$\\ + META]` | `$$` | `$` | `[^\s'"$ + META]`
fn bareword_consume(s: &str, i: usize) -> usize {
    let bytes = s.as_bytes();
    let b = bytes[i];
    if b == b'\\' && i + 1 < bytes.len() {
        let n = bytes[i + 1] as char;
        if n == '\'' || n == '"' || n == '$' || n == '\\' || is_meta(n) {
            return 2;
        }
    }
    if b == b'$' {
        // `$$` 或单 `$`（ANSI_C 引用已在 chunker 层优先整体消费）。
        return if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            2
        } else {
            1
        };
    }
    let c = b as char;
    if !is_js_whitespace(c) && c != '\'' && c != '"' && c != '$' && !is_meta(c) {
        return 1;
    }
    0
}

/// chunker 的第二组：从 `i` 贪心消费 ANSI_C / BAREWORD / 双引号 / 单引号组件。
/// 返回 chunk 结束字节位置；一个组件都消费不了则返回 None。
fn scan_chunk(s: &str, i: usize) -> Option<usize> {
    let len = s.len();
    let mut p = i;
    let mut matched = false;
    loop {
        if p >= len {
            break;
        }
        if let Some(end) = scan_ansi_c_quote(s, p) {
            p = end;
            matched = true;
            continue;
        }
        let n = bareword_consume(s, p);
        if n > 0 {
            p += n;
            matched = true;
            continue;
        }
        let bytes = s.as_bytes();
        if bytes[p] == b'"' {
            if let Some(end) = scan_double_quote(s, p) {
                p = end;
                matched = true;
                continue;
            }
        }
        if bytes[p] == b'\'' {
            if let Some(end) = scan_single_quote(s, p) {
                p = end;
                matched = true;
                continue;
            }
        }
        break;
    }
    if matched {
        Some(p)
    } else {
        None
    }
}

/// matchAll(string, chunker)：从左到右收集所有非重叠匹配
/// (起始字节位置, 匹配文本)。两处都不匹配时跳过一个字符。
fn find_matches(s: &str) -> Vec<(usize, String)> {
    let len = s.len();
    let mut matches = Vec::new();
    let mut i = 0usize;
    while i < len {
        if let Some((op, n)) = scan_control(s, i) {
            matches.push((i, op.to_string()));
            i += n;
            continue;
        }
        if let Some(end) = scan_chunk(s, i) {
            matches.push((i, s[i..end].to_string()));
            i = end;
            continue;
        }
        i += 1;
    }
    matches
}

// ────────────────────────────────────────────────────────────────────────────
// parse（parse.js）—— ANSI-C 引用
// ────────────────────────────────────────────────────────────────────────────

/// ANSI-C 转义分类结果：(替换文本, 反斜杠后消费的字符数)。
/// 不匹配已知转义（如纯 `\` 结尾、`\q`）返回 None。
fn classify_ansi_escape(rest: &[char]) -> Option<(String, usize)> {
    let first = *rest.first()?;
    // `c(?:\\|[\s\S])`：后随任意一个字符（含 `\`）。
    if first == 'c' {
        let ctrl = *rest.get(1)?;
        let ch = if ctrl == '?' {
            '\u{007F}'
        } else {
            char::from_u32((ctrl as u32) & 0x1F).unwrap_or(ctrl)
        };
        return Some((ch.to_string(), 2));
    }
    // `[0-7]{1,3}`：八进制。
    if ('0'..='7').contains(&first) {
        let mut digits = String::new();
        for i in 0..3 {
            match rest.get(i) {
                Some(d) if ('0'..='7').contains(d) => digits.push(*d),
                _ => break,
            }
        }
        let n = digits.len();
        let v = u32::from_str_radix(&digits, 8).unwrap_or(0) & 0xFF;
        return Some((
            char::from_u32(v).map(|c| c.to_string()).unwrap_or_default(),
            n,
        ));
    }
    // `x[\dA-Fa-f]{1,2}` / `u{1,4}` / `U{1,8}`：十六进制。
    if matches!(first, 'x' | 'u' | 'U') {
        let max_len = if first == 'x' { 2 } else if first == 'u' { 4 } else { 8 };
        let mut hex = String::new();
        for i in 0..max_len {
            match rest.get(1 + i) {
                Some(d) if d.is_ascii_hexdigit() => hex.push(*d),
                _ => break,
            }
        }
        if hex.is_empty() {
            return None;
        }
        let consumed = 1 + hex.len();
        let cp = u32::from_str_radix(&hex, 16).ok()?;
        if cp > 0x10FFFF {
            // expandAnsiCEscape 对超范围返回原匹配 m（含反斜杠）。
            let mut original = String::from("\\");
            original.push(first);
            original.push_str(&hex);
            return Some((original, consumed));
        }
        if let Some(c) = char::from_u32(cp) {
            return Some((c.to_string(), consumed));
        }
        // 孤立代理项（JS 可表示、Rust 不能）：退化为字面量。
        let mut original = String::from("\\");
        original.push(first);
        original.push_str(&hex);
        return Some((original, consumed));
    }
    // `[abeEfnrtv\\'"?]`：单字符集合（ANSI_C_LETTERS/CHARS 映射）。
    let ch = match first {
        'a' => '\u{0007}',
        'b' => '\u{0008}',
        'e' | 'E' => '\u{001B}',
        'f' => '\u{000C}',
        'n' => '\u{000A}',
        'r' => '\u{000D}',
        't' => '\u{0009}',
        'v' => '\u{000B}',
        '\\' | '\'' | '"' | '?' => first,
        _ => return None,
    };
    Some((ch.to_string(), 1))
}

/// expandAnsiC(body)：展开 `\x` / `\n` / `\cX` 等转义，NUL 截断。
fn expand_ansi_c(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            match classify_ansi_escape(&chars[i + 1..]) {
                Some((text, consumed)) => {
                    out.push_str(&text);
                    i += 1 + consumed;
                }
                None => {
                    out.push('\\');
                    i += 1;
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    // `.split('\0')[0]`：NUL 结束字符串（bash 语义）。
    match out.find('\u{0}') {
        Some(idx) => out[..idx].to_string(),
        None => out,
    }
}

/// closesAnsiC(s, i)：`$'...'` 是否在串内闭合。
/// 闭合时返回闭合引号之后的字符索引（对齐 ansiCAt.lastIndex），否则 None。
fn closes_ansi_c(chars: &[(usize, char)], ci: usize) -> Option<usize> {
    if chars.get(ci)?.1 != '$' {
        return None;
    }
    if chars.get(ci + 1)?.1 != '\'' {
        return None;
    }
    let len = chars.len();
    let mut k = ci + 2;
    while k < len {
        let ch = chars[k].1;
        if ch == '\'' {
            return Some(k + 1);
        }
        if ch == '\\' {
            if k + 1 < len {
                // `\\[\s\S]` 转义对继续作为 body 一部分。
                k += 2;
            } else {
                // `\\?$`：串尾反斜杠，视为未闭合。
                return None;
            }
        } else {
            // `[^\\']`
            k += 1;
        }
    }
    None
}

// ────────────────────────────────────────────────────────────────────────────
// parse（parse.js）—— chunk 状态机
// ────────────────────────────────────────────────────────────────────────────

/// parseEnvVar：消费 `$` 后的变量名并查询 env（getVar）。
/// `*ci` 的语义与 parse.js 一致：返回时为让外层 `i++` 落在正确位置处的值。
fn parse_env_var(
    chars: &[(usize, char)],
    ci: &mut usize,
    lookup: &dyn Fn(&str) -> Option<EnvValue>,
    token: &str,
) -> Result<String, String> {
    // i += 1：跳过 `$`。
    *ci += 1;
    let len = chars.len();
    let first = chars.get(*ci).map(|p| p.1);
    let varname: String;
    if first == Some('{') {
        *ci += 1;
        if chars.get(*ci).map(|p| p.1) == Some('}') {
            // `throw new Error('Bad substitution: ' + s.slice(i - 2, i + 1))` → "${}"。
            return Err("Bad substitution: ${}".into());
        }
        // 按花括号深度匹配，嵌套 `${` 的内部 `}` 不结束外层替换。
        let mut depth = 1i32;
        let mut varend = *ci;
        while depth > 0 && varend < len {
            let ch = chars[varend].1;
            if ch == '{' && varend > 0 && chars[varend - 1].1 == '$' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
            }
            varend += 1;
        }
        if depth != 0 {
            // `throw new Error('Bad substitution: ' + s.slice(i))`。
            let rest: String = chars.iter().skip(*ci).map(|p| p.1).collect();
            return Err(format!("Bad substitution: {rest}"));
        }
        let close = varend - 1; // `}` 的字符索引
        varname = chars
            .iter()
            .skip(*ci)
            .take(close - *ci)
            .map(|p| p.1)
            .collect();
        *ci = close;
    } else if matches!(
        first,
        Some('*') | Some('@') | Some('#') | Some('?') | Some('$') | Some('!') | Some('-')
    ) {
        // 特殊变量名单字符；不移动 ci（外层 i++ 跳过它）。
        varname = first.map(|c| c.to_string()).unwrap_or_default();
    } else {
        // `[^\w\d_]` 边界：消费最大 [A-Za-z0-9_] 串。
        let vstart = *ci;
        let mut wend = vstart;
        while wend < len {
            let ch = chars[wend].1;
            if ch.is_ascii_alphanumeric() || ch == '_' {
                wend += 1;
            } else {
                break;
            }
        }
        if wend == len {
            // `!varend`：剩余全部是 word 字符 → varname 为全部，i = s.length。
            varname = chars.iter().skip(vstart).map(|p| p.1).collect();
            *ci = len;
        } else if wend == vstart {
            // varend.index == 0：varname 为空，i 回退一位（外层 ++ 回到原位）。
            varname = String::new();
            *ci = vstart - 1;
        } else {
            varname = chars
                .iter()
                .skip(vstart)
                .take(wend - vstart)
                .map(|p| p.1)
                .collect();
            *ci = wend - 1;
        }
    }
    // getVar(env, '', varname)
    Ok(match lookup(&varname) {
        Some(EnvValue::Str(s)) => s,
        Some(EnvValue::Json(v)) => match serde_json::to_string(&v) {
            Ok(json) => format!("{token}{json}{token}"),
            Err(e) => return Err(e.to_string()),
        },
        None => {
            if varname.is_empty() {
                "$".to_string()
            } else {
                String::new()
            }
        }
    })
}

enum ChunkOutcome {
    Many(Vec<ParseEntry>),
    None,
}

/// parse 的单 chunk 处理（parse.js matches.map 回调的主体）。
fn process_chunk(
    chunk: &str,
    match_index: usize,
    full: &str,
    lookup: &dyn Fn(&str) -> Option<EnvValue>,
    token: &str,
    commented: &mut bool,
) -> Result<ChunkOutcome, String> {
    if chunk.is_empty() || *commented {
        return Ok(ChunkOutcome::None);
    }
    if is_control_op(chunk) {
        return Ok(ChunkOutcome::Many(vec![ParseEntry::Control {
            op: chunk.to_string(),
        }]));
    }

    let chars: Vec<(usize, char)> = chunk.char_indices().collect();
    let len = chars.len();
    let mut quote: Option<char> = None;
    let mut esc = false;
    let mut out = String::new();
    let mut is_glob = false;

    let mut ci = 0usize;
    while ci < len {
        let c = chars[ci].1;
        // isGlob = isGlob || (!quote && (c === '*' || c === '?'))
        if quote.is_none() && (c == '*' || c == '?') {
            is_glob = true;
        }
        if esc {
            out.push(c);
            esc = false;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '\'' {
                // 单引号内一切字面。
                out.push(c);
            } else if c == '\\' {
                // 双引号内：`\` 仅脱义 `" \ $`，其余保留反斜杠；
                // `\` + 一个字符 = i 多进 1（循环再 +1，共 +2）。
                match chars.get(ci + 1).map(|p| p.1) {
                    Some(n) if n == '"' || n == '\\' || n == '$' => {
                        out.push(n);
                        ci += 1;
                    }
                    Some(n) => {
                        out.push('\\');
                        out.push(n);
                        ci += 1;
                    }
                    None => {
                        out.push('\\');
                    }
                }
            } else if c == '$' {
                let value = parse_env_var(&chars, &mut ci, lookup, token)?;
                out.push_str(&value);
                // parse_env_var 已按 JS 语义留好位置，直接靠循环 +1 落位。
            } else {
                out.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if is_control_char(c) {
            // 防御分支（chunk 内未转义的控制字符理论上不存在）。
            return Ok(ChunkOutcome::Many(vec![ParseEntry::Control {
                op: chunk.to_string(),
            }]));
        } else if c == '#' {
            // 注释：`#` 后续全部为注释内容（full 字符串的绝对位置）。
            *commented = true;
            let content_start = match chars.get(ci + 1) {
                Some(p) => match_index + p.0,
                None => match_index + chunk.len(),
            };
            let comment = if content_start <= full.len() {
                full[content_start..].to_string()
            } else {
                String::new()
            };
            let comment_entry = ParseEntry::Comment { comment };
            if !out.is_empty() {
                return Ok(ChunkOutcome::Many(vec![
                    ParseEntry::Str(std::mem::take(&mut out)),
                    comment_entry,
                ]));
            }
            return Ok(ChunkOutcome::Many(vec![comment_entry]));
        } else if c == '\\' {
            esc = true;
        } else if c == '$' && chars.get(ci + 1).map(|p| p.1) == Some('\'') {
            if let Some(close) = closes_ansi_c(&chars, ci) {
                // `$'...'`：body 展开后拼入；i = lastIndex - 1（循环 +1 到 close）。
                let body: String = chars
                    .iter()
                    .skip(ci + 2)
                    .take((close - 1).saturating_sub(ci + 2))
                    .map(|p| p.1)
                    .collect();
                out.push_str(&expand_ansi_c(&body));
                ci = close - 1;
            } else {
                // 串内未闭合的 `$'`：退回普通 `$` 展开。
                let value = parse_env_var(&chars, &mut ci, lookup, token)?;
                out.push_str(&value);
            }
        } else if c == '$' {
            let value = parse_env_var(&chars, &mut ci, lookup, token)?;
            out.push_str(&value);
        } else {
            out.push(c);
        }
        ci += 1;
    }

    if is_glob {
        return Ok(ChunkOutcome::Many(vec![ParseEntry::Glob {
            pattern: out,
        }]));
    }
    Ok(ChunkOutcome::Many(vec![ParseEntry::Str(out)]))
}

/// 函数 env 的 TOKEN 往返（parse.js module.exports 的 reduce 后处理）：
/// 字符串 token 按 TOKEN…TOKEN 段切分，防 TOKEN 段 JSON.parse 回对象。
fn post_process_fn_env(mapped: Vec<ParseEntry>, token: &str) -> Result<Vec<ParseEntry>, String> {
    let mut acc: Vec<ParseEntry> = Vec::new();
    for entry in mapped {
        let s = match entry {
            ParseEntry::Str(s) => s,
            other => {
                acc.push(other);
                continue;
            }
        };
        if !s.contains(token) {
            acc.push(ParseEntry::Str(s));
            continue;
        }
        // s.split(RegExp('(' + TOKEN + '.*?' + TOKEN + ')', 'g'))：
        // JS `.` 排除换行；此处按首次出现切分，差异见模块注释。
        let mut rest: &str = &s;
        loop {
            let start = match rest.find(token) {
                Some(p) => p,
                None => {
                    if !rest.is_empty() {
                        acc.push(ParseEntry::Str(rest.to_string()));
                    }
                    break;
                }
            };
            let after_first = &rest[start + token.len()..];
            let rel = match after_first.find(token) {
                Some(r) => r,
                None => {
                    // 落单 TOKEN：剩余按原样保留（JS filter(Boolean) 后等价）。
                    if !rest.is_empty() {
                        acc.push(ParseEntry::Str(rest.to_string()));
                    }
                    break;
                }
            };
            if start > 0 {
                acc.push(ParseEntry::Str(rest[..start].to_string()));
            }
            let json = &after_first[..rel];
            let parsed = serde_json::from_str::<Value>(json)
                .map_err(|e| format!("Invalid JSON in env expansion: {e}"))?;
            push_json_entry(&mut acc, parsed)?;
            rest = &after_first[rel + token.len()..];
        }
    }
    Ok(acc)
}

/// 将 JSON 往返的对象映射为 ParseEntry：op / glob / comment 三种已知形状。
fn push_json_entry(acc: &mut Vec<ParseEntry>, value: Value) -> Result<(), String> {
    let map = match value {
        Value::Object(map) => map,
        _ => return Err("Invalid JSON in env expansion: expected object".into()),
    };
    if let Some(op) = map.get("op").and_then(|v| v.as_str()) {
        if op == "glob" {
            if let Some(pattern) = map.get("pattern").and_then(|v| v.as_str()) {
                acc.push(ParseEntry::Glob {
                    pattern: pattern.to_string(),
                });
            }
            // glob 缺 pattern → 视为未知形状：丢弃。
        } else {
            acc.push(ParseEntry::Control {
                op: op.to_string(),
            });
        }
    } else if let Some(comment) = map.get("comment").and_then(|v| v.as_str()) {
        acc.push(ParseEntry::Comment {
            comment: comment.to_string(),
        });
    }
    // 其余未知形状：丢弃（模块注释有说明）。
    Ok(())
}

/// shell-quote `parse(s, env, opts)`。opts 未实现（见模块注释）。
fn parse_shell(string: &str, env: Option<&ShellEnv<'_>>) -> Result<Vec<ParseEntry>, String> {
    let matches = find_matches(string);
    if matches.is_empty() {
        return Ok(vec![]);
    }
    let lookup: Box<dyn Fn(&str) -> Option<EnvValue> + '_> = match env {
        Some(ShellEnv::Map(map)) => Box::new(move |key: &str| {
            map.get(key).cloned().map(EnvValue::Str)
        }),
        Some(ShellEnv::Function(f)) => Box::new(move |key: &str| f(key)),
        None => Box::new(|_: &str| None),
    };
    let token = generate_token();
    let mut commented = false;
    let mut mapped: Vec<ParseEntry> = Vec::new();
    for (start, chunk) in &matches {
        let outcome = process_chunk(chunk, *start, string, &lookup, &token, &mut commented)?;
        if let ChunkOutcome::Many(entries) = outcome {
            mapped.extend(entries);
        }
    }
    if env.map_or(false, |e| matches!(e, ShellEnv::Function(_))) {
        post_process_fn_env(mapped, &token)
    } else {
        Ok(mapped)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// TS 包装（shellQuote.ts）
// ────────────────────────────────────────────────────────────────────────────

/// tryParseShellCommand：安全包装，失败时 logError 并返回失败结果。
pub fn try_parse_shell_command(cmd: &str, env: Option<ShellEnv<'_>>) -> ShellParseResult {
    match parse_shell(cmd, env.as_ref()) {
        Ok(tokens) => ShellParseResult::Success { tokens },
        Err(error) => {
            // TS：if (error instanceof Error) logError(error)。
            log_error(std::io::Error::new(std::io::ErrorKind::Other, error.clone()));
            ShellParseResult::Failed { error }
        }
    }
}

/// tryQuoteShellArgs：严格校验 + 引号。
/// TS 拒绝 object/symbol/function 参数；Rust 入参为 String，校验恒通过。
pub fn try_quote_shell_args(args: &[String]) -> ShellQuoteResult {
    let tokens: Vec<ParseEntry> = args.iter().cloned().map(ParseEntry::Str).collect();
    match quote_tokens(&tokens) {
        Ok(quoted) => ShellQuoteResult::Success { quoted },
        Err(error) => {
            log_error(std::io::Error::new(std::io::ErrorKind::Other, error.clone()));
            ShellQuoteResult::Failed { error }
        }
    }
}

/// TS `quote(args)`：先严格引号，失败走 lenient 兜底（jsonStringify 再引号）；
/// 再失败抛 'Failed to quote shell arguments safely'。
/// Rust 无 panic：二次失败时退化为逐参数单引号（对 String 入参不可达）。
pub fn quote(args: &[String]) -> String {
    if let ShellQuoteResult::Success { quoted } = try_quote_shell_args(args) {
        return quoted;
    }
    // lenient fallback：jsonStringify(arg) 后再引号。
    let json_args: Vec<String> = args
        .iter()
        .map(|a| json_stringify(&Value::String(a.clone())))
        .collect();
    let tokens: Vec<ParseEntry> = json_args.iter().cloned().map(ParseEntry::Str).collect();
    if let Ok(quoted) = quote_tokens(&tokens) {
        return quoted;
    }
    // SECURITY：绝不返回未加引号的内容；最终兜底为逐参数单引号。
    args.iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

// ────────────────────────────────────────────────────────────────────────────
// 畸形 token 检测（shellQuote.ts）
// ────────────────────────────────────────────────────────────────────────────

/// 统计 token 中未被反斜杠转义的引号数量（对齐 TS `(?<!\\)"/g`）。
fn count_unescaped(s: &str, quote: char) -> usize {
    let mut count = 0usize;
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c == quote && prev != Some('\\') {
            count += 1;
        }
        prev = Some(c);
    }
    count
}

/// hasMalformedTokens：检测 shell-quote 误解析产生的畸形 token。
///
/// 两类检查：
/// 1. 原始命令中存在未闭合引号（shell-quote 会静默丢弃不匹配的引号，
///    token 层面无痕迹，须回查原始串，按 bash 引号语义扫描奇偶校验）；
/// 2. token 本身括号/引号不平衡（JSON 类歧义输入被 shell 规则拆碎，
///    如 `echo {"hi":"hi;evil"}`）。
pub fn has_malformed_tokens(command: &str, parsed: &[ParseEntry]) -> bool {
    // 检查 1：按 bash 语义扫描原始命令——单引号内反斜杠无转义作用。
    let mut in_single = false;
    let mut in_double = false;
    let mut double_count = 0u32;
    let mut single_count = 0u32;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && !in_single {
            i += 2;
            continue;
        }
        if c == '"' && !in_single {
            double_count += 1;
            in_double = !in_double;
        } else if c == '\'' && !in_double {
            single_count += 1;
            in_single = !in_single;
        }
        i += 1;
    }
    if double_count % 2 != 0 || single_count % 2 != 0 {
        return true;
    }

    // 检查 2：字符串 token 的括号/引号平衡性。
    for entry in parsed {
        let ParseEntry::Str(s) = entry else {
            continue;
        };
        if s.matches('{').count() != s.matches('}').count() {
            return true;
        }
        if s.matches('(').count() != s.matches(')').count() {
            return true;
        }
        if s.matches('[').count() != s.matches(']').count() {
            return true;
        }
        if count_unescaped(s, '"') % 2 != 0 {
            return true;
        }
        if count_unescaped(s, '\'') % 2 != 0 {
            return true;
        }
    }
    false
}

/// hasShellQuoteSingleQuoteBug：检测利用 shell-quote 单引号内反斜杠
/// 处理错误的 `\` 模式。
///
/// bash 中单引号保留所有字符字面值（反斜杠无特殊含义），而 shell-quote
/// 的 chunker 正则把 `\'` 当作转义，导致 `'\' <payload> '\''` 把 payload
/// 藏进一个 shell-quote 误判的单一引号串。
pub fn has_shell_quote_single_quote_bug(command: &str) -> bool {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut idx = 0usize;
    while idx < chars.len() {
        let (byte_pos, c) = chars[idx];
        if c == '\\' && !in_single_quote {
            // 单引号外反斜杠转义下一字符。
            idx += 2;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            idx += 1;
            continue;
        }
        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            if !in_single_quote {
                // 刚闭合一个单引号；统计其前方连续反斜杠数量。
                let mut backslash_count = 0usize;
                let mut j = idx as isize - 1;
                while j >= 0 && chars[j as usize].1 == '\\' {
                    backslash_count += 1;
                    j -= 1;
                }
                // 奇数个尾随反斜杠：shell-quote 必然误判。
                if backslash_count > 0 && backslash_count % 2 == 1 {
                    return true;
                }
                // 偶数个尾随反斜杠：仅当命令中还存在后续单引号时，
                // chunker 才会拿它当假闭合引号、合并 token。
                if backslash_count > 0
                    && backslash_count % 2 == 0
                    && command[byte_pos + 1..].contains('\'')
                {
                    return true;
                }
            }
        }
        idx += 1;
    }
    false
}
