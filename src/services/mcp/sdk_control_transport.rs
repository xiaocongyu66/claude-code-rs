//! sdk_control_transport.rs — SDK MCP 控制传输桥
//! （重写自 `SdkControlTransport.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `SdkControlClientTransport`（CLI 侧）：send() 把 MCP 消息包成控制
//!   请求经 `SendMcpMessageCallback` 发给 SDK 进程，再把经路由回来的
//!   响应交给本侧接收端，供 MCP 客户端读取。
//! - `SdkControlServerTransport`（SDK 侧）：send() 仅把响应经回调回传
//!   （请求/响应关联由 SDK 侧 Query 负责）。
//! - 消息 ID 全程保留，多服务器以 server_name 路由。
//!
//! 已知偏差：TS 的回调式 Transport（onmessage/onerror/onclose）在 Rust 侧
//! 用 channel 表达：`start()` 返回消息接收端。

use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::services::mcp::types::JSONRPCMessage;

/// MCP 消息发送回调：按服务器名发出消息并等待响应（对齐 TS
/// `SendMcpMessageCallback`）。
pub type SendMcpMessageCallback =
    Arc<dyn Fn(&str, &JSONRPCMessage) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<JSONRPCMessage>> + Send>> + Send + Sync>;

/// CLI 侧传输：把 MCP 协议消息转成发往 SDK 进程的控制请求。
pub struct SdkControlClientTransport {
    server_name: String,
    send_mcp_message: SendMcpMessageCallback,
    is_closed: Arc<AtomicClosed>,
    on_close: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<JSONRPCMessage>>>,
    tx_for_self: Mutex<Option<mpsc::UnboundedSender<JSONRPCMessage>>>,
}

use std::sync::atomic::{AtomicBool, Ordering};

struct AtomicClosed(AtomicBool);

impl SdkControlClientTransport {
    pub fn new(server_name: impl Into<String>, send_mcp_message: SendMcpMessageCallback) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<JSONRPCMessage>();
        Self {
            server_name: server_name.into(),
            send_mcp_message,
            is_closed: Arc::new(AtomicClosed(AtomicBool::new(false))),
            on_close: Mutex::new(None),
            rx: Mutex::new(Some(rx)),
            tx_for_self: Mutex::new(Some(tx)),
        }
    }

    /// 启动：取走消息接收端（仅一次）。
    pub async fn start(&self) -> Result<mpsc::UnboundedReceiver<JSONRPCMessage>, String> {
        self.rx
            .lock()
            .await
            .take()
            .ok_or_else(|| "Transport already started".to_string())
    }

    /// 发送：把消息投给 SDK 进程，响应转发到本侧接收端。
    pub async fn send(&self, message: JSONRPCMessage) -> Result<(), String> {
        if self.is_closed.0.load(Ordering::SeqCst) {
            return Err("Transport is closed".to_string());
        }
        let response = (self.send_mcp_message)(&self.server_name, &message).await;
        if let Some(response) = response {
            let tx = self.tx_for_self.lock().await;
            if let Some(tx) = tx.as_ref() {
                let _ = tx.send(response);
            }
        }
        Ok(())
    }

    /// 关闭（幂等）：触发 onclose。
    pub async fn close(&self) -> Result<(), String> {
        if self.is_closed.0.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(handler) = self.on_close.lock().await.take() {
            handler();
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
}

/// SDK 侧传输：把响应经回调回传（请求关联由 Query 负责）。
pub struct SdkControlServerTransport {
    send_response: Arc<dyn Fn(&JSONRPCMessage) + Send + Sync>,
    is_closed: AtomicBool,
    on_close: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<JSONRPCMessage>>>,
}

impl SdkControlServerTransport {
    pub fn new(send_response: impl Fn(&JSONRPCMessage) + Send + Sync + 'static) -> Self {
        Self {
            send_response: Arc::new(send_response),
            is_closed: AtomicBool::new(false),
            on_close: Mutex::new(None),
            rx: Mutex::new(None),
        }
    }

    pub async fn set_incoming_receiver(&self, rx: mpsc::UnboundedReceiver<JSONRPCMessage>) {
        *self.rx.lock().await = Some(rx);
    }

    pub fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }

    /// send：简单透传（对齐 TS）。
    pub fn send(&self, message: &JSONRPCMessage) -> Result<(), String> {
        if self.is_closed() {
            return Err("Transport is closed".to_string());
        }
        (self.send_response)(message);
        Ok(())
    }

    /// 关闭（幂等）。
    pub async fn close(&self) -> Result<(), String> {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(handler) = self.on_close.lock().await.take() {
            handler();
        }
        Ok(())
    }
}
