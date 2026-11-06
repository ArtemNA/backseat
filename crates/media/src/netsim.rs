//! Test-only network impairment for one viewer: a UDP relay inside the viewer process that
//! drops a share of the host's RTP packets and can squeeze everything from the host through
//! a slow link with a deep buffer, like a weak mobile connection. STUN, DTLS and RTCP are
//! never dropped, so the session stays up while media suffers. ICE candidates are rewritten
//! so both peers only see the relay.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What the relay does to traffic from the host.
#[derive(Debug, Clone, Copy, Default)]
pub struct Impairment {
    /// Share of RTP packets dropped, in percent.
    pub loss_pct: f64,
    /// Link rate in kbit/s; 0 means unlimited. Excess waits in a queue, as in a modem buffer.
    pub cap_kbit: u64,
    /// Fixed one-way delay added to every packet, in ms.
    pub delay_ms: u64,
}

impl Impairment {
    pub fn active(&self) -> bool {
        self.loss_pct > 0.0 || self.cap_kbit > 0 || self.delay_ms > 0
    }
}

/// Longest the slow link may queue before it drops, like a finite modem buffer.
const MAX_QUEUE: Duration = Duration::from_secs(4);

pub struct LossRelay {
    /// Receives from the host, sends to it.
    host_side: UdpSocket,
    /// Receives from the local webrtcbin, sends to it.
    viewer_side: UdpSocket,
    host_addr: Mutex<Option<SocketAddr>>,
    viewer_addr: Mutex<Option<SocketAddr>>,
    imp: Impairment,
    /// Packets waiting for the slow link: (time to deliver, bytes).
    queue: Mutex<VecDeque<(Instant, Vec<u8>)>>,
    queued: Condvar,
    pub dropped: AtomicU64,
    pub forwarded: AtomicU64,
    /// Packets dropped because the slow link's queue was full.
    pub overflow: AtomicU64,
    /// Largest queueing delay seen, in microseconds.
    pub max_queue_us: AtomicU64,
}

fn is_rtp(b: &[u8]) -> bool {
    b.len() > 1 && (128..192).contains(&b[0]) && !(200..=204).contains(&b[1])
}

impl LossRelay {
    pub fn start(imp: Impairment) -> Arc<Self> {
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
            imp,
            queue: Mutex::new(VecDeque::new()),
            queued: Condvar::new(),
            dropped: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            overflow: AtomicU64::new(0),
            max_queue_us: AtomicU64::new(0),
        });
        let a = r.clone();
        std::thread::spawn(move || a.pump_from_host());
        let b = r.clone();
        std::thread::spawn(move || b.pump_from_viewer());
        if imp.cap_kbit > 0 || imp.delay_ms > 0 {
            let c = r.clone();
            std::thread::spawn(move || c.deliver_queued());
        }
        r
    }

    fn to_viewer(&self, b: &[u8]) {
        if let Some(to) = *self.viewer_addr.lock().unwrap() {
            let _ = self.viewer_side.send_to(b, to);
        }
    }

    /// Puts a packet on the slow link. `link_free` is when the link finishes sending what is
    /// already queued; the packet leaves after it, plus its own transmission time.
    fn enqueue(&self, b: &[u8], link_free: &mut Instant) {
        let now = Instant::now();
        let start = (*link_free).max(now);
        if start - now > MAX_QUEUE {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let tx = Duration::from_nanos((b.len() as u64 * 8 * 1_000_000).checked_div(self.imp.cap_kbit).unwrap_or(0));
        *link_free = start + tx;
        self.max_queue_us.fetch_max((start - now).as_micros() as u64, Ordering::Relaxed);
        let due = *link_free + Duration::from_millis(self.imp.delay_ms);
        self.queue.lock().unwrap().push_back((due, b.to_vec()));
        self.queued.notify_one();
    }

    fn deliver_queued(&self) {
        loop {
            let mut q = self.queue.lock().unwrap();
            while q.is_empty() {
                q = self.queued.wait(q).unwrap();
            }
            let due = q.front().unwrap().0;
            let now = Instant::now();
            if due > now {
                drop(q);
                std::thread::sleep(due - now);
                continue;
            }
            let (_, b) = q.pop_front().unwrap();
            drop(q);
            self.to_viewer(&b);
        }
    }

    fn pump_from_host(&self) {
        let mut buf = vec![0u8; 2048];
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        // Experiment knob: never drop the last packet of a frame (RTP marker bit).
        let spare_marker = std::env::var_os("STREAMER_DEBUG_LOSS_SPARE_MARKER").is_some();
        // Experiment knob: log every dropped packet (pt, seq, marker, rtp time) to a file.
        let mut log = std::env::var_os("STREAMER_DEBUG_LOSS_LOG").and_then(|p| std::fs::File::create(p).ok());
        let started = Instant::now();
        let shaped = self.imp.cap_kbit > 0 || self.imp.delay_ms > 0;
        let mut link_free = Instant::now();
        loop {
            let Ok(n) = self.host_side.recv(&mut buf) else { continue };
            let marker = n > 1 && buf[1] & 0x80 != 0;
            if is_rtp(&buf[..n]) && !(spare_marker && marker) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if ((rng % 100_000) as f64) < self.imp.loss_pct * 1000.0 {
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
            // STUN and DTLS go straight through, so connectivity checks and the handshake
            // are not starved; RTP and RTCP share the slow link like on a real network.
            if shaped && n > 1 && (128..192).contains(&buf[0]) {
                self.enqueue(&buf[..n], &mut link_free);
            } else {
                self.to_viewer(&buf[..n]);
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
