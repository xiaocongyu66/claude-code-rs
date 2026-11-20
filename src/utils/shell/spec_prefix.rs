//! spec_prefix.rs — Fig spec 驱动的命令前缀提取。
//!
//! 重写自 `src/utils/shell/specPrefix.ts`。给定命令名 + 参数数组 + 它的
//! @withfig/autocomplete spec，遍历 spec 找出有意义的前缀能深入到参数的
//! 第几层。例如 `git -C /repo status --short` → `git status`（spec 说 -C
//! 带一个值，跳过它，发现 `status` 是已知子命令）。
//!
//! 对 `(command, args, CommandSpec)` 纯函数 — 不依赖解析器。从
//! src/utils/bash/prefix.ts 抽出以便 PowerShell 的提取器复用；
//! 外部 CLI（git, npm, kubectl）是与 shell 无关的。
//!
//! 注意：`CommandSpec` / `CommandSpecArgument` / `CommandSpecOption` 镜像
//! `src/utils/bash/registry.ts` 的同名类型（registry 转 Rust 后应统一引用，
//! 届时删除本模块的重复定义）。仅包含本模块实际消费的字段。

use std::collections::HashMap;

use once_cell::sync::Lazy;

/// TS: `URL_PROTOCOLS = ['http://', 'https://', 'ftp://']`
const URL_PROTOCOLS: [&str; 3] = ["http://", "https://", "ftp://"];

/// 无视 fig spec 时的深度覆写（spec 在 native/node 构建中不可用时）。
/// 没有这些覆写，calculate_depth 会回退到 2，产生过宽的前缀。
/// TS: `DEPTH_RULES: Record<string, number>`
pub static DEPTH_RULES: Lazy<HashMap<&'static str, u32>> = Lazy::new(|| {
    let mut rules = HashMap::new();
    rules.insert("rg", 2); // pattern 参数是必需的，尽管 path 是可变参数
    rules.insert("pre-commit", 2);
    // 深子命令树的 CLI 工具（如 gcloud scheduler jobs list）
    rules.insert("gcloud", 4);
    rules.insert("gcloud compute", 6);
    rules.insert("gcloud beta", 6);
    rules.insert("aws", 4);
    rules.insert("az", 4);
    rules.insert("kubectl", 3);
    rules.insert("docker", 3);
    rules.insert("dotnet", 3);
    rules.insert("git push", 2);
    rules
});

/// TS: `Argument.isModule?: string | boolean` 的镜像。
/// 真值语义：`true` 或非空字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleSpec {
    Flag,
    Name(String),
}

impl ModuleSpec {
    /// TS 中 `a?.isModule` 的真值判断（boolean true 或非空字符串）。
    fn is_truthy(&self) -> bool {
        match self {
            ModuleSpec::Flag => true,
            ModuleSpec::Name(name) => !name.is_empty(),
        }
    }
}

/// TS: `Argument`（registry.ts），仅保留本模块消费的字段。
#[derive(Debug, Clone, Default)]
pub struct CommandSpecArgument {
    pub is_dangerous: bool,
    /// 无限重复，如 `echo hello world`
    pub is_variadic: bool,
    pub is_optional: bool,
    /// 包装命令，如 timeout, sudo
    pub is_command: bool,
    /// 用于 python -m 及类似模块参数
    pub is_module: Option<ModuleSpec>,
}

impl CommandSpecArgument {
    fn is_module_truthy(&self) -> bool {
        self.is_module.as_ref().is_some_and(ModuleSpec::is_truthy)
    }
}

/// TS: `Option.name: string | string[]` 的镜像。
#[derive(Debug, Clone)]
pub enum SpecName {
    Single(String),
    Many(Vec<String>),
}

impl Default for SpecName {
    fn default() -> Self {
        SpecName::Single(String::new())
    }
}

impl SpecName {
    /// 名称列表中是否包含指定 flag（精确匹配，区分大小写）。
    /// TS: `Array.isArray(opt.name) ? opt.name.includes(flag) : opt.name === flag`
    fn contains(&self, flag: &str) -> bool {
        match self {
            SpecName::Single(name) => name == flag,
            SpecName::Many(names) => names.iter().any(|name| name == flag),
        }
    }

    /// 与小写名称做大小写不敏感匹配。
    /// TS: `sub.name.toLowerCase() === argLower`（name 可为数组）。
    fn matches_lowercase(&self, lowercased: &str) -> bool {
        match self {
            SpecName::Single(name) => name.to_lowercase() == lowercased,
            SpecName::Many(names) => names.iter().any(|name| name.to_lowercase() == lowercased),
        }
    }
}

/// TS: `Argument | Argument[]` 的镜像 — `toArray` 语义统一为数组视图。
#[derive(Debug, Clone)]
pub enum SpecArgs {
    Single(Box<CommandSpecArgument>),
    Many(Vec<CommandSpecArgument>),
}

impl SpecArgs {
    /// TS: `toArray(spec.args).some(pred)`
    fn any(&self, predicate: impl Fn(&CommandSpecArgument) -> bool) -> bool {
        match self {
            SpecArgs::Single(arg) => predicate(arg),
            SpecArgs::Many(args) => args.iter().any(predicate),
        }
    }

    /// TS: `toArray(spec.args)[0]`
    fn first(&self) -> Option<&CommandSpecArgument> {
        match self {
            SpecArgs::Single(arg) => Some(arg),
            SpecArgs::Many(args) => args.first(),
        }
    }

    /// TS: `toArray(spec.args).findIndex(pred)` — 首个匹配的位置，无匹配为 -1。
    fn position(&self, predicate: impl Fn(&CommandSpecArgument) -> bool) -> Option<usize> {
        match self {
            SpecArgs::Single(arg) if predicate(arg) => Some(0),
            SpecArgs::Single(_) => None,
            SpecArgs::Many(args) => args.iter().position(predicate),
        }
    }

    /// TS: `!Array.isArray(spec.args)`
    fn is_single(&self) -> bool {
        matches!(self, SpecArgs::Single(_))
    }
}

/// TS: `Option`（registry.ts）。
#[derive(Debug, Clone, Default)]
pub struct CommandSpecOption {
    pub name: SpecName,
    pub args: Option<SpecArgs>,
}

/// TS: `CommandSpec`（registry.ts），仅保留本模块消费的字段。
/// 空的 `subcommands` / `options` 等价 TS 的 undefined。
#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    pub name: SpecName,
    pub subcommands: Vec<CommandSpec>,
    pub args: Option<SpecArgs>,
    pub options: Vec<CommandSpecOption>,
}

// 检查参数是否匹配已知子命令（大小写不敏感：PowerShell 调用方传入
// 原始大小写的参数；fig spec 的名称是小写）
fn is_known_subcommand(arg: &str, spec: Option<&CommandSpec>) -> bool {
    let Some(spec) = spec else {
        return false;
    };
    if spec.subcommands.is_empty() {
        return false;
    }
    let arg_lower = arg.to_lowercase();
    spec.subcommands
        .iter()
        .any(|sub| sub.name.matches_lowercase(&arg_lower))
}

// 基于 spec 判断 flag 是否带参数，或使用启发式
fn flag_takes_arg(flag: &str, next_arg: Option<&str>, spec: Option<&CommandSpec>) -> bool {
    // 检查 flag 是否在 spec.options 中
    if let Some(spec) = spec {
        if let Some(option) = spec.options.iter().find(|opt| opt.name.contains(flag)) {
            return option.args.is_some();
        }
    }
    // 启发式：若下一个参数不是 flag 且不是已知子命令，假定它是 flag 的值
    if let (Some(spec), Some(next_arg)) = (spec, next_arg) {
        if !spec.subcommands.is_empty() && !next_arg.starts_with('-') {
            return !is_known_subcommand(next_arg, Some(spec));
        }
    }
    false
}

// 跳过 flag 及其值，找到第一个子命令
fn find_first_subcommand(args: &[String], spec: Option<&CommandSpec>) -> Option<String> {
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg.is_empty() {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            if flag_takes_arg(arg, args.get(i + 1).map(|a| a.as_str()), spec) {
                i += 1; // 跳过 flag 的值
            }
            i += 1;
            continue;
        }
        match spec {
            Some(spec) if !spec.subcommands.is_empty() => {
                if is_known_subcommand(arg, spec) {
                    return Some(arg.to_string());
                }
                // 未知位置参数 — 继续扫描
            }
            _ => return Some(arg.to_string()),
        }
        i += 1;
    }
    None
}

/// 给定命令名、参数与它的 fig spec，计算命令前缀。
/// `git -C /repo status --short` → `git status`。
///
/// 在 TS 中为 async（为将来 spec 异步加载留口子），但函数体内无 await，
/// 故 Rust 版本为同步。
pub fn build_prefix(command: &str, args: &[String], spec: Option<&CommandSpec>) -> String {
    let max_depth = calculate_depth(command, args, spec) as usize;
    let mut parts: Vec<&str> = vec![command];
    let has_subcommands = spec.is_some_and(|s| !s.subcommands.is_empty());
    let mut found_subcommand = false;

    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg.is_empty() || parts.len() >= max_depth {
            break;
        }

        if arg.starts_with('-') {
            // 特殊情况：python -c 应在 -c 之后停止
            // TS: `['python', 'python3'].includes(command.toLowerCase())`
            if arg == "-c"
                && (command.eq_ignore_ascii_case("python")
                    || command.eq_ignore_ascii_case("python3"))
            {
                break;
            }

            // 检查应包含进前缀的 isCommand/isModule flag
            if let Some(spec) = spec {
                if let Some(option) = spec.options.iter().find(|opt| opt.name.contains(arg)) {
                    if option.args.as_ref().is_some_and(|args_spec| {
                        args_spec.any(|arg_spec| {
                            arg_spec.is_command || arg_spec.is_module_truthy()
                        })
                    }) {
                        parts.push(arg);
                        i += 1;
                        continue;
                    }
                }
            }

            // 对带子命令的命令，跳过全局 flag 以找到子命令
            if has_subcommands && !found_subcommand {
                if flag_takes_arg(arg, args.get(i + 1).map(|a| a.as_str()), spec) {
                    i += 1; // 跳过 flag 的值
                }
                i += 1;
                continue;
            }
            break; // 遇 flag 停止（原始行为）
        }

        if should_stop_at_arg(arg, &args[..i], spec) {
            break;
        }
        if has_subcommands && !found_subcommand {
            found_subcommand = is_known_subcommand(arg, spec);
        }
        parts.push(arg);
        i += 1;
    }

    parts.join(" ")
}

fn calculate_depth(command: &str, args: &[String], spec: Option<&CommandSpec>) -> u32 {
    // 跳过 flag 及其值，找第一个子命令
    let first_subcommand = find_first_subcommand(args, spec);
    let command_lower = command.to_lowercase();
    let key = match &first_subcommand {
        Some(subcommand) => format!("{command_lower} {}", subcommand.to_lowercase()),
        None => command_lower.clone(),
    };
    if let Some(depth) = DEPTH_RULES.get(key.as_str()) {
        return *depth;
    }
    if let Some(depth) = DEPTH_RULES.get(command_lower.as_str()) {
        return *depth;
    }
    let Some(spec) = spec else {
        return 2;
    };

    if !spec.options.is_empty() && args.iter().any(|arg| arg.starts_with('-')) {
        for arg in args {
            if !arg.starts_with('-') {
                continue;
            }
            let option = spec.options.iter().find(|opt| opt.name.contains(arg));
            if option
                .and_then(|option| option.args.as_ref())
                .is_some_and(|args_spec| {
                    args_spec.any(|arg_spec| arg_spec.is_command || arg_spec.is_module_truthy())
                })
            {
                return 3;
            }
        }
    }

    // 用已找到的 firstSubcommand 定位子命令 spec
    if let Some(first_subcommand) = &first_subcommand {
        if !spec.subcommands.is_empty() {
            let first_sub_lower = first_subcommand.to_lowercase();
            let subcommand = spec
                .subcommands
                .iter()
                .find(|sub| sub.name.matches_lowercase(&first_sub_lower));
            if let Some(subcommand) = subcommand {
                if let Some(sub_args) = &subcommand.args {
                    if sub_args.any(|arg| arg.is_command) {
                        return 3;
                    }
                    if sub_args.any(|arg| arg.is_variadic) {
                        return 2;
                    }
                }
                if !subcommand.subcommands.is_empty() {
                    return 4;
                }
                // 无参数声明的叶子子命令（git show, git log, git tag）：
                // 第 3 个词是瞬态的（SHA、ref、tag 名）→ 死的过度具体规则，
                // 如 PowerShell(git show 81210f8:*)。这不是 isOptional 的情况 —
                // `git fetch` 声明了可选的 remote/branch，
                // `git fetch origin` 在 bash/prefix.test.ts:912 中被测试为
                // 有意的 remote 限定。
                if subcommand.args.is_none() {
                    return 2;
                }
                return 3;
            }
        }
    }

    if let Some(spec_args) = &spec.args {
        if spec_args.any(|arg| arg.is_command) {
            return if spec_args.is_single()
                && spec_args.first().is_some_and(|arg| arg.is_command)
            {
                2
            } else {
                std::cmp::min(
                    2 + spec_args.position(|arg| arg.is_command).unwrap_or(0) as u32,
                    3,
                )
            };
        }

        if spec.subcommands.is_empty() {
            if spec_args.any(|arg| arg.is_variadic) {
                return 1;
            }
            if let Some(first) = spec_args.first() {
                if !first.is_optional {
                    return 2;
                }
            }
        }
    }

    match &spec.args {
        Some(args_spec) if args_spec.any(|arg| arg.is_dangerous) => 3,
        _ => 2,
    }
}

fn should_stop_at_arg(
    arg: &str,
    prior_args: &[String],
    spec: Option<&CommandSpec>,
) -> bool {
    if arg.starts_with('-') {
        return true;
    }

    // TS 用 UTF-16 索引/长度；此处按 Unicode 标量值计算（char_indices），
    // 对 CLI 参数无实质差异。
    let char_len = arg.chars().count();
    let has_extension = match arg.rfind('.') {
        Some(byte_index) => {
            let dot_char_index = arg[..byte_index].chars().count();
            dot_char_index > 0
                && dot_char_index < char_len - 1
                && !arg[byte_index + 1..].contains(':')
        }
        None => false,
    };

    let has_file = arg.contains('/') || has_extension;
    let has_url = URL_PROTOCOLS.iter().any(|proto| arg.starts_with(proto));

    if !has_file && !has_url {
        return false;
    }

    // 检查是否处于 python 模块的 -m flag 之后
    if let Some(spec) = spec {
        if !spec.options.is_empty()
            && !prior_args.is_empty()
            && prior_args.last().is_some_and(|last| last == "-m")
        {
            let option = spec
                .options
                .iter()
                .find(|opt| opt.name.contains("-m"));
            if option
                .and_then(|option| option.args.as_ref())
                .is_some_and(|args_spec| args_spec.any(|arg_spec| arg_spec.is_module_truthy()))
            {
                return false; // 不要在模块名处停止
            }
        }
    }

    // 对真实文件/URL，无论上下文一律停止
    true
}
