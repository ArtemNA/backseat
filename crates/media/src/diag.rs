//! Diagnostics for sound that crackles (2026-11-24: a stream's sound and a voice started to
//! crackle after pauses, cured only by joining again). They change no sound; they only say,
//! in the log, where timestamps break and what the jitter buffers do:
//! - `watch_ts`: gaps and overlaps between consecutive buffers' timestamps at a pad, and
//!   buffers flagged as a discontinuity. The first break of each minute at once, then a
//!   summary per minute when there were any.
//! - `watch_jitterbuffers`: once a minute, what each jitter buffer of a webrtcbin did (lost,
//!   late, duplicates, average jitter) when any of those changed.
use gst::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A break bigger than this between buffers counts (a jitter buffer's normal rounding stays
/// well below; a crackle's holes are 10 ms and more).
const BREAK_NS: i64 = 2_000_000;

/// Timestamp continuity of one stream of buffers.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct TsStats {
    pub buffers: u64,
    pub gaps: u64,
    pub gap_max_ms: f64,
    pub overlaps: u64,
    pub overlap_max_ms: f64,
    pub disconts: u64,
    expected: Option<u64>,
}

/// What one buffer did to the continuity.
#[derive(Debug, PartialEq)]
pub enum Break {
    Gap(f64),
    Overlap(f64),
}

impl TsStats {
    /// One buffer: its timestamp and duration (ns), and whether it is flagged as a
    /// discontinuity. STREAMER_DEBUG_DIAG_BLIND: breaks are not seen (a test's spoiled run).
    pub fn feed(&mut self, pts: Option<u64>, dur: Option<u64>, discont: bool) -> Option<Break> {
        self.buffers += 1;
        if discont {
            self.disconts += 1;
        }
        let pts = pts?;
        static BLIND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let blind = *BLIND.get_or_init(|| std::env::var_os("STREAMER_DEBUG_DIAG_BLIND").is_some());
        let mut out = None;
        if let (Some(exp), false) = (self.expected, blind) {
            let d = pts as i64 - exp as i64;
            if d > BREAK_NS {
                let ms = d as f64 / 1e6;
                self.gaps += 1;
                self.gap_max_ms = self.gap_max_ms.max(ms);
                out = Some(Break::Gap(ms));
            } else if d < -BREAK_NS {
                let ms = -d as f64 / 1e6;
                self.overlaps += 1;
                self.overlap_max_ms = self.overlap_max_ms.max(ms);
                out = Some(Break::Overlap(ms));
            }
        }
        self.expected = dur.map(|d| pts + d);
        out
    }

    fn any(&self) -> bool {
        self.gaps + self.overlaps + self.disconts > 0
    }

    /// The counts so far, and a fresh start for the next minute (the expected next timestamp
    /// stays).
    fn take(&mut self) -> TsStats {
        let expected = self.expected;
        let out = std::mem::take(self);
        self.expected = expected;
        out
    }
}

/// Tests only: STREAMER_DEBUG_NO_DIAG turns these watches off (to see whether they disturb
/// what they watch).
fn off() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("STREAMER_DEBUG_NO_DIAG").is_some())
}

/// Watches the timestamps of the buffers through `pad`, naming them `name` in the log.
pub fn watch_ts(pad: &gst::Pad, name: String) {
    if off() {
        return;
    }
    let st = Mutex::new((TsStats::default(), Instant::now(), false));
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(b) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let mut g = st.lock().unwrap();
        let (stats, since, told) = &mut *g;
        let brk = stats.feed(b.pts().map(|t| t.nseconds()), b.duration().map(|t| t.nseconds()), b.flags().contains(gst::BufferFlags::DISCONT));
        if let (Some(brk), false) = (brk, *told) {
            *told = true;
            eprintln!("diag ts {name}: {brk:?} at {:?} (more this minute go to the summary)", b.pts());
        }
        if since.elapsed() >= Duration::from_secs(60) {
            let s = stats.take();
            if s.any() {
                eprintln!(
                    "diag ts {name}, last minute: {} buffers, gaps {} (max {:.1} ms), overlaps {} (max {:.1} ms), flagged discontinuities {}",
                    s.buffers, s.gaps, s.gap_max_ms, s.overlaps, s.overlap_max_ms, s.disconts
                );
            }
            *since = Instant::now();
            *told = false;
        }
        gst::PadProbeReturn::Ok
    });
}

/// Once a minute, each jitter buffer of `webrtc` that lost, got late or duplicate packets in
/// that minute: its counts and average jitter. `label` names the connection in the log.
pub fn watch_jitterbuffers(webrtc: &gst::Element, label: String) {
    if off() {
        return;
    }
    let Some(rtpbin) = webrtc.downcast_ref::<gst::Bin>().and_then(|b| b.by_name("rtpbin")) else { return };
    rtpbin.connect("new-jitterbuffer", false, move |v| {
        let jb = v[1].get::<gst::Element>().unwrap();
        let (weak, label) = (jb.downgrade(), label.clone());
        let _ = std::thread::Builder::new().name("diag-jb".into()).spawn(move || {
            let read = |jb: &gst::Element| {
                let s = jb.property::<gst::Structure>("stats");
                let g = |k: &str| s.get::<u64>(k).unwrap_or(0);
                (g("num-pushed"), g("num-lost"), g("num-late"), g("num-duplicates"), g("avg-jitter"))
            };
            let mut last = match weak.upgrade() {
                Some(jb) => read(&jb),
                None => return,
            };
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let Some(jb) = weak.upgrade() else { return };
                let now = read(&jb);
                let (lost, late, dup) = (now.1.saturating_sub(last.1), now.2.saturating_sub(last.2), now.3.saturating_sub(last.3));
                if lost + late + dup > 0 {
                    eprintln!(
                        "diag jitterbuffer {label} {}: last minute {} packets out, lost {lost}, late {late}, duplicates {dup}, average jitter {:.1} ms",
                        jb.name(),
                        now.0.saturating_sub(last.0),
                        now.4 as f64 / 1e6
                    );
                }
                last = now;
            }
        });
        None
    });
}

/// Counts what a leaky queue throws away (its "overrun"), logged once a minute when any.
pub fn watch_queue_drops(queue: &gst::Element, name: String) {
    let n = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let n2 = n.clone();
    queue.connect("overrun", false, move |_| {
        n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    });
    let weak = queue.downgrade();
    let _ = std::thread::Builder::new().name("diag-queue".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(60));
        if weak.upgrade().is_none() {
            return;
        }
        let k = n.swap(0, std::sync::atomic::Ordering::Relaxed);
        if k > 0 {
            eprintln!("diag queue {name}: full {k} times in the last minute (dropping its oldest)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Continuous 10 ms buffers: no breaks; a 10 ms hole and a 15 ms step back are seen with
    /// their size. Spoiled: STREAMER_DEBUG_DIAG_BLIND.
    #[test]
    fn breaks_are_seen() {
        let mut s = TsStats::default();
        for i in 0..10 {
            assert_eq!(s.feed(Some(i * 10 * MS), Some(10 * MS), false), None);
        }
        assert_eq!(s.feed(Some(110 * MS), Some(10 * MS), false), Some(Break::Gap(10.0)));
        assert_eq!(s.feed(Some(105 * MS), Some(10 * MS), true), Some(Break::Overlap(15.0)));
        assert_eq!(s.feed(Some(116 * MS), Some(10 * MS), false), None, "1 ms of rounding is not a break");
        let t = s.take();
        assert_eq!((t.buffers, t.gaps, t.overlaps, t.disconts), (13, 1, 1, 1));
        assert_eq!(s.feed(Some(126 * MS), Some(10 * MS), false), None, "the next minute goes on from the last buffer");
    }
}
