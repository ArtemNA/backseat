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
/// The hard side's sockets. Fewer outside Windows: a macOS app may have only 256 file
/// descriptors in all.
const SOCKETS: usize = if cfg!(windows) { 256 } else { 128 };
/// The slots punching is for (messages for others are ignored).
const SLOTS: [&str; 2] = ["voice", "watch"];
/// How long a punch may take before it is given up.
const PUNCH_FOR: Duration = Duration::from_secs(15);
/// How long to wait for the other side's "info".
const INFO_WAIT: Duration = Duration::from_secs(6);

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
    // Without the second answer the kind cannot be told: a provider's NAT taken for an easy
    // one costs a round of probing for nothing.
    let b = b?;
    Some(if b != a { Kind::Hard(ip) } else { Kind::Easy(a) })
}

fn info_text(k: Kind) -> String {
    match k {
        Kind::Easy(a) => format!("easy {a}"),
        Kind::Hard(ip) => format!("hard {ip}"),
    }
}

fn parse_info(s: &str) -> Option<Kind> {
    let (k, a) = s.split_once(' ')?;
    // An address a member gives us is one we will send probes to: not ours, not everyone's,
    // not a low port.
    // A public address only (one on our own network needs no punching; a test plays NATs
    // with this machine's LAN address).
    let usable = |ip: Ipv4Addr| {
        let o = ip.octets();
        let local = ip.is_private() || ip.is_link_local() || ip.is_loopback() || (o[0] == 100 && (64..128).contains(&o[1]));
        !(ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() || (local && local_test().is_none()))
    };
    match k {
        "easy" => a.parse::<SocketAddr>().ok().filter(|a| a.port() >= 1024 && matches!(a.ip(), std::net::IpAddr::V4(ip) if usable(ip))).map(Kind::Easy),
        "hard" => a.parse::<Ipv4Addr>().ok().filter(|ip| usable(*ip)).map(Kind::Hard),
        _ => None,
    }
}

const PROBE: &[u8] = b"BSP1-probe";
const OK: &[u8] = b"BSP1-ok";

/// What a socket lets in. Behind a real NAT that filters by port (most home routers), only
/// packets from addresses it sent to; tests (STREAMER_DEBUG_PUNCH_LOCAL) play that filter
/// here, or the hard side's probes would come straight in and hide how it really goes: the
/// easy side hears the hard side's answer, not its probe (round 1 of review: the easy side
/// took only probes and missed every answer).
struct Filter {
    strict: bool,
    sent: std::collections::HashSet<SocketAddr>,
}

impl Filter {
    fn new() -> Self {
        Self { strict: local_test().is_some() && std::env::var_os("STREAMER_DEBUG_PUNCH_NO_FILTER").is_none(), sent: Default::default() }
    }
    fn sent(&mut self, to: SocketAddr) {
        if self.strict {
            self.sent.insert(to);
        }
    }
    fn lets_in(&self, from: SocketAddr) -> bool {
        !self.strict || self.sent.contains(&from)
    }
}

/// After a hole: tells the other side for a moment, so one lost packet does not leave it
/// without (the other side stops probing once it has its hole).
fn confirm(sock: &UdpSocket, peer: SocketAddr) {
    for _ in 0..10 {
        let _ = sock.send_to(OK, peer);
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Easy side, the other easy: both send to each other's address until one gets through.
fn punch_easy_easy(sock: &UdpSocket, peer: SocketAddr, until: Instant) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut next = Instant::now();
    let mut filter = Filter::new();
    while Instant::now() < until {
        if Instant::now() >= next {
            let _ = sock.send_to(PROBE, peer);
            filter.sent(peer);
            next += Duration::from_millis(50);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer && filter.lets_in(from) && (&buf[..n] == PROBE || &buf[..n] == OK) => {
                confirm(sock, peer);
                return Some(Pinned { port: sock.local_addr().ok()?.port(), peer });
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    None
}

/// Easy side, the other hard: probes random ports of `ip` (~500 a second; a late wake sends
/// what it missed, up to 10 at once) until the hard side's probe or answer comes in through
/// one of them.
fn punch_easy_hard(sock: &UdpSocket, ip: Ipv4Addr, until: Instant, rng: &mut Rng) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut next = Instant::now();
    let mut filter = Filter::new();
    let mut sent = 0u64;
    while Instant::now() < until {
        let mut burst = 0;
        while Instant::now() >= next && burst < 10 {
            let to = SocketAddr::new(ip.into(), 1024 + (rng.next() % (65536 - 1024)) as u16);
            let _ = sock.send_to(PROBE, to);
            filter.sent(to);
            next += Duration::from_millis(2);
            burst += 1;
            sent += 1;
        }
        if burst == 10 {
            next = Instant::now();
        }
        match sock.recv_from(&mut buf) {
            // STREAMER_DEBUG_PUNCH_EASY_NO_OK: the hard side's answer is not taken (the spoiled
            // variant: as in the first beta).
            Ok((n, from)) if from.ip() == std::net::IpAddr::V4(ip) && filter.lets_in(from) && (&buf[..n] == PROBE || (&buf[..n] == OK && std::env::var_os("STREAMER_DEBUG_PUNCH_EASY_NO_OK").is_none())) => {
                eprintln!("punch: in after {sent} probes");
                confirm(sock, from);
                return Some(Pinned { port: sock.local_addr().ok()?.port(), peer: from });
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_micros(300)),
        }
    }
    eprintln!("punch: {sent} probes, none answered");
    None
}

/// Hard side: sends from each of its sockets to the easy side every 200 ms until one gets a
/// probe or an answer back; that socket's port is the hole.
fn punch_hard(socks: &[UdpSocket], peer: SocketAddr, until: Instant) -> Option<Pinned> {
    let mut buf = [0u8; 64];
    let mut round = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
    while Instant::now() < until {
        if round.elapsed() >= Duration::from_millis(200) {
            round = Instant::now();
            for s in socks {
                let _ = s.send_to(PROBE, peer);
            }
        }
        let mut any = false;
        for (i, s) in socks.iter().enumerate() {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                any = true;
                if from == peer && (&buf[..n] == PROBE || &buf[..n] == OK) {
                    eprintln!("punch: in on socket {i} of {}", socks.len());
                    confirm(s, peer);
                    return Some(Pinned { port: s.local_addr().ok()?.port(), peer });
                }
            }
        }
        if !any {
            // 256 sockets a round: not more often than this (CPU).
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    None
}

/// Sends "p/<kind>" messages to a peer: `(to, kind, payload)`.
pub type Send = Arc<dyn Fn(PeerId, &str, &str) + std::marker::Send + Sync>;

/// A punch with this peer for this slot is running already (the caller leaves it to that one).
#[derive(Debug, PartialEq)]
pub struct Busy;

/// After this many punches in a row with a peer that did not come through, no more for a
/// while (a peer that cannot be punched cost 15 s of probing every round).
const GIVE_UP_AFTER: u32 = 2;
const GIVE_UP_FOR: Duration = Duration::from_secs(180);

/// One session's punching: talks to the others through `send`, hears them through
/// `on_message`.
pub struct Puncher {
    send: Send,
    /// What came from each peer: ("info:<slot>" or "done:<slot>") -> (payload, when).
    heard: Mutex<HashMap<(PeerId, String), (String, Instant)>>,
    heard_cv: std::sync::Condvar,
    /// Punches that did not come through, in a row, per peer.
    failed: Mutex<HashMap<PeerId, (u32, Instant)>>,
    /// Punches running now: (peer, slot).
    running: Mutex<std::collections::HashSet<(PeerId, String)>>,
    /// Told when a peer's "info" comes and no punch with it for that slot is running: the
    /// other side's connection failed first, and ours may start punching now instead of
    /// waiting for its own failure (more than INFO_WAIT apart, neither side punched).
    on_info: Mutex<Option<Arc<dyn Fn(PeerId, &str) + std::marker::Send + Sync>>>,
}

/// Takes a running punch off the set however it ends.
struct Running<'a>(&'a Puncher, (PeerId, String));
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.lock().unwrap().remove(&self.1);
    }
}

impl Puncher {
    pub fn new(send: Send) -> Arc<Self> {
        Arc::new(Self { send, heard: Mutex::default(), heard_cv: std::sync::Condvar::new(), failed: Mutex::default(), running: Mutex::default(), on_info: Mutex::default() })
    }

    /// `f(peer, slot)` when a peer's "info" comes while we are not punching with it.
    pub fn on_info(&self, f: impl Fn(PeerId, &str) + std::marker::Send + Sync + 'static) {
        *self.on_info.lock().unwrap() = Some(Arc::new(f));
    }

    /// A "p/" message from `from` (the "p/" taken off `kind`): "info:<slot>" or "done:<slot>".
    pub fn on_message(&self, from: PeerId, kind: &str, payload: &str) {
        let known = kind.split_once(':').is_some_and(|(k, slot)| (k == "info" || k == "done") && SLOTS.contains(&slot));
        if !known || payload.len() > 100 {
            return;
        }
        let mut h = self.heard.lock().unwrap();
        // A member's messages only for their own keys; a few slots each at most.
        if h.len() > 256 {
            h.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(60));
        }
        h.insert((from, kind.to_string()), (payload.to_string(), Instant::now()));
        self.heard_cv.notify_all();
        drop(h);
        if let Some(slot) = kind.strip_prefix("info:") {
            let hook = self.on_info.lock().unwrap().clone();
            // STREAMER_DEBUG_PUNCH_NO_JOIN: we wait for our own failure (the spoiled variant).
            let join = std::env::var_os("STREAMER_DEBUG_PUNCH_NO_JOIN").is_none();
            if let (Some(f), false, true) = (hook, self.busy(from, slot), join) {
                f(from, slot);
            }
        }
    }

    /// The peer's `kind` message that came after `since`, waiting up to `wait` for it.
    fn wait_for(&self, peer: PeerId, kind: &str, since: Instant, wait: Duration) -> Option<String> {
        let key = (peer, kind.to_string());
        let until = Instant::now() + wait;
        let mut h = self.heard.lock().unwrap();
        loop {
            if let Some((p, at)) = h.get(&key) {
                if *at >= since {
                    return Some(p.clone());
                }
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            h = self.heard_cv.wait_timeout(h, until - now).unwrap().0;
        }
    }

    /// Whether a punch with `peer` for `slot` is running.
    pub fn busy(&self, peer: PeerId, slot: &str) -> bool {
        self.running.lock().unwrap().contains(&(peer, slot.to_string()))
    }

    /// Tells `peer` our hole for `slot` is ready (the side that does not reconnect).
    pub fn send_done(&self, peer: PeerId, slot: &str) {
        (self.send)(peer, &format!("done:{slot}"), "1");
    }

    /// Waits for `peer` to say its hole for `slot` is ready (sent after `since`).
    pub fn wait_done(&self, peer: PeerId, slot: &str, since: Instant, wait: Duration) -> bool {
        self.wait_for(peer, &format!("done:{slot}"), since, wait).is_some()
    }

    /// Punches a hole to `peer` for `slot` (both sides call this for the same slot after
    /// their connection failed). Blocks up to ~25 s; None when it did not work, Busy when one
    /// is running already. The sockets are closed on return, the port is free for the ICE agent.
    pub fn punch(&self, peer: PeerId, slot: &str) -> Result<Option<Pinned>, Busy> {
        let key = (peer, slot.to_string());
        if !self.running.lock().unwrap().insert(key.clone()) {
            return Err(Busy);
        }
        let guard = Running(self, key);
        if let Some((n, at)) = self.failed.lock().unwrap().get(&peer).copied() {
            if n >= GIVE_UP_AFTER && at.elapsed() < GIVE_UP_FOR {
                eprintln!("punch: {slot} with {peer}: not again yet ({n} in a row did not come through)");
                return Ok(None);
            }
        }
        let (got, tried) = self.punch_once(peer, slot);
        drop(guard);
        let mut f = self.failed.lock().unwrap();
        match got {
            Some(_) => {
                f.remove(&peer);
            }
            // Only probing that came to nothing counts (not a STUN server or a peer that did
            // not answer: those say nothing about whether a hole can be made).
            None if tried => {
                let e = f.entry(peer).or_insert((0, Instant::now()));
                *e = (e.0 + 1, Instant::now());
            }
            None => {}
        }
        Ok(got)
    }

    /// The hole, and whether probing ran at all.
    fn punch_once(&self, peer: PeerId, slot: &str) -> (Option<Pinned>, bool) {
        let mut rng = Rng::new();
        // Theirs counts if it came lately: they may have started a moment before us. Older
        // ones are from an earlier round and name a socket long closed.
        let fresh = Instant::now().checked_sub(INFO_WAIT).unwrap_or_else(Instant::now);
        let Ok(first) = bind() else { return (None, false) };
        let Some(mine) = kind_of(&first, &mut rng) else {
            eprintln!("punch: {slot} with {peer}: no answer from the STUN servers");
            return (None, false);
        };
        (self.send)(peer, &format!("info:{slot}"), &info_text(mine));
        let Some(theirs) = self.wait_for(peer, &format!("info:{slot}"), fresh, INFO_WAIT).and_then(|p| parse_info(&p)) else {
            eprintln!("punch: {slot} with {peer}: the other side did not answer");
            return (None, false);
        };
        let ip = |k: Kind| match k {
            Kind::Easy(a) => a.ip(),
            Kind::Hard(ip) => ip.into(),
        };
        if ip(mine) == ip(theirs) && local_test().is_none() {
            eprintln!("punch: {slot} with {peer}: the same public address (one network); no hole to punch");
            return (None, false);
        }
        let until = Instant::now() + PUNCH_FOR;
        let t0 = Instant::now();
        let got = match (mine, theirs) {
            (Kind::Easy(_), Kind::Easy(a)) => punch_easy_easy(&first, a, until),
            (Kind::Easy(_), Kind::Hard(ip)) => punch_easy_hard(&first, ip, until, &mut rng),
            (Kind::Hard(_), Kind::Easy(a)) => {
                let mut socks = vec![first];
                socks.extend((1..SOCKETS).filter_map(|_| bind().ok()));
                if socks.len() < SOCKETS {
                    eprintln!("punch: only {} sockets could be opened", socks.len());
                }
                punch_hard(&socks, a, until)
            }
            (Kind::Hard(_), Kind::Hard(_)) => {
                eprintln!("punch: {slot} with {peer}: both sides behind a NAT that changes ports; cannot punch");
                return (None, false);
            }
        };
        match got {
            Some(p) => eprintln!("punch: {slot} with {peer}: through in {} ms ({:?} to {:?}), local port {}", t0.elapsed().as_millis(), kind_name(mine), kind_name(theirs), p.port),
            None => eprintln!("punch: {slot} with {peer}: nothing came through in {} s ({:?} to {:?})", PUNCH_FOR.as_secs(), kind_name(mine), kind_name(theirs)),
        }
        (got, true)
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
    fn a_second_punch_is_busy() {
        let p = Puncher::new(Arc::new(|_, _, _| {}));
        p.running.lock().unwrap().insert((7, "voice".to_string()));
        assert_eq!(p.punch(7, "voice"), Err(Busy));
        assert!(p.busy(7, "voice"));
        // Given up on: told as no hole, and the slot is free again after.
        p.running.lock().unwrap().clear();
        p.failed.lock().unwrap().insert(7, (GIVE_UP_AFTER, Instant::now()));
        assert_eq!(p.punch(7, "voice"), Ok(None));
        assert!(!p.busy(7, "voice"));
    }

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
