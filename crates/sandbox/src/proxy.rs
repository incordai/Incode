//! The allowlisting proxy behind `network.mode = "proxy"`.
//!
//! The confined command may open TCP only to this proxy (the kernel enforces
//! that, see [`crate::Enforcement`]); the proxy runs in the unconfined parent
//! and lets through only the hosts the policy allows. It speaks the two forms
//! every HTTP client uses through `HTTPS_PROXY` / `HTTP_PROXY`:
//!
//! - `CONNECT host:port` — TLS and anything else tunnelled; the proxy sees the
//!   host name and nothing of the content.
//! - `GET http://host/path` — plain HTTP; forwarded with `Connection: close`,
//!   so a kept-alive connection cannot be reused for a second host unseen.
//!
//! Decisions are made on the NAME the client asked for, and when the proxy
//! connects directly the name is resolved once and the connection goes to the
//! address that was checked, so a DNS answer cannot redirect an allowed name
//! to a private address or the cloud metadata service between the check and
//! the connect. When this process itself sits behind a proxy (`HTTPS_PROXY`
//! in the parent's environment), requests are chained through it with
//! `CONNECT`; that proxy resolves, so the private-address check cannot apply
//! and the report says so.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const HEAD_LIMIT: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// One allowlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Lower-case host, or the suffix after `*.` when `wildcard`.
    host: String,
    wildcard: bool,
    port: Port,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Port {
    /// 80 and 443: what a bare host name means.
    Web,
    Any,
    One(u16),
}

impl Rule {
    /// Parse `host`, `host:port`, `host:*`, `*.suffix` or `*.suffix:port`.
    ///
    /// # Errors
    /// For an empty host, a bad port, or a wildcard anywhere but the front.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (host, port) = split_host_port(text);
        let port = match port {
            None => Port::Web,
            Some("*") => Port::Any,
            Some(p) => Port::One(
                p.parse()
                    .map_err(|_| format!("network.allow `{text}`: `{p}` is not a port"))?,
            ),
        };
        let host = normalise_host(host);
        let (host, wildcard) = match host.strip_prefix("*.") {
            Some(rest) => (rest.to_string(), true),
            None => (host, false),
        };
        if host.is_empty() || host.contains('*') || host.contains('/') {
            return Err(format!(
                "network.allow `{text}`: expected host, host:port, host:* or *.domain"
            ));
        }
        Ok(Self {
            host,
            wildcard,
            port,
        })
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        let host_ok = if self.wildcard {
            host.ends_with(&format!(".{}", self.host))
        } else {
            host == self.host
        };
        let port_ok = match self.port {
            Port::Web => port == 80 || port == 443,
            Port::Any => true,
            Port::One(p) => p == port,
        };
        host_ok && port_ok
    }
}

/// `host:port` split, keeping an IPv6 literal in brackets whole.
fn split_host_port(text: &str) -> (&str, Option<&str>) {
    if let Some(rest) = text.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &text[..end + 2];
            let port = rest[end + 1..].strip_prefix(':');
            return (host, port);
        }
    }
    match text.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        _ => (text, None),
    }
}

fn normalise_host(host: &str) -> String {
    host.trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The proxy's allowlist and settings.
#[derive(Debug, Clone)]
pub struct Allowlist {
    rules: Vec<Rule>,
    allow_private: bool,
    /// This machine's ports a request may reach on a loopback name
    /// (`network.loopback_ports`), connected to on a loopback address only.
    loopback_ports: Vec<u16>,
}

impl Allowlist {
    /// # Errors
    /// When an entry does not parse.
    pub fn new(entries: &[String], allow_private: bool) -> Result<Self, String> {
        let rules = entries
            .iter()
            .map(|e| Rule::parse(e))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            rules,
            allow_private,
            loopback_ports: Vec::new(),
        })
    }

    /// The same list, also letting a request reach `ports` on this machine
    /// (`localhost`, `127.0.0.1`, `::1`). Set from the policy by
    /// `Sandbox::run` (`lib.rs`) only.
    #[must_use]
    pub fn with_loopback_ports(mut self, ports: &[u16]) -> Self {
        self.loopback_ports = ports.to_vec();
        self
    }

    #[must_use]
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = normalise_host(host);
        self.rules.iter().any(|r| r.allows(&host, port)) || self.reaches_loopback(&host, port)
    }

    /// Whether `host:port` is one of this machine's ports the list opens.
    fn reaches_loopback(&self, host: &str, port: u16) -> bool {
        self.loopback_ports.contains(&port) && is_loopback_name(&normalise_host(host))
    }
}

/// `localhost`, or a loopback address written out (`127.0.0.0/8`, `::1`).
/// No other name: one that merely resolves to loopback is not this machine
/// by the client's own words.
fn is_loopback_name(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || is_mapped_loopback(ip))
}

fn is_mapped_loopback(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()))
}

/// Whether an address is one an allowed public name must never lead to.
#[must_use]
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// One decision the proxy made, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub target: String,
    pub allowed: bool,
    pub reason: String,
}

/// Where requests go after the allowlist: straight out, or through the proxy
/// this process itself was given.
#[derive(Debug, Clone)]
pub struct Upstream {
    addr: String,
    authorization: Option<String>,
}

impl Upstream {
    /// The upstream named by `HTTPS_PROXY` / `https_proxy` / `HTTP_PROXY` /
    /// `http_proxy` in `env`, if it is an `http://` proxy.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let raw = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
            .iter()
            .find_map(|k| get(k).filter(|v| !v.is_empty()))?;
        let rest = raw.strip_prefix("http://")?;
        let rest = rest.trim_end_matches('/');
        let (auth, addr) = match rest.rsplit_once('@') {
            Some((auth, addr)) => (Some(auth), addr),
            None => (None, rest),
        };
        Some(Self {
            addr: addr.to_string(),
            authorization: auth.map(|a| format!("Basic {}", base64(a.as_bytes()))),
        })
    }
}

/// A running proxy. Dropping it stops accepting new connections.
pub struct Proxy {
    port: u16,
    decisions: Arc<Mutex<Vec<Decision>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    pub upstream: Option<String>,
}

impl Proxy {
    /// Start listening on 127.0.0.1 on a free port.
    ///
    /// # Errors
    /// When no port can be bound.
    pub fn start(allowlist: Allowlist, upstream: Option<Upstream>) -> io::Result<Self> {
        Self::listen_on(TcpListener::bind(("127.0.0.1", 0))?, allowlist, upstream)
    }

    /// Start listening on 127.0.0.1 on the first free port in `ports`
    /// (inclusive): on Windows under network deny the command may reach only
    /// those ([`crate::windows::offline`]).
    ///
    /// # Errors
    /// When every port in the range is taken (one per concurrent run).
    pub fn start_in(
        allowlist: Allowlist,
        upstream: Option<Upstream>,
        ports: (u16, u16),
    ) -> io::Result<Self> {
        let listener = (ports.0..=ports.1)
            .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "every port in {}-{} is taken (one per proxy run at once; \
                         `sbx setup-windows --proxy-ports` widens the range)",
                        ports.0, ports.1
                    ),
                )
            })?;
        Self::listen_on(listener, allowlist, upstream)
    }

    fn listen_on(
        listener: TcpListener,
        allowlist: Allowlist,
        upstream: Option<Upstream>,
    ) -> io::Result<Self> {
        let port = listener.local_addr()?.port();
        let decisions = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared = Arc::new((allowlist, upstream.clone()));
        {
            let decisions = Arc::clone(&decisions);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let shared = Arc::clone(&shared);
                    let decisions = Arc::clone(&decisions);
                    thread::spawn(move || {
                        let (allowlist, upstream) = &*shared;
                        if let Some(d) = serve(conn, allowlist, upstream.as_ref()) {
                            if let Ok(mut all) = decisions.lock() {
                                all.push(d);
                            }
                        }
                    });
                }
            });
        }
        Ok(Self {
            port,
            decisions,
            stop,
            upstream: upstream.map(|u| u.addr),
        })
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The proxy URL a client is given.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every decision so far.
    #[must_use]
    pub fn decisions(&self) -> Vec<Decision> {
        self.decisions.lock().map(|d| d.clone()).unwrap_or_default()
    }

    /// The variables that point a client at this proxy.
    #[must_use]
    pub fn client_env(&self) -> Vec<(String, String)> {
        let url = self.url();
        let mut out: Vec<(String, String)> = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ]
        .iter()
        .map(|k| ((*k).to_string(), url.clone()))
        .collect();
        // Nothing bypasses the proxy: a bypassed host would be refused by
        // the kernel anyway, with a less helpful error.
        out.push(("NO_PROXY".into(), String::new()));
        out.push(("no_proxy".into(), String::new()));
        out
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// The parsed head of one client request.
struct Head {
    method: String,
    host: String,
    port: u16,
    /// For plain HTTP: the head rewritten for the origin server.
    forward: Option<Vec<u8>>,
}

fn serve(
    client: TcpStream,
    allowlist: &Allowlist,
    upstream: Option<&Upstream>,
) -> Option<Decision> {
    let _ = client.set_read_timeout(Some(IO_TIMEOUT));
    let mut reader = BufReader::new(client.try_clone().ok()?);
    let mut writer = client;
    let head = match read_head(&mut reader) {
        Ok(h) => h,
        Err(e) => {
            let _ = respond(&mut writer, 400, &e);
            return None;
        }
    };
    let target = format!("{}:{}", head.host, head.port);

    if !allowlist.allows(&head.host, head.port) {
        let reason = "not in network.allow".to_string();
        let _ = respond(&mut writer, 403, &format!("sbx: {target} is {reason}"));
        return Some(Decision {
            target,
            allowed: false,
            reason,
        });
    }

    // One of this machine's own ports goes straight to a loopback address:
    // an upstream proxy cannot reach it, and nothing else private opens.
    let to_loopback = allowlist.reaches_loopback(&head.host, head.port);
    let upstream_conn = match upstream {
        _ if to_loopback => connect_loopback(&head.host, head.port),
        Some(up) => connect_via(up, &head.host, head.port),
        None => connect_direct(&head.host, head.port, allowlist.allow_private),
    };
    let mut server = match upstream_conn {
        Ok(s) => s,
        Err(reason) => {
            let _ = respond(&mut writer, 502, &format!("sbx: {target}: {reason}"));
            return Some(Decision {
                target,
                allowed: false,
                reason,
            });
        }
    };

    if head.method == "CONNECT" {
        if writer
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .is_err()
        {
            return None;
        }
    } else if let Some(forward) = &head.forward {
        if server.write_all(forward).is_err() {
            return None;
        }
    }
    // Whatever the client sent after the head is still in the reader.
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() && server.write_all(&buffered).is_err() {
        return None;
    }
    pipe(writer, server);
    Some(Decision {
        target,
        allowed: true,
        reason: if upstream.is_some() {
            "allowed (through the upstream proxy)".into()
        } else {
            "allowed".into()
        },
    })
}

fn read_head(reader: &mut BufReader<TcpStream>) -> Result<Head, String> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("reading the request: {e}"))?;
        total += n;
        if n == 0 || total > HEAD_LIMIT {
            return Err("incomplete or oversized request head".into());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        lines.push(line.trim_end_matches(['\r', '\n']).to_string());
    }
    let first = lines.first().ok_or("empty request")?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or("no method")?.to_string();
    let target = parts.next().ok_or("no target")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&target);
        let port = port
            .and_then(|p| p.parse().ok())
            .ok_or("CONNECT needs host:port")?;
        return Ok(Head {
            method: "CONNECT".into(),
            host: normalise_host(host),
            port,
            forward: None,
        });
    }

    let rest = target
        .strip_prefix("http://")
        .ok_or("only absolute http:// requests and CONNECT are proxied")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority);
    let port = match port {
        Some(p) => p.parse().map_err(|_| "bad port")?,
        None => 80,
    };
    let mut forward = format!("{method} {path} {version}\r\n");
    for line in &lines[1..] {
        let name = line.split(':').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.to_ascii_lowercase().starts_with("proxy-")
        {
            continue;
        }
        forward.push_str(line);
        forward.push_str("\r\n");
    }
    forward.push_str("Connection: close\r\n\r\n");
    Ok(Head {
        method,
        host: normalise_host(host),
        port,
        forward: Some(forward.into_bytes()),
    })
}

/// Resolve once, refuse a private answer unless allowed, and connect to the
/// very address that was checked.
fn connect_direct(host: &str, port: u16, allow_private: bool) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve: {e}"))?
        .collect();
    let usable: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .filter(|a| allow_private || !is_private(a.ip()))
        .collect();
    if usable.is_empty() {
        return Err(if addrs.is_empty() {
            "resolved to no address".into()
        } else {
            "resolves only to private addresses (set network.allow_private to permit)".into()
        });
    }
    let mut last = String::from("no address answered");
    for addr in usable {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(last)
}

/// Connect to `port` on this machine: `host` is a loopback name, and only a
/// loopback answer is used, so a hosts file that points `localhost`
/// elsewhere leads nowhere.
fn connect_loopback(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve: {e}"))?
        .filter(|a| a.ip().is_loopback() || is_mapped_loopback(a.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} resolves to no loopback address"));
    }
    let mut last = String::from("no address answered");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(last)
}

/// A tunnel to `host:port` through the upstream proxy.
fn connect_via(up: &Upstream, host: &str, port: u16) -> Result<TcpStream, String> {
    let addr = up
        .addr
        .to_socket_addrs()
        .map_err(|e| format!("upstream proxy {}: {e}", up.addr))?
        .next()
        .ok_or_else(|| format!("upstream proxy {} resolved to nothing", up.addr))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("upstream proxy {}: {e}", up.addr))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = &up.authorization {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("upstream proxy: {e}"))?;
    // Read the reply head byte by byte so nothing past it is consumed.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > HEAD_LIMIT {
            return Err("upstream proxy reply too large".into());
        }
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return Err("upstream proxy closed the connection".into()),
        }
    }
    let status = String::from_utf8_lossy(&head);
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        let line = status.lines().next().unwrap_or("").to_string();
        return Err(format!("upstream proxy refused: {line}"));
    }
    let _ = stream.set_read_timeout(None);
    Ok(stream)
}

fn respond(writer: &mut TcpStream, code: u16, message: &str) -> io::Result<()> {
    let reason = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Bad Gateway",
    };
    let body = format!("{message}\n");
    write!(
        writer,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Copy both ways until either side closes.
fn pipe(client: TcpStream, server: TcpStream) {
    let _ = client.set_read_timeout(None);
    let _ = server.set_read_timeout(None);
    let (Ok(mut c2), Ok(mut s2)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let up = thread::spawn(move || {
        let mut client = client;
        let mut server = server;
        let _ = io::copy(&mut client, &mut server);
        let _ = server.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut s2, &mut c2);
    let _ = c2.shutdown(Shutdown::Write);
    let _ = up.join();
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[&str]) -> Allowlist {
        let entries: Vec<String> = entries.iter().map(|s| (*s).to_string()).collect();
        Allowlist::new(&entries, false).expect("parses")
    }

    #[test]
    fn rules_match_hosts_ports_and_wildcards() {
        let l = list(&["crates.io", "*.npmjs.org", "example.com:8443", "any.dev:*"]);
        assert!(l.allows("crates.io", 443));
        assert!(l.allows("CRATES.IO.", 80));
        assert!(!l.allows("crates.io", 22));
        assert!(!l.allows("static.crates.io", 443), "no implicit subdomains");
        assert!(l.allows("registry.npmjs.org", 443));
        assert!(!l.allows("npmjs.org", 443), "a wildcard is not the apex");
        assert!(!l.allows("evilnpmjs.org", 443));
        assert!(l.allows("example.com", 8443));
        assert!(!l.allows("example.com", 443));
        assert!(l.allows("any.dev", 5432));
    }

    #[test]
    fn a_proxy_listens_within_its_port_range_or_says_it_is_full() {
        // A port that was free a moment ago, and one held now.
        let free = TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("binds")
            .port();
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        let held_port = held.local_addr().expect("addr").port();
        let full = Proxy::start_in(list(&["x.dev"]), None, (held_port, held_port));
        assert!(full.is_err_and(
            |e| e.kind() == io::ErrorKind::AddrInUse && e.to_string().contains("--proxy-ports")
        ));
        if let Ok(p) = Proxy::start_in(list(&["x.dev"]), None, (free, free)) {
            assert_eq!(p.port(), free);
            assert_eq!(p.url(), format!("http://127.0.0.1:{free}"));
        }
    }

    #[test]
    fn bad_rules_are_errors() {
        for bad in ["", "a.*.com", "host:port", "http://x.com"] {
            assert!(Rule::parse(bad).is_err(), "{bad} should not parse");
        }
        assert!(Rule::parse("[::1]:443").is_ok());
    }

    #[test]
    fn private_addresses_are_recognised() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_private(ip.parse().expect("ip")), "{ip}");
        }
        for ip in ["1.1.1.1", "140.82.112.3", "2606:4700::1111"] {
            assert!(!is_private(ip.parse().expect("ip")), "{ip}");
        }
    }

    #[test]
    fn upstream_is_read_with_credentials() {
        let up = Upstream::from_env(|k| {
            (k == "https_proxy").then(|| "http://user:pa%20ss@proxy.local:3128/".to_string())
        })
        .expect("an http proxy");
        assert_eq!(up.addr, "proxy.local:3128");
        assert_eq!(up.authorization.as_deref(), Some("Basic dXNlcjpwYSUyMHNz"));
        assert!(Upstream::from_env(|_| Some("socks5://x:1".into())).is_none());
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want);
        }
    }

    /// A refused host gets a 403 that names it, and the decision is kept.
    #[test]
    fn a_host_outside_the_list_is_refused_with_a_reason() {
        let proxy = Proxy::start(list(&["allowed.example"]), None).expect("starts");
        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).expect("connects");
        s.write_all(b"CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\n\r\n")
            .expect("writes");
        let mut reply = String::new();
        s.read_to_string(&mut reply).expect("reads");
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains("denied.example:443"), "{reply}");
        thread::sleep(Duration::from_millis(50));
        let d = proxy.decisions();
        assert_eq!(d.len(), 1);
        assert!(!d[0].allowed);
    }

    /// Plain HTTP to an allowed host is forwarded in origin form with
    /// `Connection: close`, and a private answer is refused unless allowed.
    #[test]
    fn plain_http_is_forwarded_and_private_answers_need_permission() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        let port = origin.local_addr().expect("addr").port();
        let server = thread::spawn(move || {
            let (mut conn, _) = origin.accept().expect("accepts");
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).expect("reads");
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("writes");
            req
        });

        let rule = format!("localhost:{port}");
        let strict = Proxy::start(list(&[&rule]), None).expect("starts");
        let mut s = TcpStream::connect(("127.0.0.1", strict.port())).expect("connects");
        write!(
            s,
            "GET http://localhost:{port}/x HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .expect("writes");
        let mut reply = String::new();
        s.read_to_string(&mut reply).expect("reads");
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
        assert!(reply.contains("private"), "{reply}");

        let open =
            Proxy::start(Allowlist::new(&[rule], true).expect("parses"), None).expect("starts");
        let mut s = TcpStream::connect(("127.0.0.1", open.port())).expect("connects");
        write!(
            s,
            "GET http://localhost:{port}/x HTTP/1.1\r\nHost: localhost\r\nProxy-Connection: keep-alive\r\n\r\n"
        )
        .expect("writes");
        let mut reply = String::new();
        s.read_to_string(&mut reply).expect("reads");
        assert!(reply.ends_with("ok"), "{reply}");
        let seen = server.join().expect("server");
        assert!(seen.starts_with("GET /x HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("Connection: close"), "{seen}");
        assert!(
            !seen.to_ascii_lowercase().contains("proxy-connection"),
            "{seen}"
        );
    }

    #[test]
    fn only_a_loopback_name_is_this_machine() {
        for host in [
            "localhost",
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_name(host), "{host}");
        }
        for host in [
            "localhost.example.com",
            "0.0.0.0",
            "10.0.0.1",
            "169.254.169.254",
            "example.com",
            "::",
        ] {
            assert!(!is_loopback_name(host), "{host}");
        }
    }

    /// A port this machine serves (`network.loopback_ports`) is reached on a
    /// loopback name, plain HTTP or CONNECT, without `allow_private`, and
    /// straight to loopback even with an upstream proxy configured; another
    /// port stays refused.
    #[test]
    fn a_loopback_port_opens_that_port_and_nothing_else() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        let port = origin.local_addr().expect("addr").port();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut conn, _) = origin.accept().expect("accepts");
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).expect("reads");
                conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .expect("writes");
            }
        });
        // An upstream that answers nothing: a loopback request must not
        // depend on it.
        let dead = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        let dead_port = dead.local_addr().expect("addr").port();
        drop(dead);
        let upstream = Upstream::from_env(|k| {
            (k == "https_proxy").then(|| format!("http://127.0.0.1:{dead_port}"))
        });
        assert!(upstream.is_some());
        let proxy = Proxy::start(list(&[]).with_loopback_ports(&[port]), upstream).expect("starts");

        let ask = |request: String| {
            let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).expect("connects");
            s.write_all(request.as_bytes()).expect("writes");
            let mut reply = String::new();
            s.read_to_string(&mut reply).expect("reads");
            reply
        };
        let plain = ask(format!(
            "GET http://127.0.0.1:{port}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        assert!(plain.ends_with("ok"), "{plain}");
        let tunnelled = ask(format!(
            "CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost\r\n\r\nGET / HTTP/1.1\r\n\r\n"
        ));
        assert!(
            tunnelled.starts_with("HTTP/1.1 200 Connection established"),
            "{tunnelled}"
        );
        assert!(tunnelled.ends_with("ok"), "{tunnelled}");
        server.join().expect("server");

        let other = if port == u16::MAX { port - 1 } else { port + 1 };
        let refused = ask(format!(
            "GET http://127.0.0.1:{other}/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ));
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        let named_elsewhere = ask(format!(
            "GET http://10.0.0.1:{port}/ HTTP/1.1\r\nHost: 10.0.0.1\r\n\r\n"
        ));
        assert!(
            named_elsewhere.starts_with("HTTP/1.1 403"),
            "{named_elsewhere}"
        );
    }
}
