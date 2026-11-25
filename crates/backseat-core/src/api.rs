//! The local control API (API.md): a small HTTP/1.1 server with JSON bodies, for phone
//! remotes, stream decks and scripts on this computer or the local network. Off unless the
//! user turns it on in the settings. Every call but `GET /api/v1/info` needs the token
//! (`Authorization: Bearer <token>` or `?token=`). std only: one thread accepts, one short
//! thread per connection (at most `MAX_CONNECTIONS`), `Connection: close`.
//!
//! What the calls do is the UI's business (`Handler`); this module only speaks HTTP.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The API's version in the path (`/api/v1/...`).
pub const VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47820;
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub port: u16,
    /// Listen on every network of this computer (other devices of the local network reach
    /// it); false: this computer only (127.0.0.1).
    pub lan: bool,
    pub token: String,
}

/// A call, as the handler sees it.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The path after `/api/v1/`, split at '/', percent-decoded: `["rooms", "ABC", "open"]`.
    pub path: Vec<String>,
    pub query: HashMap<String, String>,
    /// The JSON body; Null when there is none.
    pub body: Value,
}

impl Request {
    pub fn path_is(&self, p: &[&str]) -> bool {
        self.path.len() == p.len() && self.path.iter().zip(p).all(|(a, b)| b.is_empty() || a == b)
    }
    /// A bool from the body (`{"muted": true}`), None when missing.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.body.get(key).and_then(|v| v.as_bool())
    }
    pub fn str(&self, key: &str) -> Option<String> {
        self.body.get(key).and_then(|v| v.as_str()).map(String::from)
    }
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    /// 200 with `"ok": true` added to an object (anything else goes under "data").
    pub fn ok(v: Value) -> Reply {
        let body = match v {
            Value::Object(mut m) => {
                m.insert("ok".into(), Value::Bool(true));
                Value::Object(m)
            }
            Value::Null => json!({ "ok": true }),
            other => json!({ "ok": true, "data": other }),
        };
        Reply { status: 200, body }
    }
    pub fn err(status: u16, msg: impl Into<String>) -> Reply {
        Reply { status, body: json!({ "ok": false, "error": msg.into() }) }
    }
}

pub type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

struct Running {
    cfg: Config,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Starts, restarts (a changed config) or stops (None) the server. The address it listens on.
pub fn run(cfg: Option<Config>, handler: Arc<Handler>) -> Result<Option<SocketAddr>, String> {
    let mut running = RUNNING.lock().unwrap();
    if let (Some(r), Some(c)) = (running.as_ref(), cfg.as_ref()) {
        if r.cfg == *c {
            return Ok(Some(r.addr));
        }
    }
    if let Some(r) = running.take() {
        r.stop.store(true, Ordering::SeqCst);
        let _ = r.thread.join();
        eprintln!("api: stopped");
    }
    let Some(cfg) = cfg else { return Ok(None) };
    if cfg.token.len() < 16 {
        return Err("the token is too short".into());
    }
    let ip = if cfg.lan { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
    let listener = TcpListener::bind((ip, cfg.port)).map_err(|e| format!("port {}: {e}", cfg.port))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let (st, token) = (stop.clone(), cfg.token.clone());
    let thread = std::thread::Builder::new().name("api".into()).spawn(move || accept_loop(listener, st, token, handler)).map_err(|e| e.to_string())?;
    eprintln!("api: listening on {addr} ({})", if cfg.lan { "local network" } else { "this computer only" });
    *running = Some(Running { cfg, addr, stop, thread });
    Ok(Some(addr))
}

/// Where the server listens now, if it runs.
pub fn address() -> Option<SocketAddr> {
    RUNNING.lock().unwrap().as_ref().map(|r| r.addr)
}

/// This computer's address in the local network: the one its default route goes out of.
/// A UDP socket "connected" to an outside address sends nothing; it only picks the route.
pub fn lan_address() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>, token: String, handler: Arc<Handler>) {
    let active = Arc::new(AtomicUsize::new(0));
    let token = Arc::new(token);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = write_reply(&stream, &Reply::err(503, "too many connections"));
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let (active, token, handler) = (active.clone(), token.clone(), handler.clone());
                let spawned = std::thread::Builder::new().name("api-conn".into()).spawn(move || {
                    serve(stream, peer, &token, &*handler);
                    active.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    eprintln!("api: cannot start a connection thread");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                eprintln!("api: accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// A parsed request line and headers, plus the body.
struct Raw {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Raw, Reply> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(Reply::err(431, "headers too large"));
        }
        let n = stream.read(&mut chunk).map_err(|_| Reply::err(408, "timeout"))?;
        if n == 0 {
            return Err(Reply::err(400, "incomplete request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| Reply::err(400, "bad request"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (first.next(), first.next()) else { return Err(Reply::err(400, "bad request line")) };
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = match headers.get("content-length") {
        Some(v) => v.parse().map_err(|_| Reply::err(400, "bad content-length"))?,
        None => 0,
    };
    if len > MAX_BODY {
        return Err(Reply::err(413, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).map_err(|_| Reply::err(408, "timeout"))?;
        if n == 0 {
            return Err(Reply::err(400, "incomplete body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Raw { method: method.to_ascii_uppercase(), target: target.to_string(), headers, body })
}

/// `plus`: '+' is a space (query strings), not in paths.
fn percent_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if plus => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compares without stopping at the first difference.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn serve(mut stream: TcpStream, peer: SocketAddr, token: &str, handler: &Handler) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let reply = match read_request(&mut stream) {
        Ok(raw) => route(raw, peer, token, handler),
        Err(r) => r,
    };
    let _ = write_reply(&stream, &reply);
}

fn route(raw: Raw, peer: SocketAddr, token: &str, handler: &Handler) -> Reply {
    if raw.method == "OPTIONS" {
        return Reply { status: 204, body: Value::Null };
    }
    let (path, query) = raw.target.split_once('?').unwrap_or((raw.target.as_str(), ""));
    let query: HashMap<String, String> = query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (percent_decode(k, true), percent_decode(v, true))).collect();
    let prefix = format!("/api/v{VERSION}");
    let Some(rest) = path.strip_prefix(&prefix).filter(|r| r.is_empty() || r.starts_with('/')) else {
        return Reply::err(404, format!("not found; the API is under {prefix}/"));
    };
    let parts: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(|s| percent_decode(s, false)).collect();
    // The one call without the token: what this is and which API it speaks.
    let public = raw.method == "GET" && (parts.is_empty() || parts == ["info"]);
    if !public {
        let given = raw.headers.get("authorization").and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))).map(str::trim).map(String::from).or_else(|| query.get("token").cloned());
        if !given.is_some_and(|g| same_secret(&g, token)) {
            eprintln!("api: {} {} from {}: wrong or missing token", raw.method, path, peer.ip());
            // Slows down guessing (the token is 128 bits anyway).
            std::thread::sleep(Duration::from_millis(300));
            return Reply::err(401, "wrong or missing token");
        }
    }
    let body = if raw.body.iter().all(|b| b.is_ascii_whitespace()) {
        Value::Null
    } else {
        match serde_json::from_slice(&raw.body) {
            Ok(v) => v,
            Err(e) => return Reply::err(400, format!("the body is not JSON: {e}")),
        }
    };
    let req = Request { method: raw.method, path: parts, query, body };
    let r = handler(&req);
    eprintln!("api: {} /{} -> {}", req.method, req.path.join("/"), r.status);
    r
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

fn write_reply(mut stream: &TcpStream, r: &Reply) -> std::io::Result<()> {
    let body = if r.status == 204 { String::new() } else { r.body.to_string() };
    // Any web page may call it (a phone's browser page), with the token; preflights of pages
    // on the public internet asking for a private address get their yes too.
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Private-Network: true\r\n\r\n",
        r.status,
        reason(r.status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(addr: SocketAddr, raw: &str) -> (u16, Value) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status: u16 = out.split(' ').nth(1).unwrap().parse().unwrap();
        let body = out.split_once("\r\n\r\n").map(|x| x.1).unwrap_or_default();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// Port 0 picks a free one; the token is checked, the path and body reach the handler.
    #[test]
    fn serves_with_the_token_only() {
        let token = "0123456789abcdef0123456789abcdef";
        let h: Arc<Handler> = Arc::new(|r: &Request| {
            if r.path_is(&["info"]) {
                return Reply::ok(json!({ "app": "test" }));
            }
            if r.method == "POST" && r.path_is(&["rooms", "", "open"]) {
                return Reply::ok(json!({ "room": r.path[1], "muted": r.bool("muted") }));
            }
            Reply::err(404, "unknown")
        });
        let addr = run(Some(Config { port: 0, lan: false, token: token.into() }), h.clone()).unwrap().unwrap();
        let (st, b) = call(addr, "GET /api/v1/info HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!((st, b["app"].as_str()), (200, Some("test")));
        let (st, _) = call(addr, "POST /api/v1/rooms/A%20B/open HTTP/1.1\r\n\r\n");
        assert_eq!(st, 401, "no token let through");
        let (st, _) = call(addr, "POST /api/v1/rooms/A%20B/open HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdeX\r\n\r\n");
        assert_eq!(st, 401, "a wrong token let through");
        let body = r#"{"muted":true}"#;
        let (st, b) = call(addr, &format!("POST /api/v1/rooms/A%20B/open HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
        assert_eq!(st, 200);
        assert_eq!(b["room"], "A B");
        assert_eq!(b["muted"], true);
        assert_eq!(b["ok"], true);
        let (st, _) = call(addr, &format!("GET /api/v1/nothing?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(st, 404);
        let (st, _) = call(addr, &format!("POST /api/v1/rooms/x/open?token={token} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{x"));
        assert_eq!(st, 400, "bad JSON accepted");
        let (st, _) = call(addr, "GET /other HTTP/1.1\r\n\r\n");
        assert_eq!(st, 404);
        run(None, h).unwrap();
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err(), "still listening after stop");
    }

    #[test]
    fn decodes_percent() {
        assert_eq!(percent_decode("a%20b%2Fc", false), "a b/c");
        assert_eq!(percent_decode("%D0%AF", false), "Я");
        assert_eq!(percent_decode("bad%2", false), "bad%2");
        assert_eq!(percent_decode("100%", false), "100%");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("a+b", true), "a b");
    }
}
