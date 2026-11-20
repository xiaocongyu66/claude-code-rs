//! ssrf_guard.rs — HTTP hook 的 SSRF 防护（重写自 `ssrfGuard.ts`）。
//!
//! 拦截私网、链路本地等不可达地址段，防止项目配置的 HTTP hook 触达云
//! 元数据端点（169.254.169.254）或内部基础设施。
//!
//! 环回（127.0.0.0/8、::1）**刻意放行**——本地 dev 策略服务器是 HTTP
//! hook 的主要用例。
//!
//! 使用全局代理或沙箱网络代理时，目标主机的 DNS 由代理解析，本防护对
//! 目标主机实际被绕过；沙箱代理自身有域名白名单。
//!
//! 拦截段（`is_blocked_address`）：
//! - IPv4：0.0.0.0/8（"this" network）、10.0.0.0/8（私网）、
//!   100.64.0.0/10（CGNAT/RFC 6598，部分云元数据如阿里云
//!   100.100.100.200）、169.254.0.0/16（链路本地/云元数据）、
//!   172.16.0.0/12（私网）、192.168.0.0/16（私网）。
//! - IPv6：::（unspecified）、fc00::/7（ULA）、fe80::/10（链路本地）、
//!   ::ffff:<v4>（映射 IPv4 落入上述 v4 段）。
//! - 放行：127.0.0.0/8、::1 及其余全部地址。
//!
//! 语义对齐：
//! - TS 的 `isBlockedV6` 依赖手工 `expandIPv6Groups`/`extractMappedIPv4`
//!   展开（因 Node `isIP` 只返回族号）；Rust 端直接用
//!   `std::net::Ipv6Addr::segments()`，映射地址与各前缀判定等价
//!   （fe80::/10 用精确掩码 `segs[0] >> 6 == 0x3FA`，即首 hextet
//!   fe80..febf，与 TS 的字符串区间比较同义）。
//! - `ssrf_guarded_lookup`：IP 字面量直连校验不走 DNS；主机名经
//!   `std::net::ToSocketAddrs`（spawn_blocking 线程内，对应 TS
//!   `dnsLookup(hostname, {all:true})`）解析后逐个校验，任一命中拦截段
//!   即拒绝。
//!   TS 的回调风格（axios `lookup` 选项）改为 `Result`；`want_all`
//!   对齐 TS 的 `options.all`。DNS 解析失败透传为 `SsrfLookupError::Dns`，
//!   空解析结果为 `ENOTFOUND`。
//!
//! 已知差异：
//! - TS 的 `isIP` 对 IPv6 zone id（fe80::1%eth0）返回 0（非法）；
//!   `std::net::Ipv6Addr` 接受 zone id。dns.lookup 的结果从不携带 zone
//!   id，该差异不影响防护路径。

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// HTTP hook 拦截错误（对齐 TS `ssrfError` 构造的 NodeJS.ErrnoException）。
#[derive(Debug)]
pub struct SsrfError {
    pub message: String,
    pub code: &'static str,
    pub hostname: String,
    pub address: String,
}

impl fmt::Display for SsrfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SsrfError {}

fn ssrf_error(hostname: &str, address: &str) -> SsrfError {
    SsrfError {
        message: format!(
            "HTTP hook blocked: {} resolves to {} (private/link-local address). Loopback (127.0.0.1, ::1) is allowed for local dev.",
            hostname, address
        ),
        code: "ERR_HTTP_HOOK_BLOCKED_ADDRESS",
        hostname: hostname.to_string(),
        address: address.to_string(),
    }
}

/// DNS 解析结果为空时的错误（对齐 TS `ENOTFOUND`）。
#[derive(Debug)]
pub struct NotFoundError {
    pub message: String,
    pub code: &'static str,
    pub hostname: String,
}

impl fmt::Display for NotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for NotFoundError {}

fn not_found_error(hostname: &str) -> NotFoundError {
    NotFoundError {
        message: format!("ENOTFOUND {}", hostname),
        code: "ENOTFOUND",
        hostname: hostname.to_string(),
    }
}

/// `ssrf_guarded_lookup` 的错误面。
#[derive(Debug)]
pub enum SsrfLookupError {
    /// 命中私网/链路本地拦截段。
    Blocked(SsrfError),
    /// 解析成功但无任何地址（ENOTFOUND）。
    NotFound(NotFoundError),
    /// DNS 解析失败（透传底层错误文本）。
    Dns(String),
}

impl fmt::Display for SsrfLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SsrfLookupError::Blocked(e) => fmt::Display::fmt(e, f),
            SsrfLookupError::NotFound(e) => fmt::Display::fmt(e, f),
            SsrfLookupError::Dns(msg) => f.write_str(msg),
        }
    }
}

impl Error for SsrfLookupError {}

/// DNS 解析结果（对齐 axios lookup 回调的二态：单个或全部）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LookupResult {
    /// `options.all` 为 false：首个地址 + 族号。
    One(String, u8),
    /// `options.all` 为 true：全部地址 + 族号。
    All(Vec<(String, u8)>),
}

fn is_blocked_v4(v4: &Ipv4Addr) -> bool {
    let [a, b, _c, _d] = v4.octets();

    // 环回显式放行
    if a == 127 {
        return false;
    }
    // 0.0.0.0/8
    if a == 0 {
        return true;
    }
    // 10.0.0.0/8
    if a == 10 {
        return true;
    }
    // 169.254.0.0/16 — 链路本地，云元数据
    if a == 169 && b == 254 {
        return true;
    }
    // 172.16.0.0/12
    if a == 172 && (16..=31).contains(&b) {
        return true;
    }
    // 100.64.0.0/10 — RFC 6598 CGNAT；部分云商用作元数据端点
    // （如阿里云 100.100.100.200）。
    if a == 100 && (64..=127).contains(&b) {
        return true;
    }
    // 192.168.0.0/16
    if a == 192 && b == 168 {
        return true;
    }

    false
}

fn is_blocked_v6(v6: &Ipv6Addr) -> bool {
    // ::1 环回显式放行
    if (*v6).is_loopback() {
        return false;
    }

    // :: unspecified
    if (*v6).is_unspecified() {
        return true;
    }

    let g = v6.segments();

    // IPv4-mapped IPv6（::ffff:a.b.c.d 等任意合法表示）。取出内嵌 IPv4
    // 委托 v4 检查——没有这一步，hex 形式的映射地址（如
    // ::ffff:a9fe:a9fe = 169.254.169.254）会绕过防护。
    if g[0] == 0 && g[1] == 0 && g[2] == 0 && g[3] == 0 && g[4] == 0 && g[5] == 0xffff {
        let v4 = Ipv4Addr::new(
            (g[6] >> 8) as u8,
            (g[6] & 0xff) as u8,
            (g[7] >> 8) as u8,
            (g[7] & 0xff) as u8,
        );
        return is_blocked_v4(&v4);
    }

    // fc00::/7 — 唯一本地地址（fc00:: 至 fdff::）
    if (g[0] >> 8) & 0xfe == 0xfc {
        return true;
    }

    // fe80::/10 — 链路本地。首 hextet 落在 fe80..febf（RFC 4291 要求
    // 其后 54 位为零，掩码判定与 TS 的 4 字符 hex 区间比较同义）。
    if g[0] >> 6 == 0x3fa {
        return true;
    }

    false
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

/// 地址是否落入 HTTP hook 不可达的段（详见模块头注释）。
/// 非法 IP 字面量返回 false——交给真实 DNS 路径处理（本函数只会被
/// dns.lookup 的结果调用，那些总是合法 IP）。
pub fn is_blocked_address(address: &str) -> bool {
    match address.parse::<IpAddr>() {
        Ok(ip) => is_blocked_ip(&ip),
        Err(_) => false,
    }
}

fn family_of(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V6(_) => 6,
        IpAddr::V4(_) => 4,
    }
}

/// dns.lookup 兼容的解析函数：解析主机名并拒绝命中拦截段的地址，保证
/// 校验过的 IP 就是 socket 连接的 IP——校验与连接之间无重绑定窗口。
///
/// 主机名为 IP 字面量时直接校验、不走 DNS。
///
/// - `want_all` 为 true 时返回全部（address, family)；否则返回首个。
/// - 解析结果逐个校验，任一命中拦截段即返回 `SsrfLookupError::Blocked`。
///
/// 阻塞式 `std::net::ToSocketAddrs`（对应 TS `dnsLookup(hostname,
/// {all: true})`）在 `spawn_blocking` 线程执行，避免阻塞异步运行时；
/// 调用方须处于 tokio 运行时上下文。
pub async fn ssrf_guarded_lookup(
    hostname: &str,
    want_all: bool,
) -> Result<LookupResult, SsrfLookupError> {
    // IP 字面量直接校验。dns.lookup 同样会短路，但此处给出更清晰的
    // 错误，并规避平台对字面量的 lookup 行为差异。
    if let Ok(ip) = hostname.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(SsrfLookupError::Blocked(ssrf_error(hostname, hostname)));
        }
        let address = ip.to_string();
        let family = family_of(&ip);
        return Ok(if want_all {
            LookupResult::All(vec![(address, family)])
        } else {
            LookupResult::One(address, family)
        });
    }

    let hostname_owned = hostname.to_string();
    let join = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        (hostname_owned.as_str(), 0u16).to_socket_addrs()
    })
    .await;
    let socket_addrs: Vec<SocketAddr> = match join {
        Ok(res) => res.map_err(|e| SsrfLookupError::Dns(e.to_string()))?.collect(),
        Err(join_error) => return Err(SsrfLookupError::Dns(join_error.to_string())),
    };

    for socket_addr in &socket_addrs {
        let ip = socket_addr.ip();
        if is_blocked_ip(&ip) {
            return Err(SsrfLookupError::Blocked(ssrf_error(hostname, &ip.to_string())));
        }
    }

    let first = socket_addrs.first().ok_or_else(|| {
        SsrfLookupError::NotFound(not_found_error(hostname))
    })?;

    if want_all {
        Ok(LookupResult::All(
            socket_addrs
                .iter()
                .map(|sa| (sa.ip().to_string(), family_of(&sa.ip())))
                .collect(),
        ))
    } else {
        Ok(LookupResult::One(
            first.ip().to_string(),
            family_of(&first.ip()),
        ))
    }
}
