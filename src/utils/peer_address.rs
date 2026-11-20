//! peer_address.rs — peer 地址解析。
//!
//! 重写自 `src/utils/peerAddress.ts`。与 peerRegistry.ts 分开存放，
//! 使 SendMessageTool 可以在不传递加载 bridge（axios）与 UDS（fs、net）
//! 模块的情况下引入 parseAddress。

/// 地址 scheme：uds / bridge / tcp / 其它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Uds,
    Bridge,
    Tcp,
    Other,
}

/// 解析 URI 风格地址为 scheme + target。
///
/// 兜底：旧版 UDS 发送方在 from= 中直接发裸 socket 路径，此时按 UDS
/// 处理，否则回复会被静默丢进 teammate 路由。（无裸 session ID 兜底
/// —— bridge 消息足够新，不存在旧发送方，且裸前缀会劫持
/// `session_manager` 这类 teammate 名。）
pub fn parse_address(to: &str) -> (Scheme, &str) {
    if let Some(rest) = to.strip_prefix("uds:") {
        return (Scheme::Uds, rest);
    }
    if let Some(rest) = to.strip_prefix("bridge:") {
        return (Scheme::Bridge, rest);
    }
    if let Some(rest) = to.strip_prefix("tcp:") {
        return (Scheme::Tcp, rest);
    }
    if to.starts_with('/') {
        return (Scheme::Uds, to);
    }
    (Scheme::Other, to)
}

/// 解析 tcp: target 为 `(host, port)`；非法格式或超范围端口返回 None。
pub fn parse_tcp_target(target: &str) -> Option<(String, u16)> {
    let (host, port) = target.split_once(':')?;
    if host.is_empty() || port.is_empty() {
        return None;
    }
    // 端口段必须全为数字（对齐 TS 的 /^\d+$/）
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u32 = port.parse().ok()?;
    (1..=65535)
        .contains(&port)
        .then(|| (host.to_string(), port as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        assert_eq!(parse_address("uds:/tmp/sock"), (Scheme::Uds, "/tmp/sock"));
        assert_eq!(parse_address("bridge:sess"), (Scheme::Bridge, "sess"));
        assert_eq!(
            parse_address("tcp:1.2.3.4:8080"),
            (Scheme::Tcp, "1.2.3.4:8080")
        );
        assert_eq!(parse_address("/tmp/sock"), (Scheme::Uds, "/tmp/sock"));
        assert_eq!(parse_address("someone"), (Scheme::Other, "someone"));
    }

    #[test]
    fn tcp_targets() {
        assert_eq!(
            parse_tcp_target("example.com:8080"),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(parse_tcp_target("example.com:0"), None);
        assert_eq!(parse_tcp_target("example.com:65536"), None);
        assert_eq!(parse_tcp_target("example.com"), None);
        assert_eq!(parse_tcp_target("example.com:abc"), None);
    }
}
