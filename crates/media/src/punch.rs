//! Hole punching for peers whose direct connection failed (ICE failed): one side's NAT keeps
//! the same public port for every destination ("easy"), the other's gives a new random port
//! to each destination ("hard": a provider's CGNAT; 2026-11-28 a friend's provider moved them
//! behind one and every connection failed). The "birthday" trick (as Tailscale does): the
//! hard side opens many sockets and sends from each to the easy side's address (many random
//! mappings, all waiting for it); the easy side sends to random ports of the hard side's
//! address until one lands in such a mapping (measured: 267 probes, 0.5 s). Two easy sides
//! just send to each other. Two hard sides cannot be helped.
//!
//! The winning socket is closed and its local port handed to the connection's ICE agent
//! (`IceConfig::pinned`): the NAT keeps the hole (it never learns the socket closed), and the
//! agent is told the other side's punched address as one more candidate.
//!
//! The two sides talk through the server like for everything else ("p/" signals): each sends
//! "info <kind> <address>" for a slot (e.g. "voice"), then both punch.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type PeerId = u64;

/// Where a punched connection's ICE agent must sit, and whom it reaches there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pinned {
    /// Our local UDP port (the NAT's hole is on it).
    pub port: u16,
    /// The other side's public address through the hole.
    pub peer: SocketAddr,
}

/// What a NAT does with a socket's outgoing packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The same public address for every destination: `addr` is this socket's.
    Easy(SocketAddr),
    /// A new public port for each destination: only the IP is known.
    Hard(Ipv4Addr),
}

const STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// The hard side's sockets.
const SOCKETS: usize = 256;
/// How long a punch may take before it is given up.
const PUNCH_FOR: Duration = Duration::from_secs(15);
/// How long to wait for the other side's "info".
const INFO_WAIT: Duration = Duration::from_secs(10);

struct Rng(u64);
impl Rng {
    fn new() -> Self {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Self(t ^ 0x9e37_79b9_7f4a_7c15 | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Tests only: STREAMER_DEBUG_PUNCH_LOCAL: no STUN; the "public" address is this machine's
/// own LAN address and the local port (the ICE agent leaves loopback out), and the kind is
/// STREAMER_DEBUG_PUNCH_KIND ("easy" or "hard", default easy).
fn local_test() -> Option<&'static str> {
    std::env::var_os("STREAMER_DEBUG_PUNCH_LOCAL")?;
    Some(if std::env::var("STREAMER_DEBUG_PUNCH_KIND").ok().as_deref() == Some("hard") { "hard" } else { "easy" })
}

fn bind() -> std::io::Result<UdpSocket> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.set_nonblocking(true)?;
    Ok(s)
}

/// `sock`'s public address as `server` sees it (STUN binding, XOR-MAPPED-ADDRESS).
fn stun(sock: &UdpSocket, server: &str, rng: &mut Rng) -> Option<SocketAddr> {
    let server = server.to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;
    let mut req = vec![0u8, 1, 0, 0, 0x21, 0x12, 0xA4, 0x42];
    let id: Vec<u8> = (0..12).map(|_| rng.next() as u8).collect();
    req.extend(&id);
    let mut buf = [0u8; 512];
    for _ in 0..4 {
        let _ = sock.send_to(&req, server);
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == server && n >= 20 && buf[8..20] == id[..] => return mapped(&buf[..n]),
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    None
}

fn mapped(m: &[u8]) -> Option<SocketAddr> {
    let mut i = 20;
    while i + 4 <= m.len() {
        let (t, l) = (u16::from_be_bytes([m[i], m[i + 1]]), u16::from_be_bytes([m[i + 2], m[i + 3]]) as usize);
        if t == 0x0020 && l >= 8 && i + 12 <= m.len() && m[i + 5] == 1 {
            let port = u16::from_be_bytes([m[i + 6], m[i + 7]]) ^ 0x2112;
            let ip = Ipv4Addr::new(m[i + 8] ^ 0x21, m[i + 9] ^ 0x12, m[i + 10] ^ 0xA4, m[i + 11] ^ 0x42);
            return Some(SocketAddr::new(ip.into(), port));
        }
        i += 4 + l.div_ceil(4) * 4;
    }
    None
}

/// The NAT's kind for `sock` (two STUN servers: the same port for both is easy).
fn kind_of(sock: &UdpSocket, rng: &mut Rng) -> Option<Kind> {
    if let Some(k) = local_test() {
        let port = sock.local_addr().ok()?.port();
        // The address a packet to the internet would leave from (nothing is sent).
        let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
        probe.connect("192.0.2.1:9").ok()?;
        let std::net::IpAddr::V4(ip) = probe.local_addr().ok()?.ip() else { return None };
        return Some(if k == "hard" { Kind::Hard(ip) } else { Kind::Easy(SocketAddr::new(ip.into(), port)) });
    }
    let a = stun(sock, STUN_SERVERS[0], rng)?;
    let b = stun(sock, STUN_SERVERS[1], rng);
    let std::net::IpAddr::V4(ip) = a.ip() else { return None };
    Some(match b {
        Some(b) if b != a => Kind::Hard(ip),
        _ => Kind::Easy(a),
    })
}

fn info_text(k: Kind) -> String {
    match k {
        Kind::Easy(a) => format!("easy {a}"),
        Kind::Hard(ip) => format!("hard {ip}"),
    }
}

fn parse_info(s: &str) -> Option<Kind> {
    let (k, a) = s.split_once(' ')?;
    match k {
        "easy" => a.parse().ok().map(Kind::Easy),
        "hard" => a.parse().ok().map(Kind::Hard),
        _ => None,
    }
}

const PROBE: &[u8] = b"BSP1-probe";
const OK: &[u8] = b"BSP1-ok";

/// Easy side, the other easy: both send to each other's address until one gets through.
fn punch_easy_easy(sock: &UdpSocket, peer: SocketAddr, until: Instant) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut next = Instant::now();
    while Instant::now() < until {
        if Instant::now() >= next {
            let _ = sock.send_to(PROBE, peer);
            next += Duration::from_millis(50);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer && (&buf[..n] == PROBE || &buf[..n] == OK) => {
                for _ in 0..5 {
                    let _ = sock.send_to(OK, peer);
                }
                return Some(Pinned { port: sock.local_addr().ok()?.port(), peer });
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    None
}

/// Easy side, the other hard: probes random ports of `ip` (~500 a second) until a packet
/// of the hard side's comes in through one.
fn punch_easy_hard(sock: &UdpSocket, ip: Ipv4Addr, until: Instant, rng: &mut Rng) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut next = Instant::now();
    while Instant::now() < until {
        if Instant::now() >= next {
            let port = 1024 + (rng.next() % (65536 - 1024)) as u16;
            let _ = sock.send_to(PROBE, SocketAddr::new(ip.into(), port));
            next += Duration::from_millis(2);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from.ip() == std::net::IpAddr::V4(ip) && &buf[..n] == PROBE => {
                for _ in 0..5 {
                    let _ = sock.send_to(OK, from);
                }
                return Some(Pinned { port: sock.local_addr().ok()?.port(), peer: from });
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_micros(300)),
        }
    }
    None
}

/// Hard side: sends from each of its sockets to the easy side every 200 ms until one gets a
/// probe or an answer back; that socket's port is the hole.
fn punch_hard(socks: &[UdpSocket], peer: SocketAddr, until: Instant) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut round = Instant::now() - Duration::from_secs(1);
    while Instant::now() < until {
        if round.elapsed() >= Duration::from_millis(200) {
            round = Instant::now();
            for s in socks {
                let _ = s.send_to(PROBE, peer);
            }
        }
        let mut any = false;
        for s in socks {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                any = true;
                if from == peer && (&buf[..n] == PROBE || &buf[..n] == OK) {
                    for _ in 0..5 {
                        let _ = s.send_to(OK, peer);
                    }
                    return Some(Pinned { port: s.local_addr().ok()?.port(), peer });
                }
            }
        }
        if !any {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    None
}

/// Sends "p/<kind>" messages to a peer: `(to, kind, payload)`.
pub type Send = Arc<dyn Fn(PeerId, &str, &str) + std::marker::Send + Sync>;

/// One session's punching: talks to the others through `send`, hears them through
/// `on_message`.
pub struct Puncher {
    send: Send,
    /// Waiting for the other side's "info" for (peer, slot).
    waiting: Mutex<HashMap<(PeerId, String), mpsc::Sender<Kind>>>,
    /// "info" that came before we started on that slot.
    early: Mutex<HashMap<(PeerId, String), Kind>>,
}

impl Puncher {
    pub fn new(send: Send) -> Arc<Self> {
        Arc::new(Self { send, waiting: Mutex::default(), early: Mutex::default() })
    }

    /// A "p/" message from `from` (the "p/" taken off `kind`).
    pub fn on_message(&self, from: PeerId, kind: &str, payload: &str) {
        let Some(slot) = kind.strip_prefix("info:") else { return };
        let Some(k) = parse_info(payload) else { return };
        let key = (from, slot.to_string());
        let tx = self.waiting.lock().unwrap().remove(&key);
        match tx {
            Some(tx) => {
                let _ = tx.send(k);
            }
            None => {
                self.early.lock().unwrap().insert(key, k);
            }
        }
    }

    /// Punches a hole to `peer` for `slot` (both sides call this for the same slot after
    /// their connection failed). Blocks up to ~25 s; None when it did not work. The sockets
    /// are closed on return, the port is free for the ICE agent.
    pub fn punch(&self, peer: PeerId, slot: &str) -> Option<Pinned> {
        let mut rng = Rng::new();
        let first = bind().ok()?;
        let Some(mine) = kind_of(&first, &mut rng) else {
            eprintln!("punch: {slot} with {peer}: no answer from the STUN servers");
            return None;
        };
        // Ready for theirs before telling ours.
        let key = (peer, slot.to_string());
        let (tx, rx) = mpsc::channel();
        let early = self.early.lock().unwrap().remove(&key);
        if early.is_none() {
            self.waiting.lock().unwrap().insert(key.clone(), tx);
        }
        (self.send)(peer, &format!("info:{slot}"), &info_text(mine));
        let theirs = match early {
            Some(k) => Some(k),
            None => rx.recv_timeout(INFO_WAIT).ok(),
        };
        self.waiting.lock().unwrap().remove(&key);
        let Some(theirs) = theirs else {
            eprintln!("punch: {slot} with {peer}: the other side did not answer");
            return None;
        };
        let until = Instant::now() + PUNCH_FOR;
        let t0 = Instant::now();
        let got = match (mine, theirs) {
            (Kind::Easy(_), Kind::Easy(a)) => punch_easy_easy(&first, a, until),
            (Kind::Easy(_), Kind::Hard(ip)) => punch_easy_hard(&first, ip, until, &mut rng),
            (Kind::Hard(_), Kind::Easy(a)) => {
                let mut socks = vec![first];
                socks.extend((1..SOCKETS).filter_map(|_| bind().ok()));
                punch_hard(&socks, a, until)
            }
            (Kind::Hard(_), Kind::Hard(_)) => {
                eprintln!("punch: {slot} with {peer}: both sides behind a NAT that changes ports; cannot punch");
                return None;
            }
        };
        match got {
            Some(p) => eprintln!("punch: {slot} with {peer}: through in {} ms ({:?} to {:?}), local port {}", t0.elapsed().as_millis(), kind_name(mine), kind_name(theirs), p.port),
            None => eprintln!("punch: {slot} with {peer}: nothing came through in {} s ({:?} to {:?})", PUNCH_FOR.as_secs(), kind_name(mine), kind_name(theirs)),
        }
        got
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Easy(_) => "easy",
        Kind::Hard(_) => "hard",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_round_trips() {
        let e = Kind::Easy("1.2.3.4:5678".parse().unwrap());
        let h = Kind::Hard(Ipv4Addr::new(5, 6, 7, 8));
        assert_eq!(parse_info(&info_text(e)), Some(e));
        assert_eq!(parse_info(&info_text(h)), Some(h));
        assert_eq!(parse_info("bogus 1"), None);
    }

    #[test]
    fn mapped_address_is_read() {
        // A STUN success response with XOR-MAPPED-ADDRESS 1.2.3.4:5678.
        let mut m = vec![1u8, 1, 0, 12, 0x21, 0x12, 0xA4, 0x42];
        m.extend([0u8; 12]);
        m.extend([0, 0x20, 0, 8, 0, 1]);
        m.extend((5678u16 ^ 0x2112).to_be_bytes());
        m.extend([1 ^ 0x21, 2 ^ 0x12, 3 ^ 0xA4, 4 ^ 0x42]);
        assert_eq!(mapped(&m), Some("1.2.3.4:5678".parse().unwrap()));
    }
}
