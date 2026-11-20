//! permission_rule_parser.rs — 权限规则字符串的解析与序列化。
//!
//! 重写自 `src/utils/permissions/permissionRuleParser.ts`。语义对齐说明：
//!
//! - TS 侧工具名常量来自 `@claude-code-hardened/builtin-tools`（值为
//!   `'Agent'` / `'TaskStop'` / `'TaskOutput'` / `'SendUserMessage'`），
//!   Rust 仓库尚无 builtin-tools 模块，按当前值内联；
//! - TS 的 `feature('KAIROS') || feature('KAIROS_BRIEF')` 门控决定是否
//!   注入 `Brief → SendUserMessage` 别名。Rust 无 Bun 编译期 feature，
//!   一律注入（dev 模式默认启用全部 feature；集合中多一个别名只会让
//!   归一化更激进，与 TS 启用态一致）；
//! - TS 的 `str.replace(/\\/g, ...)` 等正则替换改为字符级扫描实现，
//!   语义逐条对齐（转义顺序、起止判定、边界条件见各函数注释）；
//! - TS 按字符索引（UTF-16 code unit）切片；Rust 按 Unicode 标量值
//!   （`Vec<char>`）处理。对常见规则字符串无行为差异。
//!
//! 规则字符串格式：`"ToolName"` 或 `"ToolName(content)"`；
//! content 中的括号需转义为 `\(` / `\)`。

use crate::types::permissions::PermissionRuleValue;

/// 旧工具名 → 当代规范名的映射（TS `LEGACY_TOOL_NAME_ALIASES`）。
/// 工具改名后在此添加 old → new，使权限规则、hooks 与持久化 wire 名
/// 解析到规范名。
const LEGACY_TOOL_NAME_ALIASES: &[(&str, &str)] = &[
    // Task → AGENT_TOOL_NAME
    ("Task", "Agent"),
    // KillShell → TASK_STOP_TOOL_NAME
    ("KillShell", "TaskStop"),
    // AgentOutputTool → TASK_OUTPUT_TOOL_NAME
    ("AgentOutputTool", "TaskOutput"),
    // BashOutputTool → TASK_OUTPUT_TOOL_NAME
    ("BashOutputTool", "TaskOutput"),
    // Brief → BRIEF_TOOL_NAME（feature('KAIROS') || feature('KAIROS_BRIEF')）
    ("Brief", "SendUserMessage"),
];

/// 把旧工具名归一化为当前规范名；无匹配时原样返回。
pub fn normalize_legacy_tool_name(name: &str) -> String {
    for (legacy, canonical) in LEGACY_TOOL_NAME_ALIASES {
        if *legacy == name {
            return canonical.to_string();
        }
    }
    name.to_string()
}

/// 返回所有会归一化为 `canonical_name` 的旧工具名（不含自身）。
pub fn get_legacy_tool_names(canonical_name: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for (legacy, canonical) in LEGACY_TOOL_NAME_ALIASES {
        if *canonical == canonical_name {
            result.push(legacy.to_string());
        }
    }
    result
}

/// 转义规则 content 中的特殊字符，使其可安全存入权限规则字符串。
/// 规则格式为 `"Tool(content)"`，因此括号必须转义。
///
/// 转义顺序（必须严格遵守）：
/// 1. 先转义反斜杠（`\` → `\\`）
/// 2. 再转义括号（`(` → `\(`，`)` → `\)`）
///
/// 与 TS 的三条全局正则替换等价：先 `\\`→`\\\\`，再 `(`→`\(`、`)`→`\)`。
pub fn escape_rule_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for ch in content.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            other => out.push(other),
        }
    }
    out
}

/// 反转义规则 content（`escapeRuleContent` 的逆操作）。
///
/// 解转义顺序（逆序）：
/// 1. 先解括号（`\(` → `(`，`\)` → `)`）
/// 2. 最后解反斜杠（`\\` → `\`）
///
/// 与 TS 的三条全局正则替换等价。扫描规则：遇到 `\` 时，
/// 若下一字符是 `(` / `)` / `\` 之一，则输出该字符并跳过两字符；
/// 否则原样保留反斜杠（与 TS 正则的「只替换匹配对」语义一致，
/// 例如 `\n` 保持不变）。
pub fn unescape_rule_content(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            match chars[i + 1] {
                '(' | ')' | '\\' => {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// 把权限规则字符串解析为组件。处理 content 中的转义括号。
///
/// 格式：`"ToolName"` 或 `"ToolName(content)"`；
/// content 可含转义括号：`\(` / `\)`。
///
/// 解析规则（逐条对齐 TS）：
/// 1. 找第一个未转义的 `(`；没有则整串就是工具名；
/// 2. 找最后一个未转义的 `)`；没有、或位置不大于 `(`、或 `)` 不在
///    末尾，都视为畸形——整串作为工具名；
/// 3. 工具名为空（如 `"(foo)"`）→ 整串作为工具名；
/// 4. content 为空或单独的 `*`（如 `"Bash()"` / `"Bash(*)"`）→ 退化为
///    工具级规则（无 ruleContent）；
/// 5. 其余情况对 content 解转义。
pub fn permission_rule_value_from_string(rule_string: &str) -> PermissionRuleValue {
    let chars: Vec<char> = rule_string.chars().collect();

    // 1. 第一个未转义的开括号
    let Some(open_paren_index) = find_first_unescaped_char(&chars, '(') else {
        return PermissionRuleValue {
            tool_name: normalize_legacy_tool_name(rule_string),
            rule_content: None,
        };
    };

    // 2. 最后一个未转义的闭括号
    let close_paren_index = match find_last_unescaped_char(&chars, ')') {
        Some(idx) if idx > open_paren_index && idx == chars.len() - 1 => idx,
        _ => {
            // 无匹配闭括号、位置非法或闭括后还有内容 —— 整串作为工具名
            return PermissionRuleValue {
                tool_name: normalize_legacy_tool_name(rule_string),
                rule_content: None,
            };
        }
    };

    let tool_name: String = chars[..open_paren_index].iter().collect();
    let raw_content: String = chars[open_paren_index + 1..close_paren_index]
        .iter()
        .collect();

    // 3. 缺工具名（如 "(foo)"）—— 整串作为工具名
    if tool_name.is_empty() {
        return PermissionRuleValue {
            tool_name: normalize_legacy_tool_name(rule_string),
            rule_content: None,
        };
    }

    // 4. 空 content（"Bash()"）或单独通配（"Bash(*)"）—— 工具级规则
    if raw_content.is_empty() || raw_content == "*" {
        return PermissionRuleValue {
            tool_name: normalize_legacy_tool_name(&tool_name),
            rule_content: None,
        };
    }

    // 5. 解转义 content
    PermissionRuleValue {
        tool_name: normalize_legacy_tool_name(&tool_name),
        rule_content: Some(unescape_rule_content(&raw_content)),
    }
}

/// 把权限规则值序列化为字符串（`permissionRuleValueFromString` 的逆操作）。
/// content 中的括号会被转义，避免解析歧义。
pub fn permission_rule_value_to_string(rule_value: &PermissionRuleValue) -> String {
    match &rule_value.rule_content {
        None => rule_value.tool_name.clone(),
        Some(content) => format!(
            "{}({})",
            rule_value.tool_name,
            escape_rule_content(content)
        ),
    }
}

/// 查找某字符第一个未转义出现的下标。
/// 「未转义」= 前面有偶数个（含 0）连续反斜杠。
/// 未找到返回 `None`（TS 返回 -1）。
fn find_first_unescaped_char(chars: &[char], target: char) -> Option<usize> {
    for i in 0..chars.len() {
        if chars[i] == target && backslash_count_before(chars, i) % 2 == 0 {
            return Some(i);
        }
    }
    None
}

/// 查找某字符最后一个未转义出现的下标。
/// 「未转义」= 前面有偶数个（含 0）连续反斜杠。
/// 未找到返回 `None`（TS 返回 -1）。
fn find_last_unescaped_char(chars: &[char], target: char) -> Option<usize> {
    for i in (0..chars.len()).rev() {
        if chars[i] == target && backslash_count_before(chars, i) % 2 == 0 {
            return Some(i);
        }
    }
    None
}

/// 统计下标 `i` 之前连续的反斜杠数量（从 `i-1` 往左数）。
fn backslash_count_before(chars: &[char], i: usize) -> u32 {
    let mut count = 0u32;
    let mut j = i as isize - 1;
    while j >= 0 && chars[j as usize] == '\\' {
        count += 1;
        j -= 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_names() {
        assert_eq!(normalize_legacy_tool_name("Task"), "Agent");
        assert_eq!(normalize_legacy_tool_name("KillShell"), "TaskStop");
        assert_eq!(normalize_legacy_tool_name("AgentOutputTool"), "TaskOutput");
        assert_eq!(normalize_legacy_tool_name("BashOutputTool"), "TaskOutput");
        assert_eq!(normalize_legacy_tool_name("Brief"), "SendUserMessage");
        assert_eq!(normalize_legacy_tool_name("Bash"), "Bash");
    }

    #[test]
    fn legacy_names_for_canonical() {
        assert_eq!(get_legacy_tool_names("Agent"), vec!["Task"]);
        assert_eq!(
            get_legacy_tool_names("TaskOutput"),
            vec!["AgentOutputTool", "BashOutputTool"]
        );
        assert_eq!(get_legacy_tool_names("Read"), Vec::<String>::new());
    }

    #[test]
    fn escapes_and_unescapes_content() {
        assert_eq!(escape_rule_content("psycopg2.connect()"), "psycopg2.connect\\(\\)");
        assert_eq!(
            escape_rule_content("echo \"test\\nvalue\""),
            "echo \"test\\\\nvalue\""
        );
        assert_eq!(
            unescape_rule_content("psycopg2.connect\\(\\)"),
            "psycopg2.connect()"
        );
    }

    #[test]
    fn escape_unescape_roundtrip() {
        let cases = [
            "npm install",
            "python -c \"print(1)\"",
            "a\\b(c)d",
            "tricky\\\\(paren",
        ];
        for case in cases {
            assert_eq!(unescape_rule_content(&escape_rule_content(case)), case);
        }
    }

    #[test]
    fn parses_tool_name_only() {
        let v = permission_rule_value_from_string("Bash");
        assert_eq!(v.tool_name, "Bash");
        assert_eq!(v.rule_content, None);
    }

    #[test]
    fn parses_tool_name_with_content() {
        let v = permission_rule_value_from_string("Bash(npm install)");
        assert_eq!(v.tool_name, "Bash");
        assert_eq!(v.rule_content.as_deref(), Some("npm install"));
    }

    #[test]
    fn parses_escaped_parens_in_content() {
        let v = permission_rule_value_from_string("Bash(python -c \"print\\(1\\)\")");
        assert_eq!(v.tool_name, "Bash");
        assert_eq!(v.rule_content.as_deref(), Some("python -c \"print(1)\""));
    }

    #[test]
    fn parses_wildcard_rule_as_tool_only() {
        for rule in ["Bash(*)", "Bash()", "Bash"] {
            let v = permission_rule_value_from_string(rule);
            assert_eq!(v.tool_name, "Bash");
            assert_eq!(v.rule_content, None);
        }
    }

    #[test]
    fn malformed_rules_fall_back_to_tool_name() {
        for rule in ["Bash(foo", "Bash foo)", "Bash(f)o", "(foo)", ""] {
            let v = permission_rule_value_from_string(rule);
            assert_eq!(v.tool_name, normalize_legacy_tool_name(rule));
            assert_eq!(v.rule_content, None);
        }
    }

    #[test]
    fn normalizes_legacy_tool_in_rule() {
        let v = permission_rule_value_from_string("Task");
        assert_eq!(v.tool_name, "Agent");
    }

    #[test]
    fn stringifies_rule_values() {
        assert_eq!(
            permission_rule_value_to_string(&PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: None,
            }),
            "Bash"
        );
        assert_eq!(
            permission_rule_value_to_string(&PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("npm install".to_string()),
            }),
            "Bash(npm install)"
        );
        assert_eq!(
            permission_rule_value_to_string(&PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("python -c \"print(1)\"".to_string()),
            }),
            "Bash(python -c \"print\\(1\\)\")"
        );
    }
}
