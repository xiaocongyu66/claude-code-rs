//! registry.rs — 命令规格（fig spec）注册表。
//!
//! 重写自 `src/utils/bash/registry.ts`，`specs/*.ts` 的 7 条内置规格
//! （pyright / timeout / sleep / alias / nohup / time / srun）以内联常量
//! 移植，顺序对齐 `specs/index.ts`。描述文本取 i18n 默认回退值（英文原文，
//! 即 `t()` 无翻译时的行为）。
//!
//! 已知差异：
//! - `load_fig_spec` 的动态 `import('@withfig/autocomplete/build/…')` 在 Rust
//!   端无运行时 npm 加载能力，始终返回 `None`（加载失败语义相同）。
//!   本地内置规格优先，仅未知命令才回落，语义不因缺少动态加载改变。
//! - TS 的 `getCommandSpec` 是 `memoizeWithLRU`（lru-cache，默认 max=100）
//!   包装的 async 函数；Rust 用手写 LRU（VecDeque，容量 100）同步等价。
//! - TS 的 `CommandSpec.args` 是 `Argument | Argument[]`，Rust 归一化为
//!   `Vec<Argument>`（空数组等价缺省）。`Option.name` 同理归一化为
//!   `OptionName::Single / Many`。
//! - TS `spec.name: string | string[]` 归一化为 OptionName 枚举。

use std::collections::VecDeque;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// 命令规格（对齐 TS CommandSpec）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CommandSpec {
    pub name: String,
    pub description: Option<String>,
    /// 子命令（对齐 TS subcommands?: CommandSpec[]）。
    pub subcommands: Vec<CommandSpec>,
    /// 参数（对齐 TS args?: Argument | Argument[]，归一化为数组）。
    pub args: Vec<Argument>,
    /// 选项（对齐 TS options?: Option[]）。
    pub options: Vec<CommandOption>,
}

/// 命令参数（对齐 TS Argument）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Argument {
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_dangerous: bool,
    /// 无限重复（如 echo hello world）。
    pub is_variadic: bool,
    pub is_optional: bool,
    /// 包装命令（timeout、sudo 等）。
    pub is_command: bool,
    /// 模块参数（python -m 等）。
    pub is_module: Option<IsModule>,
}

/// `isModule: string | boolean` 的联合类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IsModule {
    Flag(bool),
    Name(String),
}

/// 命令选项（对齐 TS Option，`name` 归一化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CommandOption {
    pub name: OptionName,
    pub description: Option<String>,
    /// 选项参数（对齐 TS args?: Argument | Argument[]）。
    pub args: Vec<Argument>,
    pub is_required: bool,
}

/// 选项名：单个或多个别名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionName {
    Single(String),
    Many(Vec<String>),
}

impl Default for OptionName {
    fn default() -> Self {
        OptionName::Single(String::new())
    }
}

impl CommandSpec {
    fn new(name: &str) -> Self {
        CommandSpec {
            name: name.to_string(),
            ..Default::default()
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 内置规格（specs/*.ts）
// ────────────────────────────────────────────────────────────────────────────

/// 内置命令规格，顺序对齐 specs/index.ts。
static BUILTIN_SPECS: Lazy<Vec<CommandSpec>> = Lazy::new(pyright_spec);

fn pyright_spec() -> Vec<CommandSpec> {
    let mut pyright = CommandSpec::new("pyright");
    pyright.description = Some("Type checker for Python".into());
    pyright.options = vec![
        CommandOption {
            name: OptionName::Many(vec!["--help".into(), "-h".into()]),
            description: Some("Show help message".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--version".into()),
            description: Some("Print pyright version and exit".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Many(vec!["--watch".into(), "-w".into()]),
            description: Some("Continue to run and watch for changes".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Many(vec!["--project".into(), "-p".into()]),
            description: Some("Use the configuration file at this location".into()),
            args: vec![Argument {
                name: Some("FILE OR DIRECTORY".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("-".into()),
            description: Some("Read file or directory list from stdin".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--createstub".into()),
            description: Some("Create type stub file(s) for import".into()),
            args: vec![Argument {
                name: Some("IMPORT".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Many(vec!["--typeshedpath".into(), "-t".into()]),
            description: Some("Use typeshed type stubs at this location".into()),
            args: vec![Argument {
                name: Some("DIRECTORY".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--verifytypes".into()),
            description: Some("Verify completeness of types in py.typed package".into()),
            args: vec![Argument {
                name: Some("IMPORT".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--ignoreexternal".into()),
            description: Some("Ignore external imports for --verifytypes".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--pythonpath".into()),
            description: Some("Path to the Python interpreter".into()),
            args: vec![Argument {
                name: Some("FILE".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--pythonplatform".into()),
            description: Some("Analyze for platform".into()),
            args: vec![Argument {
                name: Some("PLATFORM".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--pythonversion".into()),
            description: Some("Analyze for Python version".into()),
            args: vec![Argument {
                name: Some("VERSION".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Many(vec!["--venvpath".into(), "-v".into()]),
            description: Some("Directory that contains virtual environments".into()),
            args: vec![Argument {
                name: Some("DIRECTORY".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--outputjson".into()),
            description: Some("Output results in JSON format".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--verbose".into()),
            description: Some("Emit verbose diagnostics".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--stats".into()),
            description: Some("Print detailed performance stats".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--dependencies".into()),
            description: Some("Emit import dependency information".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--level".into()),
            description: Some("Minimum diagnostic level".into()),
            args: vec![Argument {
                name: Some("LEVEL".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--skipunannotated".into()),
            description: Some("Skip type analysis of unannotated functions".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--warnings".into()),
            description: Some("Use exit code of 1 if warnings are reported".into()),
            ..Default::default()
        },
        CommandOption {
            name: OptionName::Single("--threads".into()),
            description: Some("Use up to N threads to parallelize type checking".into()),
            args: vec![Argument {
                name: Some("N".into()),
                is_optional: true,
                ..Default::default()
            }],
            ..Default::default()
        },
    ];
    pyright.args = vec![Argument {
        name: Some("files".into()),
        description: Some(
            "Specify files or directories to analyze (overrides config file)".into(),
        ),
        is_variadic: true,
        is_optional: true,
        ..Default::default()
    }];

    let timeout = CommandSpec {
        name: "timeout".into(),
        description: Some("Run a command with a time limit".into()),
        args: vec![
            Argument {
                name: Some("duration".into()),
                description: Some(
                    "Duration to wait before timing out (e.g., 10, 5s, 2m)".into(),
                ),
                is_optional: false,
                ..Default::default()
            },
            Argument {
                name: Some("command".into()),
                description: Some("Command to run".into()),
                is_command: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let sleep = CommandSpec {
        name: "sleep".into(),
        description: Some("Delay for a specified amount of time".into()),
        args: vec![Argument {
            name: Some("duration".into()),
            description: Some(
                "Duration to sleep (seconds or with suffix like 5s, 2m, 1h)".into(),
            ),
            is_optional: false,
            ..Default::default()
        }],
        ..Default::default()
    };

    let alias = CommandSpec {
        name: "alias".into(),
        description: Some("Create or list command aliases".into()),
        args: vec![Argument {
            name: Some("definition".into()),
            description: Some("Alias definition in the form name=value".into()),
            is_optional: true,
            is_variadic: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let nohup = CommandSpec {
        name: "nohup".into(),
        description: Some("Run a command immune to hangups".into()),
        args: vec![Argument {
            name: Some("command".into()),
            description: Some("Command to run with nohup".into()),
            is_command: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let time = CommandSpec {
        name: "time".into(),
        description: Some("Time a command".into()),
        args: vec![Argument {
            name: Some("command".into()),
            description: Some("Command to time".into()),
            is_command: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let srun = CommandSpec {
        name: "srun".into(),
        description: Some("Run a command on SLURM cluster nodes".into()),
        options: vec![
            CommandOption {
                name: OptionName::Many(vec!["-n".into(), "--ntasks".into()]),
                description: Some("Number of tasks".into()),
                args: vec![Argument {
                    name: Some("count".into()),
                    description: Some("Number of tasks to run".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            CommandOption {
                name: OptionName::Many(vec!["-N".into(), "--nodes".into()]),
                description: Some("Number of nodes".into()),
                args: vec![Argument {
                    name: Some("count".into()),
                    description: Some("Number of nodes to allocate".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        args: vec![Argument {
            name: Some("command".into()),
            description: Some("Command to run on the cluster".into()),
            is_command: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    // 顺序对齐 specs/index.ts：pyright, timeout, sleep, alias, nohup, time, srun。
    vec![pyright, timeout, sleep, alias, nohup, time, srun]
}

// ────────────────────────────────────────────────────────────────────────────
// fig spec 动态加载（Rust 侧不可用）
// ────────────────────────────────────────────────────────────────────────────

/// `loadFigSpec`：动态加载 `@withfig/autocomplete/build/<command>.js`。
///
/// Rust 运行时无 npm 包加载能力，始终返回 `None`（与 TS 的加载失败一致）。
/// 命令名守卫逻辑对齐 TS：空 / 含 `/` `\` `..` / `-` 开头（`-` 除外）无效。
pub fn load_fig_spec(command: &str) -> Option<CommandSpec> {
    if command.is_empty()
        || command.contains('/')
        || command.contains('\\')
        || command.contains("..")
        || (command.starts_with('-') && command != "-")
    {
        return None;
    }
    // 无 @withfig/autocomplete 加载能力：等价加载失败。
    None
}

// ────────────────────────────────────────────────────────────────────────────
// getCommandSpec（memoizeWithLRU 等价）
// ────────────────────────────────────────────────────────────────────────────

/// LRU 缓存（对齐 memoizeWithLRU 默认 max=100）。
struct LruSpecCache {
    max: usize,
    entries: VecDeque<(String, CommandSpec)>,
}

impl LruSpecCache {
    fn new(max: usize) -> Self {
        LruSpecCache {
            max,
            entries: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<CommandSpec> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, v) = self.entries.remove(pos)?;
        let v = v.clone();
        self.entries.push_front((k, v.clone()));
        Some(v)
    }

    fn set(&mut self, key: String, value: CommandSpec) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        }
        self.entries.push_front((key, value));
        while self.entries.len() > self.max {
            self.entries.pop_back();
        }
    }
}

static SPEC_CACHE: Lazy<std::sync::Mutex<LruSpecCache>> =
    Lazy::new(|| std::sync::Mutex::new(LruSpecCache::new(100)));

/// `getCommandSpec(command)`：先查内置规格，再回落动态加载；LRU 记忆。
/// TS 为 async（动态 import 天然异步），Rust 同步（动态加载不可用）。
pub fn get_command_spec(command: &str) -> Option<CommandSpec> {
    {
        let mut cache = SPEC_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(spec) = cache.get(command) {
            return Some(spec);
        }
    }
    let spec = BUILTIN_SPECS
        .iter()
        .find(|s| s.name == command)
        .cloned()
        .or_else(|| load_fig_spec(command));
    if let Some(ref spec) = spec {
        let mut cache = SPEC_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.set(command.to_string(), spec.clone());
    }
    spec
}

/// 仅供测试：清空 LRU 缓存（对齐 memoizeWithLRU 的 `.cache.clear()`）。
pub fn clear_command_spec_cache() {
    let mut cache = SPEC_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entries.clear();
}

/// 仅供测试：检查缓存中是否已存在某命令条目。
#[cfg(test)]
pub fn command_spec_cache_contains(command: &str) -> bool {
    let cache = SPEC_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entries.iter().any(|(k, _)| k == command)
}

