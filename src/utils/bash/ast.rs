//! ast.rs — 基于 AST 的 bash 命令安全分析。
//!
//! 重写自 `src/utils/bash/ast.ts`。设计属性保持 **fail-closed**：任何未显式
//! allowlist 的节点类型都整条命令判为 'too-complex'，交由人工确认。
//!
//! 语义对齐：
//! - TS 的 tree-sitter WASM/NAPI 节点 → `bash_parser::TsNode`（纯 Rust 兼容
//!   实现）；节点类型、startIndex/endIndex（UTF-8 字节偏移）、children 结构
//!   逐条对齐。
//! - TS 的 `PARSE_ABORTED` Symbol 由入参 `ParseRoot::Aborted` 表达。
//! - TS 的 SHELL_KEYWORDS 复用 `bash_parser::is_shell_keyword`。
//! - TS 正则中的断言（`(?=)`）与反引用在 Rust 手工扫描实现。
//! - TS 的异步路径（parseForSecurity）为同步（解析器无 WASM init）。

use std::collections::HashMap;

use crate::utils::bash::bash_parser::{is_shell_keyword, TsNode};
use crate::utils::bash::parser::{parse_command_raw, RawParseResult};

/// 重定向算符（对齐 TS Redirect['op']）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    Gt,
    GtGt,
    Lt,
    LtLt,
    GtAmp,
    GtPipe,
    LtAmp,
    AmpGt,
    AmpGtGt,
    LtLtLt,
}

impl RedirectOp {
    pub fn as_str(self) -> &'static str {
        match self {
            RedirectOp::Gt => ">",
            RedirectOp::GtGt => ">>",
            RedirectOp::Lt => "<",
            RedirectOp::LtLt => "<<",
            RedirectOp::GtAmp => ">&",
            RedirectOp::GtPipe => ">|",
            RedirectOp::LtAmp => "<&",
            RedirectOp::AmpGt => "&>",
            RedirectOp::AmpGtGt => "&>>",
            RedirectOp::LtLtLt => "<<<",
        }
    }
}

/// 重定向（对齐 TS Redirect）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub op: RedirectOp,
    pub target: String,
    pub fd: Option<i64>,
}

/// env 赋值（SimpleCommand.envVars 元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// 简单命令（对齐 TS SimpleCommand）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// argv[0] 为命令名，其余为已解析引号的参数。
    pub argv: Vec<String>,
    /// 前导 VAR=val 赋值。
    pub env_vars: Vec<EnvVar>,
    /// 输入/输出重定向。
    pub redirects: Vec<Redirect>,
    /// 该命令的原始源片段（UI 展示用）。
    pub text: String,
}

/// `parseForSecurity` 结果（对齐 TS ParseForSecurityResult）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseForSecurityResult {
    Simple { commands: Vec<SimpleCommand> },
    TooComplex {
        reason: String,
        node_type: Option<String>,
    },
    ParseUnavailable,
}

/// `parseForSecurityFromAst` 的 AST 入参（对齐 TS 的 `Node | typeof PARSE_ABORTED`）。
#[derive(Debug)]
pub enum ParseRoot<'a> {
    Node(&'a TsNode),
    Aborted,
}

/// `checkSemantics` 结果（对齐 TS SemanticCheckResult）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCheckResult {
    pub ok: bool,
    pub reason: Option<String>,
}

impl SemanticCheckResult {
    fn ok_() -> Self {
        SemanticCheckResult {
            ok: true,
            reason: None,
        }
    }

    fn fail(reason: &str) -> Self {
        SemanticCheckResult {
            ok: false,
            reason: Some(reason.to_string()),
        }
    }
}

/// 变量赋值中间结果（walkVariableAssignment）。
#[derive(Debug, Clone)]
struct VarAssignment {
    name: String,
    value: String,
    is_append: bool,
}

const CMDSUB_PLACEHOLDER: &str = "__CMDSUB_OUTPUT__";
const VAR_PLACEHOLDER: &str = "__TRACKED_VAR__";

const STRUCTURAL_TYPES: &[&str] = &["program", "list", "pipeline", "redirected_statement"];

const SEPARATOR_TYPES: &[&str] = &["&&", "||", ";", ";;", "&", "|&", "\n"];

/// 已知安全 env 变量（bash/OS 设定，值非任意用户输入）。
const SAFE_ENV_VARS: &[&str] = &[
    "HOME",
    "PWD",
    "OLDPWD",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "HOSTNAME",
    "UID",
    "EUID",
    "PPID",
    "RANDOM",
    "SECONDS",
    "LINENO",
    "TMPDIR",
    "BASH_VERSION",
    "BASHPID",
    "SHLVL",
    "HISTFILE",
    "IFS",
];

/// 特殊 shell 变量（$? $$ $! $# $0 $-）。注意：'@' 与 '*' 故意不在内。
const SPECIAL_VAR_NAMES: &[&str] = &["?", "$", "!", "#", "0", "-"];

/// "不可静态分析"节点类型清单（analytics 用；真正安全属性靠 allowlist）。
const DANGEROUS_TYPES: &[&str] = &[
    "command_substitution",
    "process_substitution",
    "expansion",
    "simple_expansion",
    "brace_expression",
    "subshell",
    "compound_statement",
    "for_statement",
    "while_statement",
    "until_statement",
    "if_statement",
    "case_statement",
    "function_definition",
    "test_command",
    "ansi_c_string",
    "translated_string",
    "herestring_redirect",
    "heredoc_redirect",
];

/// zsh 模块内置命令（zmodload 加载，非 PATH 上的二进制）。
const ZSH_DANGEROUS_BUILTINS: &[&str] = &[
    "zmodload",
    "emulate",
    "sysopen",
    "sysread",
    "syswrite",
    "sysseek",
    "zpty",
    "ztcp",
    "zsocket",
    "zf_rm",
    "zf_mv",
    "zf_ln",
    "zf_chmod",
    "zf_chown",
    "zf_mkdir",
    "zf_rmdir",
    "zf_chgrp",
];

/// 把参数当 shell 代码执行的内置命令。
const EVAL_LIKE_BUILTINS: &[&str] = &[
    "eval",
    "source",
    ".",
    "exec",
    "command",
    "builtin",
    "fc",
    "coproc",
    "noglob",
    "nocorrect",
    "trap",
    "enable",
    "mapfile",
    "readarray",
    "hash",
    "bind",
    "complete",
    "compgen",
    "alias",
    "let",
];

/// 内部重解析 NAME 操作数并算术求值 `arr[EXPR]` 下标的内置命令。
fn subscript_eval_flags(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "test" | "[" | "[[" => Some(&["-v", "-R"]),
        "printf" => Some(&["-v"]),
        "read" => Some(&["-a"]),
        "unset" => Some(&["-v"]),
        "wait" => Some(&["-p"]),
        _ => None,
    }
}

const TEST_ARITH_CMP_OPS: &[&str] = &["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];

/// 每个非 flag 位置参数都是 NAME（会被算术求值下标）的内置命令。
const BARE_SUBSCRIPT_NAME_BUILTINS: &[&str] = &["read", "unset"];

/// `read` 中后随数据（而非 NAME）的 flag。
const READ_DATA_FLAGS: &[&str] = &["-p", "-d", "-n", "-N", "-t", "-u", "-i"];

// ────────────────────────────────────────────────────────────────────────────
// 小工具：字符级扫描（替代 JS 正则中的断言/反引用场景）
// ────────────────────────────────────────────────────────────────────────────

fn contains_any_placeholder(value: &str) -> bool {
    value.contains(CMDSUB_PLACEHOLDER) || value.contains(VAR_PLACEHOLDER)
}

/// 裸 `$VAR` 的不安全字符：IFS（空格/tab/换行）与 glob 字符（* ? [）。
fn is_bare_var_unsafe(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '*' | '?' | '[')
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

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 控制字符（0x00-0x08、0x0B-0x1F、0x7F）。
fn has_control_char(s: &str) -> bool {
    s.bytes()
        .any(|b| b <= 0x08 || (0x0B..=0x1F).contains(&b) || b == 0x7F)
}

/// Unicode 隐形空白（NBSP、零宽空格、行/段分隔符、BOM 等）。
fn has_unicode_whitespace(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

/// `\` 紧邻空白（`\\[ \t]`），或非空白字符后的 `\<LF>` 行连接。
fn has_backslash_whitespace(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] == '\\' {
            if i + 1 < len && (chars[i + 1] == ' ' || chars[i + 1] == '\t') {
                return true;
            }
        }
        if i + 2 < len
            && chars[i] != ' '
            && chars[i] != '\t'
            && chars[i] != '\n'
            && chars[i] != '\\'
            && chars[i + 1] == '\\'
            && chars[i + 2] == '\n'
        {
            return true;
        }
    }
    false
}

fn has_zsh_tilde_bracket(s: &str) -> bool {
    s.contains("~[")
}

/// 词首 `=cmd`（zsh EQUALS 展开）。
fn has_zsh_equals_expansion(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] != '=' {
            continue;
        }
        let prev_ok = i == 0
            || is_js_whitespace(chars[i - 1])
            || matches!(chars[i - 1], ';' | '&' | '|');
        if !prev_ok {
            continue;
        }
        if i + 1 < len && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_') {
            return true;
        }
    }
    false
}

/// 花括号展开模式：`{a,b}` 或 `{a..b}`（花括号内须有 , 或 ..）。
fn is_brace_expansion(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] != '{' {
            continue;
        }
        let mut j = i + 1;
        while j < len
            && chars[j] != '{'
            && chars[j] != '}'
            && !is_js_whitespace(chars[j])
        {
            j += 1;
        }
        let comma = j < len && chars[j] == ',';
        let dots = j + 1 < len && chars[j] == '.' && chars[j + 1] == '.';
        if !(comma || dots) {
            continue;
        }
        j += if dots { 2 } else { 1 };
        while j < len
            && chars[j] != '{'
            && chars[j] != '}'
            && !is_js_whitespace(chars[j])
        {
            j += 1;
        }
        if j < len && chars[j] == '}' {
            return true;
        }
    }
    false
}

/// `{` 与引号字符的组合（花括号展开混淆检测）。运行于已掩码的命令。
fn has_brace_with_quote(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] != '{' {
            continue;
        }
        let mut j = i + 1;
        while j < len && chars[j] != '}' {
            if chars[j] == '\'' || chars[j] == '"' {
                return true;
            }
            j += 1;
        }
    }
    false
}

/// 掩码单引号/双引号上下文中的 `{`（bash 感知的单趟扫描，对齐 TS）。
fn mask_brace_in_quoted_contexts(cmd: &str) -> String {
    if !cmd.contains('{') {
        return cmd.to_string();
    }
    let chars: Vec<char> = cmd.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(cmd.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;
    while i < len {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            out.push(if c == '{' { ' ' } else { c });
            i += 1;
        } else if in_double {
            if c == '\\' && i + 1 < len && (chars[i + 1] == '"' || chars[i + 1] == '\\') {
                out.push(c);
                out.push(chars[i + 1]);
                i += 2;
            } else {
                if c == '"' {
                    in_double = false;
                }
                out.push(if c == '{' { ' ' } else { c });
                i += 1;
            }
        } else if c == '\\' && i + 1 < len {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
        } else {
            if c == '\'' {
                in_single = true;
            } else if c == '"' {
                in_double = true;
            }
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `/proc/*/environ`（JS `.` 不跨行：/proc/ 与 /environ 须同行）。
fn is_proc_environ(s: &str) -> bool {
    for line in s.split('\n') {
        if let Some(p) = line.find("/proc/") {
            if line[p + "/proc/".len()..].contains("/environ") {
                return true;
            }
        }
    }
    false
}

/// 换行后随 `#`（注释注入检测）。
fn has_newline_hash(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0usize;
    while i < len {
        if chars[i] == '\n' {
            let mut j = i + 1;
            while j < len && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if j < len && chars[j] == '#' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `\bsystem\s*\(`——jq system() 调用检测。
fn has_jq_system_call(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let word: Vec<char> = "system".chars().collect();
    let mut i = 0usize;
    while i + word.len() <= len {
        if chars[i..i + word.len()] == word[..] {
            let prev_ok = i == 0 || !is_word_char(chars[i - 1]);
            let mut j = i + word.len();
            while j < len && is_js_whitespace(chars[j]) {
                j += 1;
            }
            if prev_ok && j < len && chars[j] == '(' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// jq 危险 flag：`-f`/`-L`（含 `--from-file` 等长形式）。
fn is_jq_dangerous_flag(arg: &str) -> bool {
    if let Some(rest) = arg.strip_prefix("--") {
        for p in ["from-file", "rawfile", "slurpfile", "library-path"] {
            if let Some(r) = rest.strip_prefix(p) {
                return r.is_empty() || r.starts_with('=');
            }
        }
        false
    } else if let Some(rest) = arg.strip_prefix('-') {
        let mut chars = rest.chars();
        match chars.next() {
            Some(c) if c == 'f' || c == 'L' => match chars.next() {
                None => true,
                Some(c2) => !c2.is_ascii_alphabetic(),
            },
            _ => false,
        }
    } else {
        false
    }
}

/// `^-[a-zA-Z]*[niaA]`（前缀匹配：declare 危险 flag）。
fn matches_declare_flag_re(arg: &str) -> bool {
    let chars: Vec<char> = arg.chars().collect();
    if chars.len() < 2 || chars[0] != '-' {
        return false;
    }
    for k in 1..chars.len() {
        if matches!(chars[k], 'n' | 'i' | 'a' | 'A') {
            return chars[1..k].iter().all(|c| c.is_ascii_alphabetic());
        }
        if !chars[k].is_ascii_alphabetic() {
            return false;
        }
    }
    false
}

/// `^[^=]*\[`（declare 位置参数含数组下标）。
fn has_subscript_before_eq(arg: &str) -> bool {
    for c in arg.chars() {
        if c == '=' {
            return false;
        }
        if c == '[' {
            return true;
        }
    }
    false
}

/// 合法 bash 变量名：`[A-Za-z_][A-Za-z0-9_]*`。
fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 移除 `${VAR}` 引用（PS4 白名单检查用）。
fn remove_var_refs(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < len {
        if chars[i] == '$' && i + 1 < len && chars[i + 1] == '{' {
            let mut j = i + 2;
            if j < len && (chars[j].is_ascii_alphabetic() || chars[j] == '_') {
                while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < len && chars[j] == '}' {
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// PS4 值字符白名单（`${VAR}` 引用移除后）。
fn ps4_value_allowed(value: &str) -> bool {
    remove_var_refs(value)
        .chars()
        .all(|c| {
            matches!(
                c,
                'a'..='z'
                    | 'A'..='Z'
                    | '0'..='9'
                    | ' '
                    | '_'
                    | '+'
                    | ':'
                    | '.'
                    | '/'
                    | '='
                    | '['
                    | '\\'
                    | ']'
                    | '-'
            )
        })
}

/// `\$\(\(` 或 `\)\)` 之外的算术叶子：数字 / hex / base#digits / 算符串。
fn is_arith_leaf(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s == "$((" {
        return true;
    }
    if s.starts_with("0x") || s.starts_with("0X") {
        return s.len() > 2 && s[2..].chars().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(idx) = s.find('#') {
        if idx > 0 {
            let (a, b) = s.split_at(idx);
            let b = &b[1..];
            return a.chars().all(|c| c.is_ascii_digit())
                && !b.is_empty()
                && b.chars().all(|c| c.is_ascii_alphanumeric());
        }
        return false;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    s.chars().all(|c| {
        matches!(
            c,
            '-' | '+'
                | '*'
                | '/'
                | '%'
                | '^'
                | '&'
                | '|'
                | '~'
                | '!'
                | '<'
                | '>'
                | '='
                | '?'
                | ':'
                | '('
                | ')'
                | ','
        )
    })
}

/// 移除 bash 引号去除规则中的反斜杠转义（`\X` → `X`）。
/// JS `\\(.)/g` 的 `.` 不含换行符。
fn unescape_backslash(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < len {
        if chars[i] == '\\'
            && i + 1 < len
            && !matches!(chars[i + 1], '\n' | '\r' | '\u{2028}' | '\u{2029}')
        {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 双引号内容里的反斜杠仅脱义 `$ ` " \，其余保留。
fn unescape_string_content(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < len {
        if chars[i] == '\\'
            && i + 1 < len
            && matches!(chars[i + 1], '$' | '`' | '"' | '\\')
        {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// walkCommand 重建 .text 时的需要引号的字符类。
fn needs_rebuild_quote(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '\\'
            | ' '
            | '\t'
            | '\n'
            | '$'
            | '`'
            | ';'
            | '|'
            | '&'
            | '<'
            | '>'
            | '('
            | ')'
            | '{'
            | '}'
            | '*'
            | '?'
            | '['
            | ']'
            | '~'
            | '#'
    )
}

fn has_dollar_ident(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 0..len {
        if chars[i] == '$'
            && i + 1 < len
            && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_')
        {
            return true;
        }
    }
    false
}

fn strip_raw_string(text: &str) -> String {
    // TS text.slice(1, -1)：长度 < 2 的输入返回空串。
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 2 {
        return String::new();
    }
    chars[1..chars.len() - 1].iter().collect()
}

fn too_complex(node: &TsNode) -> ParseForSecurityResult {
    let reason = if node.node_type == "ERROR" {
        "Parse error".to_string()
    } else if DANGEROUS_TYPES.contains(&node.node_type.as_str()) {
        format!("Contains {}", node.node_type)
    } else {
        format!("Unhandled node type: {}", node.node_type)
    };
    ParseForSecurityResult::TooComplex {
        reason,
        node_type: Some(node.node_type.clone()),
    }
}

fn too_complex_reason(reason: &str, node_type: &str) -> ParseForSecurityResult {
    let node_type = if node_type.is_empty() {
        None
    } else {
        Some(node_type.to_string())
    };
    ParseForSecurityResult::TooComplex {
        reason: reason.to_string(),
        node_type,
    }
}

/// 节点 → 重定向算符（REDIRECT_OPS 映射）。
fn redirect_op(node_type: &str) -> Option<RedirectOp> {
    match node_type {
        ">" => Some(RedirectOp::Gt),
        ">>" => Some(RedirectOp::GtGt),
        "<" => Some(RedirectOp::Lt),
        "<<" => Some(RedirectOp::LtLt),
        ">&" => Some(RedirectOp::GtAmp),
        "<&" => Some(RedirectOp::LtAmp),
        ">|" => Some(RedirectOp::GtPipe),
        "&>" => Some(RedirectOp::AmpGt),
        "&>>" => Some(RedirectOp::AmpGtGt),
        "<<<" => Some(RedirectOp::LtLtLt),
        _ => None,
    }
}

/// analytics 用节点类型 ID（0=未知，-1=ERROR，-2=缺省，其余=清单序+1）。
pub fn node_type_id(node_type: Option<&str>) -> i64 {
    let node_type = match node_type {
        Some(t) => t,
        None => return -2,
    };
    if node_type == "ERROR" {
        return -1;
    }
    match DANGEROUS_TYPES.iter().position(|t| *t == node_type) {
        Some(i) => i as i64 + 1,
        None => 0,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 入口
// ────────────────────────────────────────────────────────────────────────────

/// 解析 bash 命令并提取平铺的简单命令列表。
/// 不可静态分析 → 'too-complex'；解析器不可用 → 'parse-unavailable'。
pub fn parse_for_security(cmd: &str) -> ParseForSecurityResult {
    // parseCommandRaw('') 返回 null（假值短路），在此显式处理；
    // 不用 trim()——Unicode 空白会被后续 pre-check 检测并拒绝。
    if cmd.is_empty() {
        return ParseForSecurityResult::Simple { commands: vec![] };
    }
    match parse_command_raw(cmd) {
        RawParseResult::Unavailable => ParseForSecurityResult::ParseUnavailable,
        RawParseResult::Node(root) => {
            parse_for_security_from_ast(cmd, ParseRoot::Node(&root))
        }
        RawParseResult::Aborted => parse_for_security_from_ast(cmd, ParseRoot::Aborted),
    }
}

/// 同 parse_for_security，但接受预解析的 AST 根（调用方可复用解析树）。
/// pre-check 仍对 `cmd` 运行——它们捕获 tree-sitter/bash 之间的已知差异。
pub fn parse_for_security_from_ast(cmd: &str, root: ParseRoot) -> ParseForSecurityResult {
    // Pre-checks：导致 tree-sitter 与 bash 词法分歧的字符。
    if has_control_char(cmd) {
        return too_complex_reason("Contains control characters", "");
    }
    if has_unicode_whitespace(cmd) {
        return too_complex_reason("Contains Unicode whitespace", "");
    }
    if has_backslash_whitespace(cmd) {
        return too_complex_reason("Contains backslash-escaped whitespace", "");
    }
    if has_zsh_tilde_bracket(cmd) {
        return too_complex_reason("Contains zsh ~[ dynamic directory syntax", "");
    }
    if has_zsh_equals_expansion(cmd) {
        return too_complex_reason("Contains zsh =cmd equals expansion", "");
    }
    if has_brace_with_quote(&mask_brace_in_quoted_contexts(cmd)) {
        return too_complex_reason(
            "Contains brace with quote character (expansion obfuscation)",
            "",
        );
    }

    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return ParseForSecurityResult::Simple { commands: vec![] };
    }

    let root_node = match root {
        ParseRoot::Aborted => {
            // SECURITY：模块已加载但解析中止（超时/节点预算/panic），
            // 对抗性输入可触发。绝不可与 unavailable 混淆路由 legacy。
            return too_complex_reason(
                "Parser aborted (timeout or resource limit) — possible adversarial input",
                "PARSE_ABORT",
            );
        }
        ParseRoot::Node(n) => n,
    };

    walk_program(root_node)
}

fn walk_program(root: &TsNode) -> ParseForSecurityResult {
    let mut commands: Vec<SimpleCommand> = Vec::new();
    // 记录同一命令内先前赋值的变量：后续 $VAR 引用可替换占位符
    // （如 NOW=$(date) && jq --arg now "$NOW"）。
    let mut var_scope: HashMap<String, String> = HashMap::new();
    if let Some(err) = collect_commands(root, &mut commands, &mut var_scope) {
        return err;
    }
    ParseForSecurityResult::Simple { commands }
}

type Scope = HashMap<String, String>;

/// 从结构性包装节点递归收集叶子 `command` 节点。
/// 返回 Some(err) 表示遇到不允许的节点类型；None 表示成功。
fn collect_commands(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    let node_type = node.node_type.as_str();

    if node_type == "command" {
        // commands 作为 innerCommands 累加器：$() 展开的内部命令一并收集。
        match walk_command(node, &[], commands, var_scope) {
            Ok(cmd) => {
                commands.push(cmd);
                None
            }
            Err(e) => Some(e),
        }
    } else if node_type == "redirected_statement" {
        walk_redirected_statement(node, commands, var_scope)
    } else if node_type == "comment" {
        None
    } else if STRUCTURAL_TYPES.contains(&node_type) {
        // `||` / `|` / `|&` / `&` 后的子句共享快照而非线性 scope
        // （RHS 可能不执行 / 管道各级在子 shell 内）。
        let is_pipeline = node_type == "pipeline";
        let needs_snapshot = !is_pipeline
            && node
                .children
                .iter()
                .any(|c| c.node_type == "||" || c.node_type == "&");
        let snapshot: Option<Scope> = if needs_snapshot {
            Some(var_scope.clone())
        } else {
            None
        };
        // pipeline 全级在子 shell 内：起始即拷贝，不污染调用方 scope。
        let mut local_scope: Option<Scope> = if is_pipeline {
            Some(var_scope.clone())
        } else {
            None
        };
        for child in &node.children {
            let child_type = child.node_type.as_str();
            if SEPARATOR_TYPES.contains(&child_type) {
                if matches!(child_type, "||" | "|" | "|&" | "&") {
                    local_scope = Some(match &snapshot {
                        Some(s) => s.clone(),
                        None => var_scope.clone(),
                    });
                }
                continue;
            }
            let scope: &mut Scope = match local_scope.as_mut() {
                Some(s) => s,
                None => var_scope,
            };
            if let Some(err) = collect_commands(child, commands, scope) {
                return Some(err);
            }
        }
        None
    } else if node_type == "negated_command" {
        // `! cmd` 只反转退出码。递归进被包装的命令。
        for child in &node.children {
            if child.node_type == "!" {
                continue;
            }
            return collect_commands(child, commands, var_scope);
        }
        None
    } else if node_type == "declaration_command" {
        // export/local/readonly/declare/typeset。
        let mut argv: Vec<String> = Vec::new();
        for child in &node.children {
            let ctype = child.node_type.as_str();
            match ctype {
                "export" | "local" | "readonly" | "declare" | "typeset" => {
                    argv.push(child.text.clone());
                }
                "word" | "number" | "raw_string" | "string" | "concatenation" => {
                    let arg = match walk_argument(child, commands, var_scope) {
                        Ok(a) => a,
                        Err(e) => return Some(e),
                    };
                    // -n（nameref）/-i（整数）/-a/-A（数组）改变赋词语义。
                    if matches!(argv.first().map(String::as_str), Some("declare" | "typeset" | "local"))
                        && matches_declare_flag_re(&arg)
                    {
                        return Some(too_complex_reason(
                            &format!(
                                "declare flag {arg} changes assignment semantics (nameref/integer/array)"
                            ),
                            "declaration_command",
                        ));
                    }
                    // 无 flag 的位置参数带下标也会算术求值。
                    if matches!(argv.first().map(String::as_str), Some("declare" | "typeset" | "local"))
                        && !arg.starts_with('-')
                        && has_subscript_before_eq(&arg)
                    {
                        return Some(too_complex_reason(
                            &format!(
                                "declare positional '{arg}' contains array subscript — bash evaluates $(cmd) in subscripts"
                            ),
                            "declaration_command",
                        ));
                    }
                    argv.push(arg);
                }
                "variable_assignment" => {
                    match walk_variable_assignment(child, commands, var_scope) {
                        Ok(ev) => {
                            apply_var_to_scope(var_scope, &ev);
                            argv.push(format!("{}={}", ev.name, ev.value));
                        }
                        Err(e) => return Some(e),
                    }
                }
                "variable_name" => {
                    // `export FOO`：裸名，无赋值。
                    argv.push(child.text.clone());
                }
                _ => return Some(too_complex(child)),
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: vec![],
            redirects: vec![],
            text: node.text.clone(),
        });
        None
    } else if node_type == "variable_assignment" {
        // 语句级裸 `VAR=value`（非命令 env 前缀）：不执行代码，
        // 值经 walkVariableAssignment 校验。不 push 命令（惰性）。
        match walk_variable_assignment(node, commands, var_scope) {
            Ok(ev) => {
                apply_var_to_scope(var_scope, &ev);
                None
            }
            Err(e) => Some(e),
        }
    } else if node_type == "for_statement" {
        collect_for_statement(node, commands, var_scope)
    } else if node_type == "if_statement" || node_type == "while_statement" {
        collect_if_or_while_statement(node, commands, var_scope)
    } else if node_type == "subshell" {
        // `(cmd1; cmd2)`：内部命令执行，scope 隔离（拷贝）。
        let mut inner_scope = var_scope.clone();
        for child in &node.children {
            if child.node_type == "(" || child.node_type == ")" {
                continue;
            }
            if let Some(err) = collect_commands(child, commands, &mut inner_scope) {
                return Some(err);
            }
        }
        None
    } else if node_type == "test_command" {
        // `[[ EXPR ]]` / `[ EXPR ]`：合成 argv[0]='[['。
        let mut argv: Vec<String> = vec!["[[".to_string()];
        for child in &node.children {
            if matches!(child.node_type.as_str(), "[[" | "]]" | "[" | "]") {
                continue;
            }
            if let Some(err) = walk_test_expr(child, &mut argv, commands, var_scope) {
                return Some(err);
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: vec![],
            redirects: vec![],
            text: node.text.clone(),
        });
        None
    } else if node_type == "unset_command" {
        // `unset FOO BAR` / `unset -f func`：移除 shell 变量/函数。
        let mut argv: Vec<String> = Vec::new();
        for child in &node.children {
            match child.node_type.as_str() {
                "unset" => argv.push(child.text.clone()),
                "variable_name" => {
                    argv.push(child.text.clone());
                    // 同步从 varScope 移除：`VAR=safe && unset VAR && rm $VAR`
                    // 必须不解析 $VAR。
                    var_scope.remove(&child.text);
                }
                "word" => {
                    match walk_argument(child, commands, var_scope) {
                        Ok(arg) => argv.push(arg),
                        Err(e) => return Some(e),
                    }
                }
                _ => return Some(too_complex(child)),
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: vec![],
            redirects: vec![],
            text: node.text.clone(),
        });
        None
    } else {
        Some(too_complex(node))
    }
}

/// collect_commands 的 for_statement 分支。
fn collect_for_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    let mut loop_var: Option<String> = None;
    let mut do_group: Option<&TsNode> = None;
    for child in &node.children {
        let ctype = child.node_type.as_str();
        if ctype == "variable_name" {
            loop_var = Some(child.text.clone());
        } else if ctype == "do_group" {
            do_group = Some(child);
        } else if matches!(ctype, "for" | "in" | "select" | ";") {
            // 结构 token
        } else if ctype == "command_substitution" {
            // `for i in $(seq 1 3)`：内部命令同样抽取校验。
            if let Some(err) = collect_command_substitution(child, commands, var_scope) {
                return Some(err);
            }
        } else {
            // 迭代值：经 walkArgument 校验（值本身丢弃，循环变量恒为占位符）。
            match walk_argument(child, commands, var_scope) {
                Ok(_) => {}
                Err(e) => return Some(e),
            }
        }
    }
    let loop_var = match loop_var {
        Some(v) => v,
        None => return Some(too_complex(node)),
    };
    let do_group = match do_group {
        Some(g) => g,
        None => return Some(too_complex(node)),
    };
    // SECURITY：`for PS4 in '$(id)'` 经 varScope.set 绕过 PS4/IFS 校验。
    if loop_var == "PS4" || loop_var == "IFS" {
        return Some(too_complex_reason(
            &format!("{loop_var} as loop variable bypasses assignment validation"),
            "for_statement",
        ));
    }
    // 循环变量恒为未知值占位符；body 用 scope 拷贝（内部赋值不外泄）。
    var_scope.insert(loop_var, VAR_PLACEHOLDER.to_string());
    let mut body_scope = var_scope.clone();
    for c in &do_group.children {
        if matches!(c.node_type.as_str(), "do" | "done" | ";") {
            continue;
        }
        if let Some(err) = collect_commands(c, commands, &mut body_scope) {
            return Some(err);
        }
    }
    None
}

/// collect_commands 的 if_statement / while_statement 分支。
fn collect_if_or_while_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    let mut seen_then = false;
    for child in &node.children {
        let ctype = child.node_type.as_str();
        if matches!(
            ctype,
            "if" | "fi" | "else" | "elif" | "while" | "until" | ";"
        ) {
            continue;
        }
        if ctype == "then" {
            seen_then = true;
            continue;
        }
        if ctype == "do_group" {
            // while body：scope 拷贝（body 赋值不外泄 past done）。
            let mut body_scope = var_scope.clone();
            for c in &child.children {
                if matches!(c.node_type.as_str(), "do" | "done" | ";") {
                    continue;
                }
                if let Some(err) = collect_commands(c, commands, &mut body_scope) {
                    return Some(err);
                }
            }
            continue;
        }
        if ctype == "elif_clause" || ctype == "else_clause" {
            // elif/else 分支：scope 拷贝（赋值不外泄 past fi）。
            let mut branch_scope = var_scope.clone();
            for c in &child.children {
                if matches!(c.node_type.as_str(), "elif" | "else" | "then" | ";") {
                    continue;
                }
                if let Some(err) = collect_commands(c, commands, &mut branch_scope) {
                    return Some(err);
                }
            }
            continue;
        }
        // 条件（未遇 then）或 then-body（已遇 then）。
        // 条件用真实 varScope（必执行）；then-body 用拷贝。
        let before = commands.len();
        if seen_then {
            let mut then_scope = var_scope.clone();
            if let Some(err) = collect_commands(child, commands, &mut then_scope) {
                return Some(err);
            }
        } else if let Some(err) = collect_commands(child, commands, var_scope) {
            return Some(err);
        }
        // 条件中的 `read VAR`：把 VAR 记入真实 scope（body 拷贝继承）。
        // 值未知（stdin）→ VAR_PLACEHOLDER。
        if !seen_then {
            for i in before..commands.len() {
                let c = &commands[i];
                if c.argv.first().map(String::as_str) == Some("read") {
                    for a in c.argv.iter().skip(1) {
                        // 跳过 flag；裸标识符参数记为变量名。
                        if !a.starts_with('-') && is_valid_var_name(a) {
                            // SECURITY：`true || read VAR` 等条件未必执行；
                            // 覆盖已跟踪字面量会隐藏路径穿越，fail closed。
                            if let Some(existing) = var_scope.get(a) {
                                if !contains_any_placeholder(existing) {
                                    return Some(too_complex_reason(
                                        &format!(
                                            "'read {a}' in condition may not execute (||/pipeline/subshell); cannot prove it overwrites tracked literal '{existing}'"
                                        ),
                                        "if_statement",
                                    ));
                                }
                            }
                            var_scope.insert(a.clone(), VAR_PLACEHOLDER.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// 递归走 test_command 的表达式树（unary/binary/negated/parenthesized）。
fn walk_test_expr(
    node: &TsNode,
    argv: &mut Vec<String>,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    let ntype = node.node_type.as_str();
    match ntype {
        "unary_expression" | "binary_expression" | "negated_expression"
        | "parenthesized_expression" => {
            for c in &node.children {
                if let Some(err) = walk_test_expr(c, argv, inner_commands, var_scope) {
                    return Some(err);
                }
            }
            None
        }
        "test_operator" | "!" | "(" | ")" | "&&" | "||" | "==" | "=" | "!=" | "<" | ">"
        | "=~" => {
            argv.push(node.text.clone());
            None
        }
        "regex" | "extglob_pattern" => {
            // =~ / == / != 的 RHS：纯模式文本，无代码执行。
            argv.push(node.text.clone());
            None
        }
        _ => {
            // 操作数：word/string/number 等，经 walkArgument 校验。
            match walk_argument(node, inner_commands, var_scope) {
                Ok(arg) => {
                    argv.push(arg);
                    None
                }
                Err(e) => Some(e),
            }
        }
    }
}

/// `redirected_statement`：命令 + 重定向；重定向挂到最后一条命令。
fn walk_redirected_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut inner_command: Option<&TsNode> = None;

    for child in &node.children {
        let ctype = child.node_type.as_str();
        if ctype == "file_redirect" {
            // `commands` 透传：重定向目标中的 $() 同样抽取内部命令。
            match walk_file_redirect(child, commands, var_scope) {
                Ok(r) => redirects.push(r),
                Err(e) => return Some(e),
            }
        } else if ctype == "heredoc_redirect" {
            if let Some(err) = walk_heredoc_redirect(child) {
                return Some(err);
            }
        } else if matches!(
            ctype,
            "command" | "pipeline" | "list" | "negated_command" | "declaration_command"
                | "unset_command"
        ) {
            inner_command = Some(child);
        } else {
            return Some(too_complex(child));
        }
    }

    let inner = match inner_command {
        Some(c) => c,
        None => {
            // `> file` 单独亦合法（截断文件）：空 argv 命令让下游看到写入。
            commands.push(SimpleCommand {
                argv: vec![],
                env_vars: vec![],
                redirects,
                text: node.text.clone(),
            });
            return None;
        }
    };

    let before = commands.len();
    if let Some(err) = collect_commands(inner, commands, var_scope) {
        return Some(err);
    }
    if commands.len() > before && !redirects.is_empty() {
        if let Some(last) = commands.last_mut() {
            last.redirects.extend(redirects);
        }
    }
    None
}

/// `file_redirect` 节点 → 算符 + 目标。目标须为静态 word/string。
fn walk_file_redirect(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Result<Redirect, ParseForSecurityResult> {
    let mut op: Option<RedirectOp> = None;
    let mut target: Option<String> = None;
    let mut fd: Option<i64> = None;

    for child in &node.children {
        let ctype = child.node_type.as_str();
        if ctype == "file_descriptor" {
            fd = child.text.parse::<i64>().ok();
        } else if let Some(opv) = redirect_op(ctype) {
            op = Some(opv);
        } else if ctype == "word" || ctype == "number" {
            // `number` 节点可含 `NN#<expansion>` 算术基展开子节点。
            if !child.children.is_empty() {
                return Err(too_complex(child));
            }
            // `echo foo > {a,b}` 在 bash 中为歧义重定向（防御性检查）。
            if is_brace_expansion(&child.text) {
                return Err(too_complex(child));
            }
            // 反引用去除：bash quote removal 把 `\X` → `X`。
            // 不做这步，`cat < /proc/self/\environ` 会绕过 PROC_ENVIRON 检测。
            target = Some(unescape_backslash(&child.text));
        } else if ctype == "raw_string" {
            target = Some(strip_raw_string(&child.text));
        } else if ctype == "string" {
            let s = walk_string(child, inner_commands, var_scope)?;
            target = Some(s);
        } else if ctype == "concatenation" {
            let s = walk_argument(child, inner_commands, var_scope)?;
            target = Some(s);
        } else {
            return Err(too_complex(child));
        }
    }

    match (op, target) {
        (Some(op), Some(target)) => Ok(Redirect { op, target, fd }),
        _ => Err(too_complex_reason(
            "Unrecognized redirect shape",
            &node.node_type,
        )),
    }
}

/// heredoc 重定向：仅引号定界（<<'EOF'）安全（body 字面）。
fn walk_heredoc_redirect(node: &TsNode) -> Option<ParseForSecurityResult> {
    let mut start_text: Option<&str> = None;
    let mut body: Option<&TsNode> = None;

    for child in &node.children {
        let ctype = child.node_type.as_str();
        if ctype == "heredoc_start" {
            start_text = Some(&child.text);
        } else if ctype == "heredoc_body" {
            body = Some(child);
        } else if matches!(ctype, "<<" | "<<-" | "heredoc_end" | "file_descriptor") {
            // 预期结构 token。
        } else {
            // SECURITY：tree-sitter 把 heredoc 定界符同行后的 pipeline /
            // command / file_redirect 放进 heredoc_redirect 子节点。
            // 绝不可静默跳过——fail closed。
            return Some(too_complex(child));
        }
    }

    let is_quoted = start_text.map_or(false, |st| {
        (st.starts_with('\'') && st.ends_with('\''))
            || (st.starts_with('"') && st.ends_with('"'))
            || st.starts_with('\\')
    });

    if !is_quoted {
        return Some(too_complex_reason(
            "Heredoc with unquoted delimiter undergoes shell expansion",
            "heredoc_redirect",
        ));
    }

    if let Some(body) = body {
        for child in &body.children {
            if child.node_type != "heredoc_content" {
                return Some(too_complex(child));
            }
        }
    }
    None
}

/// here-string 重定向（`<<< content`）：content 进 stdin，非 argv/路径。
fn walk_herestring_redirect(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    for child in &node.children {
        if child.node_type == "<<<" {
            continue;
        }
        let content = match walk_argument(child, inner_commands, var_scope) {
            Ok(c) => c,
            Err(e) => return Some(e),
        };
        // content 不进 argv，但 NEWLINE_HASH 不变量（checkSemantics 依赖）
        // 仍需在此扫描。
        if has_newline_hash(&content) {
            return Some(too_complex(child));
        }
    }
    None
}

/// 走 `command` 节点提取 argv。未显式处理的子节点类型 → too-complex。
fn walk_command(
    node: &TsNode,
    extra_redirects: &[Redirect],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Result<SimpleCommand, ParseForSecurityResult> {
    let mut argv: Vec<String> = Vec::new();
    let mut env_vars: Vec<EnvVar> = Vec::new();
    let mut redirects: Vec<Redirect> = extra_redirects.to_vec();

    for child in &node.children {
        let ctype = child.node_type.as_str();
        match ctype {
            "variable_assignment" => {
                let ev = walk_variable_assignment(child, inner_commands, var_scope)?;
                // env 前缀赋值只对命令可见，不进全局 varScope。
                env_vars.push(EnvVar {
                    name: ev.name,
                    value: ev.value,
                });
            }
            "command_name" => {
                let name_node = child.children.first().unwrap_or(child);
                let arg = walk_argument(name_node, inner_commands, var_scope)?;
                argv.push(arg);
            }
            "word" | "number" | "raw_string" | "string" | "concatenation"
            | "arithmetic_expansion" => {
                let arg = walk_argument(child, inner_commands, var_scope)?;
                argv.push(arg);
            }
            // NOTE：裸 command_substitution 参数故意不处理——$() 输出即参数，
            // 对路径敏感命令（cd/rm/chmod）占位符会隐藏真实路径。
            "simple_expansion" => {
                // 裸 `$VAR` 参数：跟踪的静态变量返回真实值。
                let v = resolve_simple_expansion(child, var_scope, false)?;
                argv.push(v);
            }
            "file_redirect" => {
                let r = walk_file_redirect(child, inner_commands, var_scope)?;
                redirects.push(r);
            }
            "herestring_redirect" => {
                if let Some(err) = walk_herestring_redirect(child, inner_commands, var_scope) {
                    return Err(err);
                }
            }
            _ => return Err(too_complex(child)),
        }
    }

    // .text 为原始源片段；下游会重新分词。若 argv 已解析入变量，
    // 原始 .text 含 `$VAR` → 重建 argv 为 .text，避免 deny 规则漏匹配。
    // 同样为含换行的 .text 重建（行连接对 argv 不可见）。
    let text = if has_dollar_ident(&node.text) || node.text.contains('\n') {
        argv.iter()
            .map(|a| {
                if a.is_empty() || a.chars().any(needs_rebuild_quote) {
                    format!("'{}'", a.replace('\'', "'\\''"))
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        node.text.clone()
    };
    Ok(SimpleCommand {
        argv,
        env_vars,
        redirects,
        text,
    })
}

/// 递归收集 command_substitution 的内部命令。内部 cleanly 解析则追加进
/// innerCommands（递归权限校验：外层与内层都必须匹配规则）。
fn collect_command_substitution(
    cs_node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Option<ParseForSecurityResult> {
    // $() 前的变量对其可见（子 shell），内部赋值不外泄 → 传拷贝。
    let mut inner_scope = var_scope.clone();
    for child in &cs_node.children {
        if matches!(child.node_type.as_str(), "$(" | "`" | ")") {
            continue;
        }
        if let Some(err) = collect_commands(child, inner_commands, &mut inner_scope) {
            return Some(err);
        }
    }
    None
}

/// 参数节点 → 字面串（引号已解析）。函数即参数位置 allowlist。
fn walk_argument(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Result<String, ParseForSecurityResult> {
    match node.node_type.as_str() {
        "word" => {
            // 反斜杠去除：unquoted 语境 `\X` → `X`。
            // checkSemantics 依赖：`\eval` 须匹配 EVAL_LIKE_BUILTINS。
            if is_brace_expansion(&node.text) {
                return Err(too_complex_reason(
                    "Word contains brace expansion syntax",
                    "word",
                ));
            }
            Ok(unescape_backslash(&node.text))
        }
        "number" => {
            // SECURITY：`NN#<expansion>` 算术基语法的 number 节点带子节点。
            if !node.children.is_empty() {
                let child_type = node
                    .children
                    .first()
                    .map(|c| c.node_type.clone());
                return Err(ParseForSecurityResult::TooComplex {
                    reason: "Number node contains expansion (NN# arithmetic base syntax)"
                        .to_string(),
                    node_type: child_type,
                });
            }
            Ok(node.text.clone())
        }
        "raw_string" => Ok(strip_raw_string(&node.text)),
        "string" => walk_string(node, inner_commands, var_scope),
        "concatenation" => {
            if is_brace_expansion(&node.text) {
                return Err(too_complex_reason("Brace expansion", "concatenation"));
            }
            let mut result = String::new();
            for child in &node.children {
                let part = walk_argument(child, inner_commands, var_scope)?;
                result.push_str(&part);
            }
            Ok(result)
        }
        "arithmetic_expansion" => {
            if let Some(err) = walk_arithmetic(node) {
                return Err(err);
            }
            Ok(node.text.clone())
        }
        "simple_expansion" => {
            // 拼接内的 `$VAR`（如 prefix$VAR）：与裸参数同规则。
            resolve_simple_expansion(node, var_scope, false)
        }
        // NOTE：参数位置的 command_substitution 故意不处理（裸或在拼接内）。
        _ => Err(too_complex(node)),
    }
}

/// 双引号 string 节点 → 字面内容。处理 tree-sitter 丢弃字面换行的
/// quirk（子节点 startIndex 间隙补 \n）与 solo-placeholder 安全门。
fn walk_string(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Result<String, ParseForSecurityResult> {
    let mut result = String::new();
    let mut cursor: i64 = -1;
    // SECURITY：纯占位符字符串（`"$(cmd)"`、未知值 `"$VAR"`）会产出
    // 等于占位符的 argv 元素，绕过下游路径校验 → 拒绝；
    // 与字面内容混合（`"prefix: $(cmd)"`）则安全。
    let mut saw_dynamic_placeholder = false;
    let mut saw_literal_content = false;
    for child in &node.children {
        // 子节点 startIndex 间隙 = tree-sitter 丢弃的换行。
        let gap = child.start_index as i64 - cursor;
        if cursor != -1 && gap > 0 && child.node_type != "\"" {
            let n = gap as usize;
            result.push_str(&"\n".repeat(n));
            saw_literal_content = true;
        }
        cursor = child.end_index as i64;
        match child.node_type.as_str() {
            "\"" => {
                // 开引号后重置游标，捕获其与首个内容子节点的间隙。
                cursor = child.end_index as i64;
            }
            "string_content" => {
                // 双引号内反斜杠仅脱义 $ ` " \，其余保留。
                result.push_str(&unescape_string_content(&child.text));
                saw_literal_content = true;
            }
            "$" => {
                // 闭引号前的裸 `$`：字面。
                result.push('$');
                saw_literal_content = true;
            }
            "command_substitution" => {
                // 特例：`$(cat <<'EOF' ... EOF)` 的 body 是已知静态串。
                match extract_safe_cat_heredoc(child) {
                    CatHeredoc::Dangerous => return Err(too_complex(child)),
                    CatHeredoc::Body(body) => {
                        // body 即替换结果（$() 去掉尾部换行）。
                        // 多行 body 不可能是合法路径：丢弃避免 NEWLINE_HASH 误报；
                        // 单行 body 必须进 argv 让路径校验看到真实目标。
                        let trimmed = body.trim_end_matches('\n');
                        if trimmed.contains('\n') {
                            saw_literal_content = true;
                        } else {
                            result.push_str(trimmed);
                            saw_literal_content = true;
                        }
                    }
                    CatHeredoc::NotMatched => {
                        // 一般 $()：递归抽取内部命令；外层 argv 用占位符。
                        if let Some(err) =
                            collect_command_substitution(child, inner_commands, var_scope)
                        {
                            return Err(err);
                        }
                        result.push_str(CMDSUB_PLACEHOLDER);
                        saw_dynamic_placeholder = true;
                    }
                }
            }
            "simple_expansion" => {
                // `"..."` 内的 `$VAR`：跟踪/安全变量解析；未跟踪则拒绝。
                let v = resolve_simple_expansion(child, var_scope, true)?;
                if v == VAR_PLACEHOLDER {
                    saw_dynamic_placeholder = true;
                } else {
                    saw_literal_content = true;
                }
                result.push_str(&v);
            }
            "arithmetic_expansion" => {
                if let Some(err) = walk_arithmetic(child) {
                    return Err(err);
                }
                result.push_str(&child.text);
                saw_literal_content = true;
            }
            _ => {
                // `"..."` 内的 expansion（${...}）等 → 拒绝。
                return Err(too_complex(child));
            }
        }
    }
    // 拒绝纯占位符字符串。
    if saw_dynamic_placeholder && !saw_literal_content {
        return Err(too_complex(node));
    }
    // tree-sitter quirk：仅含空白的双引号串（`" "`、`"\t"`）不产生
    // string_content 子节点；无任何内容子节点但源片段长于 `""` → 拒绝。
    if !saw_literal_content && !saw_dynamic_placeholder {
        let text_len = node.text.chars().count();
        if text_len > 2 {
            return Err(too_complex(node));
        }
    }
    Ok(result)
}

/// `$(cat <<'DELIM'...DELIM)` 的 heredoc body 提取。
enum CatHeredoc {
    Body(String),
    Dangerous,
    NotMatched,
}

fn extract_safe_cat_heredoc(sub_node: &TsNode) -> CatHeredoc {
    // 期望：$(` + 一个 redirected_statement + `)`。
    let mut stmt: Option<&TsNode> = None;
    for child in &sub_node.children {
        let ctype = child.node_type.as_str();
        if matches!(ctype, "$(" | ")") {
            continue;
        }
        if ctype == "redirected_statement" && stmt.is_none() {
            stmt = Some(child);
        } else {
            return CatHeredoc::NotMatched;
        }
    }
    let stmt = match stmt {
        Some(s) => s,
        None => return CatHeredoc::NotMatched,
    };

    // redirected_statement 须为：command(cat) + heredoc_redirect（引号定界）。
    let mut saw_cat = false;
    let mut body: Option<String> = None;
    for child in &stmt.children {
        if child.node_type == "command" {
            // 须为裸 `cat`——无参数、无 env 赋值。
            let cmd_children: Vec<&TsNode> = child.children.iter().collect();
            if cmd_children.len() != 1 {
                return CatHeredoc::NotMatched;
            }
            let name_node = cmd_children[0];
            if name_node.node_type != "command_name" || name_node.text != "cat" {
                return CatHeredoc::NotMatched;
            }
            saw_cat = true;
        } else if child.node_type == "heredoc_redirect" {
            // 复用校验器：引号定界 + body 纯文本。
            if walk_heredoc_redirect(child).is_some() {
                return CatHeredoc::NotMatched;
            }
            for hc in &child.children {
                if hc.node_type == "heredoc_body" {
                    body = Some(hc.text.clone());
                }
            }
        } else {
            return CatHeredoc::NotMatched;
        }
    }

    let body = match (saw_cat, body) {
        (true, Some(b)) => b,
        _ => return CatHeredoc::NotMatched,
    };
    // SECURITY：body 经替换进入外层 argv，`/proc/self/environ` 等必须在此
    // 显式拒绝（语义同 `cat /proc/self/environ`）。
    if is_proc_environ(&body) {
        return CatHeredoc::Dangerous;
    }
    // jq system() 同理（checkSemantics 看不到 heredoc body）。
    if has_jq_system_call(&body) {
        return CatHeredoc::Dangerous;
    }
    CatHeredoc::Body(body)
}

/// 变量赋值节点 → { name, value, isAppend }。
fn walk_variable_assignment(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut Scope,
) -> Result<VarAssignment, ParseForSecurityResult> {
    let mut name: Option<String> = None;
    let mut value = String::new();
    let mut is_append = false;

    for child in &node.children {
        let ctype = child.node_type.as_str();
        if ctype == "variable_name" {
            name = Some(child.text.clone());
        } else if ctype == "=" || ctype == "+=" {
            // `PATH+=":/new"`：`+=` 为独立算符节点。
            is_append = ctype == "+=";
        } else if ctype == "command_substitution" {
            // $() 作为变量值：输出为字符串存储（非位置参数）。
            if let Some(err) = collect_command_substitution(child, inner_commands, var_scope) {
                return Err(err);
            }
            value = CMDSUB_PLACEHOLDER.to_string();
        } else if ctype == "simple_expansion" {
            // `VAR=$OTHER`：赋值 RHS 不做分词/glob 展开，按字符串语境解析。
            let v = resolve_simple_expansion(child, var_scope, true)?;
            value = v;
        } else {
            let v = walk_argument(child, inner_commands, var_scope)?;
            value = v;
        }
    }

    let name = match name {
        Some(n) => n,
        None => {
            return Err(too_complex_reason(
                "Variable assignment without name",
                "variable_assignment",
            ))
        }
    };
    // SECURITY：tree-sitter 接受非法变量名（`1VAR=value`），bash 会把
    // 其当命令执行 → 不可视为惰性赋值。
    if !is_valid_var_name(&name) {
        return Err(too_complex_reason(
            &format!("Invalid variable name (bash treats as command): {name}"),
            "variable_assignment",
        ));
    }
    // SECURITY：IFS 赋值改变后续 $VAR 的分词行为，静态模型无法覆盖。
    if name == "IFS" {
        return Err(too_complex_reason(
            "IFS assignment changes word-splitting — cannot model statically",
            "variable_assignment",
        ));
    }
    // SECURITY：PS4 经 promptvars 在 set -x 追踪时展开。
    // ALLOWLIST 策略：拒绝 +=、拒绝占位符、其余仅允许 ${VAR} 引用
    // 与白名单字符（无裸 $、无 `\`、无反引号、无括号）。
    if name == "PS4" {
        if is_append {
            return Err(too_complex_reason(
                "PS4 += cannot be statically verified — combine into a single PS4= assignment",
                "variable_assignment",
            ));
        }
        if contains_any_placeholder(&value) {
            return Err(too_complex_reason(
                "PS4 value derived from cmdsub/variable — runtime unknowable",
                "variable_assignment",
            ));
        }
        if !ps4_value_allowed(&value) {
            return Err(too_complex_reason(
                "PS4 value outside safe charset — only ${VAR} refs and [A-Za-z0-9 _+:.=/[]-] allowed",
                "variable_assignment",
            ));
        }
    }
    // SECURITY：赋值 RHS 的 `~` 会被 bash 展开（赋值期），静态无法建模 → 拒绝。
    if value.contains('~') {
        return Err(too_complex_reason(
            "Tilde in assignment value — bash may expand at assignment time",
            "variable_assignment",
        ));
    }
    Ok(VarAssignment {
        name,
        value,
        is_append,
    })
}

/// 解析 `simple_expansion`（$VAR）节点。
/// inside_string=true 表示位于 `string` 节点内（`"$VAR"`）。
fn resolve_simple_expansion(
    node: &TsNode,
    var_scope: &Scope,
    inside_string: bool,
) -> Result<String, ParseForSecurityResult> {
    let mut var_name: Option<String> = None;
    let mut is_special = false;
    for c in &node.children {
        if c.node_type == "variable_name" {
            var_name = Some(c.text.clone());
            break;
        }
        if c.node_type == "special_variable_name" {
            var_name = Some(c.text.clone());
            is_special = true;
            break;
        }
    }
    let var_name = match var_name {
        Some(n) => n,
        None => return Err(too_complex(node)),
    };
    // 跟踪变量：字面值直接返回（让下游路径校验看到真实值）；
    // 非字面值（含占位符）仅在字符串内允许为 VAR_PLACEHOLDER。
    if let Some(tracked_value) = var_scope.get(&var_name) {
        if contains_any_placeholder(tracked_value) {
            if !inside_string {
                return Err(too_complex(node));
            }
            return Ok(VAR_PLACEHOLDER.to_string());
        }
        // 纯字面值。裸参数位置还有 bash 分词/glob 风险：
        // `VAR="-rf /" && rm $VAR` → bash 跑两个参数。
        // 空值裸参数会消失（`V="" && $V eval x`）。
        if !inside_string {
            if tracked_value.is_empty() || tracked_value.chars().any(is_bare_var_unsafe) {
                return Err(too_complex(node));
            }
        }
        return Ok(tracked_value.clone());
    }
    // SAFE_ENV_VARS + 特殊变量（$? $$ $# $0-$9 等）：值由 shell 控制，
    // 仅允许嵌在字符串内。
    if inside_string {
        if SAFE_ENV_VARS.contains(&var_name.as_str()) {
            return Ok(VAR_PLACEHOLDER.to_string());
        }
        if is_special
            && (SPECIAL_VAR_NAMES.contains(&var_name.as_str())
                || var_name.chars().all(|c| c.is_ascii_digit()))
        {
            return Ok(VAR_PLACEHOLDER.to_string());
        }
    }
    Err(too_complex(node))
}

/// 应用变量赋值到 scope，处理 `+=` 拼接语义。
/// 任一侧含占位符 → 结果判为 VAR_PLACEHOLDER（裸引用时拒绝）。
fn apply_var_to_scope(var_scope: &mut Scope, ev: &VarAssignment) {
    let existing = var_scope.get(&ev.name).cloned().unwrap_or_default();
    let combined = if ev.is_append {
        format!("{existing}{}", ev.value)
    } else {
        ev.value.clone()
    };
    var_scope.insert(
        ev.name.clone(),
        if contains_any_placeholder(&combined) {
            VAR_PLACEHOLDER.to_string()
        } else {
            combined
        },
    );
}

/// 递归校验 arithmetic_expansion 节点：仅允许字面数值表达式。
fn walk_arithmetic(node: &TsNode) -> Option<ParseForSecurityResult> {
    for child in &node.children {
        if child.children.is_empty() {
            if !is_arith_leaf(&child.text) {
                return Some(too_complex_reason(
                    &format!(
                        "Arithmetic expansion references variable or non-literal: {}",
                        child.text
                    ),
                    "arithmetic_expansion",
                ));
            }
        } else {
            match child.node_type.as_str() {
                "binary_expression" | "unary_expression" | "ternary_expression"
                | "parenthesized_expression" => {
                    if let Some(err) = walk_arithmetic(child) {
                        return Some(err);
                    }
                }
                _ => return Some(too_complex(child)),
            }
        }
    }
    None
}

// ────────────────────────────────────────────────────────────────────────────
// argv 后置语义检查（checkSemantics）
// ────────────────────────────────────────────────────────────────────────────

fn is_duration_arg(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0usize;
    while i < len && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    // 可选小数部分
    if i < len && chars[i] == '.' {
        let dot = i + 1;
        i += 1;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == dot {
            return false;
        }
    }
    // 可选单位
    if i < len {
        if matches!(chars[i], 's' | 'm' | 'h' | 'd') {
            i += 1;
        }
    }
    i == len
}

/// `^[A-Za-z0-9_.+-]+$`
fn is_env_value(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
}

/// `^-[ks][A-Za-z0-9_.+-]+$`（timeout 的 -k5 / -sTERM）
fn is_timeout_fused_flag(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 || chars[0] != '-' {
        return false;
    }
    if !matches!(chars[1], 'k' | 's') {
        return false;
    }
    chars[2..].iter().all(|c| is_env_value(&c.to_string()))
}

/// `^--(?:kill-after|signal)=[A-Za-z0-9_.+-]+$`
fn is_timeout_long_value_flag(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix("--") {
        for p in ["kill-after", "signal"] {
            if let Some(r) = rest.strip_prefix(p) {
                return r.starts_with('=') && is_env_value(&r[1..]);
            }
        }
    }
    false
}

/// `^-?\d+$`
fn is_signed_int(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    !body.is_empty() && body.chars().all(|c| c.is_ascii_digit())
}

fn contains_expansion_char(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '$' | '(' | '`'))
}

/// stdbuf 短 flag 分离形式：`-[ioe]`
fn is_stdbuf_short_sep(s: &str) -> bool {
    matches!(s, "-i" | "-o" | "-e")
}

/// stdbuf 短 flag 融合形式：`-[ioe]X`
fn is_stdbuf_short_fused(s: &str) -> bool {
    s.len() > 2
        && s.starts_with('-')
        && matches!(s.chars().nth(1), Some('i') | Some('o') | Some('e'))
}

/// stdbuf 长 flag：`--(input|output|error)=`
fn is_stdbuf_long(s: &str) -> bool {
    s.starts_with("--input=")
        || s.starts_with("--output=")
        || s.starts_with("--error=")
}

/// `^-[^-]*[flags]`（fc 的 -e/-s、compgen 的 -C/-F/-W）前缀匹配。
fn has_short_flag_any(s: &str, flags: &[char]) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 || chars[0] != '-' || chars[1] == '-' {
        return false;
    }
    for c in &chars[1..] {
        if *c == '-' {
            return false;
        }
        if flags.contains(c) {
            return true;
        }
    }
    false
}

/// argv 中是否含数组下标（`[`）。
fn has_subscript(arg: &str) -> bool {
    arg.contains('[')
}

/// 后置语义检查：parseForSecurity 返回 'simple' 后运行，
/// 捕获可分词但按名称/参数内容仍危险的命令。返回首个失败或 ok。
pub fn check_semantics(commands: &[SimpleCommand]) -> SemanticCheckResult {
    for cmd in commands {
        // 剥离安全包装命令（nohup/time/timeout N/nice/env/stdbuf），
        // 使 `nohup eval "..."` 按被包装命令校验。
        let mut a: Vec<String> = cmd.argv.clone();
        loop {
            // clone 避免借用跨 a 的重赋值。
            let first = a.first().cloned().unwrap_or_default();
            let first = first.as_str();
            if first == "time" || first == "nohup" {
                a = a[1..].to_vec();
            } else if first == "timeout" {
                // timeout [flags] DURATION cmd
                let mut i = 1usize;
                let mut fail: Option<String> = None;
                let len = a.len();
                while i < len {
                    let arg = a[i].clone();
                    if matches!(arg.as_str(), "--foreground" | "--preserve-status" | "--verbose") {
                        i += 1;
                    } else if is_timeout_long_value_flag(&arg) {
                        i += 1;
                    } else if matches!(arg.as_str(), "--kill-after" | "--signal")
                        && i + 1 < len
                        && is_env_value(&a[i + 1])
                    {
                        i += 2;
                    } else if arg.starts_with("--") {
                        // 未知长 flag 或值不可识别 → fail closed。
                        fail = Some(format!(
                            "timeout with {arg} flag cannot be statically analyzed"
                        ));
                        break;
                    } else if arg == "-v" {
                        i += 1;
                    } else if matches!(arg.as_str(), "-k" | "-s")
                        && i + 1 < len
                        && is_env_value(&a[i + 1])
                    {
                        i += 2;
                    } else if is_timeout_fused_flag(&arg) {
                        i += 1;
                    } else if arg.starts_with('-') {
                        fail = Some(format!(
                            "timeout with {arg} flag cannot be statically analyzed"
                        ));
                        break;
                    } else {
                        break; // 非 flag：应为 duration
                    }
                }
                if let Some(reason) = fail {
                    return SemanticCheckResult::fail(&reason);
                }
                if i < a.len() {
                    if is_duration_arg(&a[i]) {
                        a = a[i + 1..].to_vec();
                    } else {
                        // GNU timeout 走 xstrtod()，接受 `.5`/`+5`/`5e-1`/`inf`
                        // 等——正则覆盖不全 → fail closed（PR #21503）。
                        return SemanticCheckResult::fail(&format!(
                            "timeout duration '{}' cannot be statically analyzed",
                            a[i]
                        ));
                    }
                } else {
                    // 无更多参数：`timeout` 单独使用，惰性。
                    break;
                }
            } else if first == "nice" {
                if a.get(1).map(String::as_str) == Some("-n")
                    && a.get(2).map(|v| is_signed_int(v)).unwrap_or(false)
                {
                    a = a[3..].to_vec();
                } else if a.get(1).map(|v| is_signed_int(v)).unwrap_or(false) {
                    a = a[2..].to_vec();
                } else if a.get(1).map(|v| contains_expansion_char(v)).unwrap_or(false) {
                    // `nice $((0-5)) cmd`：bash 展开为 legacy `-5` 语法，
                    // 静态无法确定被包装命令 → fail closed。
                    return SemanticCheckResult::fail(&format!(
                        "nice argument '{}' contains expansion — cannot statically determine wrapped command",
                        a[1]
                    ));
                } else {
                    a = a[1..].to_vec();
                }
            } else if first == "env" {
                // env [VAR=val...] [-i] [-0] [-v] [-u NAME...] cmd
                let mut i = 1usize;
                let len = a.len();
                let mut fail: Option<String> = None;
                while i < len {
                    let arg = &a[i];
                    if arg.contains('=') && !arg.starts_with('-') {
                        i += 1;
                    } else if matches!(arg.as_str(), "-i" | "-0" | "-v") {
                        i += 1;
                    } else if arg == "-u" && i + 1 < len {
                        i += 2;
                    } else if arg.starts_with('-') {
                        // -S（argv 切分）、-C/-P（改 cwd/PATH）等 → 拒绝。
                        fail = Some(format!(
                            "env with {arg} flag cannot be statically analyzed"
                        ));
                        break;
                    } else {
                        break; // 被包装命令
                    }
                }
                if let Some(reason) = fail {
                    return SemanticCheckResult::fail(&reason);
                }
                if i < a.len() {
                    a = a[i..].to_vec();
                } else {
                    break; // `env` 单独：惰性
                }
            } else if first == "stdbuf" {
                let mut i = 1usize;
                let len = a.len();
                let mut fail: Option<String> = None;
                while i < len {
                    let arg = &a[i];
                    if is_stdbuf_short_sep(arg) && i + 1 < len {
                        i += 2;
                    } else if is_stdbuf_short_fused(arg) {
                        i += 1;
                    } else if is_stdbuf_long(arg) {
                        i += 1;
                    } else if arg.starts_with('-') {
                        fail = Some(format!(
                            "stdbuf with {arg} flag cannot be statically analyzed"
                        ));
                        break;
                    } else {
                        break;
                    }
                }
                if let Some(reason) = fail {
                    return SemanticCheckResult::fail(&reason);
                }
                if i > 1 && i < a.len() {
                    a = a[i..].to_vec();
                } else {
                    break; // stdbuf 无 flag 或无被包装命令：惰性
                }
            } else {
                break;
            }
        }
        let name = match a.first() {
            Some(n) => n.clone(),
            None => continue,
        };

        // SECURITY：空命令名——未加引号的空展开在命令位置是绕过。
        if name.is_empty() {
            return SemanticCheckResult::fail(
                "Empty command name — argv[0] may not reflect what bash runs",
            );
        }

        // argv[0] 应永不可能是占位符（防御性）。
        if name.contains(CMDSUB_PLACEHOLDER) || name.contains(VAR_PLACEHOLDER) {
            return SemanticCheckResult::fail(
                "Command name is runtime-determined (placeholder argv[0])",
            );
        }

        // argv[0] 以算符/flag 开头：命令残片。
        if name.starts_with('-') || name.starts_with('|') || name.starts_with('&') {
            return SemanticCheckResult::fail(
                "Command appears to be an incomplete fragment",
            );
        }

        // SECURITY：内部重解析 NAME 操作数的内置命令（bash 算术求值
        // `arr[EXPR]` 下标，含 $(cmd)）。三种形式：分离 `-v NAME`、
        // 组合短 flag `-ra`、融合 `-vNAME`。
        if let Some(danger_flags) = subscript_eval_flags(&name) {
            for i in 1..a.len() {
                let arg = &a[i];
                // 分离形式：`-v` 后随含 `[` 的下一参数。
                if danger_flags.iter().any(|f| *f == arg.as_str())
                    && a.get(i + 1).map(|v| has_subscript(v)).unwrap_or(false)
                {
                    return SemanticCheckResult::fail(&format!(
                        "'{name} {arg}' operand contains array subscript — bash evaluates $(cmd) in subscripts"
                    ));
                }
                // 组合短 flag：`-ra`（= -r -a）。
                if arg.len() > 2
                    && arg.starts_with('-')
                    && !arg.starts_with("--")
                    && !arg.contains('[')
                {
                    for flag in danger_flags {
                        if flag.len() == 2 && arg.contains(&flag[1..]) {
                            if a.get(i + 1).map(|v| has_subscript(v)).unwrap_or(false) {
                                return SemanticCheckResult::fail(&format!(
                                    "'{name} {flag}' (combined in '{arg}') operand contains array subscript — bash evaluates $(cmd) in subscripts"
                                ));
                            }
                        }
                    }
                }
                // 融合形式：`-vNAME`。
                for flag in danger_flags {
                    if flag.len() == 2
                        && arg.starts_with(flag)
                        && arg.len() > 2
                        && arg.contains('[')
                    {
                        return SemanticCheckResult::fail(&format!(
                            "'{name} {flag}' (fused) operand contains array subscript — bash evaluates $(cmd) in subscripts"
                        ));
                    }
                }
            }
        }

        // `[[ ARG OP ARG ]]` 算术比较：两侧操作数都被算术求值。
        if name == "[[" {
            // a[0]='[[' 含 `[`，a[1] 为首个操作数；二元算符最早出现在下标 2。
            for i in 2..a.len() {
                if !TEST_ARITH_CMP_OPS.contains(&a[i].as_str()) {
                    continue;
                }
                let prev_sub = a.get(i - 1).map(|v| has_subscript(v)).unwrap_or(false);
                let next_sub = a.get(i + 1).map(|v| has_subscript(v)).unwrap_or(false);
                if prev_sub || next_sub {
                    return SemanticCheckResult::fail(&format!(
                        "'[[ ... {} ... ]]' operand contains array subscript — bash arithmetically evaluates $(cmd) in subscripts",
                        a[i]
                    ));
                }
            }
        }

        // read/unset：每个裸位置参数都是 NAME（无需 flag 触发）。
        if BARE_SUBSCRIPT_NAME_BUILTINS.contains(&name.as_str()) {
            let mut skip_next = false;
            let mut i = 1usize;
            while i < a.len() {
                let arg = &a[i];
                if skip_next {
                    skip_next = false;
                    i += 1;
                    continue;
                }
                if arg.starts_with('-') {
                    if name == "read" {
                        if READ_DATA_FLAGS.contains(&arg.as_str()) {
                            skip_next = true;
                        } else if arg.len() > 2 && !arg.starts_with("--") {
                            // 组合短 flag（如 `-rp`）：首个数据 flag 字符
                            // 消费其余字符作操作数（`-p[foo]`），或在末尾时
                            // 消费下一参数（`-rp '[foo]'`）。
                            let chars: Vec<char> = arg.chars().collect();
                            for j in 1..chars.len() {
                                let candidate = format!("-{}", chars[j]);
                                if READ_DATA_FLAGS.contains(&candidate.as_str()) {
                                    if j == chars.len() - 1 {
                                        skip_next = true;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    i += 1;
                    continue;
                }
                if arg.contains('[') {
                    return SemanticCheckResult::fail(&format!(
                        "'{name}' positional NAME '{arg}' contains array subscript — bash evaluates $(cmd) in subscripts"
                    ));
                }
                i += 1;
            }
        }

        // shell 保留字作 argv[0] → tree-sitter 误解析。
        if is_shell_keyword(&name) {
            return SemanticCheckResult::fail(&format!(
                "Shell keyword '{name}' as command name — tree-sitter mis-parse"
            ));
        }

        // NEWLINE_HASH：argv / env 值 / 重定向目标中的换行+# 会隐藏参数。
        for arg in &cmd.argv {
            if arg.contains('\n') && has_newline_hash(arg) {
                return SemanticCheckResult::fail(
                    "Newline followed by # inside a quoted argument can hide arguments from path validation",
                );
            }
        }
        for ev in &cmd.env_vars {
            if ev.value.contains('\n') && has_newline_hash(&ev.value) {
                return SemanticCheckResult::fail(
                    "Newline followed by # inside an env var value can hide arguments from path validation",
                );
            }
        }
        for r in &cmd.redirects {
            if r.target.contains('\n') && has_newline_hash(&r.target) {
                return SemanticCheckResult::fail(
                    "Newline followed by # inside a redirect target can hide arguments from path validation",
                );
            }
        }

        // jq system() 与文件读取 flag（AST 路径的镜像防御）。
        if name == "jq" {
            for arg in &a {
                if has_jq_system_call(arg) {
                    return SemanticCheckResult::fail(
                        "jq command contains system() function which executes arbitrary commands",
                    );
                }
            }
            if a.iter().any(|arg| is_jq_dangerous_flag(arg)) {
                return SemanticCheckResult::fail(
                    "jq command contains dangerous flags that could execute code or read arbitrary files",
                );
            }
        }

        if ZSH_DANGEROUS_BUILTINS.contains(&name.as_str()) {
            return SemanticCheckResult::fail(&format!(
                "Zsh builtin '{name}' can bypass security checks"
            ));
        }

        if EVAL_LIKE_BUILTINS.contains(&name.as_str()) {
            // `command -v/-V foo` 仅打印路径，不执行 → 豁免。
            // `fc -l` 列历史 → 豁免；含 -e/-s 则执行 → 拒绝。
            // `compgen -c/-f/-v` 只列补全 → 豁免；含 -C/-F/-W 则拒绝。
            let exempt = match name.as_str() {
                "command" => a.get(1).map(|x| x == "-v" || x == "-V").unwrap_or(false),
                "fc" => !a[1..]
                    .iter()
                    .any(|arg| has_short_flag_any(arg, &['e', 's'])),
                "compgen" => !a[1..]
                    .iter()
                    .any(|arg| has_short_flag_any(arg, &['C', 'F', 'W'])),
                _ => false,
            };
            if !exempt {
                return SemanticCheckResult::fail(&format!(
                    "'{name}' evaluates arguments as shell code"
                ));
            }
        }

        // /proc/*/environ 暴露其他进程的环境变量（含密钥）。
        // argv 与重定向目标都查（`cat < /proc/self/environ`）。
        for arg in &cmd.argv {
            if arg.contains("/proc/") && is_proc_environ(arg) {
                return SemanticCheckResult::fail(
                    "Accesses /proc/*/environ which may expose secrets",
                );
            }
        }
        for r in &cmd.redirects {
            if r.target.contains("/proc/") && is_proc_environ(&r.target) {
                return SemanticCheckResult::fail(
                    "Accesses /proc/*/environ which may expose secrets",
                );
            }
        }
    }
    SemanticCheckResult::ok_()
}
