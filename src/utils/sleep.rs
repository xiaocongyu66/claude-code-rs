//! sleep.rs — 可取消的 sleep 与超时竞速（重写自 `src/utils/sleep.ts`）。
//!
//! 语义对齐说明：
//! - TS 的 `AbortSignal` 对齐 `abort_controller::CancellationToken`。
//!   该令牌目前无异步等待原语，取消检测用最多 10ms 一次的轮询
//!  （`ABORT_POLL_INTERVAL_MS`）：语义一致、唤醒延迟 ≤10ms。
//! - TS `sleep` 的 `throwOnAbort` / `abortError`：Rust 端以
//!   `SleepOutcome::Aborted` 表达，由调用方决定映射成错误还是继续
//!  （等价 throwOnAbort 时 `return Err(...)`）。`unref` 是 Node 计时器
//!   专有概念，Rust 无对应物，忽略。
//! - TS `withTimeout` 返回 `Promise<T>`，inner reject 时透传该错误；
//!   Rust 端 `F: Future<Output = T>`，inner 错误由调用方在 future 内
//!   自行 map（Rust 无非类型化的 rejection）。
//! - 超时不会取消底层工作（与 TS 一致），只把控制权交还调用方。

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::utils::abort_controller::CancellationToken;

/// 取消检测轮询间隔：abort 后至多这么久后 sleep 返回。
const ABORT_POLL_INTERVAL_MS: u64 = 10;

/// sleep 的结果（对齐 TS sleep 的 resolve / abort 两条路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepOutcome {
    /// 计时完成（TS 的 `setTimeout` 到期 resolve）。
    Completed,
    /// 令牌取消（TS 默认静默 resolve；throwOnAbort 时调用方转成错误）。
    Aborted,
}

/// 可取消的 sleep：`ms` 毫秒后完成；令牌取消时立即返回。
///
/// 调用方应在 await 之后自行检查 `token.aborted()`（对齐 TS 注释：
/// 默认 abort 静默 resolve）。
pub async fn sleep(ms: u64, token: Option<&CancellationToken>) -> SleepOutcome {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(token) = token {
            if token.aborted() {
                return SleepOutcome::Aborted;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return SleepOutcome::Completed;
        }
        let remaining = deadline - now;
        let chunk = remaining.min(Duration::from_millis(ABORT_POLL_INTERVAL_MS));
        tokio::time::sleep(chunk).await;
    }
}

/// 超时错误（对齐 TS `withTimeout` 的 `Error(message)` rejection）。
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TimeoutError {
    message: String,
}

impl TimeoutError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 让 future 与超时竞速：`ms` 毫秒内未完成则返回 `Err(TimeoutError)`。
/// 不会取消底层 future（对齐 TS 注释：仅交还控制权）。
pub async fn with_timeout<F, T>(
    future: F,
    ms: u64,
    message: impl Into<String>,
) -> Result<T, TimeoutError>
where
    F: Future<Output = T>,
{
    match tokio::time::timeout(Duration::from_millis(ms), future).await {
        Ok(value) => Ok(value),
        Err(_) => Err(TimeoutError::new(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleep_completes_after_ms() {
        let start = Instant::now();
        assert_eq!(sleep(50, None).await, SleepOutcome::Completed);
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn sleep_zero_completes() {
        assert_eq!(sleep(0, None).await, SleepOutcome::Completed);
    }

    #[tokio::test]
    async fn sleep_aborts_immediately_when_token_pre_aborted() {
        let token = CancellationToken::new();
        token.abort(None);
        assert_eq!(sleep(60_000, Some(&token)).await, SleepOutcome::Aborted);
    }

    #[tokio::test]
    async fn sleep_aborts_mid_wait() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token_clone.abort(None);
        });
        let outcome = sleep(60_000, Some(&token)).await;
        assert_eq!(outcome, SleepOutcome::Aborted);
    }

    #[tokio::test]
    async fn with_timeout_succeeds_in_time() {
        let result =
            with_timeout(async { 7 }, 1000, "too slow").await;
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn with_timeout_errors_on_timeout() {
        let result =
            with_timeout(
                tokio::time::sleep(Duration::from_secs(60)),
                10,
                "timed out",
            )
            .await;
        let err = result.unwrap_err();
        assert_eq!(err.message(), "timed out");
    }
}
