//! raw_read.rs — 阻塞事件循环之外执行 MDM 子进程读取的最小模块。
//!
//! 重写自 `src/utils/settings/mdm/rawRead.ts`。最小导入：仅 mdm 常量 +
//! std。两种使用模式：
//! 1. 启动：`start_mdm_raw_read()` 在进程初始化时触发一次，结果稍后经
//!    `get_mdm_raw_read()` 取回；
//! 2. 轮询/回退：`fire_raw_read()` 按需现场读取。
//!
//! 原始 stdout 由 mdmSettings.ts 经 `consume_raw_read_result()` 消费。
//!
//! 语义对齐说明：
//! - TS 用 `execFile` + Promise 异步执行；Rust 无异步运行时依赖，改为
//!   同步 `std::process::Command`（`start_mdm_raw_read` 首次调用时同步
//!   执行并缓存，get 立即返回）；
//! - TS 的 `execFile` timeout 选项（5000ms）用 `try_wait` 轮询实现：
//!   超过截止时间则 kill 子进程并视为失败；
//! - `process.platform === 'darwin' / 'win32'` 的运行时分派改为
//!   `cfg!(target_os = ...)` 编译期分派（Rust 按平台构建二进制）。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(target_os = "darwin")]
use super::constants::{get_macos_plist_paths, PLUTIL_ARGS_PREFIX, PLUTIL_PATH};
use super::constants::MDM_SUBPROCESS_TIMEOUT_MS;
#[cfg(target_os = "windows")]
use super::constants::{
    WINDOWS_REGISTRY_KEY_PATH_HKCU, WINDOWS_REGISTRY_KEY_PATH_HKLM, WINDOWS_REGISTRY_VALUE_NAME,
};

/// macOS plist 读取结果（空数组表示无可用来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlistStdout {
    pub stdout: String,
    pub label: String,
}

/// 一次批量原始读取的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawReadResult {
    /// macOS：plist 读取结果（`None` 表示平台不支持 plist 来源）
    pub plist_stdouts: Option<Vec<PlistStdout>>,
    /// Windows：HKLM 注册表输出
    pub hklm_stdout: Option<String>,
    /// Windows：HKCU 注册表输出
    pub hkcu_stdout: Option<String>,
}

/// 执行命令并带超时；返回 stdout（code === 0 时）。超时则 kill 并返回 None。
fn exec_file_with_timeout(cmd: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_millis(MDM_SUBPROCESS_TIMEOUT_MS);
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status;
        }
        if Instant::now() >= deadline {
            // 超时：终止子进程，视为失败（对齐 execFile 的 timeout 行为）
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    if !status.success() {
        let _ = child.wait();
        return None;
    }

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout {
        // stdout 已被 piped 捕获；子进程退出后管道数据仍可读取
        if out.read_to_string(&mut stdout).is_err() {
            return None;
        }
    }
    Some(stdout)
}

/// Fire fresh subprocess reads for MDM settings and return raw stdout.
/// On macOS: spawns plutil for each plist path in parallel, picks first winner.
/// On Windows: spawns reg query for HKLM and HKCU in parallel.
/// On Linux: returns empty (no MDM equivalent).
pub fn fire_raw_read() -> RawReadResult {
    #[cfg(target_os = "darwin")]
    {
        let plist_paths = get_macos_plist_paths();

        let all_results: Vec<(String, &'static str, bool)> = plist_paths
            .into_iter()
            .map(|entry| {
                // Fast-path: skip the plutil subprocess if the plist file does not
                // exist. Spawning plutil takes ~5ms even for an immediate ENOENT,
                // and non-MDM machines never have these files.
                if !std::path::Path::new(&entry.path).exists() {
                    return (String::new(), entry.label, false);
                }
                let mut args: Vec<&str> = PLUTIL_ARGS_PREFIX.to_vec();
                args.push(&entry.path);
                let stdout = exec_file_with_timeout(PLUTIL_PATH, &args);
                // 对齐 TS：`ok: code === 0 && !!stdout` —— 空 stdout 视为失败
                match stdout {
                    Some(out) if !out.is_empty() => (out, entry.label, true),
                    _ => (String::new(), entry.label, false),
                }
            })
            .collect();

        // First source wins (array is in priority order)
        let winner = all_results.into_iter().find(|(_, _, ok)| *ok);
        RawReadResult {
            plist_stdouts: Some(
                winner
                    .into_iter()
                    .map(|(stdout, label)| PlistStdout { stdout, label: label.to_string() })
                    .collect(),
            ),
            hklm_stdout: None,
            hkcu_stdout: None,
        }
    }

    #[cfg(target_os = "windows")]
    {
        let hklm = exec_file_with_timeout(
            "reg",
            &[
                "query",
                WINDOWS_REGISTRY_KEY_PATH_HKLM,
                "/v",
                WINDOWS_REGISTRY_VALUE_NAME,
            ],
        );
        let hkcu = exec_file_with_timeout(
            "reg",
            &[
                "query",
                WINDOWS_REGISTRY_KEY_PATH_HKCU,
                "/v",
                WINDOWS_REGISTRY_VALUE_NAME,
            ],
        );
        RawReadResult {
            plist_stdouts: None,
            hklm_stdout: hklm,
            hkcu_stdout: hkcu,
        }
    }

    #[cfg(not(any(target_os = "darwin", target_os = "windows")))]
    {
        RawReadResult {
            plist_stdouts: None,
            hklm_stdout: None,
            hkcu_stdout: None,
        }
    }
}

static RAW_READ: Mutex<Option<RawReadResult>> = Mutex::new(None);

/// Fire raw subprocess reads once for startup. Called at process init.
/// Results are consumed via `get_mdm_raw_read()`.
pub fn start_mdm_raw_read() {
    let mut guard = RAW_READ.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(fire_raw_read());
    }
}

/// Get the startup read result. Returns `None` if `start_mdm_raw_read()`
/// wasn't called.
pub fn get_mdm_raw_read() -> Option<RawReadResult> {
    let guard = RAW_READ.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone()
}
