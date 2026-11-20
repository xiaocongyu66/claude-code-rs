//! path.rs — 路径展开与辅助函数。
//!
//! 重写自 `src/utils/path.ts`。语义对齐：
//!
//! - `expand_path`：把可能含 `~` 的路径展开为绝对路径。Windows 上
//!   POSIX 风格路径（如 `/c/Users/...`）自动转成 Windows 格式；
//!   返回值始终为当前平台的原生格式。所有路径做 NFC 规范化
//!   （对齐 TS 的 `.normalize('NFC')`）。空/纯空白路径返回
//!   baseDir 的规范化结果。
//! - `to_relative_path`：绝对路径转相对 cwd 的路径以省 token；
//!   在 cwd 之外（以 .. 开头）则原样返回。
//! - `get_directory_for_path`：目录则返回自身，文件/不存在则返回父目录；
//!   UNC 路径**不做文件系统操作**（防 NTLM 凭证泄漏）。
//! - `contains_path_traversal`：检测 `../`、`..\`、以 `..` 结尾的模式。
//! - `normalize_path_for_config_key`：规范化后统一用正斜杠，做 JSON key。
//!
//! 偏差：
//! - TS 用 Node `path` 模块的跨平台语义；Rust 侧 std::path 在 Windows
//!   上行为不同，路径规范化用本模块的词法实现（不解析符号链接）。
//! - `sanitizePath` 的再导出未带过来：sessionStoragePortable 尚未转
//!   Rust，该函数将由其对应模块自行提供。
//! - `homedir()` 用 HOME / USERPROFILE 环境变量近似（无 dirs crate）。

use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

use crate::utils::fs_operations::StdFs;
use crate::utils::fs_operations::FsOperations;
use crate::utils::platform::{get_platform, Platform};
use crate::utils::windows_paths::posix_path_to_windows_path;

fn home_dir() -> String {
    if cfg!(windows) {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    } else {
        std::env::var("HOME").unwrap_or_default()
    }
}

/// 词法规范化：解析 `.` 与 `..`、折叠重复分隔符（不触碰文件系统）。
/// 以 `/` 统一处理（TS 的 path 行为在 POSIX 下如此；Windows 分支由
/// expand_path 的 posix 处理路径走这条）。
fn lexical_normalize(path: &str) -> String {
    let is_absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if *last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                parts.push("..");
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    match (is_absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

fn lexical_resolve(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        lexical_normalize(path)
    } else {
        lexical_normalize(&format!("{base}/{path}"))
    }
}

fn is_synthetic_posix_path(value: &str) -> bool {
    value.contains('/') && !value.contains('\\') && !regex_drive_prefix(value)
}

fn regex_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_absolute_path(p: &str) -> bool {
    p.starts_with('/') || (cfg!(windows) && regex_drive_prefix(p)) || p.starts_with('\\')
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// 展开 `~` / `~/...`、解析相对路径为绝对路径，返回平台原生格式。
///
/// - `~` → 用户主目录；`~/x` → 主目录下 x
/// - 绝对路径直接规范化
/// - 相对路径相对 base_dir（缺省 cwd）解析
/// - Windows 上 `/c/Users/...` 形式的 POSIX 路径先转成 Windows 格式
pub fn expand_path(path: &str, base_dir: Option<&str>) -> Result<String, ExpandPathError> {
    let actual_base_dir = match base_dir {
        Some(b) => b.to_string(),
        None => match crate::utils::cwd::get_cwd() {
            cwd if !cwd.is_empty() => cwd,
            _ => StdFs.cwd().map_err(ExpandPathError::Io)?,
        },
    };

    if actual_base_dir.contains('\0') {
        return Err(ExpandPathError::NullBytes);
    }
    if path.contains('\0') {
        return Err(ExpandPathError::NullBytes);
    }

    // 空或纯空白路径：Windows 上若 base 是合成 POSIX 路径，按 posix 规范化
    let trimmed_path = path.trim();
    if trimmed_path.is_empty() {
        if get_platform() == Platform::Windows && is_synthetic_posix_path(&actual_base_dir) {
            return Ok(nfc(&lexical_normalize(&actual_base_dir)));
        }
        return Ok(nfc(&lexical_normalize(&actual_base_dir)));
    }

    // 家目录标记
    if trimmed_path == "~" {
        return Ok(nfc(&lexical_normalize(&home_dir())));
    }
    if let Some(rest) = trimmed_path.strip_prefix("~/") {
        return Ok(nfc(&lexical_resolve(&home_dir(), rest)));
    }

    // Windows 上把 POSIX 风格路径（/c/Users/...）转成 Windows 格式
    let mut processed_path = trimmed_path.to_string();
    if get_platform() == Platform::Windows {
        let bytes = trimmed_path.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
            processed_path = posix_path_to_windows_path(trimmed_path);
        }
    }

    // 绝对路径
    if is_absolute_path(&processed_path) {
        if get_platform() == Platform::Windows && is_synthetic_posix_path(&processed_path) {
            return Ok(nfc(&lexical_normalize(&processed_path)));
        }
        return Ok(nfc(&lexical_normalize(&processed_path)));
    }

    // 相对路径
    if get_platform() == Platform::Windows
        && is_synthetic_posix_path(&actual_base_dir)
        && !regex_drive_prefix(&processed_path)
        && !processed_path.starts_with("\\\\")
    {
        return Ok(nfc(&lexical_resolve(
            &lexical_normalize(&actual_base_dir),
            &processed_path,
        )));
    }
    Ok(nfc(&lexical_resolve(
        &lexical_normalize(&actual_base_dir),
        &processed_path,
    )))
}

#[derive(Debug, thiserror::Error)]
pub enum ExpandPathError {
    #[error("Path contains null bytes")]
    NullBytes,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 把绝对路径转成相对 cwd 的路径（省 token）。
/// 在 cwd 之外（以 .. 开头）则原样返回绝对路径。
pub fn to_relative_path(absolute_path: &str) -> String {
    let cwd = crate::utils::cwd::get_cwd();
    let relative_path = lexical_relative(&cwd, absolute_path);
    if relative_path.starts_with("..") {
        absolute_path.to_string()
    } else {
        relative_path
    }
}

fn lexical_relative(from: &str, to: &str) -> String {
    let from = lexical_normalize(from);
    let to = lexical_normalize(to);
    if from == to {
        return String::new();
    }
    let from_parts: Vec<&str> = from.split('/').collect();
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_parts.len() - common;
    let mut result: Vec<String> = std::iter::repeat("..".to_string()).take(ups).collect();
    result.extend(to_parts[common..].iter().map(|s| s.to_string()));
    result.join("/")
}

/// 取文件/目录路径的目录部分。
/// 是目录返回自身；是文件或不存在则返回父目录。
pub fn get_directory_for_path(path: &str) -> String {
    let absolute_path = expand_path(path, None).unwrap_or_else(|_| path.to_string());
    // SECURITY: UNC 路径不做文件系统操作，防 NTLM 凭证泄漏。
    if absolute_path.starts_with("\\\\") || absolute_path.starts_with("//") {
        return dirname(&absolute_path);
    }
    if let Ok(stats) = StdFs.stat_sync(&absolute_path) {
        if stats.is_dir() {
            return absolute_path;
        }
    }
    dirname(&absolute_path)
}

fn dirname(path: &str) -> String {
    let normalized = lexical_normalize(path);
    match normalized.trim_end_matches('/').rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => normalized[..idx].to_string(),
        None => ".".to_string(),
    }
}

/// 路径是否含指向父目录的穿越模式（`../`、`..\`、以 `..` 结尾）。
pub fn contains_path_traversal(path: &str) -> bool {
    // 等价 TS 正则 /(?:^|[\\/])\.\.(?:[\\/]|$)/
    let bytes = path.as_bytes();
    let mut i = 0usize;
    while i + 2 <= bytes.len() {
        if bytes[i] == b'.' && bytes[i + 1] == b'.' {
            let before_ok = i == 0 || bytes[i - 1] == b'/' || bytes[i - 1] == b'\\';
            let after_idx = i + 2;
            let after_ok = after_idx == bytes.len()
                || bytes[after_idx] == b'/'
                || bytes[after_idx] == b'\\';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 为 JSON 配置 key 规范化路径：先词法规范化（解析 . 和 ..），
/// 再把反斜杠统一为正斜杠（Windows 上 git / Node / 用户输入混合斜杠）。
pub fn normalize_path_for_config_key(path: &str) -> String {
    lexical_normalize(path).replace('\\', "/")
}
