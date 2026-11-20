//! frontmatter_parser.rs — Markdown frontmatter（YAML）解析。
//!
//! 重写自 `src/utils/frontmatterParser.ts`。语义对齐：
//!
//! - 提取 `---` 分隔符之间的 YAML frontmatter 并解析。
//! - 含特殊 YAML 字符的裸值会被自动加引号后重试解析（glob 模式
//!   `**\/*.{ts,tsx}` 之类的场景）。
//! - `split_path_in_frontmatter`：逗号分隔 + 花括号展开（尊重花括号内
//!   的逗号），同时接受 YAML 字符串数组。
//! - `parse_positive_int_from_frontmatter` / `coerce_description_to_string` /
//!   `parse_boolean_frontmatter` / `parse_shell_frontmatter`：
//!   frontmatter 值的校验与强转助手。
//!
//! 偏差：
//! - TS 的 FrontmatterData 保留索引签名 `[key: string]: unknown`；
//!   Rust 侧用 serde_json::Value 对象承载全部字段，并提供类型化访问器。
//! - `HooksSettings`（settings/types）未转 Rust，hooks 字段以
//!   serde_json::Value 保留。
//! - FRONTMATTER_REGEX 的 `^---\s*\n([\s\S]*?)---\s*\n?` 用 regex crate
//!   手写锚定实现（固定锚定字符串开头，与 JS 的 ^ + m 语义等价）。

use regex::Regex;

use once_cell::sync::Lazy;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::yaml::parse_yaml;

/// 解析结果。
#[derive(Debug, Clone)]
pub struct ParsedMarkdown {
    /// frontmatter 键值（JSON 对象；字段以 serde_json::Value 保留，
    /// 对齐 TS 的宽松类型 + 索引签名）。
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// 去掉 frontmatter 后的正文。
    pub content: String,
}

/// frontmatter 的 region 匹配：`^---\s*\n([\s\S]*?)---\s*\n?`
static FRONTMATTER_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^---[ \t]*\r?\n").expect("valid frontmatter start regex")
});

// YAML 特殊字符（未加引号时）：
// - { } 是 flow mapping 指示符
// - * 是 anchor/alias 指示符
// - [ ] 是 flow sequence 指示符
// - ': '（冒号+空格）是 key 指示符 —— 匹配模式而非裸 ':'，这样
//   '12:34' 时间与 'https://' URL 保持不加引号
// - # 是注释指示符；& 是 anchor 指示符；! 是 tag 指示符
// - | > 是 block scalar 指示符（仅在开头）；% 是 directive；@ ` 保留
fn value_has_special_chars(value: &str) -> bool {
    value == "|"
        || value == ">"
        || value.starts_with('%')
        || value.starts_with('@')
        || value.starts_with('`')
        || value.contains('{')
        || value.contains('}')
        || value.contains('[')
        || value.contains(']')
        || value.contains('*')
        || value.contains('&')
        || value.contains('#')
        || value.contains("!|")
        || value.contains(": ")
}

/// 预处理 frontmatter 文本：给含特殊 YAML 字符的值加引号，
/// 让 `**\/*.{ts,tsx}` 之类的 glob 模式能被正确解析。
fn quote_problematic_values(frontmatter_text: &str) -> String {
    // 匹配简单 `key: value` 行（无缩进、非列表项、非块标量）
    let line_re = Regex::new(r"^([a-zA-Z_-]+):[ \t]+(.+)$").expect("valid line regex");

    frontmatter_text
        .split('\n')
        .map(|line| {
            let Some(caps) = line_re.captures(line) else {
                return line.to_string();
            };
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            if key.is_empty() || value.is_empty() {
                return line.to_string();
            }
            // 已加引号则跳过
            if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                return line.to_string();
            }
            if value_has_special_chars(value) {
                // 双引号并转义已有双引号与反斜杠
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                return format!("{key}: \"{escaped}\"");
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn yaml_to_frontmatter_object(
    yaml_value: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    match yaml_value {
        serde_json::Value::Object(map) => Some(map.clone()),
        _ => None,
    }
}

/// 解析 Markdown 内容，提取 frontmatter 与正文。
///
/// `source_path` 仅用于失败时的日志定位。
pub fn parse_frontmatter(markdown: &str, source_path: Option<&str>) -> ParsedMarkdown {
    // 匹配 `^---\s*\n([\s\S]*?)---\s*\n?`
    if let Some(start_match) = FRONTMATTER_START.captures(markdown) {
        let body_start = start_match.get(0).map(|m| m.end()).unwrap_or(0);
        // 找到第一个 `---` 结束分隔符（非贪婪）
        if let Some(end) = find_closing_fence(&markdown[body_start..]) {
            let frontmatter_text = &markdown[body_start..body_start + end.rel_start];
            // \s*（空白）+ --- + \s*\n?：吃掉结尾换行
            let consumed = body_start + end.rel_end;
            let content = &markdown[consumed..];

            let frontmatter = parse_frontmatter_yaml(frontmatter_text, source_path);
            return ParsedMarkdown {
                frontmatter,
                content: content.to_string(),
            };
        }
    }

    ParsedMarkdown {
        frontmatter: serde_json::Map::new(),
        content: markdown.to_string(),
    }
}

struct ClosingFence {
    /// `---` 分隔符开始处（相对 body_start）
    rel_start: usize,
    /// `---` + 尾随空白/换行消费到何处
    rel_end: usize,
}

fn find_closing_fence(body: &str) -> Option<ClosingFence> {
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'-' && bytes[i + 2] == b'-'
            && (i + 3 == bytes.len() || bytes[i + 3] == b'\n' || bytes[i + 3] == b'\r' || bytes[i + 3] == b' ' || bytes[i + 3] == b'\t')
        {
            let rel_start = i;
            let mut j = i + 3;
            // ---\s*\n?：消耗空白
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                j += 1;
            }
            return Some(ClosingFence {
                rel_start,
                rel_end: j,
            });
        }
        i += 1;
    }
    None
}

fn parse_frontmatter_yaml(frontmatter_text: &str, source_path: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    // 先直接解析
    if let Ok(parsed) = parse_yaml(frontmatter_text) {
        if let Some(map) = yaml_to_frontmatter_object(&parsed) {
            return map;
        }
    }
    // 解析失败：给问题值加引号后重试
    if let Ok(parsed) = parse_yaml(&quote_problematic_values(frontmatter_text)) {
        if let Some(map) = yaml_to_frontmatter_object(&parsed) {
            return map;
        }
    }
    // 仍失败：记日志（便于用户排查坏掉的 frontmatter），返回空
    let location = source_path.map(|p| format!(" in {p}")).unwrap_or_default();
    log_for_debugging(
        &format!("Failed to parse YAML frontmatter{location}"),
        DebugLogLevel::Warn,
    );
    serde_json::Map::new()
}

/// 按逗号分隔并展开花括号模式；花括号内的逗号不当分隔符。
/// 同时接受 YAML 字符串数组。
///
/// 例：
/// - "a, b" → ["a", "b"]
/// - "a, src/*.{ts,tsx}" → ["a", "src/*.ts", "src/*.tsx"]
/// - "{a,b}/{c,d}" → ["a/c", "a/d", "b/c", "b/d"]
pub fn split_path_in_frontmatter(input: &FrontmatterValue) -> Vec<String> {
    match input {
        FrontmatterValue::Array(items) => items
            .iter()
            .flat_map(|item| split_path_in_frontmatter(&FrontmatterValue::String(item.clone())))
            .collect(),
        FrontmatterValue::String(s) => {
            // 按逗号分隔（尊重花括号）
            let parts = split_respecting_braces(s);
            parts.into_iter().flat_map(|p| expand_braces(&p)).collect()
        }
    }
}

/// frontmatter 输入值（字符串或字符串数组）。
#[derive(Debug, Clone)]
pub enum FrontmatterValue {
    String(String),
    Array(Vec<String>),
}

fn split_respecting_braces(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut brace_depth = 0i32;

    for char in input.chars() {
        if char == '{' {
            brace_depth += 1;
            current.push(char);
        } else if char == '}' {
            brace_depth -= 1;
            current.push(char);
        } else if char == ',' && brace_depth == 0 {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
            current.clear();
        } else {
            current.push(char);
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_string());
    }
    parts
}

/// 展开花括号模式。
///
/// 例："src/*.{ts,tsx}" → ["src/*.ts", "src/*.tsx"]；
/// "{a,b}/{c,d}" → ["a/c", "a/d", "b/c", "b/d"]。
fn expand_braces(pattern: &str) -> Vec<String> {
    // 找第一组花括号
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let close = pattern[open..].find('}').map(|c| open + c);
    let Some(close) = close else {
        return vec![pattern.to_string()];
    };

    let prefix = &pattern[..open];
    let alternatives = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];

    let mut expanded = Vec::new();
    for part in alternatives.split(',') {
        let combined = format!("{prefix}{}{suffix}", part.trim());
        // 递归展开剩余花括号
        expanded.extend(expand_braces(&combined));
    }
    expanded
}

/// 从 frontmatter 值解析正整数。
///
/// 接受数字或其字符串形式；非法或非正返回 None。
pub fn parse_positive_int_from_frontmatter(value: &serde_json::Value) -> Option<u64> {
    let parsed = match value {
        serde_json::Value::Number(n) => n.as_u64()?,
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    (parsed > 0).then_some(parsed)
}

/// 校验并强转 description 值。
///
/// - 字符串 trim 后返回（空串返回 None）
/// - 数字/布尔转字符串
/// - 数组/对象等非标量：记日志并丢弃（返回 None）
/// - null/undefined/空白：None
pub fn coerce_description_to_string(
    value: &serde_json::Value,
    component_name: Option<&str>,
    plugin_name: Option<&str>,
) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => {
            // 非标量描述（数组、对象）无效：记日志并丢弃
            let source = match (plugin_name, component_name) {
                (Some(plugin), Some(component)) => format!("{plugin}:{component}"),
                (None, Some(component)) => component.to_string(),
                _ => "unknown".to_string(),
            };
            log_for_debugging(
                &format!("Description invalid for {source} - omitting"),
                DebugLogLevel::Warn,
            );
            None
        }
    }
}

/// 解析布尔 frontmatter 值：仅字面 true 或 "true" 返回 true。
pub fn parse_boolean_frontmatter(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Bool(true)) || value == "true"
}

/// shell: frontmatter 接受的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontmatterShell {
    Bash,
    PowerShell,
}

impl FrontmatterShell {
    pub fn as_str(self) -> &'static str {
        match self {
            FrontmatterShell::Bash => "bash",
            FrontmatterShell::PowerShell => "powershell",
        }
    }
}

/// 解析并校验 `shell:` frontmatter 字段。
///
/// 缺省/null/空 → None（调用方默认 bash）。无法识别的值：记警告并
/// 返回 None（回退 bash，与 effort 等字段的降级行为一致）。
pub fn parse_shell_frontmatter(value: &serde_json::Value, source: &str) -> Option<FrontmatterShell> {
    if value.is_null() {
        return None;
    }
    let s = match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let normalized = s.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    match normalized.as_str() {
        "bash" => Some(FrontmatterShell::Bash),
        "powershell" => Some(FrontmatterShell::PowerShell),
        _ => {
            log_for_debugging(
                &format!(
                    "Frontmatter 'shell: {s}' in {source} is not recognized. Valid values: bash, powershell. Falling back to bash."
                ),
                DebugLogLevel::Warn,
            );
            None
        }
    }
}
