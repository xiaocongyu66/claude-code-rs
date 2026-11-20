//! git_config_parser.rs — `.git/config` 文件的轻量解析器。
//!
//! 重写自 `src/utils/git/gitConfigParser.ts`。已对照 git 的 config.c：
//!   - section 名大小写不敏感，字符集为字母数字 + 连字符
//!   - subsection（带引号）大小写敏感，支持 `\\` 与 `\"` 转义
//!   - key 名大小写不敏感，字母数字 + 连字符
//!   - 值可选引号包裹，支持行内注释（`#` 或 `;`）与反斜杠转义
//!
//! 唯一外部行为差异：TS 用 `fs/promises` 异步读取且以 UTF-8 替换非法字节，
//! 本实现用 `std::fs` 同步读取并做 `from_utf8_lossy` 替换（Cargo.toml
//! 未引入异步运行时）；读取错误一律转为 `None`，与 TS 的 try/catch →
//! return null 语义一致。

use std::path::Path;

/// Parse a single value from .git/config.
/// Finds the first matching key under the given section/subsection.
pub fn parse_git_config_value(
    git_dir: &Path,
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Option<String> {
    let bytes = std::fs::read(Path::new(git_dir).join("config")).ok()?;
    let config = String::from_utf8_lossy(&bytes);
    parse_config_string(&config, section, subsection, key)
}

/// Parse a config value from an in-memory config string.
pub fn parse_config_string(
    config: &str,
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Option<String> {
    let section_lower = section.to_ascii_lowercase();
    let key_lower = key.to_ascii_lowercase();

    let mut in_section = false;
    for line in config.split('\n') {
        let trimmed = line.trim();

        // Skip empty lines and comment-only lines
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.chars().next();
        if first == Some('#') || first == Some(';') {
            continue;
        }

        // Section header
        if first == Some('[') {
            in_section = matches_section_header(trimmed, &section_lower, subsection);
            continue;
        }

        if !in_section {
            continue;
        }

        // Key-value line: find the key name
        if let Some((parsed_key, parsed_value)) = parse_key_value(trimmed) {
            if parsed_key.to_ascii_lowercase() == key_lower {
                return Some(parsed_value);
            }
        }
    }

    None
}

/// Parse a key = value line. Returns None if the line doesn't contain a valid key.
fn parse_key_value(line: &str) -> Option<(String, String)> {
    // Read key: alphanumeric + hyphen, starting with alpha
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() && is_key_char(chars[i]) {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let key: String = chars[..i].iter().collect();

    // Skip whitespace
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }

    // Must have '='
    if i >= chars.len() || chars[i] != '=' {
        // Boolean key with no value — not relevant for our use cases
        return None;
    }
    i += 1; // skip '='

    // Skip whitespace after '='
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }

    let value = parse_value(&chars, i);
    Some((key, value))
}

/// Parse a config value starting at position i.
/// Handles quoted strings, escape sequences, and inline comments.
fn parse_value(chars: &[char], start: usize) -> String {
    let mut result = String::new();
    let mut in_quote = false;
    let mut i = start;

    while i < chars.len() {
        let ch = chars[i];

        // Inline comments outside quotes end the value
        if !in_quote && (ch == '#' || ch == ';') {
            break;
        }

        if ch == '"' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }

        if ch == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if in_quote {
                // Inside quotes: recognize escape sequences
                match next {
                    'n' => result.push('\n'),
                    't' => result.push('\t'),
                    'b' => result.push('\u{8}'),
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    // Git silently drops the backslash for unknown escapes
                    other => result.push(other),
                }
                i += 2;
                continue;
            }
            // Outside quotes: backslash at end of line = continuation (we
            // don't handle multi-line since we split on \n, but handle \\)
            if next == '\\' {
                result.push('\\');
                i += 2;
                continue;
            }
            // Fallthrough — treat backslash literally outside quotes
        }

        result.push(ch);
        i += 1;
    }

    // Trim trailing whitespace from unquoted portions.
    // Git trims trailing whitespace that isn't inside quotes, but since we
    // process char-by-char and quotes toggle, the simplest correct approach
    // for single-line values is to trim the result when not ending in a quote.
    if !in_quote {
        trim_trailing_whitespace(&mut result);
    }

    result
}

fn trim_trailing_whitespace(s: &mut String) {
    while s.ends_with([' ', '\t']) {
        s.pop();
    }
}

/// Check if a config line like `[remote "origin"]` matches the given section/subsection.
/// Section matching is case-insensitive; subsection matching is case-sensitive.
fn matches_section_header(line: &str, section_lower: &str, subsection: Option<&str>) -> bool {
    let chars: Vec<char> = line.chars().collect();
    // line starts with '['
    let mut i = 1;

    // Read section name
    while i < chars.len()
        && chars[i] != ']'
        && chars[i] != ' '
        && chars[i] != '\t'
        && chars[i] != '"'
    {
        i += 1;
    }
    let found_section: String = chars[1..i].iter().collect();
    let found_section = found_section.to_ascii_lowercase();

    if found_section != section_lower {
        return false;
    }

    let Some(subsection) = subsection else {
        // Simple section: must end with ']'
        return i < chars.len() && chars[i] == ']';
    };

    // Skip whitespace before subsection quote
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }

    // Must have opening quote
    if i >= chars.len() || chars[i] != '"' {
        return false;
    }
    i += 1; // skip opening quote

    // Read subsection — case-sensitive, handle \\ and \" escapes
    let mut found_subsection = String::new();
    while i < chars.len() && chars[i] != '"' {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            // Git drops the backslash for any escape in subsections
            found_subsection.push(next);
            i += 2;
            continue;
        }
        found_subsection.push(chars[i]);
        i += 1;
    }

    // Must have closing quote followed by ']'
    if i >= chars.len() || chars[i] != '"' {
        return false;
    }
    i += 1; // skip closing quote

    if i >= chars.len() || chars[i] != ']' {
        return false;
    }

    found_subsection == subsection
}

fn is_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-'
}
