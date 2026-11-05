//! NAT classification with plain STUN binding requests (RFC 5389), no dependencies.
//! One local socket asks several STUN servers on different IPs what address they see.
//! The same mapping for every server means an "easy" NAT (endpoint-independent mapping):
//! direct connections usually work. Different mappings mean a "hard" NAT: direct
//! connections fail against another hard NAT and often against an easy one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const MAGIC: u32 = 0x2112_A442;

pub const DEFAULT_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun1.l.google.com:19302"];

fn request(id: &[u8; 12]) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // Binding request
    b[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    b[8..20].copy_from_slice(id);
    b
}

/// Parses XOR-MAPPED-ADDRESS (or MAPPED-ADDRESS) from a Binding success response.
fn parse(resp: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if resp.len() < 20 || resp[0..2] != 0x0101u16.to_be_bytes() || &resp[8..20] != id {
        return None;
    }
    let len = u16::from_be_bytes([resp[2], resp[3]]) as usize;
    let mut i = 20;
    let end = (20 + len).min(resp.len());
    let mut plain = None;
    while i + 4 <= end {
        let t = u16::from_be_bytes([resp[i], resp[i + 1]]);
        let l = u16::from_be_bytes([resp[i + 2], resp[i + 3]]) as usize;
        let v = resp.get(i + 4..i + 4 + l)?;
        if (t == 0x0020 || t == 0x0001) && l >= 8 && v[1] == 0x01 {
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            let mut ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if t == 0x0020 {
                port ^= (MAGIC >> 16) as u16;
                ip ^= MAGIC;
                return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
            }
            plain = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        i += 4 + l.div_ceil(4) * 4;
    }
    plain
}

/// Asks one server for our mapped address from `sock`, with retries.
pub fn mapped(sock: &UdpSocket, server: SocketAddr) -> Option<SocketAddr> {
    let mut id = [0u8; 12];
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    id.copy_from_slice(&seed.to_le_bytes()[..12]);
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        sock.send_to(&request(&id), server).ok()?;
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if from == server {
                if let Some(a) = parse(&buf[..n], &id) {
                    return Some(a);
                }
            }
        }
    }
    None
}

#[derive(Debug)]
pub struct Report {
    pub local: SocketAddr,
    /// (server, address it saw), None when it did not answer.
    pub answers: Vec<(SocketAddr, Option<SocketAddr>)>,
    pub verdict: Verdict,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No STUN server answered: UDP is blocked or there is no internet.
    UdpBlocked,
    /// The mapped address equals our local one.
    NoNat,
    /// Same mapping for every server.
    Easy,
    /// Different mapping per server; `port_step` is the port difference between two
    /// consecutive mappings when it is constant, which makes ports predictable.
    Hard { port_step: Option<i32> },
}

/// Resolves the first IPv4 address of each server name.
pub fn resolve(servers: &[&str]) -> Vec<SocketAddr> {
    servers
        .iter()
        .filter_map(|s| s.to_socket_addrs().ok()?.find(|a| a.is_ipv4()))
        .collect()
}

pub fn classify(servers: &[SocketAddr]) -> std::io::Result<Report> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    // The OS's own source address towards the first server, to spot a public address.
    let local_ip = servers
        .first()
        .and_then(|s| UdpSocket::bind("0.0.0.0:0").ok().and_then(|p| p.connect(s).ok().map(|_| p)))
        .and_then(|p| p.local_addr().ok())
        .map(|a| a.ip());
    let local = SocketAddr::new(local_ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), sock.local_addr()?.port());
    let answers: Vec<_> = servers.iter().map(|s| (*s, mapped(&sock, *s))).collect();
    let seen: Vec<SocketAddr> = answers.iter().filter_map(|(_, a)| *a).collect();
    Ok(Report { local, answers, verdict: verdict(local, &seen) })
}

/// The verdict from the addresses servers saw, in query order.
pub fn verdict(local: SocketAddr, seen: &[SocketAddr]) -> Verdict {
    if seen.is_empty() {
        Verdict::UdpBlocked
    } else if seen.iter().all(|a| *a == local) {
        Verdict::NoNat
    } else if seen.windows(2).all(|w| w[0] == w[1]) {
        Verdict::Easy
    } else {
        let ports: Vec<i32> = seen.iter().map(|a| a.port() as i32).collect();
        let steps: Vec<i32> = ports.windows(2).map(|w| w[1] - w[0]).collect();
        let port_step = (steps.len() >= 2 && steps.windows(2).all(|w| w[0] == w[1])).then(|| steps[0]);
        Verdict::Hard { port_step }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn verdicts() {
        let local = a("192.168.1.2:5000");
        assert_eq!(verdict(local, &[]), Verdict::UdpBlocked);
        assert_eq!(verdict(local, &[local, local]), Verdict::NoNat);
        assert_eq!(verdict(local, &[a("1.2.3.4:5000"), a("1.2.3.4:5000")]), Verdict::Easy);
        assert_eq!(
            verdict(local, &[a("1.2.3.4:6000"), a("1.2.3.4:6002"), a("1.2.3.4:6004")]),
            Verdict::Hard { port_step: Some(2) }
        );
        assert_eq!(
            verdict(local, &[a("1.2.3.4:6000"), a("1.2.3.4:9113"), a("1.2.3.4:4021")]),
            Verdict::Hard { port_step: None }
        );
        // Two answers give one step only: not enough to call the ports predictable.
        assert_eq!(verdict(local, &[a("1.2.3.4:6000"), a("1.2.3.4:6001")]), Verdict::Hard { port_step: None });
    }

    #[test]
    fn parses_xor_mapped_address() {
        let id = [7u8; 12];
        // Success response with XOR-MAPPED-ADDRESS for 203.0.113.5:40000.
        let ip = u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC;
        let port = 40000u16 ^ (MAGIC >> 16) as u16;
        let mut r = vec![0x01, 0x01, 0x00, 0x0c];
        r.extend_from_slice(&MAGIC.to_be_bytes());
        r.extend_from_slice(&id);
        r.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        r.extend_from_slice(&port.to_be_bytes());
        r.extend_from_slice(&ip.to_be_bytes());
        assert_eq!(parse(&r, &id), Some("203.0.113.5:40000".parse().unwrap()));
        // A different transaction id is rejected.
        assert_eq!(parse(&r, &[8u8; 12]), None);
    }
}
