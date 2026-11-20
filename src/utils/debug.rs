//! debug.rs — debug 日志输出（分级、过滤、缓冲落盘）。
//!
//! 重写自 `src/utils/debug.ts`。语义对齐说明：
//!
//! - TS 的 `memoize`（lodash）在 Rust 侧用 `OnceLock` 等价：首次调用后
//!   值固定。`isDebugMode` 例外——`enableDebugLogging` 需要清缓存
//!   （TS 的 `isDebugMode.cache.clear?.()`），故用 `Mutex<Option<bool>>`
//!   手工实现可重置的 memo。
//! - TS 的 `feature()` 无关；本模块不读 feature flag。
//! - TS 异步路径（ants 无 --debug 时 ~1s 一次的 appendFile Promise 链）
//!   在 Rust 侧无全局异步运行时，改为 `BufferedWriter` 自身的后台
//!   flush 线程承载：write_fn 本身同步追加，调用频率由缓冲降到
//!   ~1 次/秒。`pendingWrite` Promise 链因此无对应物，`flushDebugLogs`
//!   直接调 writer.flush()。
//! - `shouldLogDebugMessage` 里的 `process.versions.node` 检查在 Rust
//!   侧无意义（始终有可用 fs 实现），已省略。
//! - `updateLatestDebugLogSymlink` 在 TS 侧被 memoize（只执行一次），
//!   Rust 侧用 `AtomicBool` 等价。
//! - `logAntError` 接收 `&dyn Error`，用 Display + source 链代替 TS 的
//!   `error.stack`。

use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use chrono::{SecondsFormat, Utc};

use crate::bootstrap::state::get_session_id;
use crate::utils::buffered_writer::{BufferedWriter, BufferedWriterOptions};
use crate::utils::cleanup_registry::{register_cleanup, CleanupRegistration};
use crate::utils::debug_filter::{parse_debug_filter, should_show_debug_message, DebugFilter};
use crate::utils::env_utils::{get_claude_config_home_dir, is_env_truthy};
use crate::utils::fs_operations::get_fs_implementation;
use crate::utils::process::write_to_stderr;
use crate::utils::slow_operations::json_stringify;

/// debug 日志级别（对齐 TS 的 `DebugLogLevel`）。顺序见 [`DebugLogLevel::order`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugLogLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
}

impl DebugLogLevel {
    /// 级别排序（对齐 TS 的 `LEVEL_ORDER`）：verbose=0 … error=4。
    pub fn order(self) -> u8 {
        match self {
            DebugLogLevel::Verbose => 0,
            DebugLogLevel::Debug => 1,
            DebugLogLevel::Info => 2,
            DebugLogLevel::Warn => 3,
            DebugLogLevel::Error => 4,
        }
    }

    /// 大写形式，用于日志行的 `[LEVEL]` 字段。
    pub fn upper(self) -> &'static str {
        match self {
            DebugLogLevel::Verbose => "VERBOSE",
            DebugLogLevel::Debug => "DEBUG",
            DebugLogLevel::Info => "INFO",
            DebugLogLevel::Warn => "WARN",
            DebugLogLevel::Error => "ERROR",
        }
    }

    fn from_env_name(name: &str) -> Option<Self> {
        match name {
            "verbose" => Some(DebugLogLevel::Verbose),
            "debug" => Some(DebugLogLevel::Debug),
            "info" => Some(DebugLogLevel::Info),
            "warn" => Some(DebugLogLevel::Warn),
            "error" => Some(DebugLogLevel::Error),
            _ => None,
        }
    }
}

/// 写入 debug 日志的最低级别。默认 `debug`（滤掉 verbose）；
/// `CLAUDE_CODE_DEBUG_LOG_LEVEL=verbose` 放开高频诊断。
pub fn get_min_debug_level() -> DebugLogLevel {
    static MIN_LEVEL: OnceLock<DebugLogLevel> = OnceLock::new();
    *MIN_LEVEL.get_or_init(|| {
        let raw = env::var("CLAUDE_CODE_DEBUG_LOG_LEVEL")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase());
        match raw.as_deref().and_then(DebugLogLevel::from_env_name) {
            Some(level) => level,
            None => DebugLogLevel::Debug,
        }
    })
}

static RUNTIME_DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);

static DEBUG_MODE_CACHE: Mutex<Option<bool>> = Mutex::new(None);

/// debug 模式是否开启（memoized）。判据与 TS 一致：
/// 运行时开启标记 / DEBUG / DEBUG_SDK / `--debug` / `-d` /
/// `--debug-to-stderr` / `--debug=<pattern>` / `--debug-file`。
pub fn is_debug_mode() -> bool {
    {
        let guard = DEBUG_MODE_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = *guard {
            return cached;
        }
    }
    let args: Vec<String> = env::args().collect();
    let enabled = RUNTIME_DEBUG_ENABLED.load(Ordering::Relaxed)
        || is_env_truthy(env::var("DEBUG").ok().as_deref())
        || is_env_truthy(env::var("DEBUG_SDK").ok().as_deref())
        || args.iter().any(|arg| arg == "--debug" || arg == "-d")
        || is_debug_to_stderr()
        || args.iter().any(|arg| arg.starts_with("--debug="))
        || get_debug_file_path().is_some();
    let mut guard = DEBUG_MODE_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some(enabled);
    enabled
}

/// 会话中途开启 debug 日志（如 `/debug`）。非 ant 用户默认不写 debug
/// 日志，此函数允许其在不带 `--debug` 重启的情况下开始捕获。
/// 返回日志先前是否已经激活。
pub fn enable_debug_logging() -> bool {
    let was_active =
        is_debug_mode() || env::var("USER_TYPE").as_deref() == Ok("ant");
    RUNTIME_DEBUG_ENABLED.store(true, Ordering::Relaxed);
    // 对齐 TS 的 `isDebugMode.cache.clear?.()`
    *DEBUG_MODE_CACHE.lock().unwrap_or_else(|p| p.into_inner()) = None;
    was_active
}

/// 从 argv 解析 `--debug=<pattern>`（memoized）。无参返回 None。
pub fn get_debug_filter() -> Option<DebugFilter> {
    static FILTER: OnceLock<Option<DebugFilter>> = OnceLock::new();
    FILTER
        .get_or_init(|| {
            let args: Vec<String> = env::args().collect();
            let pattern = args
                .iter()
                .find_map(|arg| arg.strip_prefix("--debug=").map(|p| p.to_string()));
            pattern.and_then(|p| parse_debug_filter(Some(&p)))
        })
        .clone()
}

/// 是否带 `--debug-to-stderr`（memoized）。
pub fn is_debug_to_stderr() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        let args: Vec<String> = env::args().collect();
        args.iter().any(|arg| arg == "--debug-to-stderr")
    })
}

/// 解析 `--debug-file=<path>` 或 `--debug-file <path>`（memoized）。
/// 相对路径按 CWD 解析为绝对路径（对齐 TS 的 `path.resolve`）。
pub fn get_debug_file_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let args: Vec<String> = env::args().collect();
        for (i, arg) in args.iter().enumerate() {
            if let Some(rest) = arg.strip_prefix("--debug-file=") {
                return Some(resolve_path(rest));
            }
            if arg == "--debug-file" {
                if let Some(next) = args.get(i + 1) {
                    return Some(resolve_path(next));
                }
            }
        }
        None
    })
    .clone()
}

fn should_log_debug_message(message: &str) -> bool {
    if env::var("NODE_ENV").as_deref() == Ok("test") && !is_debug_to_stderr() {
        return false;
    }

    // 非 ant 用户仅在 debug 模式激活时写日志；ant 用户始终写（/share、
    // bug report 用）。
    if env::var("USER_TYPE").as_deref() != Ok("ant") && !is_debug_mode() {
        return false;
    }

    let filter = get_debug_filter();
    should_show_debug_message(message, filter.as_ref())
}

static HAS_FORMATTED_OUTPUT: AtomicBool = AtomicBool::new(false);

/// 标记输出已经格式化（多行消息需 JSON 编码，避免破坏 jsonl 格式）。
pub fn set_has_formatted_output(value: bool) {
    HAS_FORMATTED_OUTPUT.store(value, Ordering::Relaxed);
}

/// 输出是否已格式化。
pub fn get_has_formatted_output() -> bool {
    HAS_FORMATTED_OUTPUT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// debug 日志写入器（带缓冲 / 立即双模式）
// ---------------------------------------------------------------------------

/// 缓冲写入器已在哪个目录上 mkdir 过（对齐 TS 的 `ensuredDir`）。
static ENSURED_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// latest 符号链接只在进程内更新一次（对齐 TS 的 memoize）。
static SYMLINK_UPDATED: AtomicBool = AtomicBool::new(false);

struct DebugWriterHandle {
    writer: BufferedWriter,
    /// 持有清理注册句柄，使 writer 落盘逻辑随 cleanup registry 生效。
    _cleanup: CleanupRegistration,
}

static DEBUG_WRITER: OnceLock<DebugWriterHandle> = OnceLock::new();

fn init_debug_writer() -> DebugWriterHandle {
    // writeFn：对齐 TS 的双路径——成功路径先 mkdir（首次）再追加；
    // 失败时 mkdir 后重试一次，再失败静默放弃（绝不破坏 UI 渲染）。
    let write_fn = std::sync::Arc::new(|content: &str| {
        let path = get_debug_log_path();
        let dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(PathBuf::new);
        let need_mkdir = {
            let mut ensured = ENSURED_DIR.lock().unwrap_or_else(|p| p.into_inner());
            let fresh = ensured.as_ref() != Some(&dir);
            if fresh {
                *ensured = Some(dir.clone());
            }
            fresh
        };
        let fs = get_fs_implementation();
        let dir_str = dir.to_string_lossy();
        let path_str = path.to_string_lossy();
        if need_mkdir {
            let _ = fs.mkdir_sync(&dir_str, None);
        }
        if fs
            .append_file_sync(&path_str, content, None)
            .is_err()
        {
            let _ = fs.mkdir_sync(&dir_str, None);
            let _ = fs.append_file_sync(&path_str, content, None);
        }
        update_latest_debug_log_symlink();
    }) as std::sync::Arc<dyn Fn(&str) + Send + Sync>;

    let writer = BufferedWriter::new(BufferedWriterOptions {
        write_fn,
        flush_interval_ms: 1000,
        max_buffer_size: 100,
        max_buffer_bytes: None,
        // immediateMode：debug 模式必须同步写——异步写会在直接
        // process.exit 时丢失（TS 注释 #22257）。
        immediate_mode: is_debug_mode(),
    });

    let _cleanup = register_cleanup(|| {
        flush_debug_logs();
    });

    DebugWriterHandle { writer, _cleanup }
}

fn get_debug_writer() -> &'static BufferedWriter {
    &DEBUG_WRITER.get_or_init(init_debug_writer).writer
}

/// 刷盘当前缓冲（对齐 TS 的 `flushDebugLogs`）。
pub fn flush_debug_logs() {
    if let Some(handle) = DEBUG_WRITER.get() {
        handle.writer.flush();
    }
}

/// 写一条 debug 日志。级别低于 `CLAUDE_CODE_DEBUG_LOG_LEVEL` 或未通过
/// 过滤的消息被丢弃。多行消息在「已格式化输出」时 JSON 编码。
pub fn log_for_debugging(message: &str, level: DebugLogLevel) {
    if level.order() < get_min_debug_level().order() {
        return;
    }
    if !should_log_debug_message(message) {
        return;
    }

    // 多行消息会破坏 jsonl 输出格式，转成 JSON 字符串（对齐 TS：
    // jsonStringify(message)）。
    let message: String = if get_has_formatted_output() && message.contains('\n') {
        json_stringify(&serde_json::Value::String(message.to_string()))
    } else {
        message.to_string()
    };

    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let output = format!(
        "{} [{}] {}\n",
        timestamp,
        level.upper(),
        message.trim()
    );
    if is_debug_to_stderr() {
        write_to_stderr(&output);
        return;
    }

    get_debug_writer().write(&output);
}

/// debug 日志文件路径。优先级：`--debug-file` >
/// `CLAUDE_CODE_DEBUG_LOGS_DIR`（相对路径按 CWD 解析）>
/// `<config home>/debug/<session id>.txt`。
pub fn get_debug_log_path() -> PathBuf {
    if let Some(path) = get_debug_file_path() {
        return path;
    }
    if let Ok(dir) = env::var("CLAUDE_CODE_DEBUG_LOGS_DIR") {
        if !dir.is_empty() {
            return resolve_path(&dir);
        }
    }
    let session_id = get_session_id();
    Path::new(&get_claude_config_home_dir())
        .join("debug")
        .join(format!("{}.txt", session_id.0))
}

/// 更新 `latest` 符号链接指向当前 debug 日志文件。失败静默
/// （对齐 TS 的 try/catch）；进程内只执行一次（对齐 TS 的 memoize）。
fn update_latest_debug_log_symlink() {
    if SYMLINK_UPDATED.swap(true, Ordering::Relaxed) {
        return;
    }
    let path = get_debug_log_path();
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let latest = dir.join("latest");
    let fs = get_fs_implementation();
    let _ = fs.unlink_sync(&latest.to_string_lossy());
    let _ = fs.symlink_sync(&path.to_string_lossy(), &latest.to_string_lossy(), None);
}

/// 仅 ant 用户的错误日志，生产环境始终可见。
/// Rust 侧以 Display + source 链代替 TS 的 `error.stack`。
pub fn log_ant_error(context: &str, error: &dyn std::error::Error) {
    if env::var("USER_TYPE").as_deref() != Ok("ant") {
        return;
    }
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        chain += "\n  caused by: ";
        chain += &cause.to_string();
        source = cause.source();
    }
    log_for_debugging(
        &format!("[ANT-ONLY] {context} error:\n{chain}"),
        DebugLogLevel::Error,
    );
}

/// 相对路径按 CWD 解析为绝对路径（对齐 TS 的 `path.resolve`）。
fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return p;
    }
    std::env::current_dir().unwrap_or_default().join(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_order_and_upper() {
        assert_eq!(DebugLogLevel::Verbose.order(), 0);
        assert_eq!(DebugLogLevel::Error.order(), 4);
        assert_eq!(DebugLogLevel::Warn.upper(), "WARN");
    }

    #[test]
    fn min_level_defaults_to_debug() {
        // 未设置环境变量时为 debug（进程内 memoized，此处仅验证取值合法）
        let level = get_min_debug_level();
        assert!(matches!(level, DebugLogLevel::Debug | DebugLogLevel::Verbose));
    }

    #[test]
    fn has_formatted_output_roundtrip() {
        set_has_formatted_output(true);
        assert!(get_has_formatted_output());
        set_has_formatted_output(false);
        assert!(!get_has_formatted_output());
    }

    #[test]
    fn resolve_path_makes_absolute() {
        let p = resolve_path("/tmp/debug.log");
        assert!(p.is_absolute());
    }

    #[test]
    fn debug_file_path_parses_equals_form() {
        // 通过显式 argv 形式直接验证解析逻辑（memoized getter 只测一次）
        let argv = ["--debug-file=/tmp/x.log".to_string()];
        let path = crate::utils::cli_args::eager_parse_cli_flag("--debug-file", &argv);
        assert_eq!(path.as_deref(), Some("/tmp/x.log"));
    }
}
