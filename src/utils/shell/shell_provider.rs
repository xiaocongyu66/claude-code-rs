//! shell_provider.rs — shell provider 抽象（bash / powershell）。
//!
//! 重写自 `src/utils/shell/shellProvider.ts`。TS 的对象类型 `ShellProvider`
//! 在 Rust 中映射为 trait；数据字段 `type` / `shellPath` / `detached`
//! 映射为访问方法。
//!
//! 对齐说明：`buildExecCommand` / `getEnvironmentOverrides` 在 TS 中是
//! async（bash provider 的 tmux socket 初始化是异步的），但 Cargo.toml
//! 暂无异步运行时，故此处为同步签名；待 bash provider 落地后再统一评估。

use std::collections::HashMap;

/// TS: `SHELL_TYPES = ['bash', 'powershell'] as const`
pub const SHELL_TYPES: &[ShellType] = &[ShellType::Bash, ShellType::PowerShell];

/// TS: `DEFAULT_HOOK_SHELL: ShellType = 'bash'`
pub const DEFAULT_HOOK_SHELL: ShellType = ShellType::Bash;

/// TS: `ShellType = 'bash' | 'powershell'`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    Bash,
    PowerShell,
}

impl ShellType {
    /// 与 TS 字符串字面量一致的表示。
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellType::Bash => "bash",
            ShellType::PowerShell => "powershell",
        }
    }
}

/// `buildExecCommand` 的参数（TS 同名 opts）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommandOpts<'a> {
    /// TS: `id: number | string`。仅用于拼接 cwd 跟踪文件名，故统一为字符串。
    pub id: String,
    pub sandbox_tmp_dir: Option<&'a str>,
    pub use_sandbox: bool,
}

/// `buildExecCommand` 的返回（TS: `{ commandString, cwdFilePath }`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommandResult {
    pub command_string: String,
    pub cwd_file_path: String,
}

/// TS: `ShellProvider` 对象类型。
pub trait ShellProvider {
    fn shell_type(&self) -> ShellType;
    fn shell_path(&self) -> &str;
    fn detached(&self) -> bool;

    /// Build the full command string including all shell-specific setup.
    /// For bash: source snapshot, session env, disable extglob, eval-wrap,
    /// pwd tracking.
    fn build_exec_command(&self, command: &str, opts: &ExecCommandOpts<'_>) -> ExecCommandResult;

    /// Shell args for spawn (e.g., `['-c', '-l', cmd]` for bash).
    fn get_spawn_args(&self, command_string: &str) -> Vec<String>;

    /// Extra env vars for this shell type.
    fn get_environment_overrides(&self, command: &str) -> HashMap<String, String>;
}
