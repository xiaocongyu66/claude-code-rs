//! sequential.rs — 异步函数的顺序执行包装器。
//!
//! 重写自 `src/utils/sequential.ts`。语义对齐：
//!
//! - TS 的 `sequential(fn)` 返回一个包装函数：并发调用会被排入 FIFO 队列，
//!   一次只执行一个，每个调用者拿到**自己**那次调用的结果（含错误）。
//! - Rust 侧落地为 `SequentialQueue`：调用方创建一次队列（等价 TS 闭包里
//!   捕获的 queue/processing 状态），之后每次 `run(f)` 等价一次包装调用。
//! - TS 用 Promise 的 resolve/reject 回传结果；Rust 用 `tokio::sync::oneshot`，
//!   返回 `Result<R, SequentialError>`（偏差：处理器任务在所有 sender drop
//!   后才会退出，正常使用不会发生 `QueueDropped`；出现它说明队列被提前
//!   drop 且仍有未完成任务，此时无法凭空造出 R，故显式报错而不是 panic）。
//! - TS 的 `fn.apply(context, args)` 在 Rust 侧由闭包 `F: FnOnce() -> Fut`
//!   携带，调用方自己绑定参数与 `this` 语义。
//!
//! 使用注意：`SequentialQueue::new()` 会 `tokio::spawn` 处理器任务，必须在
//! tokio 运行时上下文中调用（TS 版在首次调用包装函数时惰性进入处理循环，
//! Rust 版在构造时启动）。

use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot};

/// 队列中待执行的一个任务：一个已装箱的异步闭包，完成后把结果写回
/// 对应的 oneshot sender。
struct Job {
    fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
}

/// `run` 的失败模式。仅当处理器任务消失（队列被 drop 仍有待处理任务）
/// 或任务自身被取消时出现。
#[derive(Debug, thiserror::Error)]
pub enum SequentialError {
    #[error("sequential queue processor exited before the task completed")]
    QueueDropped,
}

/// 顺序执行队列。等价 TS `sequential()` 返回的包装函数所持有的内部状态。
pub struct SequentialQueue {
    sender: mpsc::UnboundedSender<Job>,
}

impl SequentialQueue {
    /// 创建队列并启动单消费者处理器任务。
    /// 必须在 tokio 运行时上下文中调用。
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                // 一次只跑一个，跑到结束再取下一个（对齐 TS 的 while 循环）。
                (job.fut).await;
            }
        });
        Self { sender: tx }
    }

    /// 提交一个异步任务，返回拿回其结果的 Future。
    ///
    /// 等价 TS 包装函数的一次调用：排队 → 处理器按 FIFO 执行 →
    /// 调用者拿到结果或错误。
    pub fn run<F, Fut, R>(&self, f: F) -> impl Future<Output = Result<R, SequentialError>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (otx, orx) = oneshot::channel::<R>();
        let fut = Box::pin(async move {
            let result = f().await;
            let _ = otx.send(result);
        });
        // channel 满只可能发生在队列被 drop 之后；忽略 send 失败，
        // 调用方会通过 orx 收到 QueueDropped。
        let _ = self.sender.send(Job { fut });
        async move { orx.map_err(|_| SequentialError::QueueDropped).await }
    }
}

impl Default for SequentialQueue {
    fn default() -> Self {
        Self::new()
    }
}
