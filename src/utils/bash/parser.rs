//! parser.rs — bash 命令解析入口（tree-sitter 兼容 AST 的薄封装）。
//!
//! 重写自 `src/utils/bash/parser.ts`。语义对齐：
//!
//! - TS 的 `feature('TREE_SITTER_BASH')` / `feature('TREE_SITTER_BASH_SHADOW')`
//!   是 bun:bundle 编译期常量（dev 默认全开、build 由 DEFAULT_BUILD_FEATURES
//!   决定），crate 内既定等价实现为运行时环境变量 `FEATURE_<FLAG>` 真值检查
//!   （见 constants/betas.rs）。
//! - TS 的 WASM/NAPI 模块加载（ensureParserInitialized / getParserModule）在
//!   Rust 侧对应纯 Rust 解析器 `bash_parser::parse_source`，无需初始化；
//!   `ensure_initialized()` 为等价空操作。`logLoadOnce` 保留进程级单次日志。
//! - TS 的 `logEvent`（analytics）未转换，按 crate 既有约定省略。
//! - TS 为 async（WASM init 天然异步）；Rust 无运行时依赖，全部同步。
//! - TS 的 `PARSE_ABORTED` Symbol 由 `RawParseResult::Aborted` 表达：
//!   模块加载成功但解析超时/超预算/panic，调用方必须 fail-closed。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::utils::bash::bash_parser::{self, TsNode};
use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// 对齐 TS 的 `Node` 别名。
pub type Node = TsNode;

/// parseCommand 返回的解析数据（对齐 TS ParsedCommandData）。
#[derive(Debug, Clone)]
pub struct ParsedCommandData {
    pub root_node: Node,
    pub env_vars: Vec<String>,
    pub command_node: Option<Node>,
    pub original_command: String,
}

/// parseCommandRaw 结果（对齐 TS 的 `Node | null | typeof PARSE_ABORTED` 联合）。
#[derive(Debug, Clone)]
pub enum RawParseResult {
    /// 解析成功。
    Node(Node),
    /// 模块未加载 / feature 关闭 / 空串 / 超长。
    Unavailable,
    /// 模块已加载但解析中止（超时 / 节点预算 / panic）——fail-closed，
    /// 不得路由回 legacy 路径。
    Aborted,
}

/// `feature('FLAG')` 等价实现：环境变量 `FEATURE_<FLAG>` 真值启用。
fn feature(flag: &str) -> bool {
    std::env::var(format!("FEATURE_{flag}"))
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

const MAX_COMMAND_LENGTH: usize = 10000;

const DECLARATION_COMMANDS: &[&str] = &[
    "export",
    "declare",
    "typeset",
    "readonly",
    "local",
    "unset",
    "unsetenv",
];

const ARGUMENT_TYPES: &[&str] = &["word", "string", "raw_string", "number"];

const SUBSTITUTION_TYPES: &[&str] = &["command_substitution", "process_substitution"];

const COMMAND_TYPES: &[&str] = &["command", "declaration_command"];

static LOGGED: AtomicBool = AtomicBool::new(false);

fn log_load_once(success: bool) {
    if LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
    log_for_debugging(
        if success {
            "tree-sitter: native module loaded"
        } else {
            "tree-sitter: unavailable"
        },
        DebugLogLevel::Debug,
    );
}

/// 等待解析器初始化。Rust 侧解析器为纯 Rust 实现，无需初始化；
/// 调用幂等，保留 API 形态（对齐 TS ensureInitialized）。
pub fn ensure_initialized() {
    if feature("TREE_SITTER_BASH") || feature("TREE_SITTER_BASH_SHADOW") {
        // 纯 Rust 解析器无 WASM init；直接成功。
    }
}

/// 解析 bash 命令，提取根节点、命令节点与 env 赋值。
/// feature 关闭 / 空串 / 超长 / 解析中止均返回 None（对齐 TS parseCommand）。
pub fn parse_command(command: &str) -> Option<ParsedCommandData> {
    if command.is_empty() || command.len() > MAX_COMMAND_LENGTH {
        return None;
    }

    if !feature("TREE_SITTER_BASH") {
        return None;
    }
    log_load_once(true);

    let root_node = bash_parser::parse_source(command, None)?;
    let command_node = find_command_node(&root_node, None);
    let env_vars = extract_env_vars(command_node.as_ref());
    Some(ParsedCommandData {
        root_node,
        env_vars,
        command_node,
        original_command: command.to_string(),
    })
}

/// 原始解析，跳过 findCommandNode/extractEnvVars（安全 walker 不需要），
/// 一次调用省一次遍历。结果语义见 [`RawParseResult`]。
pub fn parse_command_raw(command: &str) -> RawParseResult {
    if command.is_empty() || command.len() > MAX_COMMAND_LENGTH {
        return RawParseResult::Unavailable;
    }
    if feature("TREE_SITTER_BASH") || feature("TREE_SITTER_BASH_SHADOW") {
        log_load_once(true);
        match bash_parser::parse_source(command, None) {
            // SECURITY：模块已加载时 None = 超时/节点预算中止
            // （PARSE_TIMEOUT_MS=50，MAX_NODES=50_000）。绝不可与
            // unavailable 混淆后路由 legacy（legacy 缺 EVAL_LIKE_BUILTINS，
            // `trap`/`enable`/`hash` 会漏检）。
            Some(node) => RawParseResult::Node(node),
            None => RawParseResult::Aborted,
        }
    } else {
        RawParseResult::Unavailable
    }
}

fn find_command_node(node: &Node, parent: Option<&Node>) -> Option<Node> {
    let node_type = node.node_type.as_str();

    if COMMAND_TYPES.contains(&node_type) {
        return Some(node.clone());
    }

    // 变量赋值后跟命令：从兄弟中找其后的 command。
    if node_type == "variable_assignment" && parent.is_some() {
        let parent = parent?;
        for c in &parent.children {
            if COMMAND_TYPES.contains(&c.node_type.as_str())
                && c.start_index > node.start_index
            {
                return Some(c.clone());
            }
        }
        return None;
    }

    // pipeline：递归第一个子节点（可能是 redirected_statement）。
    if node_type == "pipeline" {
        for child in &node.children {
            if let Some(result) = find_command_node(child, Some(node)) {
                return Some(result);
            }
        }
        return None;
    }

    // redirected_statement：找内部 command。
    if node_type == "redirected_statement" {
        return node
            .children
            .iter()
            .find(|c| COMMAND_TYPES.contains(&c.node_type.as_str()))
            .cloned();
    }

    // 递归搜索。
    for child in &node.children {
        if let Some(result) = find_command_node(child, Some(node)) {
            return Some(result);
        }
    }
    None
}

fn extract_env_vars(command_node: Option<&Node>) -> Vec<String> {
    let node = match command_node {
        Some(n) if n.node_type == "command" => n,
        _ => return vec![],
    };
    let mut env_vars = Vec::new();
    for child in &node.children {
        if child.node_type == "variable_assignment" {
            env_vars.push(child.text.clone());
        } else if child.node_type == "command_name" || child.node_type == "word" {
            break;
        }
    }
    env_vars
}

/// 提取命令参数（对齐 TS extractCommandArguments）。
pub fn extract_command_arguments(command_node: &Node) -> Vec<String> {
    // declaration_command：仅返回 builtin 名（export/declare/...）。
    if command_node.node_type == "declaration_command" {
        if let Some(first_child) = command_node.children.first() {
            if DECLARATION_COMMANDS.contains(&first_child.text.as_str()) {
                return vec![first_child.text.clone()];
            }
        }
        return vec![];
    }

    let mut args = Vec::new();
    let mut found_command_name = false;
    for child in &command_node.children {
        if child.node_type == "variable_assignment" {
            continue;
        }
        // 命令名。
        if child.node_type == "command_name"
            || (!found_command_name && child.node_type == "word")
        {
            found_command_name = true;
            args.push(child.text.clone());
            continue;
        }
        // 参数。
        if ARGUMENT_TYPES.contains(&child.node_type.as_str()) {
            args.push(strip_quotes(&child.text));
        } else if SUBSTITUTION_TYPES.contains(&child.node_type.as_str()) {
            break;
        }
    }
    args
}

fn strip_quotes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= 2 {
        let first = chars[0];
        let last = chars[chars.len() - 1];
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return chars[1..chars.len() - 1].iter().collect();
        }
    }
    text.to_string()
}
