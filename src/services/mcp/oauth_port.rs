//! oauth_port.rs — OAuth 回调端口工具（重写自 `oauthPort.ts`）。
//!
//! 语义对齐（对 TS 原文）：
//! - Windows 动态端口段 49152-65535 预留，回退段为 39152-49151；
//!   其他平台直接使用 49152-65535。
//! - 回退端口 3118。
//! - `build_redirect_uri`：RFC 8252 §7.3 loopback 重定向 URI（固定
//!   `/callback` 路径，端口任选）。
//! - `find_available_port`：优先 `MCP_OAUTH_CALLBACK_PORT` 环境变量，
//!   之后在区间内随机探测（最多 100 次），最后回退 3118。
//!
//! 实现说明：TS 用 `http.createServer` 试探端口占用；Rust 侧用
/// `std::net::TcpListener::bind` 等效探测（bind 成功即端口可用，
/// drop 立即释放）。结果完全一致。

use std::net::TcpListener;

use rand::Rng;

use crate::utils::platform::{get_platform, Platform};

/// 回退端口（TS 同名常量）。
pub const REDIRECT_PORT_FALLBACK: u16 = 3118;

/// 重定向端口区间（按平台选择，同 TS）。
fn redirect_port_range() -> (u16, u16) {
    if get_platform() == Platform::Windows {
        (39152, 49151)
    } else {
        (49152, 65535)
    }
}

/// 在 localhost 上构造固定 `/callback` 路径的重定向 URI。
pub fn build_redirect_uri(port: Option<u16>) -> String {
    let port = port.unwrap_or(REDIRECT_PORT_FALLBACK);
    format!("http://localhost:{port}/callback")
}

/// 读取 `MCP_OAUTH_CALLBACK_PORT` 环境变量（>0 才生效）。
fn get_mcp_oauth_callback_port() -> Option<u16> {
    std::env::var("MCP_OAUTH_CALLBACK_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
}

/// 探测单个端口是否可用（bind 成功即空闲）。
fn port_is_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 在指定区间内为 OAuth 回调找一个可用端口。
///
/// 顺序：环境变量配置端口 → 区间内随机探测（≤100 次）→ 回退端口。
/// 全部失败返回 `Err`（对齐 TS 抛 `No available ports for OAuth redirect`）。
pub fn find_available_port() -> Result<u16, String> {
    if let Some(configured) = get_mcp_oauth_callback_port() {
        return Ok(configured);
    }

    let (min, max) = redirect_port_range();
    let range = (max - min + 1) as u32;
    let max_attempts = std::cmp::min(range, 100);

    let mut rng = rand::thread_rng();
    for _ in 0..max_attempts {
        let port = min + (rng.gen_range(0..range) as u16);
        if port_is_available(port) {
            return Ok(port);
        }
    }

    if port_is_available(REDIRECT_PORT_FALLBACK) {
        return Ok(REDIRECT_PORT_FALLBACK);
    }

    Err("No available ports for OAuth redirect".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_redirect_uri() {
        assert_eq!(build_redirect_uri(Some(3118)), "http://localhost:3118/callback");
        assert_eq!(
            build_redirect_uri(None),
            format!("http://localhost:{REDIRECT_PORT_FALLBACK}/callback")
        );
    }

    #[test]
    fn finds_a_port() {
        let port = find_available_port().ok();
        assert!(port.is_some());
    }
}
