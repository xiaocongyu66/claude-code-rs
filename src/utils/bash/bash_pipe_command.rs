//! bash_pipe_command.rs — 管道命令重排。
//!
//! 重写自 `src/utils/bash/bashPipeCommand.ts`。把 `cmd1 | cmd2` 的 stdin
//! 重定向挪到第一条命令之后，修复 eval 把整条管道视作单一单元导致
//! `< /dev/null` 落在 eval 自身的问题。语义对齐：
//!
//! - 所有回退路径（反引号 / `$(` / `$VAR` / 控制结构 / 换行 / shell-quote
//!   单引号 bug / 解析失败 / 畸形 token / 无管道）都走
//!   `quoteWithEvalStdinRedirect`（单引号包整条命令 + ` < /dev/null`）。
//! - rebuild 逻辑（fd 重符合并、env 赋值引号、glob 保留）逐条对齐 TS。

use crate::utils::bash::shell_quote::{
    has_malformed_tokens, has_shell_quote_single_quote_bug, quote, try_parse_shell_command,
    ParseEntry, ShellParseResult,
};

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

/// 重排管道命令，把 stdin 重定向放到第一条命令之后。
pub fn rearrange_pipe_command(command: &str) -> String {
    // 反引号：shell-quote 处理不好。
    if command.contains('`') {
        return quote_with_eval_stdin_redirect(command);
    }
    // `$(`：shell-quote 把 ( ) 拆成独立算符，无法识别命令替换。
    if command.contains("$(") {
        return quote_with_eval_stdin_redirect(command);
    }
    // `$VAR` / `${VAR`：shell-quote 无 env 时静默丢弃 `$` 引用，
    // 即便保留也会在 rebuild 时被转义阻止运行时展开（#9732）。
    if contains_dollar_var(command) {
        return quote_with_eval_stdin_redirect(command);
    }
    // for/while/until/if/case/select：shell-quote 无法正确解析控制结构，
    // 会把结构体内的管道误判为顶层管道。
    if contains_control_structure(command) {
        return quote_with_eval_stdin_redirect(command);
    }
    // 先合并行连接（\<newline>），shell-quote 不处理它，会产生空串 token。
    let joined = join_continuation_lines(command);
    // 裸换行是命令分隔符；shell-quote 只当空白 → 回退。
    if joined.contains('\n') {
        return quote_with_eval_stdin_redirect(command);
    }
    // shell-quote 单引号 bug 检测。
    if has_shell_quote_single_quote_bug(&joined) {
        return quote_with_eval_stdin_redirect(command);
    }
    let parse_result = try_parse_shell_command(&joined, None);
    let parsed = match parse_result {
        ShellParseResult::Success { tokens } => tokens,
        ShellParseResult::Failed { .. } => {
            return quote_with_eval_stdin_redirect(command);
        }
    };
    if has_malformed_tokens(&joined, &parsed) {
        return quote_with_eval_stdin_redirect(command);
    }
    let first_pipe_index = find_first_pipe_operator(&parsed);
    // TS 的 `firstPipeIndex <= 0`：无管道（-1）或管道在首位（0）都回退。
    let first_pipe_index = match first_pipe_index {
        Some(i) if i > 0 => i,
        _ => return quote_with_eval_stdin_redirect(command),
    };
    // 重建：first_command < /dev/null | rest_of_pipeline
    let mut parts = build_command_parts(&parsed, 0, first_pipe_index);
    parts.push("< /dev/null".to_string());
    parts.extend(build_command_parts(&parsed, first_pipe_index, parsed.len()));
    single_quote_for_eval(&parts.join(" "))
}

/// 查找解析结果中第一个管道算符的位置。
fn find_first_pipe_operator(parsed: &[ParseEntry]) -> Option<usize> {
    for (i, entry) in parsed.iter().enumerate() {
        if is_operator(entry, Some("|")) {
            return Some(i);
        }
    }
    None
}

/// 从解析条目构建命令片段；处理字符串与算符，文件描述符重定向
/// （2>&1 / 2>/dev/null / 2> &1）合并为单个片段。
fn build_command_parts(parsed: &[ParseEntry], start: usize, end: usize) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    // 是否已见过非 env 赋值的字符串 token（env 赋值仅命令起始合法）。
    let mut seen_non_env_var = false;
    let mut i = start;
    while i < end {
        let entry = &parsed[i];

        // 文件描述符重定向（2>&1、2>/dev/null、2> &1）。
        if let ParseEntry::Str(s) = entry {
            if is_fd(s) && i + 2 < end {
                if let ParseEntry::Control { op } = &parsed[i + 1] {
                    let target = &parsed[i + 2];
                    // 2>&1
                    if op == ">&" {
                        if let ParseEntry::Str(t) = target {
                            if is_fd(t) {
                                parts.push(format!("{s}>&{t}"));
                                i += 3;
                                continue;
                            }
                        }
                    }
                    if op == ">" {
                        // 2>/dev/null
                        if let ParseEntry::Str(t) = target {
                            if t == "/dev/null" {
                                parts.push(format!("{s}>/dev/null"));
                                i += 3;
                                continue;
                            }
                            // 2> &1（> 与 &1 之间有空格）
                            if let Some(fd) = t.strip_prefix('&') {
                                if is_fd(fd) {
                                    parts.push(format!("{s}>&{fd}"));
                                    i += 3;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }

        match entry {
            ParseEntry::Str(s) => {
                // 命令起始的 env 赋值（VAR=value）单独引号 value。
                let is_env_var = !seen_non_env_var && is_environment_variable_assignment(s);
                if is_env_var {
                    if let Some(eq) = s.find('=') {
                        let name = &s[..eq];
                        let value = &s[eq + 1..];
                        parts.push(format!("{}={}", name, quote(&[value.to_string()])));
                    } else {
                        // is_environment_variable_assignment 保证含 '='。
                        parts.push(quote(&[s.clone()]));
                    }
                } else {
                    seen_non_env_var = true;
                    parts.push(quote(&[s.clone()]));
                }
            }
            ParseEntry::Control { op } => {
                parts.push(op.clone());
                // 命令分隔符后重置：下一条命令可再带 env 赋值。
                if is_command_separator(op) {
                    seen_non_env_var = false;
                }
            }
            ParseEntry::Glob { pattern } => {
                // glob 不加引号——需要保留给 shell 展开。
                parts.push(pattern.clone());
            }
            ParseEntry::Comment { .. } => {
                // TS 的 else-if 链不匹配 string 与 operator，直接跳过。
            }
        }
        i += 1;
    }
    parts
}

/// 是否 env 赋值（VAR=value）：名称 [A-Za-z_][A-Za-z0-9_]* 后随 `=`。
fn is_environment_variable_assignment(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() || !(is_word_char_start(chars[0])) {
        return false;
    }
    for (i, c) in chars.iter().enumerate() {
        if *c == '=' {
            return i > 0;
        }
        if !is_word_char(*c) {
            return false;
        }
    }
    false
}

fn is_word_char_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// 是否命令分隔符（分隔后 env 赋值重新合法）。
fn is_command_separator(op: &str) -> bool {
    matches!(op, "&&" | "||" | ";")
}

/// 判断条目是否（指定值的）算符。
fn is_operator(entry: &ParseEntry, op: Option<&str>) -> bool {
    match entry {
        ParseEntry::Control { op: entry_op } => match op {
            Some(o) => entry_op == o,
            None => true,
        },
        _ => false,
    }
}

/// 检测命令是否含 bash 控制结构（for/while/until/if/case/select）。
/// 匹配词边界后的关键字 + 空白，避免误报含这些词的参数。
fn contains_control_structure(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    let words = ["for", "while", "until", "if", "case", "select"];
    let mut i = 0usize;
    while i < len {
        // \b：前一字符非 \w 或行首。
        if i > 0 && is_word_char(chars[i - 1]) {
            i += 1;
            continue;
        }
        for w in words {
            let wl = w.len();
            if i + wl > len {
                continue;
            }
            let candidate: Vec<char> = chars[i..i + wl].to_vec();
            let wchars: Vec<char> = w.chars().collect();
            if candidate != wchars {
                continue;
            }
            // \s：后随 JS 空白。
            if i + wl < len && is_js_whitespace(chars[i + wl]) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 检测 `$` 引用 shell 变量（$VAR、${VAR}）。
fn contains_dollar_var(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '$' {
            if let Some(c) = chars.get(i + 1) {
                if c.is_ascii_alphabetic() || *c == '_' || *c == '{' {
                    return true;
                }
            }
        }
    }
    false
}

/// `quote(cmd) + ' < /dev/null'`：eval 内管道正常工作。
fn quote_with_eval_stdin_redirect(command: &str) -> String {
    format!("{} < /dev/null", single_quote_for_eval(command))
}

/// 单引号包裹用于 eval 的参数，内嵌单引号经 `'"'"'` 转义。
/// 不用 shell-quote 的 quote()：它在含单引号时切到双引号模式并
/// 把 `!` 转义成 `\!`，破坏 jq/awk 的 `select(.x != .y)` 过滤器。
fn single_quote_for_eval(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// 合并行连接（反斜杠 + 换行）：仅当反斜杠数量为奇数时
/// 最后一个反斜杠转义换行；偶数时全部成对、换行保留。
fn join_continuation_lines(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(command.len());
    let mut i = 0usize;
    while i < len {
        if chars[i] == '\\' {
            let start = i;
            let mut j = i;
            while j < len && chars[j] == '\\' {
                j += 1;
            }
            if j < len && chars[j] == '\n' {
                let count = j - start;
                if count % 2 == 1 {
                    // 奇数：保留 count-1 个反斜杠，吞掉换行。
                    for _ in 0..count - 1 {
                        out.push('\\');
                    }
                } else {
                    // 偶数：原样保留（含换行）。
                    for c in &chars[start..=j] {
                        out.push(*c);
                    }
                }
                i = j + 1;
                continue;
            }
            out.push('\\');
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_fd(s: &str) -> bool {
    matches!(s, "0" | "1" | "2")
}
