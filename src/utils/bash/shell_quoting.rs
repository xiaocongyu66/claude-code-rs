//! shell_quoting.rs — shell 命令引号封装。
//!
//! 重写自 `src/utils/bash/shellQuoting.ts`。语义对齐：
//!
//! - heredoc / 多行字符串检测与单引号转义路径（shell-quote 的 `!` 转义在
//!   这些场景下会破坏命令）按 TS 注释逐条实现。
//! - TS 正则中的反引用（`\1`）与断言（`(?![<(])` / `(?=...)`）在 Rust
//!   regex crate 中无直接等价，改用手工扫描，匹配语义逐条核对。
//! - `quote` 引号逻辑来自 `shell_quote.rs`（shell-quote 1.12.0 手写实现）。

use crate::utils::bash::shell_quote::quote;

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

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 检测命令是否包含 heredoc。匹配 `<<EOF`、`<<'EOF'`、`<<"EOF"`、`<<-EOF`、
/// `<<-'EOF'`、`<<\EOF` 等形式；先排除移位算符（`\d << \d`、`[[ n << n ]]`、
/// `$(( ... << ... ))`）。
fn contains_heredoc(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    if len < 2 {
        return false;
    }
    // 排除 1：/\d\s*<<\s*\d/
    let mut i = 0usize;
    while i + 1 < len {
        if chars[i].is_ascii_digit() {
            let mut j = i + 1;
            while j < len && is_js_whitespace(chars[j]) {
                j += 1;
            }
            if j + 1 < len && chars[j] == '<' && chars[j + 1] == '<' {
                let mut k = j + 2;
                while k < len && is_js_whitespace(chars[k]) {
                    k += 1;
                }
                if k < len && chars[k].is_ascii_digit() {
                    return false;
                }
            }
        }
        i += 1;
    }
    // 排除 2：/\[\[\s*\d+\s*<<\s*\d+\s*\]\]/
    if let Some(start) = find_substring(&chars, "[[") {
        let mut j = start + 2;
        while j < len && is_js_whitespace(chars[j]) {
            j += 1;
        }
        let d1_start = j;
        while j < len && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j > d1_start {
            while j < len && is_js_whitespace(chars[j]) {
                j += 1;
            }
            if j + 1 < len && chars[j] == '<' && chars[j + 1] == '<' {
                let mut k = j + 2;
                while k < len && is_js_whitespace(chars[k]) {
                    k += 1;
                }
                let d2_start = k;
                while k < len && chars[k].is_ascii_digit() {
                    k += 1;
                }
                if k > d2_start {
                    let mut m = k;
                    while m < len && is_js_whitespace(chars[m]) {
                        m += 1;
                    }
                    if m + 1 < len && chars[m] == ']' && chars[m + 1] == ']' {
                        return false;
                    }
                }
            }
        }
    }
    // 排除 3：/\$\(\(.*<<.*\)\)/（同一行内）
    for i in 0..len {
        if chars[i] == '$' && i + 2 < len && chars[i + 1] == '(' && chars[i + 2] == '(' {
            // 在同一行内找 `<<` 再找 `))`
            let mut j = i + 3;
            let mut shift = None;
            while j < len && chars[j] != '\n' {
                if j + 1 < len && chars[j] == '<' && chars[j + 1] == '<' {
                    shift = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(s) = shift {
                let mut k = s + 2;
                while k + 1 < len && chars[k] != '\n' {
                    if chars[k] == ')' && chars[k + 1] == ')' {
                        return false;
                    }
                    k += 1;
                }
            }
        }
    }
    // heredoc 模式：/<<-?\s*(?:(['"]?)(\w+)\1|\\(\w+))/
    let mut i = 0usize;
    while i + 1 < len {
        if chars[i] == '<' && chars[i + 1] == '<' {
            let mut q = i + 2;
            if q < len && chars[q] == '-' {
                q += 1;
            }
            while q < len && is_js_whitespace(chars[q]) {
                q += 1;
            }
            // 定界符三种形式：引号包裹 / 反斜杠前缀 / 裸词。
            if q < len && chars[q] == '\\' {
                let w = q + 1;
                if w < len && is_word_char(chars[w]) {
                    return true;
                }
            } else if q < len && (chars[q] == '\'' || chars[q] == '"') {
                let qch = chars[q];
                let mut w = q + 1;
                while w < len && is_word_char(chars[w]) {
                    w += 1;
                }
                if w > q + 1 && w < len && chars[w] == qch {
                    return true;
                }
            } else {
                let mut w = q;
                while w < len && is_word_char(chars[w]) {
                    w += 1;
                }
                if w > q {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// 从 `chars` 中查找子串，返回首个起始字符索引。
fn find_substring(chars: &[char], needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    let n = needle.len();
    if n == 0 || chars.len() < n {
        return None;
    }
    (0..=chars.len() - n).find(|&i| chars[i..i + n] == needle[..])
}

/// 检测命令是否包含多行引号字符串（单/双引号内含真实换行）。
/// `'(?:[^'\\]|\\.)*\n(?:[^'\\]|\\.)*'` 及其双引号版本。
fn contains_multiline_string(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    // 分别扫描单引号与双引号两种形式。
    for &qch in &['\'', '"'] {
        for i in 0..len {
            if chars[i] != qch {
                continue;
            }
            let mut j = i + 1;
            let mut saw_newline = false;
            while j < len {
                let c = chars[j];
                if c == '\\' && j + 1 < len {
                    j += 2;
                } else if c == qch {
                    if saw_newline {
                        return true;
                    }
                    break; // 闭引号在换行之前：此起始不匹配
                } else if c == '\n' {
                    saw_newline = true;
                    j += 1;
                } else {
                    j += 1;
                }
            }
        }
    }
    false
}

/// 按规则为 shell 命令加引号；保留 heredoc 与多行字符串原样（shell-quote
/// 在这些场景会错误转义 `!`）。
pub fn quote_shell_command(command: &str, add_stdin_redirect: bool) -> String {
    if contains_heredoc(command) || contains_multiline_string(command) {
        let escaped = command.replace('\'', "'\"'\"'");
        let quoted = format!("'{escaped}'");
        // heredoc 自带 stdin，不加重定向。
        if contains_heredoc(command) {
            return quoted;
        }
        // 多行字符串（无 heredoc）按需加重定向。
        return if add_stdin_redirect {
            format!("{quoted} < /dev/null")
        } else {
            quoted
        };
    }
    if add_stdin_redirect {
        return quote(&[command.to_string(), "<".into(), "/dev/null".into()]);
    }
    quote(&[command.to_string()])
}

/// 检测命令是否已有 stdin 重定向（`< file`、`</path`、`< /dev/null` 等），
/// 但不算 `<<EOF`（heredoc）、`<<`（移位）、`<(`（进程替换）。
pub fn has_stdin_redirect(command: &str) -> bool {
    // /(?:^|[\s;&|])<(?![<(])\s*\S+/
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] != '<' {
            continue;
        }
        // 前置：行首或空白 / ; & |
        let prev_ok = i == 0 || {
            let p = chars[i - 1];
            is_js_whitespace(p) || matches!(p, ';' | '&' | '|')
        };
        if !prev_ok {
            continue;
        }
        // 后随不能是 < 或 (
        if i + 1 < len && (chars[i + 1] == '<' || chars[i + 1] == '(') {
            continue;
        }
        // \s*\S+
        let mut j = i + 1;
        while j < len && is_js_whitespace(chars[j]) {
            j += 1;
        }
        if j < len && !is_js_whitespace(chars[j]) {
            return true;
        }
    }
    false
}

/// 判断是否可以安全地给命令追加 stdin 重定向。
pub fn should_add_stdin_redirect(command: &str) -> bool {
    // heredoc 会与 stdin 冲突。
    if contains_heredoc(command) {
        return false;
    }
    if has_stdin_redirect(command) {
        return false;
    }
    true
}

/// 将 Windows CMD 风格的 `>nul` 重定向改写为 POSIX `/dev/null`。
///
/// 匹配 `>nul`、`> NUL`、`2>nul`、`&>nul`、`>>nul`（大小写不敏感）；
/// 不匹配 `>null`、`>nullable`、`>nul.txt`、`cat nul.txt`。
/// 限制：不做 shell 引号感知，`echo ">nul"` 也会被改写（TS 原注释认定为
/// 可接受的附带影响）。
pub fn rewrite_windows_null_redirect(command: &str) -> String {
    // /(\d?&?>+\s*)[Nn][Uu][Ll](?=\s|$|[|&;)\n])/g → `$1/dev/null`
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(command.len() + 16);
    let mut i = 0usize;
    while i < len {
        if let Some(prefix_end) = scan_nul_redirect(&chars, i) {
            // 前缀 `\d?&?>+\s*` 原样保留，NUL 替换为 /dev/null。
            for c in &chars[i..prefix_end] {
                out.push(*c);
            }
            out.push_str("/dev/null");
            i = prefix_end + 3; // 跳过 NUL 三个字符
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 在位置 `i` 尝试匹配 `(\d?&?>+\s*)[Nn][Uu][Ll](?=\s|$|[|&;)\n])`，
/// 成功返回前缀结束位置（指向 NUL 的首字符）。
fn scan_nul_redirect(chars: &[char], i: usize) -> Option<usize> {
    let len = chars.len();
    let mut p = i;
    // `\d?`：出现数字即必须消费（正则回溯证明其他组合无意义）。
    if p < len && chars[p].is_ascii_digit() {
        p += 1;
    }
    if p < len && chars[p] == '&' {
        p += 1;
    }
    // `>+`：贪心最长（同理唯一可行）。
    let mut q = p;
    while q < len && chars[q] == '>' {
        q += 1;
    }
    if q == p {
        return None;
    }
    // `\s*`：最长空白串。
    let mut w = q;
    while w < len && is_js_whitespace(chars[w]) {
        w += 1;
    }
    // [Nn][Uu][Ll]
    if w + 3 > len {
        return None;
    }
    let nul: [char; 3] = [chars[w], chars[w + 1], chars[w + 2]];
    if !matches!(nul[0], 'N' | 'n') || !matches!(nul[1], 'U' | 'u') || !matches!(nul[2], 'L' | 'l')
    {
        return None;
    }
    // 后随：\s | 串尾 | | & ; ) \n 之一。
    let after = w + 3;
    let ok = after >= len
        || is_js_whitespace(chars[after])
        || matches!(chars[after], '|' | '&' | ';' | ')');
    if ok {
        Some(w)
    } else {
        None
    }
}
