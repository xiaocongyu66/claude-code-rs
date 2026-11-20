//! errors.rs — 全项目错误类型与错误判定工具。
//!
//! 重写自 `src/utils/errors.ts`。语义对齐说明：
//! - TS 的 `class X extends Error` 映射为 thiserror 风格的错误类型
//!   （实现 `std::error::Error` + `Display`）；
//! - TS 的 `this.name` 在 Rust 无对应物（类型即名字），略去；
//! - `isErrorLike` 处理的是 JS 跨 realm 错误对象，Rust 里一切错误值
//!   都已实现 `std::error::Error`，故恒为真（保留函数兼容调用方）；
//! - `@anthropic-ai/sdk` 的 `APIUserAbortError` 在 SDK 的 Rust 对应物
//!   落地前，先在本模块内定义同义占位类型 `ApiUserAbortError`；
//! - `shortErrorStack`：Rust 无 V8 式 stack 字符串，返回 `{:?}` 的精简
//!   形式（错误链），栈回溯后续按需通过 backtrace 特性引入。

use thiserror::Error;

/// 基类等价物：带消息的通用错误（TS `ClaudeError`）。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ClaudeError {
    message: String,
}

impl ClaudeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// TS `MalformedCommandError extends Error`。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct MalformedCommandError {
    message: String,
}

impl MalformedCommandError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// TS `AbortError`；消息可空（构造器参数可选）。
#[derive(Debug, Error)]
#[error("{}", message.as_deref().unwrap_or("AbortError"))]
pub struct AbortError {
    message: Option<String>,
}

impl AbortError {
    pub fn new(message: Option<impl Into<String>>) -> Self {
        Self { message: message.map(Into::into) }
    }
}

/// `@anthropic-ai/sdk` 的 `APIUserAbortError` 占位（Rust SDK 未落地前）。
#[derive(Debug, Error)]
#[error("Request was aborted by the user")]
pub struct ApiUserAbortError;

/// 配置文件解析错误（TS `ConfigParseError`）。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ConfigParseError {
    message: String,
    /// 出错的配置文件路径。
    pub file_path: String,
    /// 解析失败时回退使用的默认配置。
    pub default_config: serde_json::Value,
}

impl ConfigParseError {
    pub fn new(
        message: impl Into<String>,
        file_path: impl Into<String>,
        default_config: serde_json::Value,
    ) -> Self {
        Self {
            message: message.into(),
            file_path: file_path.into(),
            default_config,
        }
    }
}

/// shell 命令执行失败（TS `ShellError`）。
#[derive(Debug, Error)]
#[error("Shell command failed")]
pub struct ShellError {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub interrupted: bool,
}

/// Teleport 操作错误（TS `TeleportOperationError`）。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct TeleportOperationError {
    message: String,
    /// 给用户看的格式化消息。
    pub formatted_message: String,
}

impl TeleportOperationError {
    pub fn new(message: impl Into<String>, formatted_message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            formatted_message: formatted_message.into(),
        }
    }
}

/// Error with a message that is safe to log to telemetry.
///
/// 单参构造：用户与遥测消息相同；双参构造：分别提供（例如完整消息含
/// 文件路径、遥测消息不含）。TS 类名刻意冗长
/// （`TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS`）以
/// 提醒确认消息脱敏。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct TelemetrySafeError {
    pub message: String,
    /// 进入遥测的消息（未传则与 message 相同）。
    pub telemetry_message: String,
}

impl TelemetrySafeError {
    pub fn new(message: impl Into<String>, telemetry_message: Option<impl Into<String>>) -> Self {
        let message = message.into();
        let telemetry_message = telemetry_message.map(Into::into).unwrap_or_else(|| message.clone());
        Self { message, telemetry_message }
    }
}

/// 携带 errno 风格 code/path 的动态错误（对应 NodeJS.ErrnoException
/// 的鸭子类型检查场景）。
#[derive(Debug, Error)]
#[error("{code}: {message}")]
pub struct ErrnoError {
    pub code: String,
    pub path: Option<String>,
    message: String,
}

impl ErrnoError {
    pub fn new(code: impl Into<String>, path: Option<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), path, message: message.into() }
    }
}

/// 在 TS 里用于嗅探跨 realm 的错误状对象；Rust 中一切错误值都实现了
/// `std::error::Error`，恒为真。
pub fn is_error_like(_e: &(dyn std::error::Error + 'static)) -> bool {
    true
}

/// 判定 abort 形态的错误：本模块 `AbortError`、SDK 的
/// `ApiUserAbortError`，或 Display 以 "AbortError" 开头的错误
/// （对齐 TS 的 `.name === 'AbortError'` 兜底）。
pub fn is_abort_error(e: &(dyn std::error::Error + 'static)) -> bool {
    if e.downcast_ref::<AbortError>().is_some() {
        return true;
    }
    if e.downcast_ref::<ApiUserAbortError>().is_some() {
        return true;
    }
    // TS 兜底：name === 'AbortError'。Rust 错误无 name 字段，用 Display
    // 前缀近似（std::error::Error 的 Display 通常以类型相关文本开头）。
    e.to_string().starts_with("AbortError")
}

/// 消息精确相等判定（TS `hasExactErrorMessage`）。
pub fn has_exactErrorMessage(e: &(dyn std::error::Error + 'static), message: &str) -> bool {
    e.to_string() == message
}

/// 从 io::Error 或 `ErrnoError` 提取 errno 符号（'ENOENT' 等）。
pub fn get_errno_code(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    if let Some(errno) = e.downcast_ref::<ErrnoError>() {
        return Some(errno.code.clone());
    }
    // io::Error 只有原始数字码；映射到 POSIX 符号（Linux/macOS 主码一致）。
    let io_err = e.downcast_ref::<std::io::Error>()?;
    match io_err.raw_os_error() {
        Some(2) => Some("ENOENT".into()),
        Some(13) => Some("EACCES".into()),
        Some(1) => Some("EPERM".into()),
        Some(20) => Some("ENOTDIR".into()),
        Some(40) => Some("ELOOP".into()),
        _ => None,
    }
}

/// 是否 ENOENT（路径不存在）。
pub fn is_enoent(e: &(dyn std::error::Error + 'static)) -> bool {
    get_errno_code(e).as_deref() == Some("ENOENT")
}

/// 提取触发 errno 的文件系统路径（若有）。
pub fn get_errno_path(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    e.downcast_ref::<ErrnoError>().and_then(|errno| errno.path.clone())
}

/// 路径缺失/不可达类错误的统一判定：
/// ENOENT（不存在）、EACCES（无权限）、EPERM（不允许）、
/// ENOTDIR（路径组件不是目录，例如 `.claude` 是个文件）、
/// ELOOP（符号链接循环）。
pub fn is_fs_inaccessible(e: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        get_errno_code(e).as_deref(),
        Some("ENOENT" | "EACCES" | "EPERM" | "ENOTDIR" | "ELOOP")
    )
}

/// axios 错误分类（TS `AxiosErrorKind`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxiosErrorKind {
    /// 401/403 —— 调用方通常跳过重试
    Auth,
    /// ECONNABORTED
    Timeout,
    /// ECONNREFUSED/ENOTFOUND
    Network,
    /// 其他 axios 错误（可能带 status）
    Http,
    /// 非 axios 错误
    Other,
}

/// axios 形态错误的占位类型（Rust 侧 HTTP 客户端落地前的分类目标）。
///
/// 对应 TS 侧带 `.isAxiosError` 标记的 axios 错误：`response.status`、
/// `code`（如 'ECONNABORTED'）与 message。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct AxiosError {
    pub status: Option<u16>,
    /// 'ECONNABORTED' / 'ECONNREFUSED' / 'ENOTFOUND' 等
    pub code: Option<String>,
    pub message: String,
}

/// 分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxiosErrorClassification {
    pub kind: AxiosErrorKind,
    pub status: Option<u16>,
    pub message: String,
}

/// 把捕获到的错误分桶（TS `classifyAxiosError`）。
///
/// 直接用 `downcast_ref::<AxiosError>` 判定（对齐 TS 检查
/// `.isAxiosError` 标记的做法），保持本模块零依赖。
pub fn classify_axios_error(e: &(dyn std::error::Error + 'static)) -> AxiosErrorClassification {
    let message = e.to_string();
    let Some(err) = e.downcast_ref::<AxiosError>() else {
        return AxiosErrorClassification { kind: AxiosErrorKind::Other, status: None, message };
    };
    let status = err.status;
    if status == Some(401) || status == Some(403) {
        return AxiosErrorClassification { kind: AxiosErrorKind::Auth, status, message };
    }
    if err.code.as_deref() == Some("ECONNABORTED") {
        return AxiosErrorClassification { kind: AxiosErrorKind::Timeout, status, message };
    }
    if matches!(err.code.as_deref(), Some("ECONNREFUSED") | Some("ENOTFOUND")) {
        return AxiosErrorClassification { kind: AxiosErrorKind::Network, status, message };
    }
    AxiosErrorClassification { kind: AxiosErrorKind::Http, status, message }
}

/// 提取错误消息（TS `errorMessage`）。
pub fn error_message(e: &(dyn std::error::Error + 'static)) -> String {
    e.to_string()
}

/// 精简错误表示（TS `shortErrorStack`）。
///
/// Rust 无 V8 式 stack 字符串，这里输出错误链（`{:?}`）；栈回溯后续视
/// 需要通过 backtrace 特性引入。`max_frames` 参数为兼容保留。
pub fn short_error_stack(e: &(dyn std::error::Error + 'static), _max_frames: usize) -> String {
    format!("{e:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_error_classification() {
        let err: Box<dyn std::error::Error + 'static> = Box::new(AbortError::new(None::<String>));
        assert!(is_abort_error(&*err));
        let api: Box<dyn std::error::Error + 'static> = Box::new(ApiUserAbortError);
        assert!(is_abort_error(&*api));
    }

    #[test]
    fn errno_classification() {
        let err: Box<dyn std::error::Error + 'static> =
            Box::new(ErrnoError::new("ENOENT", Some("/x".into()), "no such file".to_string()));
        assert!(is_enoent(&*err));
        assert_eq!(get_errno_path(&*err), Some("/x".to_string()));
        assert!(is_fs_inaccessible(&*err));
    }

    #[test]
    fn axios_classification() {
        let err: Box<dyn std::error::Error + 'static> = Box::new(AxiosError {
            status: Some(401),
            code: None,
            message: "unauthorized".into(),
        });
        assert_eq!(
            classify_axios_error(&*err),
            AxiosErrorClassification {
                kind: AxiosErrorKind::Auth,
                status: Some(401),
                message: "unauthorized".into(),
            }
        );
        let timeout: Box<dyn std::error::Error + 'static> = Box::new(AxiosError {
            status: None,
            code: Some("ECONNABORTED".into()),
            message: "timeout".into(),
        });
        assert_eq!(classify_axios_error(&*timeout).kind, AxiosErrorKind::Timeout);
        let other: Box<dyn std::error::Error + 'static> = Box::new(ClaudeError::new("boom"));
        assert_eq!(classify_axios_error(&*other).kind, AxiosErrorKind::Other);
    }

    #[test]
    fn telemetry_safe_error_defaults() {
        let e = TelemetrySafeError::new("msg", None::<String>);
        assert_eq!(e.telemetry_message, "msg");
    }
}
