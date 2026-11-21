//! in_process_transport.rs — 进程内链接传输对
//! （重写自 `InProcessTransport.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `InProcessTransport` 实现 MCP 传输契约：`send()` 投递到对端接收端，
//!   `close()` 先触发本侧 onclose、再关闭对端（双方各只触发一次）。
//! - TS 用 `queueMicrotask` 异步投递以避免同步请求/响应的栈深度问题；
//!   Rust 侧通过 channel 天然异步，投递语义一致。
//! - `create_linked_transport_pair()` 返回互连的一对传输
//!   `[client_transport, server_transport]`。
//!
//! 回调式 TS 接口（onmessage/onclose）在 Rust 侧以 channel + `on_close`
//! 回调表达：`start()` 返回消息接收端，close 回调单独注册。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::services::mcp::types::JSONRPCMessage;

/// 进程内 MCP 传输一侧（TS `InProcessTransport`）。
pub struct InProcessTransport {
    /// 本侧接收端；`start()` 时移交出去。
    own_rx: Mutex<Option<mpsc::UnboundedReceiver<JSONRPCMessage>>>,
    /// 投递到对端接收端的发送端。
    peer_tx: Mutex<Option<mpsc::UnboundedSender<JSONRPCMessage>>>,
    /// 对端引用（close 时联动）。
    peer: Mutex<Option<Arc<InProcessTransport>>>,
    closed: AtomicBool,
    on_close: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl InProcessTransport {
    fn new(
        own_rx: mpsc::UnboundedReceiver<JSONRPCMessage>,
        peer_tx: mpsc::UnboundedSender<JSONRPCMessage>,
    ) -> Self {
        Self {
            own_rx: Mutex::new(Some(own_rx)),
            peer_tx: Mutex::new(Some(peer_tx)),
            peer: Mutex::new(None),
            closed: AtomicBool::new(false),
            on_close: Mutex::new(None),
        }
    }

    /// 启动：取走本侧消息接收端（仅一次）。
    pub async fn start(&self) -> Result<mpsc::UnboundedReceiver<JSONRPCMessage>, String> {
        self.own_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| "Transport already started".to_string())
    }

    /// 发送给对端；已关闭抛 "Transport is closed"（对齐 TS）。
    pub async fn send(&self, message: JSONRPCMessage) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("Transport is closed".to_string());
        }
        let tx = self.peer_tx.lock().await;
        match tx.as_ref() {
            Some(tx) => tx.send(message).map_err(|_| "Peer closed".to_string()),
            None => Err("Transport has no peer".to_string()),
        }
    }

    /// 关闭本侧（幂等）：触发本侧 onclose，再级联关闭对端。
    pub async fn close(&self) -> Result<(), String> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(handler) = self.on_close.lock().await.take() {
            handler();
        }
        let peer = self.peer.lock().await;
        if let Some(peer) = peer.as_ref() {
            if !peer.closed.swap(true, Ordering::SeqCst) {
                if let Some(handler) = peer.on_close.lock().await.take() {
                    handler();
                }
            }
        }
        Ok(())
    }

    /// 注册 close 回调（对齐 TS `onclose`）。
    pub async fn set_on_close<F>(&self, handler: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.on_close.lock().await = Some(Arc::new(handler));
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// 创建一对互连的进程内传输。
///
/// 返回 `[client_transport, server_transport]`：在一侧 send 的消息到达
/// 另一侧接收端；任一侧 close 触发双方 onclose。
pub async fn create_linked_transport_pair() -> (Arc<InProcessTransport>, Arc<InProcessTransport>) {
    let (tx_for_a, rx_of_a) = mpsc::unbounded_channel::<JSONRPCMessage>();
    let (tx_for_b, rx_of_b) = mpsc::unbounded_channel::<JSONRPCMessage>();

    let a = Arc::new(InProcessTransport::new(rx_of_a, tx_for_b));
    let b = Arc::new(InProcessTransport::new(rx_of_b, tx_for_a));

    // 纯 async 锁：`blocking_lock` 会在 tokio 运行时线程上 panic
    *a.peer.lock().await = Some(b.clone());
    *b.peer.lock().await = Some(a.clone());

    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mcp::types::JSONRPCNotification;

    fn notification(method: &str) -> JSONRPCMessage {
        JSONRPCMessage::Notification(JSONRPCNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: None,
        })
    }

    #[tokio::test]
    async fn delivers_to_peer_and_closes_both() {
        let (a, b) = create_linked_transport_pair().await;
        let mut rx_b = b.start().await.unwrap();

        a.send(notification("ping")).await.unwrap();
        let received = rx_b.recv().await.unwrap();
        match received {
            JSONRPCMessage::Notification(n) => assert_eq!(n.method, "ping"),
            _ => panic!("expected notification"),
        }

        let closed = Arc::new(AtomicBool::new(false));
        let closed_clone = closed.clone();
        b.set_on_close(move || closed_clone.store(true, Ordering::SeqCst)).await;

        a.close().await.unwrap();
        assert!(a.is_closed());
        assert!(b.is_closed());
        assert!(closed.load(Ordering::SeqCst));
    }
}
