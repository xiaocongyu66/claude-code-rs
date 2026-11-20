//! permission_validation.rs — 权限规则格式与内容校验。
//!
//! 重写自 `src/utils/settings/permissionValidation.ts`。
//!
//! 已知语义偏差（被依赖模块尚未转 Rust，待下一波消除）：
//! - TS 的 `permissionRuleValueFromString`（utils/permissions/permissionRuleParser.ts）
//!   会先做 legacy 工具名别名归一化（Task→Agent、KillShell→TaskStop、
//!   AgentOutputTool/BashOutputTool→TaskOutput、KAIROS 开启时 Brief→BRIEF_TOOL_NAME）。
//!   别名表常量位于 builtin-tools 包（TS），Rust 侧尚未落地，本模块内联了
//!   **不含别名归一化**的解析器。对本模块的校验语义无影响（校验只按
//!   精确工具名比较，如 `VaultHttpFetch`、大小写首字母）；legacy 别名的
//!   **匹配**路径在 permissionRuleParser 转换后需同步补上。
//! - `mcpInfoFromString`（services/mcp/mcpStringUtils.ts）为纯字符串函数，
//!   直接内联本模块（不依赖 normalization.ts 的 normalizeNameForMCP）。
//!
//! 其余逻辑逐条对齐：括号转义计数、空括号检测、MCP 规则、自定义规则表、
//! Bash `:*` 与通配检查、文件通配检查、VaultHttpFetch 的 key@host 强制与
//! behavior 感知的 whole-tool allow 禁止。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::utils::string_utils::capitalize;
use crate::utils::settings::tool_validation_config::{
    get_custom_validation, is_bash_prefix_tool, is_file_pattern_tool, CustomValidationResult,
};

/// VaultHttpFetch 规则内容的 `<key>@<host>` 校验（对齐 TS 正则）。
/// host 支持 IPv6 括号形式与可选端口（1-65535），或显式 `*` 通配。
static VAULT_KEY_HOST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^[A-Za-z0-9._-]{1,128}@(?:\*|(?:\[[A-Fa-f0-9:]+\]|[A-Za-z0-9.-]{1,253})(?::(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5]))?)$",
    )
    .expect("VAULT_KEY_HOST_RE is a constant regex")
});

/// 控制字符检测（对齐 TS 的 `/[\x00-\x1F\x7F]/`）。
static CONTROL_CHAR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\x00-\x1F\x7F]").expect("CONTROL_CHAR_RE is a constant regex")
});

/// 文件通配边界检查（对齐 TS 的 `/^\*|\*$|\*\*|\/\*|\*\.|\*\)/`）。
static WILDCARD_BOUNDARY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\*|\*$|\*\*|/\*|\*\.|\*\)").expect("WILDCARD_BOUNDARY_RE is a constant regex")
});

/// 禁止 whole-tool allow 的密钥类工具（allow 形态必须按 key 显式授权；
/// deny 形态不受限，可作 kill switch）。
const VAULT_WHOLE_TOOL_ALLOW_FORBIDDEN: &[&str] = &[
    "LocalVaultFetch", // future tool (not yet implemented; safe to remove if renamed)
    "VaultHttpFetch",  // PR-2 (LOCAL-WIRING)
];

/// 校验结果（对齐 TS 的返回对象）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionRuleValidation {
    pub valid: bool,
    pub error: Option<String>,
    pub suggestion: Option<String>,
    pub examples: Option<Vec<String>>,
}

impl PermissionRuleValidation {
    fn invalid(error: &str) -> Self {
        Self {
            valid: false,
            error: Some(error.to_string()),
            ..Default::default()
        }
    }
}

/// 解析后的权限规则值（对齐 TS `PermissionRuleValue` 的校验相关子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRuleValue {
    pub tool_name: String,
    pub rule_content: Option<String>,
}

/// MCP 工具名信息（对齐 TS `mcpInfoFromString` 的返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInfo {
    pub server_name: String,
    pub tool_name: Option<String>,
}

/// `mcpInfoFromString`：从 "mcp__serverName__toolName" 提取 server/tool。
/// 已知限制与 TS 一致：server 名含 `__` 时解析不准。
pub fn mcp_info_from_string(tool_string: &str) -> Option<McpInfo> {
    let mut parts = tool_string.split("__");
    let mcp_part = parts.next()?;
    let server_name = parts.next()?;
    if mcp_part != "mcp" || server_name.is_empty() {
        return None;
    }
    let tool_name: Vec<&str> = parts.collect();
    let tool_name = if tool_name.is_empty() {
        None
    } else {
        Some(tool_name.join("__"))
    };
    Some(McpInfo {
        server_name: server_name.to_string(),
        tool_name,
    })
}

/// 反转义规则内容（对齐 TS `unescapeRuleContent`：先 `\(`→`(`、`\)`→`)`，
/// 再 `\\`→`\`）。
fn unescape_rule_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 1 < bytes.len()
            && matches!(bytes[i + 1], b'(' | b')' | b'\\')
        {
            out.push(bytes[i + 1] as char);
            i += 2;
        } else {
            // 复制一个完整 UTF-8 字符（对齐 TS 按字面保留多字节字符）
            let mut end = i + 1;
            while end < bytes.len() && !content.is_char_boundary(end) {
                end += 1;
            }
            out.push_str(&content[i..end]);
            i = end;
        }
    }
    out
}

/// index 处的字符是否被转义（前面有奇数个反斜杠）。
fn is_escaped(s: &[u8], index: usize) -> bool {
    let mut backslash_count = 0;
    let mut j = index.saturating_sub(1);
    while s.get(j) == Some(&b'\\') {
        backslash_count += 1;
        if j == 0 {
            break;
        }
        j -= 1;
    }
    backslash_count % 2 != 0
}

/// 统计未转义的字符出现次数。
fn count_unescaped_char(s: &str, ch: u8) -> usize {
    let bytes = s.as_bytes();
    let mut count = 0;
    for i in 0..bytes.len() {
        if bytes[i] == ch && !is_escaped(bytes, i) {
            count += 1;
        }
    }
    count
}

/// 是否存在未转义的空括号 "()"。
fn has_unescaped_empty_parens(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'(' && bytes[i + 1] == b')' && !is_escaped(bytes, i) {
            return true;
        }
    }
    false
}

/// `permissionRuleValueFromString`（不含 legacy 别名归一化，见模块头说明）。
/// 格式 "ToolName" 或 "ToolName(content)"；content 可含转义括号。
fn parse_rule_value(rule_string: &str) -> ParsedRuleValue {
    let bytes = rule_string.as_bytes();

    // 第一个未转义的左括号
    let mut open_index = None;
    for i in 0..bytes.len() {
        if bytes[i] == b'(' && !is_escaped(bytes, i) {
            open_index = Some(i);
            break;
        }
    }
    let Some(open) = open_index else {
        return ParsedRuleValue {
            tool_name: rule_string.to_string(),
            rule_content: None,
        };
    };

    // 最后一个未转义的右括号
    let mut close_index = None;
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b')' && !is_escaped(bytes, i) {
            close_index = Some(i);
            break;
        }
    }
    let Some(close) = close_index else {
        return ParsedRuleValue {
            tool_name: rule_string.to_string(),
            rule_content: None,
        };
    };

    // 右括号必须在末尾，且 toolName 非空，否则整体视作工具名
    if close != bytes.len() - 1 || open == 0 {
        return ParsedRuleValue {
            tool_name: rule_string.to_string(),
            rule_content: None,
        };
    }

    let tool_name = &rule_string[..open];
    let raw_content = &rule_string[open + 1..close];

    // 空内容或独立通配符（"Bash()" / "Bash(*)"）视作工具级规则
    if raw_content.is_empty() || raw_content == "*" {
        return ParsedRuleValue {
            tool_name: tool_name.to_string(),
            rule_content: None,
        };
    }

    ParsedRuleValue {
        tool_name: tool_name.to_string(),
        rule_content: Some(unescape_rule_content(raw_content)),
    }
}

/// 校验权限规则格式与内容。
///
/// `behavior` 为 `Some("allow"|"deny"|"ask")` 时启用行为相关检查（如
/// vault 工具的 whole-tool allow 拒绝、deny 路径的 key@host 强制）。
pub fn validate_permission_rule(rule: &str, behavior: Option<&str>) -> PermissionRuleValidation {
    // Empty rule check
    if rule.trim().is_empty() {
        return PermissionRuleValidation::invalid("Permission rule cannot be empty");
    }

    // Check parentheses matching first (only count unescaped parens)
    let open_count = count_unescaped_char(rule, b'(');
    let close_count = count_unescaped_char(rule, b')');
    if open_count != close_count {
        return PermissionRuleValidation {
            valid: false,
            error: Some("Mismatched parentheses".to_string()),
            suggestion: Some(
                "Ensure all opening parentheses have matching closing parentheses".to_string(),
            ),
            examples: None,
        };
    }

    // Check for empty parentheses (escape-aware)
    if has_unescaped_empty_parens(rule) {
        let tool_name = rule.split('(').next().unwrap_or("");
        if tool_name.is_empty() {
            return PermissionRuleValidation {
                valid: false,
                error: Some("Empty parentheses with no tool name".to_string()),
                suggestion: Some("Specify a tool name before the parentheses".to_string()),
                examples: None,
            };
        }
        return PermissionRuleValidation {
            valid: false,
            error: Some("Empty parentheses".to_string()),
            suggestion: Some(format!(
                "Either specify a pattern or use just \"{}\" without parentheses",
                tool_name
            )),
            examples: Some(vec![
                tool_name.to_string(),
                format!("{}(some-pattern)", tool_name),
            ]),
        };
    }

    // Parse the rule
    let parsed = parse_rule_value(rule);

    // MCP validation - must be done before general tool validation
    if let Some(mcp_info) = mcp_info_from_string(&parsed.tool_name) {
        // MCP rules cannot have any pattern/content (parentheses)
        // Check both parsed content and raw string since the parser normalizes
        // standalone wildcards (e.g., "mcp__server(*)") to undefined ruleContent
        if parsed.rule_content.is_some() || count_unescaped_char(rule, b'(') > 0 {
            let mut examples = vec![
                format!("mcp__{}", mcp_info.server_name),
                format!("mcp__{}__*", mcp_info.server_name),
            ];
            if let Some(tool) = mcp_info.tool_name.as_deref() {
                if tool != "*" {
                    examples.push(format!("mcp__{}__{}", mcp_info.server_name, tool));
                }
            }
            return PermissionRuleValidation {
                valid: false,
                error: Some("MCP rules do not support patterns in parentheses".to_string()),
                suggestion: Some(format!(
                    "Use \"{}\" without parentheses, or use \"mcp__{}__*\" for all tools",
                    parsed.tool_name, mcp_info.server_name
                )),
                examples: Some(examples),
            };
        }

        return PermissionRuleValidation {
            valid: true,
            ..Default::default()
        };
    }

    // Tool name validation (for non-MCP tools)
    if parsed.tool_name.is_empty() {
        return PermissionRuleValidation::invalid("Tool name cannot be empty");
    }

    // Check tool name starts with uppercase (standard tools)
    let first_char = parsed.tool_name.chars().next().unwrap_or(' ');
    let upper = first_char.to_uppercase().next();
    if upper.is_some() && upper != Some(first_char) {
        return PermissionRuleValidation {
            valid: false,
            error: Some("Tool names must start with uppercase".to_string()),
            suggestion: Some(format!("Use \"{}\"", capitalize(&parsed.tool_name))),
            examples: None,
        };
    }

    // Check for custom validation rules first
    if let Some(custom_validation) = get_custom_validation(&parsed.tool_name) {
        if let Some(content) = parsed.rule_content.as_deref() {
            let custom_result: CustomValidationResult = custom_validation(content);
            if !custom_result.valid {
                return PermissionRuleValidation {
                    valid: false,
                    error: custom_result.error,
                    suggestion: custom_result.suggestion,
                    examples: custom_result.examples,
                };
            }
        }
    }

    // Bash-specific validation
    if is_bash_prefix_tool(&parsed.tool_name) {
        if let Some(content) = parsed.rule_content.as_deref() {
            // Check for common :* mistakes - :* must be at the end (legacy prefix syntax)
            if content.contains(":*") && !content.ends_with(":*") {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some("The :* pattern must be at the end".to_string()),
                    suggestion: Some(
                        "Move :* to the end for prefix matching, or use * for wildcard matching"
                            .to_string(),
                    ),
                    examples: Some(vec![
                        "Bash(npm run:*) - prefix matching (legacy)".to_string(),
                        "Bash(npm run *) - wildcard matching".to_string(),
                    ]),
                };
            }

            // Check for :* without a prefix
            if content == ":*" {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some("Prefix cannot be empty before :*".to_string()),
                    suggestion: Some("Specify a command prefix before :*".to_string()),
                    examples: Some(vec!["Bash(npm:*)".to_string(), "Bash(git:*)".to_string()]),
                };
            }
        }
    }

    // File tool validation
    if is_file_pattern_tool(&parsed.tool_name) {
        if let Some(content) = parsed.rule_content.as_deref() {
            // Check for :* in file patterns (common mistake from Bash patterns)
            if content.contains(":*") {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some("The \":*\" syntax is only for Bash prefix rules".to_string()),
                    suggestion: Some(
                        "Use glob patterns like \"*\" or \"**\" for file matching".to_string(),
                    ),
                    examples: Some(vec![
                        format!("{}(*.ts) - matches .ts files", parsed.tool_name),
                        format!("{}(src/**) - matches all files in src", parsed.tool_name),
                        format!(
                            "{}(**/*.test.ts) - matches test files",
                            parsed.tool_name
                        ),
                    ]),
                };
            }

            // Warn about wildcards not at boundaries
            if content.contains('*')
                && !WILDCARD_BOUNDARY_RE.is_match(content)
                && !content.contains("**")
            {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some("Wildcard placement might be incorrect".to_string()),
                    suggestion: Some(
                        "Wildcards are typically used at path boundaries".to_string(),
                    ),
                    examples: Some(vec![
                        format!("{}(*.js) - all .js files", parsed.tool_name),
                        format!("{}(src/*) - all files directly in src", parsed.tool_name),
                        format!("{}(src/**) - all files recursively in src", parsed.tool_name),
                    ]),
                };
            }
        }
    }

    // H2 fix (codecov-100 audit): defensive pre-validation of ruleContent
    // before any regex is run. Reject oversized / control-char input.
    if parsed.tool_name == "VaultHttpFetch" {
        if let Some(rc) = parsed.rule_content.as_deref() {
            if rc.len() > 384 {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some(format!(
                        "VaultHttpFetch rule content is too long ({} chars; max 384)",
                        rc.len()
                    )),
                    suggestion: Some(
                        "Use a shorter key name and host, or use the wildcard form <key>@*"
                            .to_string(),
                    ),
                    examples: None,
                };
            }
            if CONTROL_CHAR_RE.is_match(rc) {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some(
                        "VaultHttpFetch rule content contains control characters (only printable ASCII allowed in key@host)".to_string(),
                    ),
                    suggestion: Some("Remove control characters from the rule content".to_string()),
                    examples: None,
                };
            }
        }
    }

    // F3 fix (Codex round 6): apply the same `<key>@<host>` enforcement on
    // the deny path.
    if parsed.tool_name == "VaultHttpFetch" && behavior == Some("deny") {
        if let Some(rc) = parsed.rule_content.as_deref() {
            if !VAULT_KEY_HOST_RE.is_match(rc) {
                return PermissionRuleValidation {
                    valid: false,
                    error: Some(
                        "VaultHttpFetch deny rule content must be '<key>@<host>' or '<key>@*' (or whole-tool deny without parentheses for kill switch)".to_string(),
                    ),
                    suggestion: Some(format!(
                        "Found '{}'. Use 'VaultHttpFetch' (no parens) for kill switch, or 'VaultHttpFetch({}@*)' for any-host.",
                        rc, rc
                    )),
                    examples: Some(vec![
                        "VaultHttpFetch — whole-tool kill switch".to_string(),
                        format!("VaultHttpFetch({}@api.github.com)", rc),
                        format!("VaultHttpFetch({}@*)", rc),
                    ]),
                };
            }
        }
    }

    // Behavior-aware checks for vault-class tools.
    if behavior == Some("allow") {
        // Forbid whole-tool allow (no parentheses, no ruleContent).
        if parsed.rule_content.is_none()
            && VAULT_WHOLE_TOOL_ALLOW_FORBIDDEN.contains(&parsed.tool_name.as_str())
        {
            return PermissionRuleValidation {
                valid: false,
                error: Some(format!(
                    "Whole-tool allow forbidden for vault tool '{}'",
                    parsed.tool_name
                )),
                suggestion: Some(format!(
                    "Use per-key + per-host allow: '{}(your-key-name@host)'",
                    parsed.tool_name
                )),
                examples: Some(vec![
                    format!("{}(github-token@api.github.com)", parsed.tool_name),
                    format!("{}(my-api@*) - allow any host (advanced)", parsed.tool_name),
                ]),
            };
        }
        // For VaultHttpFetch specifically, require the rule content to be
        // formatted as `<key>@<host>` (or `<key>@*`).
        if parsed.tool_name == "VaultHttpFetch" {
            if let Some(rc) = parsed.rule_content.as_deref() {
                if !VAULT_KEY_HOST_RE.is_match(rc) {
                    return PermissionRuleValidation {
                        valid: false,
                        error: Some(
                            "VaultHttpFetch rule content must be '<key>@<host>' or '<key>@*'".to_string(),
                        ),
                        suggestion: Some(format!(
                            "Found '{}'. Use e.g. 'github-token@api.github.com' or 'admin-key@127.0.0.1:8443' to bind a key to a host.",
                            rc
                        )),
                        examples: Some(vec![
                            "VaultHttpFetch(github-token@api.github.com)".to_string(),
                            "VaultHttpFetch(local-admin@localhost:8443)".to_string(),
                            "VaultHttpFetch(stripe-key@*) - any host (advanced)".to_string(),
                        ]),
                    };
                }
            }
        }
    }

    PermissionRuleValidation {
        valid: true,
        ..Default::default()
    }
}

/// `PermissionRuleSchema`（zod string + superRefine）的 Rust 对应物：
/// 字符串形式的单条权限规则校验，失败时返回组装后的 message（对齐 TS
/// `message += \`. ${suggestion}\` + \`. Examples: ...\``）。
pub fn permission_rule_schema_validate(val: &str) -> Result<(), String> {
    let result = validate_permission_rule(val, None);
    if result.valid {
        return Ok(());
    }
    let mut message = result.error.unwrap_or_default();
    if let Some(suggestion) = result.suggestion {
        message.push_str(". ");
        message.push_str(&suggestion);
    }
    if let Some(examples) = result.examples {
        if !examples.is_empty() {
            message.push_str(". Examples: ");
            message.push_str(&examples.join(", "));
        }
    }
    Err(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_paren_rules() {
        assert!(!validate_permission_rule("", None).valid);
        assert!(!validate_permission_rule("   ", None).valid);
        let r = validate_permission_rule("Bash(", None);
        assert!(!r.valid);
        assert_eq!(r.error.as_deref(), Some("Mismatched parentheses"));
        let r = validate_permission_rule("Bash()", None);
        assert!(!r.valid);
        assert_eq!(r.error.as_deref(), Some("Empty parentheses"));
        let r = validate_permission_rule("()", None);
        assert!(!r.valid);
        assert_eq!(r.error.as_deref(), Some("Empty parentheses with no tool name"));
    }

    #[test]
    fn mcp_rules() {
        assert!(validate_permission_rule("mcp__server", None).valid);
        assert!(validate_permission_rule("mcp__server__tool", None).valid);
        assert!(validate_permission_rule("mcp__server__*", None).valid);
        // MCP 规则不接受括号：parser 会把独立通配符 `(*)` 归一化掉，但原始
        // 字符串含 `(` → 仍判非法（对齐 TS 的双重检查）
        let r = validate_permission_rule("mcp__server(*)", None);
        assert!(!r.valid, "{:?}", r);
        let r = validate_permission_rule("mcp__server(foo)", None);
        assert!(!r.valid);
    }

    #[test]
    fn tool_name_rules() {
        assert!(validate_permission_rule("Bash", None).valid);
        let r = validate_permission_rule("bash(npm install)", None);
        assert!(!r.valid);
        let r = validate_permission_rule("Bash(npm install)", None);
        assert!(r.valid, "{:?}", r);
        // Bash :* checks
        let r = validate_permission_rule("Bash(npm run:* extra)", None);
        assert!(!r.valid);
        let r = validate_permission_rule("Bash(:*)", None);
        assert!(!r.valid);
        assert!(validate_permission_rule("Bash(npm:*)", None).valid);
    }

    #[test]
    fn file_pattern_rules() {
        let r = validate_permission_rule("Read(npm:*)", None);
        assert!(!r.valid);
        assert!(validate_permission_rule("Read(*.ts)", None).valid);
        assert!(validate_permission_rule("Read(src/**)", None).valid);
        // Wildcard in the middle → flagged
        let r = validate_permission_rule("Read(foo*bar)", None);
        assert!(!r.valid);
    }

    #[test]
    fn vault_rules() {
        // allow behavior: whole-tool forbidden
        let r = validate_permission_rule("VaultHttpFetch", Some("allow"));
        assert!(!r.valid);
        // deny behavior: whole-tool OK (kill switch)
        assert!(validate_permission_rule("VaultHttpFetch", Some("deny")).valid);
        // bare key without host rejected on allow
        let r = validate_permission_rule("VaultHttpFetch(github-token)", Some("allow"));
        assert!(!r.valid);
        // key@host OK
        assert!(
            validate_permission_rule("VaultHttpFetch(github-token@api.github.com)", Some("allow"))
                .valid
        );
        assert!(validate_permission_rule("VaultHttpFetch(key@*)", Some("allow")).valid);
        // deny requires key@host too
        let r = validate_permission_rule("VaultHttpFetch(github-token)", Some("deny"));
        assert!(!r.valid);
        // too long
        let long = format!("VaultHttpFetch({})", "a".repeat(400));
        assert!(!validate_permission_rule(&long, Some("allow")).valid);
        // control chars
        let r = validate_permission_rule("VaultHttpFetch(a\x01b@host)", Some("allow"));
        assert!(!r.valid);
    }

    #[test]
    fn escaped_parens() {
        // Escaped parens inside content are preserved
        assert!(validate_permission_rule(r#"Bash(python -c "print\(1\)")"#, None).valid);
        let r = parse_rule_value(r#"Bash(python -c "print\(1\)")"#);
        assert_eq!(r.tool_name, "Bash");
        assert_eq!(r.rule_content.as_deref(), Some(r#"python -c "print(1)""#));
    }

    #[test]
    fn web_search_custom_rule() {
        let r = validate_permission_rule("WebSearch(claude ai)", None);
        assert!(r.valid, "{:?}", r);
        let r = validate_permission_rule("WebSearch(claude*)", None);
        assert!(!r.valid);
    }

    #[test]
    fn schema_validate_message() {
        let err = permission_rule_schema_validate("Bash(npm run:* extra)").unwrap_err();
        assert!(err.starts_with("The :* pattern must be at the end"));
    }
}
