//! Test-only network impairment for one viewer: a UDP relay inside the viewer process that
//! drops a share of the host's RTP packets. STUN, DTLS and RTCP pass, so the session stays
//! up while media suffers. ICE candidates are rewritten so both peers only see the relay.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct LossRelay {
    /// Receives from the host, sends to it.
    host_side: UdpSocket,
    /// Receives from the local webrtcbin, sends to it.
    viewer_side: UdpSocket,
    host_addr: Mutex<Option<SocketAddr>>,
    viewer_addr: Mutex<Option<SocketAddr>>,
    loss_pct: f64,
    pub dropped: AtomicU64,
    pub forwarded: AtomicU64,
}

fn is_rtp(b: &[u8]) -> bool {
    b.len() > 1 && (128..192).contains(&b[0]) && !(200..=204).contains(&b[1])
}

impl LossRelay {
    pub fn start(loss_pct: f64) -> Arc<Self> {
        let open = || {
            let s = UdpSocket::bind("0.0.0.0:0").expect("relay socket");
            s.set_read_timeout(Some(Duration::from_millis(100))).ok();
            s
        };
        let r = Arc::new(Self {
            host_side: open(),
            viewer_side: open(),
            host_addr: Mutex::new(None),
            viewer_addr: Mutex::new(None),
            loss_pct,
            dropped: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
        });
        let a = r.clone();
        std::thread::spawn(move || a.pump_from_host());
        let b = r.clone();
        std::thread::spawn(move || b.pump_from_viewer());
        r
    }

    fn pump_from_host(&self) {
        let mut buf = vec![0u8; 2048];
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        // Experiment knob: never drop the last packet of a frame (RTP marker bit).
        let spare_marker = std::env::var_os("STREAMER_DEBUG_LOSS_SPARE_MARKER").is_some();
        // Experiment knob: log every dropped packet (pt, seq, marker, rtp time) to a file.
        let mut log = std::env::var_os("STREAMER_DEBUG_LOSS_LOG").and_then(|p| std::fs::File::create(p).ok());
        let started = std::time::Instant::now();
        loop {
            let Ok(n) = self.host_side.recv(&mut buf) else { continue };
            let marker = n > 1 && buf[1] & 0x80 != 0;
            if is_rtp(&buf[..n]) && !(spare_marker && marker) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if ((rng % 100_000) as f64) < self.loss_pct * 1000.0 {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    if let (Some(f), true) = (log.as_mut(), n >= 12) {
                        use std::io::Write;
                        let seq = u16::from_be_bytes([buf[2], buf[3]]);
                        let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        let _ = writeln!(
                            f,
                            "{:.3} pt={} seq={} marker={} ts={}",
                            started.elapsed().as_secs_f64(),
                            buf[1] & 0x7f,
                            seq,
                            marker as u8,
                            ts
                        );
                    }
                    continue;
                }
                self.forwarded.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(to) = *self.viewer_addr.lock().unwrap() {
                let _ = self.viewer_side.send_to(&buf[..n], to);
            }
        }
    }

    fn pump_from_viewer(&self) {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok(n) = self.viewer_side.recv(&mut buf) else { continue };
            if let Some(to) = *self.host_addr.lock().unwrap() {
                let _ = self.host_side.send_to(&buf[..n], to);
            }
        }
    }

    /// Splits an ICE candidate line into its fields, for the first IPv4 UDP candidate only.
    fn udp4(candidate: &str) -> Option<(Vec<String>, SocketAddr)> {
        let parts: Vec<String> = candidate.split(' ').map(str::to_string).collect();
        if parts.len() < 6 || !parts[2].eq_ignore_ascii_case("udp") || !parts[4].contains('.') {
            return None;
        }
        let addr = format!("{}:{}", parts[4], parts[5]).parse().ok()?;
        Some((parts, addr))
    }

    /// Our own candidate, before it goes to the host: the host must send to the relay.
    /// Returns None to withhold the candidate.
    pub fn rewrite_local(&self, candidate: &str) -> Option<String> {
        if candidate.is_empty() {
            return Some(String::new());
        }
        let (mut parts, addr) = Self::udp4(candidate)?;
        let mut slot = self.viewer_addr.lock().unwrap();
        if slot.is_some() {
            return None;
        }
        *slot = Some(addr);
        parts[5] = self.host_side.local_addr().unwrap().port().to_string();
        Some(parts.join(" "))
    }

    /// The host's candidate, before our webrtcbin sees it: we must send to the relay.
    pub fn rewrite_remote(&self, candidate: &str) -> Option<String> {
        if candidate.is_empty() {
            return Some(String::new());
        }
        let (mut parts, addr) = Self::udp4(candidate)?;
        let mut slot = self.host_addr.lock().unwrap();
        if slot.is_some() {
            return None;
        }
        *slot = Some(addr);
        parts[4] = local_ip_towards(addr.ip()).to_string();
        parts[5] = self.viewer_side.local_addr().unwrap().port().to_string();
        Some(parts.join(" "))
    }
}

/// The local address the OS would use to reach `remote`, where the relay is reachable.
fn local_ip_towards(remote: IpAddr) -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect((remote, 9)).map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .unwrap_or(remote)
}
