//! powershell_provider.rs — PowerShell shell provider。
//!
//! 重写自 `src/utils/shell/powershellProvider.ts`。模块依赖的
//! `sessionEnvVars` 已有 Rust 实现（`crate::utils::session_env_vars`），
//! `ShellProvider` 抽象见 `super::shell_provider`。

use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, MutexGuard};

use super::shell_provider::{ExecCommandOpts, ExecCommandResult, ShellProvider, ShellType};
use crate::utils::session_env_vars;

/// PowerShell invocation flags + command. Shared by the provider's
/// getSpawnArgs and the hook spawn path in hooks.ts so the flag set stays
/// in one place.
pub fn build_powershell_args(cmd: &str) -> Vec<String> {
    vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        cmd.into(),
    ]
}

/// Base64-encode a string as UTF-16LE for PowerShell's -EncodedCommand.
/// Same encoding the parser uses (parser.ts toUtf16LeBase64). The output
/// is [A-Za-z0-9+/=] only — survives ANY shell-quoting layer, including
/// @anthropic-ai/sandbox-runtime's shellquote.quote() which would
/// otherwise corrupt !$? to \!$? when re-wrapping a single-quoted string
/// in double quotes. Review 2964609818.
fn encode_power_shell_command(ps_command: &str) -> String {
    // TS `Buffer.from(s, 'utf16le')`：按 UTF-16 码元（小端）展开，
    // 星空平面字符作为代理对写入两个码元——`encode_utf16` 与之等价。
    let utf16: Vec<u16> = ps_command.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for unit in utf16 {
        bytes.push(unit as u8);
        bytes.push((unit >> 8) as u8);
    }
    base64_encode(&bytes)
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准字母表 + padding 的 base64 编码（Cargo.toml 未引入 base64 crate）。
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(B64_ALPHABET[(n >> 18 & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[(n >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `path/posix` 的 join 语义：始终以 '/' 拼接。
fn posix_join(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub struct PowerShellProvider {
    shell_path: String,
    /// TS 闭包捕获的 `currentSandboxTmpDir`（buildExecCommand 写入，
    /// getEnvironmentOverrides 读取）。
    current_sandbox_tmp_dir: Mutex<Option<String>>,
}

impl PowerShellProvider {
    pub fn new(shell_path: impl Into<String>) -> Self {
        Self {
            shell_path: shell_path.into(),
            current_sandbox_tmp_dir: Mutex::new(None),
        }
    }

    fn lock_sandbox_tmp_dir(&self) -> MutexGuard<'_, Option<String>> {
        self.current_sandbox_tmp_dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// TS: `createPowerShellProvider(shellPath: string): ShellProvider`
pub fn create_power_shell_provider(shell_path: impl Into<String>) -> PowerShellProvider {
    PowerShellProvider::new(shell_path)
}

impl ShellProvider for PowerShellProvider {
    fn shell_type(&self) -> ShellType {
        ShellType::PowerShell
    }

    fn shell_path(&self) -> &str {
        &self.shell_path
    }

    fn detached(&self) -> bool {
        false
    }

    fn build_exec_command(&self, command: &str, opts: &ExecCommandOpts<'_>) -> ExecCommandResult {
        // Stash sandboxTmpDir for getEnvironmentOverrides (mirrors bashProvider)
        let current_sandbox_tmp_dir: Option<String> = if opts.use_sandbox {
            opts.sandbox_tmp_dir.map(str::to_string)
        } else {
            None
        };
        *self.lock_sandbox_tmp_dir() = current_sandbox_tmp_dir.clone();

        // When sandboxed, tmpdir() is not writable — the sandbox only
        // allows writes to sandboxTmpDir. Put the cwd tracking file there
        // so the inner pwsh can actually write it. Only applies on
        // Linux/macOS/WSL2; on Windows native, sandbox is never enabled so
        // this branch is dead.
        let file_name = format!("claude-pwd-ps-{}", opts.id);
        let cwd_file_path = match &current_sandbox_tmp_dir {
            Some(sandbox_tmp_dir) => posix_join(sandbox_tmp_dir, &file_name),
            None => env::temp_dir()
                .join(&file_name)
                .to_string_lossy()
                .into_owned(),
        };
        let escaped_cwd_file_path = cwd_file_path.replace('\'', "''");
        // Exit-code capture: prefer $LASTEXITCODE when a native exe ran.
        // On PS 5.1, a native command that writes to stderr while the
        // stream is PS-redirected (e.g. `git push 2>&1`) sets $? = $false
        // even when the exe returned exit 0 — so `if (!$?)` reports a
        // false positive. $LASTEXITCODE is $null only when no native exe
        // has run in the session; in that case fall back to $? for
        // cmdlet-only pipelines. Tradeoff: `native-ok; cmdlet-fail` now
        // returns 0 (was 1). Reverse is also true: `native-fail; cmdlet-ok`
        // now returns the native exit code (was 0 — old logic only looked
        // at $? which the trailing cmdlet set true). Both rarer than the
        // git/npm/curl stderr case.
        let cwd_tracking = format!(
            "\n; $_ec = if ($null -ne $LASTEXITCODE) {{ $LASTEXITCODE }} \
             elseif ($?) {{ 0 }} else {{ 1 }}\n\
             ; (Get-Location).Path | Out-File -FilePath '{escaped_cwd_file_path}' \
             -Encoding utf8 -NoNewline\n\
             ; exit $_ec"
        );
        let ps_command = command.to_string() + &cwd_tracking;

        // Sandbox wraps the returned commandString as
        // `<binShell> -c '<cmd>'` — hardcoded `-c`, no way to inject
        // -NoProfile -NonInteractive. So for the sandbox path, build a
        // command that itself invokes pwsh with the full flag set.
        // Shell.ts passes /bin/sh as the sandbox binShell, producing:
        // `bwrap ... sh -c 'pwsh -NoProfile ... -EncodedCommand ...'`.
        // The non-sandbox path returns the bare PS command; getSpawnArgs()
        // adds the flags via buildPowerShellArgs().
        //
        // -EncodedCommand (base64 UTF-16LE), not -Command: the sandbox
        // runtime applies its OWN shellquote.quote() on top of whatever we
        // build. Any string containing ' triggers double-quote mode which
        // escapes ! as \! — POSIX sh preserves that literally, pwsh parse
        // error. Base64 is [A-Za-z0-9+/=] — no chars that any quoting
        // layer can corrupt. Review 2964609818.
        //
        // shellPath is POSIX-single-quoted so a space-containing install
        // path (e.g. /opt/my tools/pwsh) survives the inner
        // `/bin/sh -c` word-split. Flags and base64 are [A-Za-z0-9+/=-]
        // only — no quoting needed.
        let command_string = if opts.use_sandbox {
            let quoted_shell_path = format!("'{}'", self.shell_path.replace('\'', "'\\''"));
            let encoded = encode_power_shell_command(&ps_command);
            [
                &quoted_shell_path,
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encoded,
            ]
            .join(" ")
        } else {
            ps_command
        };

        ExecCommandResult {
            command_string,
            cwd_file_path,
        }
    }

    fn get_spawn_args(&self, command_string: &str) -> Vec<String> {
        build_powershell_args(command_string)
    }

    fn get_environment_overrides(&self, _command: &str) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = HashMap::new();
        // Apply session env vars set via /env (child processes only, not
        // the REPL). Without this, `/env PATH=...` affects Bash tool
        // commands but not PowerShell — so PyCharm users with a stripped
        // PATH can't self-rescue.
        // Ordering: session vars FIRST so the sandbox TMPDIR below can't
        // be overridden by `/env TMPDIR=...`. bashProvider.ts has these in
        // the opposite order (pre-existing), but sandbox isolation should
        // win.
        for (key, value) in session_env_vars::get_session_env_vars().iter() {
            env.insert(key.clone(), value.clone());
        }
        if let Some(current_sandbox_tmp_dir) = &*self.lock_sandbox_tmp_dir() {
            // PowerShell on Linux/macOS honors TMPDIR for
            // [System.IO.Path]::GetTempPath()
            env.insert("TMPDIR".into(), current_sandbox_tmp_dir.clone());
            env.insert("CLAUDE_CODE_TMPDIR".into(), current_sandbox_tmp_dir.clone());
        }
        env
    }
}
