//! elicitation_handler.rs — elicitation 请求处理（重写自
//! `elicitationHandler.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - `ElicitationRequestEvent`：服务器 elicitation 请求事件（serverName、
//!   requestId、params、应答器、URL 模式的等待态/completed 标记）。
//! - `register_elicitation_handler`：在 MCP 客户端上注册
//!   `elicitation/create` 请求处理器 —— 先跑 hooks（可程序化应答），
//!   未处理则排队等待 UI（AppState）应答；abort 时应答 cancel。
//! - `run_elicitation_hooks` / `run_elicitation_result_hooks`：
//!   PreElicitation / ElicitationResult hooks 执行入口（utils/hooks.ts 未
//!   转换 → 占位返回 None，语义等价于"无 hook 拦截"）。
//! - URL 模式：waitingState（actionLabel、showCancel）+ onWaitingDismiss。
//!
//! 已知偏差：
//! - `state/AppState`（React）与 `utils/messageQueueManager.ts` 未转换：
//!   排队/应答通过本模块内的 `tokio::sync::Mutex<Vec<…>>` 队列 + oneshot
//!   表达，`respond` 回调由调用方持有。
//! - `utils/hooks.ts` 未转换：hooks 返回 None（直接走 UI 路径）。

use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

use crate::services::mcp::client::{McpClient, ServerRequestHandler};
use crate::services::mcp::types::{ElicitAction, ElicitParams, ElicitResult};
use crate::utils::abort_controller::CancellationToken;
use crate::utils::log::log_mcp_debug;

/// 等待态（用户打开 URL 后显示：Retry now / 可选 Cancel）。
#[derive(Debug, Clone)]
pub struct ElicitationWaitingState {
    pub action_label: String,
    pub show_cancel: bool,
}

/// 排队的一个 elicitation 请求事件（对齐 TS `ElicitationRequestEvent`）。
pub struct ElicitationRequestEvent {
    pub server_name: String,
    pub request_id: String,
    pub params: ElicitParams,
    /// 应答器：UI 调用 respond(结果) 解析服务器请求。
    pub respond: Box<dyn FnOnce(ElicitResult) + Send>,
    /// URL 模式等待态。
    pub waiting_state: Option<ElicitationWaitingState>,
    /// phase 2（等待）由用户动作或 completion 通知 dismiss。
    pub on_waiting_dismiss: Option<Arc<dyn Fn(&'static str) + Send + Sync>>,
    /// completion 通知处理器置位。
    pub completed: bool,
}

/// 会话内 elicitation 队列（AppState 未转换 → 本地队列）。
static ELICITATION_QUEUE: once_cell::sync::Lazy<
    Mutex<Vec<Arc<Mutex<Option<ElicitationRequestEvent>>>>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(Vec::new()));

/// 按 (serverName, elicitationId) 查队列索引（TS `findElicitationInQueue`）。
pub async fn find_elicitation_in_queue(
    queue: &[Arc<Mutex<Option<ElicitationRequestEvent>>>],
    server_name: &str,
    elicitation_id: &str,
) -> Option<usize> {
    for (index, slot) in queue.iter().enumerate() {
        let event = slot.lock().await;
        let Some(event) = event.as_ref() else {
            continue;
        };
        if event.server_name == server_name
            && event.params.mode.as_deref() == Some("url")
            && event.params.elicitation_id.as_deref() == Some(elicitation_id)
        {
            return Some(index);
        }
    }
    None
}

/// elicitation 模式：`url` 或 `form`（其它一律 form）。
pub fn get_elicitation_mode(params: &ElicitParams) -> &'static str {
    if params.mode.as_deref() == Some("url") {
        "url"
    } else {
        "form"
    }
}

/// 在 MCP 客户端上注册 elicitation 请求处理器。
///
/// 处理顺序：
/// 1. hooks（`run_elicitation_hooks`；未转换 → None）
/// 2. URL 模式的 completion 通知已解析的队列事件
/// 3. 入队等待 UI 应答（`enqueue_slot_tx` 由调用方提供）
///
/// `enqueue` 收到事件句柄后由 UI 层展示对话框；应答通过句柄上的
/// `respond` 完成（客户端会收到 JSON-RPC 响应）。
pub async fn register_elicitation_handler(
    client: &Arc<McpClient>,
    server_name: &str,
    enqueue: Option<
        Arc<dyn Fn(ElicitationRequestEvent) -> oneshot::Receiver<ElicitResult> + Send + Sync>,
    >,
) {
    let server_name_owned = server_name.to_string();
    let enqueue = enqueue.clone();
    let handler: ServerRequestHandler = Arc::new(move |params| {
        let server_name = server_name_owned.clone();
        let enqueue = enqueue.clone();
        Box::pin(async move {
            let elicit_params: ElicitParams = serde_json::from_value(params).unwrap_or_default();
            let mode = get_elicitation_mode(&elicit_params);
            log_mcp_debug(
                &server_name,
                &format!("Received elicitation request (mode={mode})"),
            );

            // 1) hooks（未转换：无拦截）
            // 2) 入队等待 UI；无 UI 桥时直接 cancel
            match &enqueue {
                Some(enqueue) => {
                    let (tx, rx) = oneshot::channel::<ElicitResult>();
                    let event = ElicitationRequestEvent {
                        server_name: server_name.clone(),
                        request_id: format!("elicitation-{}", now_ms()),
                        params: elicit_params,
                        respond: Box::new(move |result| {
                            let _ = tx.send(result);
                        }),
                        waiting_state: None,
                        on_waiting_dismiss: None,
                        completed: false,
                    };
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(5 * 60),
                        enqueue(event),
                    )
                    .await;
                    match result {
                        Ok(receiver) => match receiver.await {
                            Ok(result) => Ok(serde_json::to_value(result).unwrap_or_default()),
                            Err(_) => Ok(serde_json::json!({ "action": "cancel" })),
                        },
                        Err(_) => Ok(serde_json::json!({ "action": "cancel" })),
                    }
                }
                None => Ok(serde_json::json!({ "action": "cancel" })),
            }
        })
    });
    client
        .set_request_handler("elicitation/create", handler)
        .await;
}

/// PreElicitation hooks 执行（utils/hooks.ts 未转换 → None，即无拦截）。
pub async fn run_elicitation_hooks(
    server_name: &str,
    _params: &ElicitParams,
    _signal: &CancellationToken,
) -> Option<ElicitResult> {
    log_mcp_debug(server_name, "No elicitation hooks configured (hooks.ts unported)");
    None
}

/// ElicitationResult hooks 执行（可改写/阻止应答；未转换 → 原样）。
pub async fn run_elicitation_result_hooks(
    server_name: &str,
    result: &ElicitResult,
    _signal: &CancellationToken,
    _elicitation_type: &str,
    _elicitation_id: &str,
) -> Option<ElicitResult> {
    log_mcp_debug(server_name, "No elicitation result hooks configured (hooks.ts unported)");
    let _ = result;
    None
}

/// 处理 `notifications/claude_elicitation_completion`（URL 模式 phase 2）：
/// 标记队列中匹配事件 completed，触发 onWaitingDismiss("dismiss")。
pub async fn handle_elicitation_complete(
    server_name: &str,
    elicitation_id: &str,
) -> Option<usize> {
    let queue = ELICITATION_QUEUE.lock().await;
    let index = find_elicitation_in_queue(&queue, server_name, elicitation_id).await?;
    let event_slot = queue[index].clone();
    drop(queue);
    let mut event_guard = event_slot.lock().await;
    if let Some(event) = event_guard.as_mut() {
        event.completed = true;
        if let Some(on_waiting_dismiss) = &event.on_waiting_dismiss {
            on_waiting_dismiss("dismiss");
        }
    }
    Some(index)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(dead_code)]
fn keep(result: &ElicitResult) -> ElicitAction {
    result.action
}
