//! platform.rs — 平台判定与 VCS 检测（重写自 `src/utils/platform.ts`）。
//!
//! 语义对齐说明：
//! - TS 的 `process.platform` 对齐 `std::env::consts::OS`
//!   （`darwin` → macos / `win32` → windows / `linux` → linux）。
//! - `memoize(...)`（lodash）对齐 `static X: OnceLock<T>` + `get_or_init`，
//!   进程内只算一次。
//! - `os.release()`（内核版本）只在 Linux 分支使用；Rust 无稳定 API，
//!   改读 `/proc/sys/kernel/osrelease`（仅 Linux），失败回退空串。
//! - TS 的异步 `readFile('/etc/os-release')` 与 `readdir()` 在本模块中
//!   走 `fs_operations` 的同步 trait（项目约定 TS 异步 fs 落地为同步），
//!   `getLinuxDistroInfo` 与 `detectVcs` 因此为同步函数。
//! - `/proc/version` 读取失败时记日志并回退（对齐 TS 的 try/catch +
//!   `logError`）。

use std::sync::OnceLock;

use regex::Regex;

use crate::utils::fs_operations::{get_fs_implementation, BufferEncoding};
use crate::utils::log::log_error_dyn;

/// 平台类型（对齐 TS `Platform`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Macos,
    Windows,
    Wsl,
    Linux,
    Unknown,
}

impl Platform {
    /// 与 TS 字面量一致的小写字符串（用于序列化/日志）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Macos => "macos",
            Platform::Windows => "windows",
            Platform::Wsl => "wsl",
            Platform::Linux => "linux",
            Platform::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 支持的平台（对齐 TS `SUPPORTED_PLATFORMS: Platform[]`）。
pub const SUPPORTED_PLATFORMS: &[Platform] = &[Platform::Macos, Platform::Wsl];

/// 当前平台（进程内只判定一次；失败回退 `unknown` 且记日志）。
pub fn get_platform() -> Platform {
    static PLATFORM: OnceLock<Platform> = OnceLock::new();
    *PLATFORM.get_or_init(detect_platform)
}

fn detect_platform() -> Platform {
    // std::env::consts::OS：macos/windows/linux/…（对齐 process.platform）
    match std::env::consts::OS {
        "macos" => Platform::Macos,
        "windows" => Platform::Windows,
        "linux" => {
            // 检查是否运行在 WSL：读 /proc/version 找 microsoft / wsl 标记
            match read_proc_version() {
                Some(proc_version) => {
                    let lower = proc_version.to_ascii_lowercase();
                    if lower.contains("microsoft") || lower.contains("wsl") {
                        Platform::Wsl
                    } else {
                        Platform::Linux
                    }
                }
                None => Platform::Linux,
            }
        }
        _ => Platform::Unknown,
    }
}

/// WSL 版本号（"1"/"2"/…）；仅 Linux 有意义，非 Linux 或无法判定返回 None。
pub fn get_wsl_version() -> Option<&'static str> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION.get_or_init(detect_wsl_version).as_deref()
}

fn detect_wsl_version() -> Option<String> {
    if std::env::consts::OS != "linux" {
        return None;
    }
    let proc_version = read_proc_version()?;
    // 先找显式版本标记（"WSL2"、"WSL3" …）
    let re = Regex::new(r"(?i)WSL(\d+)").ok()?;
    if let Some(caps) = re.captures(&proc_version) {
        if let Some(num) = caps.get(1) {
            return Some(num.as_str().to_string());
        }
    }
    // 无显式版本但含 Microsoft：WSL1（原始格式 "4.4.0-19041-Microsoft"）
    if proc_version.to_ascii_lowercase().contains("microsoft") {
        return Some("1".to_string());
    }
    None
}

/// Linux 发行版信息（对齐 TS `LinuxDistroInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxDistroInfo {
    pub linux_distro_id: Option<String>,
    pub linux_distro_version: Option<String>,
    pub linux_kernel: String,
}

/// 读取 /etc/os-release 的 ID / VERSION_ID 与内核版本；非 Linux 或
/// os-release 缺失时仍返回（id/version 为 None）。
pub fn get_linux_distro_info() -> Option<LinuxDistroInfo> {
    static INFO: OnceLock<Option<LinuxDistroInfo>> = OnceLock::new();
    INFO.get_or_init(detect_distro_info).clone()
}

fn detect_distro_info() -> Option<LinuxDistroInfo> {
    if std::env::consts::OS != "linux" {
        return None;
    }

    let mut result = LinuxDistroInfo {
        linux_distro_id: None,
        linux_distro_version: None,
        // os.release() → /proc/sys/kernel/osrelease（失败回退空串）
        linux_kernel: read_osrelease().unwrap_or_default(),
    };

    // /etc/os-release 可能不存在于所有 Linux 系统 → 静默跳过
    if let Ok(content) = get_fs_implementation().read_file_sync(
        "/etc/os-release",
        BufferEncoding::Utf8,
    ) {
        let re = match Regex::new(r"^(ID|VERSION_ID)=(.*)$") {
            Ok(re) => re,
            Err(_) => return Some(result),
        };
        for line in content.split('\n') {
            if let Some(caps) = re.captures(line) {
                let key = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
                let value = caps
                    .get(2)
                    .map(|m| m.as_str())
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                match key {
                    "ID" => result.linux_distro_id = Some(value),
                    "VERSION_ID" => result.linux_distro_version = Some(value),
                    _ => {}
                }
            }
        }
    }

    Some(result)
}

/// 检测目录（默认当前工作目录）里存在的 VCS 类型（对齐 TS `detectVcs`）。
pub fn detect_vcs(dir: Option<&str>) -> Vec<String> {
    const VCS_MARKERS: [(&str, &str); 8] = [
        (".git", "git"),
        (".hg", "mercurial"),
        (".svn", "svn"),
        (".p4config", "perforce"),
        ("$tf", "tfs"),
        (".tfvc", "tfs"),
        (".jj", "jujutsu"),
        (".sl", "sapling"),
    ];

    let mut detected: Vec<String> = Vec::new();
    let push = |v: &str, detected: &mut Vec<String>| {
        if !detected.iter().any(|d| d == v) {
            detected.push(v.to_string());
        }
    };

    // Perforce 由环境变量判定
    if std::env::var_os("P4PORT").is_some() {
        push("perforce", &mut detected);
    }

    let target_dir = match dir {
        Some(d) => d.to_string(),
        None => match get_fs_implementation().cwd() {
            Ok(cwd) => cwd,
            Err(_) => return detected,
        },
    };

    if let Ok(entries) = get_fs_implementation().readdir_sync(&target_dir) {
        for (marker, vcs) in VCS_MARKERS {
            if entries.iter().any(|e| e.name == marker) {
                push(vcs, &mut detected);
            }
        }
    }
    // 目录不可读：静默跳过（对齐 TS catch）

    detected
}

fn read_proc_version() -> Option<String> {
    match get_fs_implementation().read_file_sync("/proc/version", BufferEncoding::Utf8) {
        Ok(content) => Some(content),
        Err(e) => {
            // 读不到 /proc/version：当作普通 Linux，记日志（对齐 TS）
            log_error_dyn(Box::new(e));
            None
        }
    }
}

fn read_osrelease() -> Option<String> {
    get_fs_implementation()
        .read_file_sync("/proc/sys/kernel/osrelease", BufferEncoding::Utf8)
        .map(|s| s.trim().to_string())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_is_one_of_known_variants() {
        let p = get_platform();
        assert!(matches!(
            p,
            Platform::Macos
                | Platform::Windows
                | Platform::Wsl
                | Platform::Linux
                | Platform::Unknown
        ));
    }

    #[test]
    fn supported_platforms_match_ts() {
        assert_eq!(
            SUPPORTED_PLATFORMS,
            &[Platform::Macos, Platform::Wsl]
        );
    }

    #[test]
    fn wsl_version_only_on_linux() {
        let v = get_wsl_version();
        if std::env::consts::OS != "linux" {
            assert_eq!(v, None);
        } else {
            // 在 Linux CI 上 /proc/version 存在但未必是 WSL：仅校验不 panic
            let _ = v;
        }
    }

    #[test]
    fn distro_info_shape() {
        if std::env::consts::OS == "linux" {
            let info = get_linux_distro_info().expect("linux yields distro info");
            // 内核版本在容器里可能读不到（空串）；id/version 可缺失，仅校验结构合法
            assert!(info.linux_kernel.len() <= 256 || info.linux_kernel.is_empty());
        } else {
            assert_eq!(get_linux_distro_info(), None);
        }
    }

    #[test]
    fn detect_vcs_does_not_panic() {
        let _ = detect_vcs(None);
        let _ = detect_vcs(Some("/nonexistent-dir-cls-platform"));
    }
}
