//! Viewer-side estimate of the host's pipeline running time, from ping/pong over the link.
//! Within a sliding window the ping with the lowest round trip wins; its error is at most
//! half of that round trip. The window follows clock drift between the two machines, which
//! a single best-ever estimate does not (stage 1: ~1 ms per 30 s of apparent creep).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Pings sent every 200 ms, so this is about 10 s.
const WINDOW: usize = 50;

#[derive(Clone)]
pub struct HostClock {
    base: Instant,
    offset_ns: Arc<AtomicI64>,
    best_rtt_ns: Arc<AtomicU64>,
    samples: Arc<Mutex<VecDeque<(u64, i64)>>>,
}

impl Default for HostClock {
    fn default() -> Self {
        Self {
            base: Instant::now(),
            offset_ns: Arc::new(AtomicI64::new(0)),
            best_rtt_ns: Arc::new(AtomicU64::new(u64::MAX)),
            samples: Arc::default(),
        }
    }
}

impl HostClock {
    pub fn local_ns(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    /// Host running time now, or None before the first pong.
    pub fn host_now(&self) -> Option<u64> {
        if self.best_rtt_ns.load(Ordering::Relaxed) == u64::MAX {
            return None;
        }
        Some((self.local_ns() as i64 + self.offset_ns.load(Ordering::Relaxed)) as u64)
    }

    /// Handles "pong <sent_local_ns> <host_running_ns>". Returns the round trip.
    pub fn on_pong(&self, payload: &str) -> Option<u64> {
        let now = self.local_ns();
        let (sent, host) = payload.split_once(' ')?;
        let (sent, host): (u64, u64) = (sent.parse().ok()?, host.parse().ok()?);
        let rtt = now.checked_sub(sent)?;
        let offset = host as i64 + rtt as i64 / 2 - now as i64;
        let mut s = self.samples.lock().unwrap();
        s.push_back((rtt, offset));
        if s.len() > WINDOW {
            s.pop_front();
        }
        let &(best_rtt, best_offset) = s.iter().min_by_key(|(r, _)| *r).unwrap();
        self.best_rtt_ns.store(best_rtt, Ordering::Relaxed);
        self.offset_ns.store(best_offset, Ordering::Relaxed);
        Some(rtt)
    }

    /// The latest round trip of the ping (through the signaling path, not the media one).
    pub fn last_rtt_ns(&self) -> Option<u64> {
        self.samples.lock().unwrap().back().map(|s| s.0)
    }

    pub fn best_rtt_ns(&self) -> Option<u64> {
        match self.best_rtt_ns.load(Ordering::Relaxed) {
            u64::MAX => None,
            v => Some(v),
        }
    }
}

/// Host side: pipeline running time in ns.
pub fn running_time(pipeline: &gst::Pipeline) -> Option<u64> {
    use gst::prelude::*;
    Some(pipeline.clock()?.time().nseconds() - pipeline.base_time()?.nseconds())
}
