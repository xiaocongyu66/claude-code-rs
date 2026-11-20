//! debug_filter.rs — debug 输出的分类过滤。
//!
//! 重写自 `src/utils/debugFilter.ts`。Cargo.toml 暂无 regex 依赖，
//! 五个模式全部改为手写扫描器，与原正则一一对齐：
//!
//! - `^MCP server ["']([^"']+)["']` → ["mcp", <name>]
//! - `^([^:[]+):` → prefix category
//! - `^\[([^\]]+)]` → bracket category
//! - 大小写不敏感子串 `1p event:` → "1p"
//! - `:\s*([^:]+?)(?:\s+(?:type|mode|status|event))?:` → 二级 category

use std::collections::HashSet;

/// 解析后的 debug 过滤配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub is_exclusive: bool,
}

/// 解析 debug 过滤串。
///
/// - `"api,hooks"` → 只包含 api 与 hooks
/// - `"!1p,!file"` → 排除 logging、file 类
/// - 空 / 混合（同时含 include 与 exclude）→ None（不过滤）
pub fn parse_debug_filter(filter_string: Option<&str>) -> Option<DebugFilter> {
    let raw = filter_string.unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }

    let filters: Vec<String> = raw
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect();

    if filters.is_empty() {
        return None;
    }

    let has_exclusive = filters.iter().any(|f| f.starts_with('!'));
    let has_inclusive = filters.iter().any(|f| !f.starts_with('!'));

    if has_exclusive && has_inclusive {
        // 混合模式视为错误情形：显示全部消息（静默返回 None）
        return None;
    }

    let clean_filters: Vec<String> = filters
        .iter()
        .map(|f| f.trim_start_matches('!').to_ascii_lowercase())
        .collect();

    let (include, exclude) = if has_exclusive {
        (Vec::new(), clean_filters)
    } else {
        (clean_filters, Vec::new())
    };

    Some(DebugFilter {
        include,
        exclude,
        is_exclusive: has_exclusive,
    })
}

/// 从消息中提取 debug 分类（小写）。
pub fn extract_debug_categories(message: &str) -> Vec<String> {
    let mut categories: Vec<String> = Vec::new();

    // 模式 3：MCP server "servername"（优先，避免误匹配）
    if let Some(name) = parse_mcp_server(message) {
        categories.push("mcp".to_string());
        categories.push(name.to_ascii_lowercase());
    } else if let Some(prefix) = parse_prefix_category(message) {
        // 模式 1："category: message"（非 MCP 时才匹配）
        categories.push(prefix);
    }

    // 模式 2：开头的 [CATEGORY]
    if let Some(bracket) = parse_bracket_category(message) {
        categories.push(bracket);
    }

    // 模式 4：消息内含 "1p event:"
    if message.to_ascii_lowercase().contains("1p event:") {
        categories.push("1p".to_string());
    }

    // 模式 5：首个冒号后的二级 category
    if let Some(secondary) = parse_secondary_category(message) {
        categories.push(secondary);
    }

    // 去重（保留顺序）
    let mut seen = HashSet::new();
    categories.retain(|c| seen.insert(c.clone()));
    categories
}

/// `^MCP server ["']([^"']+)["']`
fn parse_mcp_server(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("MCP server ")?;
    let (quote, inner) = match rest.chars().next() {
        Some('"') => ("\"", &rest[1..]),
        Some('\'') => ("'", &rest[1..]),
        _ => return None,
    };
    let end = inner.find(quote)?;
    Some(&inner[..end])
}

/// `^([^:[]+):` —— 行首到第一个冒号或 `[` 之前
fn parse_prefix_category(message: &str) -> Option<String> {
    let bytes = message.as_bytes();
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            end = Some(i);
            break;
        }
        if b == b'[' {
            return None;
        }
    }
    let end = end?;
    if end == 0 {
        return None;
    }
    Some(message[..end].trim().to_ascii_lowercase())
}

/// `^\[([^\]]+)]`
fn parse_bracket_category(message: &str) -> Option<String> {
    let rest = message.strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(rest[..end].trim().to_ascii_lowercase())
}

/// `:\s*([^:]+?)(?:\s+(?:type|mode|status|event))?:`
fn parse_secondary_category(message: &str) -> Option<String> {
    let colon = message.find(':')?;
    let after = &message[colon + 1..];
    // 跳过空白
    let after = after.trim_start();
    // 取到下一个冒号（正则 `[^:]+?` 非贪婪 + 尾部词）
    let next_colon = after.find(':')?;
    let mut candidate = after[..next_colon].trim();
    // 可选尾随词：(type|mode|status|event)
    for word in [" type", " mode", " status", " event"] {
        if let Some(stripped) = candidate.strip_suffix(word) {
            candidate = stripped.trim_end();
            break;
        }
    }
    // 合法 category：长度 < 30 且不含空格
    if candidate.len() < 30 && !candidate.contains(' ') && !candidate.is_empty() {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

/// 已提取的分类是否应当展示。
pub fn should_show_debug_categories(categories: &[String], filter: Option<&DebugFilter>) -> bool {
    let Some(filter) = filter else {
        return true; // 无过滤 → 全显
    };

    // 无分类消息：两种模式下都不展示（inclusive 需命中分类；
    // exclusive 出于安全默认排除未分类消息）
    if categories.is_empty() {
        return false;
    }

    if filter.is_exclusive {
        // 任一分类在排除表中即隐藏
        !categories.iter().any(|cat| filter.exclude.contains(cat))
    } else {
        // 任一分类在包含表中即展示
        categories.iter().any(|cat| filter.include.contains(cat))
    }
}

/// 组合提取与过滤：消息是否应当展示。
pub fn should_show_debug_message(message: &str, filter: Option<&DebugFilter>) -> bool {
    if filter.is_none() {
        return true;
    }
    let categories = extract_debug_categories(message);
    should_show_debug_categories(&categories, filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters() {
        assert_eq!(parse_debug_filter(None), None);
        assert_eq!(parse_debug_filter(Some("   ")), None);
        let f = parse_debug_filter(Some("api,hooks")).unwrap();
        assert_eq!(f.include, vec!["api".to_string(), "hooks".to_string()]);
        assert!(f.exclude.is_empty());
        assert!(!f.is_exclusive);
        let f = parse_debug_filter(Some("!1p,!file")).unwrap();
        assert!(f.include.is_empty());
        assert_eq!(f.exclude, vec!["1p".to_string(), "file".to_string()]);
        assert!(f.is_exclusive);
        // 混合 → None
        assert_eq!(parse_debug_filter(Some("api,!hooks")), None);
    }

    #[test]
    fn extracts_categories() {
        assert_eq!(
            extract_debug_categories("api: something happened"),
            vec!["api".to_string()]
        );
        assert_eq!(
            extract_debug_categories("[ANT-ONLY] 1P event: tengu_timer"),
            vec!["ant-only".to_string(), "1p".to_string()]
        );
        assert_eq!(
            extract_debug_categories("MCP server \"myserver\": ready"),
            vec!["mcp".to_string(), "myserver".to_string()]
        );
        // 二级分类
        let cats = extract_debug_categories("AutoUpdaterWrapper: Installation type: development");
        assert!(cats.iter().any(|c| c == "installation"));
    }

    #[test]
    fn filtering() {
        assert!(should_show_debug_message("anything", None));

        let inclusive = parse_debug_filter(Some("api")).unwrap();
        assert!(should_show_debug_message("api: hit", Some(&inclusive)));
        assert!(!should_show_debug_message("other: hit", Some(&inclusive)));
        assert!(!should_show_debug_message(
            "no categories here",
            Some(&inclusive)
        ));

        let exclusive = parse_debug_filter(Some("!api")).unwrap();
        assert!(!should_show_debug_message("api: hit", Some(&exclusive)));
        assert!(should_show_debug_message("other: hit", Some(&exclusive)));
    }
}
