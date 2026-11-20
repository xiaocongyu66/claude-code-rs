//! health_check.rs — MCP 连接活性探测（重写自 `healthCheck.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - 一次性 ping 探测 `probe_server_health`：2s 超时，结构化结果
//!   （healthy/latencyMs/unreachable/error），永不抛错。
//! - 周期探测 `start_health_check`：30s 间隔、连续 3 次失败标记 unhealthy，
//!   healthy→unhealthy 与 unhealthy→healthy 转换时回调一次；返回停止函数，
//!   按服务器名幂等（重启替换旧任务）。
//! - 全套功能由 `CLAUDE_CODE_MCP_HEALTH_CHECK=1` 显式开启（默认关）。
//!
//! 实现说明：TS 对 SDK `client.ping()` 加本地 timer 竞速；Rust 端
//! `McpClient::ping` 本身即 tokio::time::timeout 包装，直接复用。
//! 周期探测用 tokio 任务 + `CancellationToken::aborted()` 轮询停止。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::services::mcp::client::McpClient;
use crate::services::mcp::types::{McpHealthStatus, MCPServerHealth};
use crate::utils::abort_controller::CancellationToken;
use crate::utils::env_utils::is_env_truthy;
use crate::utils::log::{log_mcp_debug, log_mcp_error};

/// mcp-dispatch health 锚点默认值：2s 探测预算。
pub const HEALTH_PROBE_TIMEOUT_MS: u64 = 2000;
/// 默认 ping 间隔（30s）。
pub const HEALTH_CHECK_INTERVAL_MS: u64 = 30_000;
/// 标记 unhealthy 前的连续失败次数。
pub const HEALTH_CHECK_FAILURE_THRESHOLD: u32 = 3;

pub const MCP_HEALTH_CHECK_ENV: &str = "CLAUDE_CODE_MCP_HEALTH_CHECK";

/// 健康探测选项。
#[derive(Debug, Clone, Default)]
pub struct HealthCheckOptions {
    /// ping 间隔（默认 30s）。
    pub interval_ms: Option<u64>,
    /// 单次 ping 超时（默认 2s）。
    pub timeout_ms: Option<u64>,
    /// 标记 unhealthy 的连续失败数（默认 3）。
    pub failure_threshold: Option<u32>,
    /// healthy→unhealthy 转换时调用一次。
    pub on_unhealthy: Option<Arc<dyn Fn(&MCPServerHealth) + Send + Sync>>,
    /// unhealthy→healthy 转换时调用一次。
    pub on_recover: Option<Arc<dyn Fn(&MCPServerHealth) + Send + Sync>>,
}

/// 单次 ping 结果（结构化，永不抛错）。
pub struct ProbeResult {
    pub healthy: bool,
    pub latency_ms: u128,
    pub unreachable: bool,
    pub error: Option<String>,
}

/// 一次性 ping：对齐 mcp-dispatch 的 2s 超时语义。
pub async fn probe_server_health(client: &McpClient, timeout_ms: u64) -> ProbeResult {
    let start = Instant::now();
    match client.ping(Some(Duration::from_millis(timeout_ms))).await {
        Ok(()) => ProbeResult {
            healthy: true,
            latency_ms: start.elapsed().as_millis(),
            unreachable: false,
            error: None,
        },
        Err(e) => ProbeResult {
            healthy: false,
            latency_ms: start.elapsed().as_millis(),
            unreachable: true,
            error: Some(e.to_string()),
        },
    }
}

/// 全部功能的环境门控（默认关）。
pub fn is_mcp_health_check_enabled() -> bool {
    is_env_truthy(std::env::var(MCP_HEALTH_CHECK_ENV).ok().as_deref())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct HealthState {
    cancel: CancellationToken,
    latest: Arc<Mutex<Option<MCPServerHealth>>>,
}

static RUNTIMES: once_cell::sync::Lazy<Mutex<HashMap<String, HealthState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

struct ProbeLoop {
    name: String,
    client: Arc<McpClient>,
    interval_ms: u64,
    timeout_ms: u64,
    failure_threshold: u32,
    options: HealthCheckOptions,
    latest: Arc<Mutex<Option<MCPServerHealth>>>,
}

impl ProbeLoop {
    async fn run(self, cancel: CancellationToken) {
        let mut failure_count: u32 = 0;
        let mut status = "healthy";
        let mut last_error: Option<String> = None;

        loop {
            tokio::time::sleep(Duration::from_millis(self.interval_ms)).await;
            if cancel.aborted() {
                break;
            }

            let result = probe_server_health(&self.client, self.timeout_ms).await;
            let last_ping_at = now_ms();

            if result.healthy {
                if status == "unhealthy" {
                    status = "healthy";
                    failure_count = 0;
                    last_error = None;
                    let health = self.build_health(status, failure_count, last_ping_at, &last_error);
                    log_mcp_debug(
                        &self.name,
                        &format!("Health check recovered ({}ms ping)", result.latency_ms),
                    );
                    {
                        let mut latest = self.latest.lock().await;
                        *latest = Some(health.clone());
                    }
                    if let Some(on_recover) = self.options.on_recover.clone() {
                        on_recover(&health);
                    }
                    continue;
                }
                failure_count = 0;
                last_error = None;
            } else {
                failure_count += 1;
                last_error = result.error.clone();
                log_mcp_debug(
                    &self.name,
                    &format!(
                        "Health check ping failed ({}/{threshold}): {}",
                        failure_count,
                        result.error.as_deref().unwrap_or("timeout"),
                        threshold = self.failure_threshold
                    ),
                );
                if failure_count >= self.failure_threshold && status != "unhealthy" {
                    status = "unhealthy";
                    log_mcp_error(
                        &self.name,
                        format!(
                            "MCP server marked unhealthy after {failure_count} failed health checks: {}",
                            result.error.as_deref().unwrap_or("timeout")
                        )
                        .into(),
                    );
                    let health =
                        self.build_health(status, failure_count, last_ping_at, &last_error);
                    {
                        let mut latest = self.latest.lock().await;
                        *latest = Some(health.clone());
                    }
                    if let Some(on_unhealthy) = self.options.on_unhealthy.clone() {
                        on_unhealthy(&health);
                    }
                    continue;
                }
            }

            let health = self.build_health(status, failure_count, last_ping_at, &last_error);
            let mut latest = self.latest.lock().await;
            *latest = Some(health);
        }
    }

    fn build_health(
        &self,
        status: &str,
        failure_count: u32,
        last_ping_at: i64,
        last_error: &Option<String>,
    ) -> MCPServerHealth {
        MCPServerHealth {
            status: if status == "unhealthy" {
                McpHealthStatus::Unhealthy
            } else {
                McpHealthStatus::Healthy
            },
            failure_count,
            last_ping_at: Some(last_ping_at),
            last_error: last_error.clone(),
        }
    }
}

/// 启动周期 ping 探测；按服务器名幂等（重启替换旧任务）。
pub async fn start_health_check(
    name: &str,
    client: Arc<McpClient>,
    options: HealthCheckOptions,
) -> CancellationToken {
    // 先停旧的（幂等）
    stop_health_check(name).await;

    let interval_ms = options.interval_ms.unwrap_or(HEALTH_CHECK_INTERVAL_MS);
    let timeout_ms = options.timeout_ms.unwrap_or(HEALTH_PROBE_TIMEOUT_MS);
    let failure_threshold = options.failure_threshold.unwrap_or(HEALTH_CHECK_FAILURE_THRESHOLD);

    let cancel = CancellationToken::new();
    let latest: Arc<Mutex<Option<MCPServerHealth>>> = Arc::new(Mutex::new(None));
    let loop_ = ProbeLoop {
        name: name.to_string(),
        client,
        interval_ms,
        timeout_ms,
        failure_threshold,
        options,
        latest: latest.clone(),
    };
    let task_cancel = cancel.clone();
    tokio::spawn(async move {
        loop_.run(task_cancel).await;
    });

    let mut runtimes = RUNTIMES.lock().await;
    runtimes.insert(name.to_string(), HealthState { cancel: cancel.clone(), latest });
    cancel
}

/// 停止某服务器的探测（幂等）。
pub async fn stop_health_check(name: &str) {
    let entry = {
        let mut runtimes = RUNTIMES.lock().await;
        runtimes.remove(name)
    };
    if let Some(state) = entry {
        state.cancel.abort(None);
    }
}

/// 某服务器最新的健康标记（若探测过）。
pub async fn get_server_health(name: &str) -> Option<MCPServerHealth> {
    let runtimes = RUNTIMES.lock().await;
    let state = runtimes.get(name)?;
    let latest = state.latest.lock().await;
    latest.clone()
}

/// 停止全部探测（会话拆卸）。
pub async fn stop_all_health_checks() {
    let names: Vec<String> = {
        let runtimes = RUNTIMES.lock().await;
        runtimes.keys().cloned().collect()
    };
    for name in names {
        stop_health_check(&name).await;
    }
}
