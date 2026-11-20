//! cache_paths.rs — 错误日志/MCP 日志等缓存目录定位。
//!
//! 重写自 `src/utils/cachePaths.ts`。语义对齐说明：
//! - 原文件用 env-paths库 取 cache 目录（Linux = $XDG_CACHE_HOME/claude-cli
//!   或 ~/.cache/claude-cli），此处直接用同套规则展开（xdg.rs）。
//! - TS 的 getFsImplementation().cwd() 只是对 fs 的测试抽象，Rust 侧直接
//!   用 std::env::current_dir()（测试不可注入的偏差，后续按需再抽象）。
//! - sanitizePath 用 djb2Hash 截断超长名（跨升级稳定性要求，见 TS 注释）。

use std::path::PathBuf;

use super::hash::djb2_hash;
use super::xdg::get_xdg_cache_home;

const CACHE_DIR_NAME: &str = "claude-cli";
const MAX_SANITIZED_LENGTH: usize = 200;

/// 取 envPaths('claude-cli') 的 cache 目录：XDG_CACHE_HOME 优先，回退
/// ~/.cache/claude-cli。
fn cache_root() -> Option<PathBuf> {
    if let Some(cache_home) = get_xdg_cache_home(None) {
        return Some(PathBuf::from(cache_home).join(CACHE_DIR_NAME));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join(CACHE_DIR_NAME))
}

/// Local sanitizePath using djb2Hash — NOT the shared version from
/// sessionStoragePortable.ts which uses Bun.hash (wyhash) when available.
/// Cache directory names must remain stable across upgrades so existing cache
/// data (error logs, MCP logs) is not orphaned.
fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let truncated: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    let hash = djb2_hash(name).unsigned_abs();
    format!("{truncated}-{}", radix36(hash))
}

/// `Math.abs(djb2Hash(name)).toString(36)`。
fn radix36(mut n: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "0".to_string())
}

fn project_dir(cwd: &str) -> String {
    sanitize_path(cwd)
}

fn current_project_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .map(|p| project_dir(&p))
}

/// CACHE_PATHS — 缓存目录定位函数集（对应 TS 的 CACHE_PATHS 对象）。
pub struct CachePaths;

impl CachePaths {
    /// <cache>/claude-cli/<sanitized-cwd>
    pub fn base_logs() -> Option<PathBuf> {
        let root = cache_root()?;
        let dir = current_project_dir()?;
        Some(root.join(dir))
    }

    /// <cache>/claude-cli/<sanitized-cwd>/errors
    pub fn errors() -> Option<PathBuf> {
        Some(Self::base_logs()?.join("errors"))
    }

    /// <cache>/claude-cli/<sanitized-cwd>/messages
    pub fn messages() -> Option<PathBuf> {
        Some(Self::base_logs()?.join("messages"))
    }

    /// <cache>/claude-cli/<sanitized-cwd>/mcp-logs-<sanitized-server-name>
    pub fn mcp_logs(server_name: &str) -> Option<PathBuf> {
        // Sanitize server name for Windows compatibility (colons are
        // reserved for drive letters)
        Some(Self::base_logs()?.join(format!("mcp-logs-{}", sanitize_path(server_name))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_non_alnum_with_dash() {
        assert_eq!(sanitize_path("/home/u/code"), "-home-u-code");
        assert_eq!(sanitize_path("server:name!"), "server-name-");
    }

    #[test]
    fn sanitize_short_names_untouched() {
        assert_eq!(sanitize_path("abc123"), "abc123");
    }

    #[test]
    fn sanitize_truncates_with_radix36_hash_suffix() {
        let long = "a".repeat(300);
        let out = sanitize_path(&long);
        // 200 个字母 + '-' + 至少 1 位 radix36
        assert!(out.chars().count() > MAX_SANITIZED_LENGTH + 1);
        let prefix: String = out.chars().take(MAX_SANITIZED_LENGTH).collect();
        assert!(out.starts_with(&prefix));
        let suffix = &out[MAX_SANITIZED_LENGTH..];
        assert!(suffix.starts_with('-'));
        // radix36 chars only after the dash
        assert!(suffix[1..].chars().all(|c| c.is_ascii_digit() || ('a'..='z').contains(&c)));
    }

    #[test]
    fn sanitize_is_stable_for_same_input() {
        let long = "a!b".repeat(100);
        assert_eq!(sanitize_path(&long), sanitize_path(&long));
    }

    #[test]
    fn radix36_matches_to_string_36() {
        assert_eq!(radix36(0), "0");
        assert_eq!(radix36(35), "z");
        assert_eq!(radix36(36), "10");
        assert_eq!(radix36(123456), "2n9c");
    }
}
