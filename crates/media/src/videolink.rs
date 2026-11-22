//! The link between the app and its video process (`crate::videoproc`): length-prefixed
//! messages over a pipe. Frames are binary (header + compressed data); everything else is
//! JSON. A broken pipe is the other side gone.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const KIND_FRAME: u8 = 1;
const KIND_JSON: u8 = 2;
/// Larger than any compressed frame we send (a 4K keyframe at 50 Mbit/s is ~1 MB).
const MAX_LEN: u32 = 64 << 20;
const NONE: u64 = u64::MAX;

/// One compressed video frame (an access unit, byte-stream).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub duration: Option<u64>,
    /// Not a keyframe.
    pub delta: bool,
    pub discont: bool,
    /// When the host captured it, on the host's clock (ns), if known.
    pub sent_host: Option<u64>,
    /// Host clock minus `qpc_ns()` when this was sent, if the host clock is known: the
    /// video process turns its own `qpc_ns()` into host time with it.
    pub host_minus_qpc: Option<i64>,
    pub data: Vec<u8>,
}

const FRAME_HEADER: usize = 8 * 5 + 4 + 1;

/// The app to its video process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ToVideo {
    /// First message: the window (inside `parent`, the app's main window; 0: its own).
    Start { parent: isize, title: String },
    /// The compressed stream's caps and the decoder for it; again when they change.
    Stream { caps: String, decoder: String },
    Place { rect: Option<(i32, i32, i32, i32)> },
    Hidden { on: bool },
    Popped { on: bool },
    Fullscreen { on: bool },
    ToggleFullscreen,
    Topmost { on: bool },
    Show,
    Title { text: String },
    Overlay { on: bool },
    OverlayText { text: String },
    Badge { on: bool },
    BadgeInfo { text: String, ok: bool },
    Status { text: String },
    PlaceAgain,
    /// The stream's sound as the app has it, for the video window's own controls.
    Audio { muted: bool, volume: f32 },
    Close,
}

/// The video process to the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WindowStateMsg {
    pub fullscreen: bool,
    pub topmost: bool,
    pub overlay: bool,
    pub badge: bool,
    pub embedded: bool,
    pub alive: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum FromVideo {
    /// Every ~100 ms: what happened since the last one.
    Report {
        state: WindowStateMsg,
        /// The window was closed by its user.
        closed: bool,
        /// Frames out of the decoder, and the gaps between them (ns).
        frames: u64,
        intervals: Vec<u64>,
        /// Host capture to decoded (ns).
        latencies: Vec<u64>,
        presented: u64,
        skipped: u64,
        max_present_us: u64,
        max_lock_us: u64,
        size: Option<(u32, u32)>,
        /// Turns of the presentation loop so far: stands still when it hangs.
        loops: u64,
    },
    /// The video window's own sound controls were used.
    Audio { muted: Option<bool>, volume: Option<f32> },
    /// The decoder needs a keyframe.
    Keyframe,
}

pub enum Msg<T> {
    Frame(Frame),
    Json(T),
}

fn opt(v: Option<u64>) -> u64 {
    v.unwrap_or(NONE)
}

fn back(v: u64) -> Option<u64> {
    (v != NONE).then_some(v)
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> std::io::Result<()> {
    let len = FRAME_HEADER + f.data.len();
    let mut head = Vec::with_capacity(5 + FRAME_HEADER);
    head.push(KIND_FRAME);
    head.extend_from_slice(&(len as u32).to_le_bytes());
    head.extend_from_slice(&opt(f.pts).to_le_bytes());
    head.extend_from_slice(&opt(f.dts).to_le_bytes());
    head.extend_from_slice(&opt(f.duration).to_le_bytes());
    head.extend_from_slice(&opt(f.sent_host).to_le_bytes());
    head.extend_from_slice(&f.host_minus_qpc.unwrap_or(i64::MIN).to_le_bytes());
    head.extend_from_slice(&(f.delta as u32 | (f.discont as u32) << 1).to_le_bytes());
    head.push(0);
    w.write_all(&head)?;
    // STREAMER_DEBUG_LINK_SLOW: 2 ms between header and data (the spoiled variant of the
    // link's delay test).
    if std::env::var_os("STREAMER_DEBUG_LINK_SLOW").is_some() {
        w.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    w.write_all(&f.data)?;
    w.flush()
}

pub fn write_json<T: Serialize>(w: &mut impl Write, m: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(m).map_err(std::io::Error::other)?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(KIND_JSON);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    w.write_all(&out)?;
    w.flush()
}

/// The next message; None when the other side closed the pipe.
pub fn read<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> std::io::Result<Option<Msg<T>>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof || e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap());
    if len > MAX_LEN {
        return Err(std::io::Error::other(format!("message of {len} bytes")));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    match head[0] {
        KIND_FRAME if body.len() >= FRAME_HEADER => {
            let u = |i: usize| u64::from_le_bytes(body[i * 8..i * 8 + 8].try_into().unwrap());
            let (pts, dts, duration, sent) = (u(0), u(1), u(2), u(3));
            let hmq = i64::from_le_bytes(body[32..40].try_into().unwrap());
            let flags = u32::from_le_bytes(body[40..44].try_into().unwrap());
            let data = body.split_off(FRAME_HEADER);
            Ok(Some(Msg::Frame(Frame {
                pts: back(pts),
                dts: back(dts),
                duration: back(duration),
                sent_host: back(sent),
                host_minus_qpc: (hmq != i64::MIN).then_some(hmq),
                delta: flags & 1 != 0,
                discont: flags & 2 != 0,
                data,
            })))
        }
        KIND_JSON => serde_json::from_slice(&body).map(|m| Some(Msg::Json(m))).map_err(std::io::Error::other),
        k => Err(std::io::Error::other(format!("unknown message kind {k}"))),
    }
}

/// A clock both processes share: the performance counter, in ns.
#[cfg(windows)]
pub fn qpc_ns() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceCounter(c: *mut i64) -> i32;
        fn QueryPerformanceFrequency(f: *mut i64) -> i32;
    }
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let f = *FREQ.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut c = 0;
    unsafe { QueryPerformanceCounter(&mut c) };
    (c as i128 * 1_000_000_000 / f as i128) as u64
}

#[cfg(not(windows))]
pub fn qpc_ns() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: usize, delta: bool) -> Frame {
        Frame {
            pts: Some(n as u64 * 16_666_667),
            dts: None,
            duration: Some(16_666_667),
            delta,
            discont: n == 0,
            sent_host: Some(5_000_000_000 + n as u64),
            host_minus_qpc: Some(-1_234_567),
            data: (0..n * 7 % 5000 + 1).map(|i| (i * 31 + n) as u8).collect(),
        }
    }

    #[test]
    fn frames_and_json_come_back_as_sent() {
        let mut buf = Vec::new();
        let frames: Vec<Frame> = (0..20).map(|n| frame(n, n % 5 != 0)).collect();
        let cmds = vec![
            ToVideo::Start { parent: 123456, title: "Стрім — ü ✓".into() },
            ToVideo::Stream { caps: "video/x-av1, stream-format=(string)obu-stream".into(), decoder: "d3d11av1dec".into() },
            ToVideo::Place { rect: Some((1, 2, 300, 200)) },
            ToVideo::Audio { muted: true, volume: 0.5 },
            ToVideo::Close,
        ];
        for (i, f) in frames.iter().enumerate() {
            write_frame(&mut buf, f).unwrap();
            if let Some(c) = cmds.get(i) {
                write_json(&mut buf, c).unwrap();
            }
        }
        let mut r = buf.as_slice();
        let (mut got_f, mut got_c) = (vec![], vec![]);
        while let Some(m) = read::<ToVideo>(&mut r).unwrap() {
            match m {
                Msg::Frame(f) => got_f.push(f),
                Msg::Json(c) => got_c.push(c),
            }
        }
        assert_eq!(got_f, frames);
        assert_eq!(got_c, cmds);
        let rep = FromVideo::Report { state: WindowStateMsg { embedded: true, alive: true, ..Default::default() }, closed: false, frames: 3, intervals: vec![1, 2], latencies: vec![9], presented: 3, skipped: 0, max_present_us: 10, max_lock_us: 2, size: Some((1920, 1080)), loops: 77 };
        let mut b = Vec::new();
        write_json(&mut b, &rep).unwrap();
        assert!(matches!(read::<FromVideo>(&mut b.as_slice()).unwrap(), Some(Msg::Json(x)) if x == rep));
    }

    #[test]
    fn a_closed_pipe_ends_the_stream_and_garbage_is_an_error() {
        assert!(read::<ToVideo>(&mut [].as_slice()).unwrap().is_none());
        assert!(read::<ToVideo>(&mut [9u8, 0, 0, 0, 0].as_slice()).is_err());
        assert!(read::<ToVideo>(&mut [1u8, 255, 255, 255, 255].as_slice()).is_err());
    }

    /// Through a real OS pipe: 50 Mbit/s of 60 fps frames (a keyframe of 400 KB every 2 s),
    /// each one's delay from written to read. The link must add well under a frame.
    #[test]
    fn a_pipe_carries_50_mbit_with_little_delay() {
        let (mut r, mut w) = std::io::pipe().unwrap();
        let n = 600;
        let writer = std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            for i in 0..n {
                let size = if i % 120 == 0 { 400_000 } else { 100_000 };
                let mut f = frame(i, i % 120 != 0);
                f.data = vec![i as u8; size];
                f.sent_host = Some(qpc_ns());
                write_frame(&mut w, &f).unwrap();
                // 60 fps.
                let due = t0 + std::time::Duration::from_micros(16_667 * (i as u64 + 1));
                std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
            }
        });
        let mut delays = Vec::new();
        let mut bytes = 0usize;
        while let Some(Msg::Frame(f)) = read::<ToVideo>(&mut r).unwrap() {
            delays.push(qpc_ns() - f.sent_host.unwrap());
            bytes += f.data.len();
            if delays.len() == n {
                break;
            }
        }
        writer.join().unwrap();
        delays.sort();
        let (med, p99) = (delays[n / 2] as f64 / 1e6, delays[n * 99 / 100] as f64 / 1e6);
        eprintln!("link: {n} frames, {:.1} Mbit/s, delay median {med:.3} ms, p99 {p99:.3} ms, max {:.3} ms", bytes as f64 * 8.0 / (n as f64 / 60.0) / 1e6, *delays.last().unwrap() as f64 / 1e6);
        assert_eq!(delays.len(), n);
        assert!(med < 1.0, "median {med} ms");
        assert!(p99 < 5.0, "p99 {p99} ms");
    }
}
