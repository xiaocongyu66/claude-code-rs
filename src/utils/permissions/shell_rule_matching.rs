//! shell_rule_matching.rs — shell 工具的权限规则匹配工具集。
//!
//! 重写自 `src/utils/permissions/shellRuleMatching.ts`。提取 shell 工具
//! 的公共逻辑：规则解析（exact / prefix / wildcard）、命令匹配、权限
//! 建议生成。
//!
//! 语义对齐说明（重要——Rust 无 regex 依赖）：
//! TS 的 `matchWildcardPattern` 通过构造 RegExp 实现通配匹配：
//! 1. `pattern.trim()`；
//! 2. 转义序列处理：`\*` → 字面量 `*`，`\\` → 字面量 `\`（TS 用
//!    `\x00ESCAPED_*\x00` 占位符暂时替换，最后再放回转义后的 regex 字面量）；
//! 3. 转义其余 regex 特殊字符；
//! 4. 剩余未转义 `*` → `.*`；
//! 5. 结尾的 ` *`（空格+唯一未转义通配）改为 `( .*)?`，使 `git *` 同时
//!    匹配 `git add` 与裸 `git`（与 `git:*` 前缀规则语义对齐）；
//! 6. `s`（dotAll）标志使 `.` 匹配换行；可选 `i` 忽略大小写。
//!
//! Rust 侧把模式编译为 `Token` 序列（`Lit` / `Any` /
//! `OptionalSpaceAny`）后用回溯匹配器实现等价语义——`.`（dotAll）对应
//! `Any` 匹配任意字符（含换行），`i` 对应字符的小写比较。占位符技巧
//! 在 token 方案下不需要：转义序列直接产出字面量 token。

use crate::types::permissions::{PermissionUpdate, PermissionUpdateDestination};

/// 解析后的权限规则判别联合（TS `ShellPermissionRule`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPermissionRule {
    /// 精确匹配
    Exact { command: String },
    ///  legacy `:*` 前缀语法
    Prefix { prefix: String },
    /// 通配符模式
    Wildcard { pattern: String },
}

/// 从 legacy `:*` 语法提取前缀（如 `"npm:*"` → `"npm"`），保持向后兼容。
///
/// 对齐 TS 正则 `/^(.+):\*$/`：字符串必须以 `:*` 结尾且至少有一个前导
/// 字符。
pub fn permission_rule_extract_prefix(permission_rule: &str) -> Option<String> {
    let rule: Vec<char> = permission_rule.chars().collect();
    if rule.len() > 2 && rule[rule.len() - 2..] == [':', '*'] {
        Some(rule[..rule.len() - 2].iter().collect())
    } else {
        None
    }
}

/// 模式是否含未转义通配符（`*` 未被 `\` 转义、且不是结尾的 legacy `:*`）。
///
/// 对齐 TS：`*` 前若跟着奇数个反斜杠则视为转义；偶数（含 0）则为通配符。
pub fn has_wildcards(pattern: &str) -> bool {
    // If it ends with :*, it's legacy prefix syntax, not wildcard
    if pattern.ends_with(":*") {
        return false;
    }
    let chars: Vec<char> = pattern.chars().collect();
    // Check for unescaped * anywhere in the pattern
    for i in 0..chars.len() {
        if chars[i] == '*' {
            // Count backslashes before this asterisk
            let mut backslash_count = 0u32;
            let mut j = i as isize - 1;
            while j >= 0 && chars[j as usize] == '\\' {
                backslash_count += 1;
                j -= 1;
            }
            // If even number of backslashes (including 0), the asterisk is unescaped
            if backslash_count % 2 == 0 {
                return true;
            }
        }
    }
    false
}

/// 编译后的模式 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// 字面量字符
    Lit(char),
    /// 未转义 `*`：匹配任意长度任意字符（含换行）
    Any,
    /// 结尾的 `( ' '* )?` 可选组：`git *` 同时匹配裸 `git`
    OptionalSpaceAny,
}

/// 通配符模式匹配。
///
/// - `*` 匹配任意字符序列（含换行，等价 TS 的 `s` 标志）
/// - `\*` 匹配字面量星号；`\\` 匹配字面量反斜杠
/// - 结尾 ` *`（唯一的未转义通配）为可选组，见模块头说明
/// - `case_insensitive` 对应 TS 的 `i` 标志
pub fn match_wildcard_pattern(pattern: &str, command: &str, case_insensitive: bool) -> bool {
    // Trim leading/trailing whitespace from pattern
    let trimmed = pattern.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    let mut tokens: Vec<Token> = Vec::with_capacity(chars.len());

    // Process the pattern to handle escape sequences: \* and \\
    let mut i = 0usize;
    let mut unescaped_star_count = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // Handle escape sequences
        if c == '\\' && i + 1 < chars.len() {
            let next_char = chars[i + 1];
            if next_char == '*' {
                // \* -> literal asterisk
                tokens.push(Token::Lit('*'));
                i += 2;
                continue;
            } else if next_char == '\\' {
                // \\ -> literal backslash
                tokens.push(Token::Lit('\\'));
                i += 2;
                continue;
            }
        }
        if c == '*' {
            // unescaped * -> wildcard
            tokens.push(Token::Any);
            unescaped_star_count += 1;
            i += 1;
            continue;
        }
        tokens.push(Token::Lit(c));
        i += 1;
    }

    // When a pattern ends with ' *' (space + unescaped wildcard) AND the
    // trailing wildcard is the ONLY unescaped wildcard, make the trailing
    // space-and-args optional so 'git *' matches both 'git add' and bare 'git'.
    // (对应 TS 的 regexPattern.endsWith(' .*') && unescapedStarCount === 1)
    let len = tokens.len();
    if len >= 2
        && matches!(tokens[len - 1], Token::Any)
        && matches!(tokens[len - 2], Token::Lit(' '))
        && unescaped_star_count == 1
    {
        tokens.pop();
        tokens.pop();
        tokens.push(Token::OptionalSpaceAny);
    }

    let command_chars: Vec<char> = command.chars().collect();
    match_tokens(&tokens, &command_chars, case_insensitive)
}

/// 回溯匹配器：token 序列对命令字符序列做完整匹配（等价 TS 的
/// `^pattern$` 锚定）。
fn match_tokens(tokens: &[Token], s: &[char], case_insensitive: bool) -> bool {
    if tokens.is_empty() {
        return s.is_empty();
    }
    match tokens[0] {
        Token::Lit(c) => {
            // '.' 在 dotAll 模式下匹配任意字符；Lit 必须精确（按大小写选项）匹配
            if s.is_empty() || !char_eq(c, s[0], case_insensitive) {
                return false;
            }
            match_tokens(&tokens[1..], &s[1..], case_insensitive)
        }
        Token::Any => {
            // .* 贪心回溯：尝试消费 0..=s.len() 个字符
            for k in 0..=s.len() {
                if match_tokens(&tokens[1..], &s[k..], case_insensitive) {
                    return true;
                }
            }
            false
        }
        Token::OptionalSpaceAny => {
            // ( ' '* )?：不匹配任何字符，或匹配一个空格后交由剩余 token
            if match_tokens(&tokens[1..], s, case_insensitive) {
                return true;
            }
            if !s.is_empty() && char_eq(' ', s[0], case_insensitive) {
                return match_tokens(&tokens[1..], &s[1..], case_insensitive);
            }
            false
        }
    }
}

#[inline]
fn char_eq(a: char, b: char, case_insensitive: bool) -> bool {
    if !case_insensitive {
        return a == b;
    }
    // 等价 regex 'i' 标志的字符大小写折叠比较
    let mut a_lower = a.to_lowercase();
    let mut b_lower = b.to_lowercase();
    a_lower.next() == b_lower.next() && a_lower.next().is_none() && b_lower.next().is_none()
}

/// 把权限规则字符串解析为结构化规则。
pub fn parse_permission_rule(permission_rule: &str) -> ShellPermissionRule {
    // Check for legacy :* prefix syntax first (backwards compatibility)
    if let Some(prefix) = permission_rule_extract_prefix(permission_rule) {
        return ShellPermissionRule::Prefix { prefix };
    }
    // Check for new wildcard syntax (contains * but not :* at end)
    if has_wildcards(permission_rule) {
        return ShellPermissionRule::Wildcard {
            pattern: permission_rule.to_string(),
        };
    }
    // Otherwise, it's an exact match
    ShellPermissionRule::Exact {
        command: permission_rule.to_string(),
    }
}

/// 为精确命令匹配生成权限更新建议。
pub fn suggestion_for_exact_command(tool_name: &str, command: &str) -> Vec<PermissionUpdate> {
    vec![PermissionUpdate::AddRules {
        rules: vec![crate::types::permissions::PermissionRuleValue {
            tool_name: tool_name.to_string(),
            rule_content: Some(command.to_string()),
        }],
        behavior: crate::types::permissions::PermissionBehavior::Allow,
        destination: PermissionUpdateDestination::LocalSettings,
    }]
}

/// 为前缀匹配生成权限更新建议。
pub fn suggestion_for_prefix(tool_name: &str, prefix: &str) -> Vec<PermissionUpdate> {
    vec![PermissionUpdate::AddRules {
        rules: vec![crate::types::permissions::PermissionRuleValue {
            tool_name: tool_name.to_string(),
            rule_content: Some(format!("{prefix}:*")),
        }],
        behavior: crate::types::permissions::PermissionBehavior::Allow,
        destination: PermissionUpdateDestination::LocalSettings,
    }]
}
