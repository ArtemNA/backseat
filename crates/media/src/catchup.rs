//! Catching up after a viewer's link falls behind. Packets queued inside the network cannot
//! be dropped by either end, so the viewer measures how far behind it is and asks the host
//! to skip video for that long: the network queue drains, and the stream resumes at the next
//! keyframe. Only this viewer's branch pauses; the encoder and other viewers are untouched.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Signaling message kind; the payload is the lag in ms.
pub const MSG: &str = "catchup";
/// Lag over the best transit above which the viewer asks to catch up.
pub const LAG_LIMIT: Duration = Duration::from_millis(250);
/// The lag must last this long, so one big keyframe does not trigger a pause.
pub const LAG_HOLD: Duration = Duration::from_millis(300);
/// The baseline is the smallest transit over this long, short enough that clock drift
/// between the machines (tens of ppm) stays far below the limit.
const BASELINE_WINDOW: Duration = Duration::from_secs(300);
const BUCKET: Duration = Duration::from_secs(10);
/// Pause bounds on the host, whatever the viewer asks.
/// The host pauses for this many times the reported lag.
const PAUSE_FACTOR: u32 = 2;
const MIN_PAUSE: Duration = Duration::from_millis(100);
const MAX_PAUSE: Duration = Duration::from_secs(5);

/// Viewer side: follows the transit of video packets and decides when to ask.
pub struct LagWatch {
    state: Mutex<WatchState>,
    pub requests: AtomicU64,
    /// Largest lag seen since the last report, in microseconds.
    pub max_lag_us: AtomicU64,
}

struct WatchState {
    /// Smallest transit per bucket, newest last: (bucket start, transit ns).
    buckets: VecDeque<(Instant, i128)>,
    over_since: Option<Instant>,
    /// No new request before this: the previous pause and the wait for a keyframe.
    quiet_until: Option<Instant>,
}

impl Default for LagWatch {
    fn default() -> Self {
        Self {
            state: Mutex::new(WatchState { buckets: VecDeque::new(), over_since: None, quiet_until: None }),
            requests: AtomicU64::new(0),
            max_lag_us: AtomicU64::new(0),
        }
    }
}

impl LagWatch {
    /// Takes one packet's transit (arrival minus RTP time, any constant offset). Returns the
    /// lag to request a catch-up for, when one is due.
    pub fn packet(&self, now: Instant, transit_ns: i128) -> Option<Duration> {
        let mut s = self.state.lock().unwrap();
        match s.buckets.back_mut() {
            Some((start, min)) if now.duration_since(*start) < BUCKET => *min = (*min).min(transit_ns),
            _ => s.buckets.push_back((now, transit_ns)),
        }
        while s.buckets.front().is_some_and(|(start, _)| now.duration_since(*start) > BASELINE_WINDOW) {
            s.buckets.pop_front();
        }
        let base = s.buckets.iter().map(|(_, m)| *m).min().unwrap();
        let lag = Duration::from_nanos((transit_ns - base).max(0) as u64);
        self.max_lag_us.fetch_max(lag.as_micros() as u64, Ordering::Relaxed);
        if lag <= LAG_LIMIT {
            s.over_since = None;
            return None;
        }
        let since = *s.over_since.get_or_insert(now);
        if now.duration_since(since) < LAG_HOLD || s.quiet_until.is_some_and(|q| now < q) {
            return None;
        }
        // Wait out the pause only. Waiting for the keyframe too (up to 2 s) let a link three
        // times slower than the stream fill 4 s of queue again; a request that comes while
        // the host still waits for a keyframe is ignored there anyway.
        s.quiet_until = Some(now + (lag * PAUSE_FACTOR).min(MAX_PAUSE) + LAG_HOLD);
        s.over_since = None;
        self.requests.fetch_add(1, Ordering::Relaxed);
        Some(lag)
    }
}

/// Host side: gates one viewer's video before its payloader. Dropping there keeps RTP
/// sequence numbers continuous, so the viewer sees a jump in time, not losses to repair.
#[derive(Default)]
pub struct Gate {
    resume_at: Mutex<Option<Instant>>,
    waiting_key: AtomicBool,
    /// A new viewer: nothing goes out before the first keyframe, and those frames are not
    /// counted as skipped (they were never the viewer's).
    starting: AtomicBool,
    pub pauses: AtomicU64,
    pub skipped_frames: AtomicU64,
}

impl Gate {
    /// A viewer asked to catch up by `lag`. A request during a pause, or while waiting for
    /// the keyframe after it, means the queue is still long: the pause restarts or extends.
    /// Ignoring those let a link three times slower than the stream climb to 4 s of queue.
    pub fn request(&self, lag: Duration) {
        let until = Instant::now() + (lag * PAUSE_FACTOR).clamp(MIN_PAUSE, MAX_PAUSE);
        let mut r = self.resume_at.lock().unwrap();
        *r = Some(r.map_or(until, |t| t.max(until)));
        self.waiting_key.store(false, Ordering::SeqCst);
        self.pauses.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether to pass one encoded frame.
    pub fn pass(&self, keyframe: bool) -> bool {
        {
            let mut r = self.resume_at.lock().unwrap();
            if let Some(t) = *r {
                if Instant::now() < t {
                    self.skipped_frames.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                *r = None;
                self.waiting_key.store(true, Ordering::SeqCst);
            }
        }
        if self.waiting_key.load(Ordering::SeqCst) {
            if !keyframe {
                if !self.starting.load(Ordering::SeqCst) {
                    self.skipped_frames.fetch_add(1, Ordering::Relaxed);
                }
                return false;
            }
            self.waiting_key.store(false, Ordering::SeqCst);
            self.starting.store(false, Ordering::SeqCst);
        }
        true
    }

    /// For a viewer that joins a running layer: the first frame it gets is a keyframe. Delta
    /// frames before it made the viewer's AV1 depayloader queue up to a GOP (2.3 s, hundreds
    /// of warnings per start) and the decoder see decreasing timestamps.
    pub fn starting() -> Self {
        let g = Self::default();
        g.waiting_key.store(true, Ordering::SeqCst);
        g.starting.store(true, Ordering::SeqCst);
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i128 = 1_000_000;

    #[test]
    fn steady_or_brief_lag_never_asks() {
        let w = LagWatch::default();
        let t0 = Instant::now();
        for i in 0..1000u64 {
            let now = t0 + Duration::from_millis(i * 10);
            // 40 ms of jitter, and a 400 ms spike lasting 100 ms (a keyframe burst).
            let extra = if (500..510).contains(&i) { 400 } else { (i % 5) as i128 * 10 };
            assert_eq!(w.packet(now, 5_000 * MS + extra * MS), None, "packet {i}");
        }
    }

    #[test]
    fn growing_queue_asks_again_after_each_pause() {
        let w = LagWatch::default();
        let t0 = Instant::now();
        let mut asked = vec![];
        for i in 0..500u64 {
            let now = t0 + Duration::from_millis(i * 10);
            // The queue grows by 100 ms every second.
            if let Some(lag) = w.packet(now, i as i128 * MS) {
                asked.push((i, lag));
            }
        }
        // Over 250 ms from 2.5 s, held 300 ms: the first request near 2.8 s. Then quiet
        // for the host's pause (twice the lag, ~0.56 s) and the hold: the next ~0.86 s later.
        assert!((280..=290).contains(&asked[0].0), "{asked:?}");
        assert!((362..=372).contains(&asked[1].0), "{asked:?}");
    }

    #[test]
    fn gate_pauses_then_waits_for_a_keyframe() {
        let g = Gate::default();
        assert!(g.pass(false));
        g.request(Duration::from_millis(60));
        assert!(!g.pass(false));
        std::thread::sleep(Duration::from_millis(140));
        assert!(!g.pass(false), "delta frames after the pause are useless without a keyframe");
        assert!(g.pass(true));
        assert!(g.pass(false));
        assert_eq!(g.pauses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn request_while_waiting_for_keyframe_pauses_again() {
        let g = Gate::default();
        g.request(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(120));
        assert!(!g.pass(false), "now waiting for a keyframe");
        g.request(Duration::from_millis(100));
        assert!(!g.pass(true), "a new pause, even for a keyframe");
        std::thread::sleep(Duration::from_millis(220));
        assert!(g.pass(true));
    }
}
