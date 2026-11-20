//! windows_paths.rs — Windows ↔ POSIX 路径转换与 git-bash 发现。
//!
//! 重写自 `src/utils/windowsPaths.ts`。语义对齐：
//!
//! - `set_shell_if_windows`：Windows 上把 SHELL 环境变量设为 git-bash
//!   路径（BashTool / Shell.ts 用户 shell 命令用），并传播
//!   CLAUDE_CODE_GIT_BASH_PATH 给子进程跳过文件系统探测。COMSPEC 不动
//!   （系统进程执行保留）。
//! - git-bash 发现顺序（每步命中即返回）：
//!   1. CLAUDE_CODE_GIT_BASH_PATH 环境变量（文件须存在）
//!   2. `where.exe bash`（PATH 上的 bash.exe）
//!   3. `where.exe git` 后按三种安装布局推导 bash 位置
//!   4. 常见安装位置直查（含 Scoop）
//! - where.exe 结果会过滤掉"当前目录内"的条目（防恶意 git.bat 假冒），
//!   以及 System32\WindowsApps 的 WSL bash 启动器。
//! - `windows_path_to_posix_path` / `posix_path_to_windows_path`：纯 JS
//!   转换（UNC / 盘符 / cygdrive / MSYS2 风格），带 LRU（500 条）。
//!
//! 偏差：
//! - TS `t('...')`（i18n）在找 git-bash 失败时只包了一段无插值的英文
//!   提示，Rust 侧 i18n 模块未转换，直接用原文。
//! - execSync 换成 exec_sync_wrapper（`sh`/`cmd` 显式 shell）。
//! - 依赖注入（GitBashDiscoveryDeps）用 trait + 结构体落地，便于测试。

use std::path::Path;

use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::exec_sync_wrapper::{exec_sync_deprecated, ExecSyncOptions};
use crate::utils::memoize::{memoize_with_lru, LruMemoized};
use crate::utils::platform::{get_platform, Platform};

// ---------------------------------------------------------------------------
// 依赖注入（对齐 TS GitBashDiscoveryDeps）
// ---------------------------------------------------------------------------

/// git-bash 发现的依赖。暴露为参数以便单测无需 mock.module 污染。
pub trait GitBashDiscoveryDeps {
    /// 路径在磁盘上是否存在。
    fn check_exists(&self, path: &str) -> bool;
    /// 执行 shell 命令并返回 trim 后的 stdout；失败返回 None。
    fn exec_command(&self, cmd: &str) -> Option<String>;
    /// 当前工作目录（用于过滤 PATH 查询结果）。
    fn cwd_fn(&self) -> String;
    /// USERPROFILE，用于推导 Scoop 安装路径。
    fn user_profile(&self) -> Option<String>;
    /// CLAUDE_CODE_GIT_BASH_PATH 覆盖值（测试注入用）。
    fn env_override(&self) -> Option<String>;
}

/// 生产环境默认依赖。
pub struct DefaultDeps;

impl GitBashDiscoveryDeps for DefaultDeps {
    fn check_exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn exec_command(&self, cmd: &str) -> Option<String> {
        let output = exec_sync_deprecated(
            cmd,
            &ExecSyncOptions {
                encoding: None, // 走字节再转字符串，等价 TS 的 encoding:'utf8'
                ..Default::default()
            },
        )
        .ok()?;
        match output {
            crate::utils::exec_sync_wrapper::ExecSyncOutput::Text(s) => Some(s.trim().to_string()),
            crate::utils::exec_sync_wrapper::ExecSyncOutput::Bytes(b) => {
                Some(String::from_utf8_lossy(&b).trim().to_string())
            }
        }
    }

    fn cwd_fn(&self) -> String {
        crate::utils::cwd::get_cwd()
    }

    fn user_profile(&self) -> Option<String> {
        std::env::var("USERPROFILE").ok()
    }

    fn env_override(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// win32 路径辅助（对齐 TS 的 path/win32，纯实现）
// ---------------------------------------------------------------------------

fn win32_is_absolute(p: &str) -> bool {
    let p = p;
    if p.starts_with('\\') {
        return true;
    }
    let bytes = p.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// 词法规范化：统一反斜杠、解析 . 与 ..（不触碰文件系统）。
fn win32_normalize(p: &str) -> String {
    let p = p.replace('/', "\\");
    let mut parts: Vec<&str> = Vec::new();
    let mut prefix = String::new();
    let body = if p.starts_with("\\\\") || p.starts_with("//") {
        prefix.push_str("\\\\");
        &p[2..]
    } else if win32_is_absolute(&p) {
        prefix.push_str(&p[..3]);
        &p[3..]
    } else {
        &p[..]
    };
    for seg in body.split(['\\', '/']) {
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
    let joined = parts.join("\\");
    if prefix.is_empty() {
        joined
    } else if prefix == "\\\\" {
        format!("\\\\{joined}")
    } else {
        format!("{prefix}{joined}")
    }
}

/// win32 resolve：绝对路径规范化，否则相对 base 词法拼接。
fn win32_resolve(base: &str, p: &str) -> String {
    if win32_is_absolute(p) || p.starts_with('\\') {
        win32_normalize(p)
    } else {
        win32_normalize(&format!("{}\\{}", win32_normalize(base), p))
    }
}

fn win32_dirname(p: &str) -> String {
    let n = win32_normalize(p);
    match n.trim_end_matches('\\').rfind('\\') {
        Some(0) => "\\".to_string(),
        Some(idx) => n[..idx].to_string(),
        None => ".".to_string(),
    }
}

/// win32 relative：from → to 的相对路径（不做盘符切换，依赖调用方保证）。
fn win32_relative(from: &str, to: &str) -> String {
    let from = win32_normalize(from);
    let to = win32_normalize(to);
    if from == to {
        return String::new();
    }
    let from_parts: Vec<&str> = from.split('\\').collect();
    let to_parts: Vec<&str> = to.split('\\').collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_parts.len() - common;
    let mut result: Vec<String> = std::iter::repeat("..".to_string()).take(ups).collect();
    result.extend(to_parts[common..].iter().map(|s| s.to_string()));
    result.join("\\")
}

// ---------------------------------------------------------------------------
// 实现
// ---------------------------------------------------------------------------

/// 搜索常见安装位置下的 bash.exe；返回首个存在的候选，无则 None。
fn search_default_bash_locations(
    check_exists: &dyn Fn(&str) -> bool,
    user_profile: Option<&str>,
) -> Option<String> {
    let candidates = [
        // 标准 Git for Windows 安装位置（两种布局）
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files\\Git\\usr\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\usr\\bin\\bash.exe",
    ];
    for candidate in candidates {
        if check_exists(candidate) {
            return Some(candidate.to_string());
        }
    }
    // Scoop: %USERPROFILE%\scoop\apps\git\current\usr\bin\bash.exe
    if let Some(profile) = user_profile {
        let scoop = format!("{profile}\\scoop\\apps\\git\\current\\usr\\bin\\bash.exe");
        if check_exists(&scoop) {
            return Some(scoop);
        }
    }
    None
}

/// 在 Windows 上查找可执行文件。先查常见安装位置（仅 git），再退到
/// where.exe；过滤掉当前目录内的条目（防恶意 git.bat/cmd/exe）。
///
/// 纯变体：依赖作为参数传入，便于单测。
fn find_executable_with_deps(
    executable: &str,
    deps: &dyn GitBashDiscoveryDeps,
) -> Option<String> {
    // git：先查常见安装位置
    if executable == "git" {
        let default_locations = [
            // 64 位优先于 32 位
            "C:\\Program Files\\Git\\cmd\\git.exe",
            "C:\\Program Files (x86)\\Git\\cmd\\git.exe",
            // 故意不查 C:\Program Files\Git\mingw64\bin\git.exe ——
            // 该目录是"裸"工具，没有环境配置
        ];
        for location in default_locations {
            if deps.check_exists(location) {
                return Some(location.to_string());
            }
        }
    }

    // 退到 where.exe
    let result = deps.exec_command(&format!("where.exe {executable}"))?;
    // SECURITY: 过滤当前目录结果，防止执行恶意 git.bat/cmd/exe。
    // 用 win32 语义解析 where.exe 的 Windows 风格路径（无论宿主 OS）。
    let paths: Vec<String> = result
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let cwd = win32_resolve(&deps.cwd_fn(), "").to_lowercase();

    for candidate_path in paths {
        let normalized_path = win32_resolve(&cwd, &candidate_path).to_lowercase();
        let path_dir = win32_dirname(&normalized_path).to_lowercase();
        // path.win32.relative(cwd, path_dir)：
        //   ''              → pathDir === cwd
        //   '..'/'../...'   → 在 cwd 之外（或之上）
        //   'subdir/...'    → 在 cwd 之内
        // 拒绝 dir 等于 cwd 或位于 cwd 之内的条目。
        let relative_path_dir = win32_relative(&cwd, &path_dir);

        if relative_path_dir.is_empty()
            || (!relative_path_dir.starts_with("..") && !win32_is_absolute(&relative_path_dir))
        {
            log_for_debugging(
                &format!(
                    "Skipping potentially malicious executable in current directory: {candidate_path}"
                ),
                DebugLogLevel::Debug,
            );
            continue;
        }

        // WSL-bash 拒绝：System32\bash.exe（WSL 启动器）与
        // WindowsApps\bash.exe（WSL App Execution Alias）绝不能当作
        // Git Bash——它们会拉起 wsl.exe 弹窗并在 Windows 任务上跑
        // Linux 语义命令。仅对 bash.exe 生效。
        if executable == "bash"
            && (normalized_path.ends_with("system32\\bash.exe")
                || normalized_path.ends_with("windowsapps\\bash.exe"))
        {
            log_for_debugging(
                &format!("Skipping WSL bash launcher (not Git Bash): {candidate_path}"),
                DebugLogLevel::Debug,
            );
            continue;
        }

        return Some(candidate_path);
    }

    None
}

/// git-bash.exe 的纯发现助手。未找到返回 None。
/// 导出供测试：可注入 checkExists / execCommand / cwdFn 隔离各分支。
pub fn find_git_bash_path_or_null_with_deps(deps: &dyn GitBashDiscoveryDeps) -> Option<String> {
    let env_override = deps
        .env_override()
        .or_else(|| std::env::var("CLAUDE_CODE_GIT_BASH_PATH").ok());

    // 1. 显式 CLAUDE_CODE_GIT_BASH_PATH 覆盖
    if let Some(env_override) = env_override {
        return if deps.check_exists(&env_override) {
            Some(env_override)
        } else {
            None
        };
    }

    // 2. 直接经 PATH 查 bash.exe。非默认安装位置（D:\software\Git\，
    //    bash 在 <root>/usr/bin/bash.exe 而非 <root>/bin/bash.exe）最可靠。
    let from_path = find_executable_with_deps("bash", deps);
    if let Some(from_path) = from_path {
        if deps.check_exists(&from_path) {
            return Some(from_path);
        }
    }

    // 3. 从 git 位置推导 bash，尝试多种布局（scoop / chocolatey /
    //    手动 portable 安装的 bash 相对 git.exe 位置不一）。
    let git_path = find_executable_with_deps("git", deps);
    if let Some(git_path) = git_path {
        let parent = win32_dirname(&git_path);
        let grandparent = win32_dirname(&parent);
        let candidates = [
            // 标准 Git for Windows：git 在 <root>/cmd/git.exe，bash 在 <root>/bin/bash.exe
            format!("{}\\bin\\bash.exe", grandparent),
            // PortableGit / 自定义：git 在 <root>/cmd/git.exe，bash 在 <root>/usr/bin/bash.exe
            format!("{}\\usr\\bin\\bash.exe", grandparent),
            // 部分安装：git 在 <root>/bin/git.exe，bash 在 <root>/bin/bash.exe
            format!("{}\\bash.exe", parent),
        ];
        for candidate in candidates {
            if deps.check_exists(&candidate) {
                return Some(candidate);
            }
        }
    }

    // 4. 最后手段：直接扫常见安装位置
    search_default_bash_locations(&|p| deps.check_exists(p), deps.user_profile().as_deref())
}

/// 查找 git-bash 的 bash.exe 路径；找不到返回 None。
///
/// 记忆化：同进程内反复调用只搜一次（对齐 TS 的 lodash memoize）。
/// 测试友好变体：不调 process.exit（与 find_git_bash_path 不同）。
pub fn find_git_bash_path_or_null() -> Option<String> {
    static CACHED: Lazy<Option<String>> =
        Lazy::new(|| find_git_bash_path_or_null_with_deps(&DefaultDeps));
    CACHED.clone()
}

/// 查找 git-bash 的 bash.exe 路径；找不到时打印错误并进程退出 1。
pub fn find_git_bash_path() -> String {
    if let Some(path) = find_git_bash_path_or_null() {
        return path;
    }
    if let Ok(env_override) = std::env::var("CLAUDE_CODE_GIT_BASH_PATH") {
        eprintln!(
            "Claude Code was unable to find CLAUDE_CODE_GIT_BASH_PATH path \"{env_override}\""
        );
    } else {
        // TS 原文（i18n 未转换，直接用英文原文）：
        eprintln!(
            "Claude Code on Windows requires git-bash (https://git-scm.com/downloads/win). If installed but not in PATH, set environment variable pointing to your bash.exe, similar to: CLAUDE_CODE_GIT_BASH_PATH=C:\\Program Files\\Git\\bin\\bash.exe"
        );
    }
    std::process::exit(1);
}

/// Windows 上把 SHELL 设为 git-bash 路径，并传播 CLAUDE_CODE_GIT_BASH_PATH。
pub fn set_shell_if_windows() {
    if get_platform() != Platform::Windows {
        return;
    }
    let git_bash_path = find_git_bash_path();
    std::env::set_var("SHELL", &git_bash_path);
    // 传播给子进程，跳过文件系统探测
    std::env::set_var("CLAUDE_CODE_GIT_BASH_PATH", &git_bash_path);
    log_for_debugging(
        &format!("Using bash path: \"{git_bash_path}\""),
        DebugLogLevel::Debug,
    );
}

// ---------------------------------------------------------------------------
// 路径转换（LRU 记忆化，容量 500）
// ---------------------------------------------------------------------------

static WINDOWS_TO_POSIX: Lazy<Arc<LruMemoized<(String,), String>>> = Lazy::new(|| {
    memoize_with_lru(
        |(windows_path,): &(String,)| {
            // UNC：\\server\share -> //server/share
            if windows_path.starts_with("\\\\") {
                return windows_path.replace('\\', "/");
            }
            // 盘符路径：C:\Users\foo -> /c/Users/foo
            let bytes = windows_path.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'/' || bytes[2] == b'\\')
            {
                let drive = bytes[0].to_ascii_lowercase() as char;
                return format!("/{drive}{}", windows_path[2..].replace('\\', "/"));
            }
            // 已是 POSIX 或相对路径 —— 只翻斜杠
            windows_path.replace('\\', "/")
        },
        |(p,)| p.clone(),
        500,
    )
});

static POSIX_TO_WINDOWS: Lazy<Arc<LruMemoized<(String,), String>>> = Lazy::new(|| {
    memoize_with_lru(
        |(posix_path,): &(String,)| {
            // UNC：//server/share -> \\server\share
            if posix_path.starts_with("//") {
                return posix_path.replace('/', "\\");
            }
            // /cygdrive/c/... 格式
            let lower = posix_path.to_lowercase();
            if lower.starts_with("/cygdrive/")
                && posix_path.len() > "/cygdrive/".len()
            {
                let rest = &posix_path["/cygdrive/".len()..];
                let drive = rest.chars().next().map(|c| c.to_ascii_uppercase());
                let body = &rest[1..];
                if let Some(drive) = drive {
                    let body = if body.is_empty() { "\\" } else { body };
                    return format!("{drive}:{}", body.replace('/', "\\"));
                }
            }
            // /c/... 格式（MSYS2/Git Bash）
            let bytes = posix_path.as_bytes();
            if bytes.len() >= 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic()
                && (bytes.len() == 2 || bytes[2] == b'/')
            {
                let drive = bytes[1].to_ascii_uppercase() as char;
                let rest = &posix_path[2..];
                let body = if rest.is_empty() { "\\" } else { rest };
                return format!("{drive}:{}", body.replace('/', "\\"));
            }
            // 已是 Windows 或相对路径 —— 只翻斜杠
            posix_path.replace('/', "\\")
        },
        |(p,)| p.clone(),
        500,
    )
});

/// Windows 路径转 POSIX 路径（纯转换，LRU 500）。
pub fn windows_path_to_posix_path(windows_path: &str) -> String {
    WINDOWS_TO_POSIX.call(&(windows_path.to_string(),))
}

/// POSIX 路径转 Windows 路径（纯转换，LRU 500）。
pub fn posix_path_to_windows_path(posix_path: &str) -> String {
    POSIX_TO_WINDOWS.call(&(posix_path.to_string(),))
}
