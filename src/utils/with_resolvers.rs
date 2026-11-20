//! with_resolvers.rs — `Promise.withResolvers` 的 Rust 对应物
//! （重写自 `src/utils/withResolvers.ts`）。
//!
//! TS 版是 ES2024 polyfill（Node 18 兼容）：把 Promise 的 resolve /
//! reject 句柄从构造器里提出来。Rust 侧用 `tokio::sync::oneshot`：
//!
//! - `receiver` ← promise（等待方）
//! - `resolver` ← resolve（发送值后 receiver 得到 `Ok`）
//!
//! 语义对齐说明：
//! - TS 的 `reject(reason)` 可以携带任意错误值；oneshot 无拒绝通道，
//!   「reject」由 `Receiver::await` 返回 `Err(RecvError)` 表达
//!   （发送端被 drop = 被拒绝/取消）。需要携带错误值时改用
//!   `with_resolvers::<Result<T, E>>()`。
//! - resolver 可重复调用安全：oneshot 发送一次后其余忽略
//!   （Receiver 收到第一个结果，不会多唤醒）。

use tokio::sync::oneshot;

/// `PromiseWithResolvers<T>` 的 Rust 对应物。
pub struct WithResolvers<T> {
    /// 等待方（TS `promise`）。
    pub receiver: oneshot::Receiver<T>,
    /// 发送方（TS `resolve`）；drop 等价 TS 的 reject 语义
    ///（receiver 收到 `Err(RecvError)`）。
    pub resolver: oneshot::Sender<T>,
}

/// 创建一对 resolver/receiver（对齐 TS `withResolvers<T>()`）。
pub fn with_resolvers<T>() -> WithResolvers<T> {
    let (resolver, receiver) = oneshot::channel::<T>();
    WithResolvers { receiver, resolver }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_with_value() {
        let w = with_resolvers::<u32>();
        // 发送前 receiver 不就绪
        let receiver = w.receiver;
        w.resolver.send(42).ok();
        assert_eq!(receiver.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn drop_resolver_rejects() {
        let w = with_resolvers::<u32>();
        drop(w.resolver);
        assert!(w.receiver.await.is_err());
    }

    #[tokio::test]
    async fn resolves_across_tasks() {
        let w = with_resolvers::<String>();
        let receiver = w.receiver;
        let task = tokio::spawn(async move { receiver.await });
        // 稍后才 resolve（对齐 TS 把 resolve 交给另一个异步路径）
        tokio::task::yield_now().await;
        w.resolver.send("done".to_string()).ok();
        assert_eq!(task.await.unwrap().unwrap(), "done");
    }
}
