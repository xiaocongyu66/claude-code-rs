//! normalization.rs — MCP 名称规范化（重写自 `normalization.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 将非 `[a-zA-Z0-9_-]` 字符替换为下划线，满足 API 模式
//!   `^[a-zA-Z0-9_-]{1,64}$`。
//! - claude.ai 服务器名（以 "claude.ai " 开头）额外折叠连续下划线并去除
//!   首尾下划线，避免与 MCP 工具名中的 `__` 分隔符冲突。
//!
//! 无依赖（TS 原文同样无依赖，避免循环 import）。

use once_cell::sync::Lazy;
use regex::Regex;

/// Claude.ai 服务器名前缀（TS 同名常量）。
pub const CLAUDEAI_SERVER_PREFIX: &str = "claude.ai ";

static INVALID_NAME_CHAR: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_-]").ok());
static MULTI_UNDERSCORE: Lazy<Option<Regex>> = Lazy::new(|| Regex::new(r"_+").ok());
static EDGE_UNDERSCORE: Lazy<Option<Regex>> = Lazy::new(|| Regex::new(r"^_|_$").ok());

/// 将单个正则应用到字符串；正则编译失败（不应发生）时原样返回。
fn re_replace(re: &Lazy<Option<Regex>>, input: &str, with: &str) -> String {
    match re.as_ref() {
        Some(rx) => rx.replace_all(input, with).into_owned(),
        None => input.to_string(),
    }
}

/// 规范化服务器名以兼容 API 模式 `^[a-zA-Z0-9_-]{1,64}$`。
pub fn normalize_name_for_mcp(name: &str) -> String {
    let mut normalized = re_replace(&INVALID_NAME_CHAR, name, "_");
    if name.starts_with(CLAUDEAI_SERVER_PREFIX) {
        normalized = re_replace(&MULTI_UNDERSCORE, &normalized, "_");
        normalized = re_replace(&EDGE_UNDERSCORE, &normalized, "");
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_invalid_chars() {
        assert_eq!(normalize_name_for_mcp("my.server"), "my_server");
        assert_eq!(normalize_name_for_mcp("my server!"), "my_server_");
        assert_eq!(normalize_name_for_mcp("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn collapses_claude_ai_names() {
        assert_eq!(
            normalize_name_for_mcp("claude.ai Slack (2)"),
            "claude_ai_Slack_2"
        );
    }
}
