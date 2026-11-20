//! xdg.rs — Claude CLI Native Installer 的 XDG Base Directory 工具。
//!
//! 重写自 `src/utils/xdg.ts`。按 XDG Base Directory 规范组织 native
//! installer 各组件的系统目录位置。
//!
//! @see https://specifications.freedesktop.org/basedir-spec/latest/

use std::collections::HashMap;

/// 环境变量映射（`XDG_STATE_HOME` 等键）。`None` 时读进程环境，
/// 对齐 TS 的 `process.env`。
pub type Env = HashMap<String, String>;

/// 覆盖项：`env`（环境变量映射）与 `homedir`（家目录），用于测试注入。
#[derive(Debug, Clone, Default)]
pub struct XdgOptions {
    pub env: Option<Env>,
    pub homedir: Option<String>,
}

struct Resolved {
    env: Env,
    home: Option<String>,
}

fn resolve_env(options: &XdgOptions) -> Env {
    match &options.env {
        Some(env) => env.clone(),
        None => std::env::vars().collect(),
    }
}

/// 解析优先级：`options.homedir` > `env.HOME` > 操作系统家目录。
fn resolve_home(env: &Env, options: &XdgOptions) -> Option<String> {
    if let Some(home) = &options.homedir {
        return Some(home.clone());
    }
    if let Some(home) = env.get("HOME") {
        return Some(home.clone());
    }
    // 回落到系统 home 目录（对齐 Node `os.homedir()`）。
    // std::env::home_dir 在目标平台读 HOME / passwd 条目。
    os_home_dir()
}

#[allow(deprecated)] // 对齐 os.homedir() 的语义
fn os_home_dir() -> Option<String> {
    std::env::home_dir().map(|path| path.to_string_lossy().into_owned())
}

fn resolve(options: &XdgOptions) -> Resolved {
    let env = resolve_env(options);
    let home = resolve_home(&env, options);
    Resolved { env, home }
}

fn is_windows_drive(base: &str) -> bool {
    let mut chars = base.chars();
    let drive_letter = match chars.next() {
        Some(c) => c.is_ascii_alphabetic(),
        None => return false,
    };
    drive_letter && chars.next() == Some(':')
}

/// 便携 join：base 形如 POSIX 风格（含 `/`、不含 `\`、不是 Windows
/// 盘符）时用 `/` 连接，否则用平台原生分隔符连接。
fn join_portable(base: &str, parts: &[&str]) -> String {
    if base.contains('/') && !base.contains('\\') && !is_windows_drive(base) {
        let mut out = String::from(base);
        for part in parts {
            out.push('/');
            out.push_str(part);
        }
        out
    } else {
        let mut path = std::path::PathBuf::from(base);
        for part in parts {
            path.push(part);
        }
        path.to_string_lossy().into_owned()
    }
}

/// XDG state home 目录（默认 `~/.local/state`）。
///
/// 家目录不可解析时返回 None（TS 源码中 `os.homedir()` 会抛错）。
pub fn get_xdg_state_home(options: Option<&XdgOptions>) -> Option<String> {
    let options = options.cloned().unwrap_or_default();
    let resolved = resolve(&options);
    resolved
        .env
        .get("XDG_STATE_HOME")
        .cloned()
        .or_else(|| {
            resolved
                .home
                .map(|home| join_portable(&home, &[".local", "state"]))
        })
}

/// XDG cache home 目录（默认 `~/.cache`）。
pub fn get_xdg_cache_home(options: Option<&XdgOptions>) -> Option<String> {
    let options = options.cloned().unwrap_or_default();
    let resolved = resolve(&options);
    resolved
        .env
        .get("XDG_CACHE_HOME")
        .cloned()
        .or_else(|| resolved.home.map(|home| join_portable(&home, &[".cache"])))
}

/// XDG data home 目录（默认 `~/.local/share`）。
pub fn get_xdg_data_home(options: Option<&XdgOptions>) -> Option<String> {
    let options = options.cloned().unwrap_or_default();
    let resolved = resolve(&options);
    resolved
        .env
        .get("XDG_DATA_HOME")
        .cloned()
        .or_else(|| {
            resolved
                .home
                .map(|home| join_portable(&home, &[".local", "share"]))
        })
}

/// 用户 bin 目录（严格说不是 XDG，但遵循同一约定），默认 `~/.local/bin`。
pub fn get_user_bin_dir(options: Option<&XdgOptions>) -> Option<String> {
    let options = options.cloned().unwrap_or_default();
    let resolved = resolve(&options);
    resolved
        .home
        .map(|home| join_portable(&home, &[".local", "bin"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(env: &[(&str, &str)], home: Option<&str>) -> XdgOptions {
        XdgOptions {
            env: Some(
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            homedir: home.map(|h| h.to_string()),
        }
    }

    #[test]
    fn defaults_from_homedir() {
        let options = opts(&[], Some("/home/test"));
        assert_eq!(
            get_xdg_state_home(Some(&options)),
            Some("/home/test/.local/state".to_string())
        );
        assert_eq!(
            get_xdg_cache_home(Some(&options)),
            Some("/home/test/.cache".to_string())
        );
        assert_eq!(
            get_xdg_data_home(Some(&options)),
            Some("/home/test/.local/share".to_string())
        );
        assert_eq!(
            get_user_bin_dir(Some(&options)),
            Some("/home/test/.local/bin".to_string())
        );
    }

    #[test]
    fn xdg_env_vars_take_precedence() {
        let options = opts(
            &[("XDG_STATE_HOME", "/tmp/state"), ("XDG_CACHE_HOME", "/tmp/cache")],
            Some("/home/test"),
        );
        assert_eq!(
            get_xdg_state_home(Some(&options)),
            Some("/tmp/state".to_string())
        );
        assert_eq!(
            get_xdg_cache_home(Some(&options)),
            Some("/tmp/cache".to_string())
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_drive_paths_use_platform_join() {
        let options = opts(&[], Some("C:\\Users\\test"));
        let bin = get_user_bin_dir(Some(&options));
        assert_eq!(bin, Some("C:\\Users\\test\\.local\\bin".to_string()));
    }

    #[test]
    #[cfg(not(windows))]
    fn posix_homedir_uses_posix_joins() {
        let options = opts(&[], Some("/home/test"));
        assert!(get_xdg_state_home(Some(&options))
            .unwrap()
            .starts_with("/home/test/"));
    }
}
