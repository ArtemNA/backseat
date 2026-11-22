//! The video of a watched stream in a process of its own (Windows): the hardware decoder and
//! the video window (`crate::present`). A fault there (2026-11-22: a divide by zero inside
//! the NVIDIA driver on the first frames of an AV1 stream took the whole app down) ends only
//! that process; the app keeps the room, voice and the stream's sound, and starts the video
//! again.
//!
//! The app side (`Remote`) keeps WebRTC, the depayloader and the parser, and sends compressed
//! frames and window commands over `crate::videolink`; the video process (`run_if_asked`, the
//! same executable with `--video-process`) decodes, shows and reports back.

use crate::present::{AudioHandler, RenderStats, WindowState};
use crate::videolink::{self, FromVideo, Msg, ToVideo, WindowStateMsg};
use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// The command-line switch that makes the executable a video process.
pub const SWITCH: &str = "--video-process";
/// More crashes than this within `CRASH_WINDOW` and the video gives up.
const MAX_RESTARTS: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// The video window's thread stands still for this long: the process hangs. Short, as a hung
/// window thread may hold up the app's own input (Windows joins the input of a child window
/// and its parent); a GPU stalled this long is reset by Windows anyway (TDR, 2 s). The window
/// thread shows signs of life at least every 500 ms, also while its window is dragged.
/// STREAMER_DEBUG_VP_HANG_SECS: another limit (the spoiled variant: 5, as before).
fn hang_limit() -> Duration {
    static D: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *D.get_or_init(|| Duration::from_secs_f64(std::env::var("STREAMER_DEBUG_VP_HANG_SECS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(2.0)))
}
/// Frames waiting for the process; more and frames are dropped until the next keyframe.
const QUEUE: usize = 120;

/// The video window of a watched stream: in this process (`Local`, the command-line tools'
/// old way and `STREAMER_VIDEO_IN_PROCESS`) or in a video process.
#[derive(Clone)]
pub enum VideoWindow {
    Local(crate::present::Window),
    Remote(Arc<Remote>),
}

impl VideoWindow {
    pub fn show(&self) {
        match self {
            Self::Local(w) => w.show(),
            Self::Remote(r) => r.command_foreground(ToVideo::Show),
        }
    }
    pub fn set_fullscreen(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_fullscreen(on),
            Self::Remote(r) => r.command_foreground(ToVideo::Fullscreen { on }),
        }
    }
    pub fn toggle_fullscreen(&self) {
        match self {
            Self::Local(w) => w.toggle_fullscreen(),
            Self::Remote(r) => r.command_foreground(ToVideo::ToggleFullscreen),
        }
    }
    pub fn set_topmost(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_topmost(on),
            Self::Remote(r) => r.command(ToVideo::Topmost { on }),
        }
    }
    pub fn close(&self) {
        match self {
            Self::Local(w) => w.close(),
            Self::Remote(r) => r.close(),
        }
    }
    pub fn place(&self, rect: Option<(i32, i32, i32, i32)>) {
        match self {
            Self::Local(w) => w.place(rect),
            Self::Remote(r) => r.command(ToVideo::Place { rect }),
        }
    }
    pub fn set_popped(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_popped(on),
            Self::Remote(r) => r.command_foreground(ToVideo::Popped { on }),
        }
    }
    pub fn set_hidden(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_hidden(on),
            Self::Remote(r) => r.command(ToVideo::Hidden { on }),
        }
    }
    pub fn place_again(&self) {
        match self {
            Self::Local(w) => w.place_again(),
            Self::Remote(r) => r.command(ToVideo::PlaceAgain),
        }
    }
    pub fn set_audio_handler(&self, handler: AudioHandler) {
        match self {
            Self::Local(w) => w.set_audio_handler(handler),
            Self::Remote(r) => r.set_audio_handler(handler),
        }
    }
    pub fn alive(&self) -> bool {
        match self {
            Self::Local(w) => w.alive(),
            Self::Remote(r) => r.state.lock().unwrap().alive,
        }
    }
    pub fn set_title(&self, title: &str) {
        match self {
            Self::Local(w) => w.set_title(title),
            Self::Remote(r) => r.command(ToVideo::Title { text: title.into() }),
        }
    }
    pub fn state(&self) -> WindowState {
        match self {
            Self::Local(w) => w.state(),
            Self::Remote(r) => {
                let s = r.state.lock().unwrap();
                WindowState { fullscreen: s.fullscreen, topmost: s.topmost, overlay: s.overlay, badge: s.badge, embedded: s.embedded }
            }
        }
    }
    pub fn set_overlay(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_overlay(on),
            Self::Remote(_) => set_all(|d| d.overlay = on),
        }
    }
    pub fn set_badge(&self, on: bool) {
        match self {
            Self::Local(w) => w.set_badge(on),
            Self::Remote(_) => set_all(|d| d.badge = on),
        }
    }
    pub fn set_badge_info(&self, text: &str, ok: bool) {
        match self {
            Self::Local(w) => w.set_badge_info(text, ok),
            Self::Remote(r) => r.command_if_changed(ToVideo::BadgeInfo { text: text.into(), ok }),
        }
    }
    pub fn set_status(&self, text: &str) {
        match self {
            Self::Local(w) => w.set_status(text),
            Self::Remote(r) => r.command_if_changed(ToVideo::Status { text: text.into() }),
        }
    }
    pub fn set_overlay_text(&self, text: &str) {
        match self {
            Self::Local(w) => w.set_overlay_text(text),
            Self::Remote(r) => r.command_if_changed(ToVideo::OverlayText { text: text.into() }),
        }
    }
    /// The stream's sound as the app now has it, for the video window's own controls.
    pub fn sync_audio(&self, muted: bool, volume: f32) {
        if let Self::Remote(r) = self {
            r.command(ToVideo::Audio { muted, volume });
        }
    }
}

/// The video goes to a process of its own unless STREAMER_VIDEO_IN_PROCESS is set (A/B and
/// a way back).
pub fn use_process() -> bool {
    std::env::var_os("STREAMER_VIDEO_IN_PROCESS").is_none()
}

// ---------------------------------------------------------------------------------------
// The app's side.

/// What the app wants the window to be: sent again to a new process after a crash.
#[derive(Clone, Default)]
struct Desired {
    parent: isize,
    title: String,
    stream: Option<(String, String)>,
    place: Option<(i32, i32, i32, i32)>,
    hidden: bool,
    popped: bool,
    topmost: bool,
    fullscreen: bool,
    overlay: bool,
    badge: bool,
    badge_info: Option<(String, bool)>,
    status: String,
    overlay_text: String,
    audio: (bool, f32),
}

/// One overlay and one badge state for all video windows, as `present` keeps in one process:
/// F3 in one window switches every window, and a new one opens with it.
static ALL: Mutex<Vec<Weak<Remote>>> = Mutex::new(Vec::new());
static OVERLAY: AtomicBool = AtomicBool::new(false);
// On by default, as in `present` (review 2026-11-22: it came up off in a video process).
static BADGE: AtomicBool = AtomicBool::new(true);

fn set_all(f: impl Fn(&mut Desired)) {
    let all: Vec<Arc<Remote>> = ALL.lock().unwrap().iter().filter_map(Weak::upgrade).collect();
    let probe = &mut Desired { overlay: OVERLAY.load(Ordering::SeqCst), badge: BADGE.load(Ordering::SeqCst), ..Default::default() };
    f(probe);
    OVERLAY.store(probe.overlay, Ordering::SeqCst);
    BADGE.store(probe.badge, Ordering::SeqCst);
    for r in all {
        r.command(ToVideo::Overlay { on: probe.overlay });
        r.command(ToVideo::Badge { on: probe.badge });
    }
}

enum Out {
    Frame(videolink::Frame),
    Json(ToVideo),
}

struct Child {
    proc: std::process::Child,
    out: mpsc::Sender<Out>,
    /// Frames sent and not yet written to the pipe.
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// Its first messages (Start, the window's state, the stream) are sent: frames before that
    /// would come before its decoder exists and be lost (third review).
    ready: Arc<AtomicBool>,
    /// A frame was dropped (queue full, or a new process): frames wait for a keyframe.
    want_key: Arc<AtomicBool>,
    started: Instant,
}

pub struct Remote {
    desired: Mutex<Desired>,
    child: Mutex<Option<Child>>,
    state: Mutex<WindowStateMsg>,
    /// Closed by its user, or the app ended it.
    pub closed: Arc<AtomicBool>,
    /// Crashed too often: the video gave up.
    pub failed: Arc<AtomicBool>,
    ending: AtomicBool,
    pub render: Arc<RenderStats>,
    audio: Mutex<Option<AudioHandler>>,
    /// Decoder statistics land here (the viewer's own).
    sink: StatsSink,
    /// Asks the streamer for a keyframe (a new process, a dropped frame, the decoder's ask).
    keyframe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    crashes: Mutex<VecDeque<Instant>>,
    health: Mutex<Health>,
    /// When the app last sent a window command: reports older than that may not undo it.
    last_command: Mutex<Instant>,
    /// Video processes started for this watch so far.
    runs: AtomicU64,
    /// The app's main window the video sits in (0: a window of its own).
    parent: isize,
    me: Weak<Remote>,
}

/// What the watchdog goes by, for the current process.
struct Health {
    started: Instant,
    /// A report came at all (until then the start may take `START_GRACE`).
    reported: bool,
    /// The presentation loop's turns, and when they last moved.
    loops: u64,
    loops_moved: Instant,
    /// Frames sent to it and decoded by it, last seen.
    sent: Option<Instant>,
    decoded: Instant,
    /// The last keyframe ask while frames wait for one.
    key_asked: Instant,
    /// Since when frames wait for a keyframe.
    key_wait: Option<Instant>,
    /// When it last got a keyframe: only from then on can its decoder be expected to give
    /// pictures (without one it rightly gives none).
    key_sent: Option<Instant>,
    /// The first keyframe sent since its decoder last gave a picture: a decoder that got one
    /// and still gives nothing hangs; one that got none may only lack it (fourth review).
    key_since_picture: Option<Instant>,
    /// Since when frames go to it without a gap of 1 s or more: a pause of the stream is not
    /// the decoder's fault (third review: frames back after 6 s got a healthy process ended).
    streak: Option<Instant>,
    /// The process these figures are about.
    pid: u32,
}

impl Health {
    fn new() -> Self {
        let now = Instant::now();
        Health { started: now, reported: false, loops: 0, loops_moved: now, sent: None, decoded: now, key_asked: now, key_wait: None, key_sent: None, key_since_picture: None, streak: None, pid: 0 }
    }
}

/// A process may take this long to send its first report (GStreamer's plugin registry is
/// rebuilt after an update, the GPU device comes up).
const START_GRACE: Duration = Duration::from_secs(20);
/// Frames go in, none come out of the decoder for this long: it hangs.
const DECODE_HANG: Duration = Duration::from_secs(5);

/// Where the video process's decoder statistics go: the viewer's own series and counters.
#[derive(Clone)]
pub struct StatsSink {
    pub frames: Arc<AtomicU64>,
    pub frame_interval: crate::stats::Series,
    pub video_latency: crate::stats::Series,
    pub video_size: Arc<Mutex<Option<(u32, u32)>>>,
}

impl Remote {
    /// Starts a video process with its window (loader until frames come).
    pub fn start(parent: isize, title: &str, sink: StatsSink) -> Arc<Remote> {
        let r = Arc::new_cyclic(|me| Remote {
            desired: Mutex::new(Desired { parent, title: title.into(), overlay: OVERLAY.load(Ordering::SeqCst), badge: BADGE.load(Ordering::SeqCst), audio: (false, 1.0), ..Default::default() }),
            child: Mutex::new(None),
            state: Mutex::new(WindowStateMsg { alive: true, embedded: parent != 0, ..Default::default() }),
            closed: Arc::default(),
            failed: Arc::default(),
            ending: AtomicBool::new(false),
            render: Arc::default(),
            audio: Mutex::new(None),
            sink,
            keyframe: Mutex::new(None),
            crashes: Mutex::new(VecDeque::new()),
            health: Mutex::new(Health::new()),
            last_command: Mutex::new(Instant::now()),
            runs: AtomicU64::new(0),
            parent,
            me: me.clone(),
        });
        ALL.lock().unwrap().retain(|w| w.strong_count() > 0);
        ALL.lock().unwrap().push(Arc::downgrade(&r));
        r.spawn();
        r.watchdog();
        r
    }

    pub fn set_keyframe_request(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.keyframe.lock().unwrap() = Some(f);
    }

    fn ask_keyframe(&self) {
        let f = self.keyframe.lock().unwrap().clone();
        if let Some(f) = f {
            f();
        }
    }

    /// The compressed stream's caps and decoder; the process builds its decoder from them.
    pub fn set_stream(&self, caps: &str, decoder: &str) {
        let mut d = self.desired.lock().unwrap();
        d.stream = Some((caps.into(), decoder.into()));
        self.send(Out::Json(ToVideo::Stream { caps: caps.into(), decoder: decoder.into() }));
        drop(d);
    }

    /// A compressed frame for the decoder. Never blocks: a full queue drops frames until the
    /// next keyframe, which is asked for.
    pub fn push(&self, f: videolink::Frame) {
        let (out, queued, want_key) = {
            let guard = self.child.lock().unwrap();
            let Some(c) = guard.as_ref() else { return };
            if !c.ready.load(Ordering::SeqCst) {
                return;
            }
            (c.out.clone(), c.queued.clone(), c.want_key.clone())
        };
        // Its queue full: it does not take frames. They count as sent, so a process that
        // stopped reading is caught by the decode watch; frames then wait for a keyframe.
        if queued.load(Ordering::SeqCst) >= QUEUE {
            want_key.store(true, Ordering::SeqCst);
            let mut h = self.health.lock().unwrap();
            let now = Instant::now();
            if h.sent.is_none_or(|s| s.elapsed() > Duration::from_secs(1)) {
                h.streak = Some(now);
            }
            h.sent = Some(now);
            // The keyframe wait counts from when frames get through again, not from now
            // (third review: after a long stall the first delta frame went on at once).
            h.key_wait = None;
            return;
        }
        if want_key.load(Ordering::SeqCst) {
            // Not sent while it waits for a keyframe: the streamer sends one every 2 s and
            // answers no viewer's ask (one viewer's trouble stays its own), so the wait can
            // be long and the process is healthy meanwhile (second review 2026-11-22).
            // A keyframe that never shows (an AV1 encoder without a sequence header on
            // keyframes) does not hold the picture for good: after 3 s frames go on.
            let waited = self.health.lock().unwrap().key_wait.get_or_insert_with(Instant::now).elapsed();
            if f.delta && waited < Duration::from_secs(3) {
                return;
            }
            if f.delta {
                eprintln!("video process: no keyframe seen in 3 s; frames go on without one");
            }
            want_key.store(false, Ordering::SeqCst);
            self.health.lock().unwrap().key_wait = None;
        }
        let key = !f.delta;
        queued.fetch_add(1, Ordering::SeqCst);
        if out.send(Out::Frame(f)).is_err() {
            queued.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let mut h = self.health.lock().unwrap();
        let now = Instant::now();
        if h.sent.is_none_or(|s| s.elapsed() > Duration::from_secs(1)) {
            h.streak = Some(now);
        }
        h.sent = Some(now);
        if key {
            // The first keyframe of this process: from then on its decoder owes pictures. (Each
            // keyframe set it anew every 2 s, so the watch never came due, third review.)
            // STREAMER_DEBUG_VP_KEY_EACH: set anew by each keyframe (the spoiled variant).
            if std::env::var_os("STREAMER_DEBUG_VP_KEY_EACH").is_some() {
                h.key_sent = Some(Instant::now());
            }
            h.key_sent.get_or_insert_with(Instant::now);
            h.key_since_picture.get_or_insert_with(Instant::now);
            if std::env::var_os("STREAMER_DEBUG_VP_KEY_EACH").is_some() {
                h.key_since_picture = Some(Instant::now());
            }
        }
    }

    fn command(&self, m: ToVideo) {
        // Only commands that set what a report also carries hold reports off (the sampler's
        // badge and overlay lines come every tick).
        if matches!(m, ToVideo::Fullscreen { .. } | ToVideo::ToggleFullscreen | ToVideo::Popped { .. } | ToVideo::Overlay { .. } | ToVideo::Badge { .. }) {
            *self.last_command.lock().unwrap() = Instant::now();
        }
        {
            let mut d = self.desired.lock().unwrap();
            match &m {
                ToVideo::Place { rect } => d.place = *rect,
                ToVideo::Hidden { on } => d.hidden = *on,
                ToVideo::Popped { on } => d.popped = *on,
                ToVideo::Topmost { on } => d.topmost = *on,
                ToVideo::Fullscreen { on } => d.fullscreen = *on,
                ToVideo::Overlay { on } => d.overlay = *on,
                ToVideo::Badge { on } => d.badge = *on,
                ToVideo::Title { text } => d.title = text.clone(),
                ToVideo::BadgeInfo { text, ok } => d.badge_info = Some((text.clone(), *ok)),
                ToVideo::Status { text } => d.status = text.clone(),
                ToVideo::OverlayText { text } => d.overlay_text = text.clone(),
                ToVideo::Audio { muted, volume } => d.audio = (*muted, *volume),
                _ => {}
            }
            // Sent under the same lock as a new process's snapshot is taken and sent: a
            // command is either in that snapshot or comes after it (second review).
            self.send(Out::Json(m));
        }
    }

    /// Sent only when it differs from the last one (the sampler repeats them every tick).
    fn command_if_changed(&self, m: ToVideo) {
        let same = {
            let d = self.desired.lock().unwrap();
            match &m {
                ToVideo::BadgeInfo { text, ok } => d.badge_info.as_ref() == Some(&(text.clone(), *ok)),
                ToVideo::Status { text } => &d.status == text,
                ToVideo::OverlayText { text } => &d.overlay_text == text,
                _ => false,
            }
        };
        if !same {
            self.command(m);
        }
    }

    /// A command that brings the window to the front: the video process may take the
    /// foreground (it is not the active process, so Windows would refuse it otherwise).
    fn command_foreground(&self, m: ToVideo) {
        if let Some(pid) = self.child.lock().unwrap().as_ref().map(|c| c.proc.id()) {
            allow_foreground(pid);
        }
        self.command(m);
    }

    /// Never blocks, and never holds `child` while sending: a video process that stopped
    /// reading must not stall the app, nor keep the watchdog from ending it (review
    /// 2026-11-22). Commands are few; frames are held to `QUEUE` by `push`.
    fn send(&self, m: Out) {
        let out = self.child.lock().unwrap().as_ref().map(|c| c.out.clone());
        if let Some(out) = out {
            let _ = out.send(m);
        }
    }

    fn set_audio_handler(&self, h: AudioHandler) {
        let now = h(None, None);
        *self.audio.lock().unwrap() = Some(h);
        self.command(ToVideo::Audio { muted: now.0, volume: now.1 });
    }

    /// Ends the video for good: the window closes and the process exits.
    pub fn close(&self) {
        if self.ending.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        let child = self.child.lock().unwrap().take();
        if let Some(mut c) = child {
            // Unbounded: never waits, even when the process stopped reading.
            let _ = c.out.send(Out::Json(ToVideo::Close));
            drop(c.out);
            std::thread::spawn(move || {
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(2) {
                    if let Ok(Some(_)) = c.proc.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                eprintln!("video process: did not exit; ended");
                let _ = c.proc.kill();
                let _ = c.proc.wait();
            });
        }
        self.state.lock().unwrap().alive = false;
    }

    fn spawn(&self) {
        if self.ending.load(Ordering::SeqCst) {
            return;
        }
        let exe = std::env::current_exe().expect("own executable");
        let mut cmd = std::process::Command::new(exe);
        cmd.env("STREAMER_VIDEO_RUN", self.runs.fetch_add(1, Ordering::SeqCst).to_string());
        cmd.arg(SWITCH).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console of its own.
            cmd.creation_flags(0x0800_0000);
        }
        let mut proc = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("video process: cannot start: {e}");
                self.failed.store(true, Ordering::SeqCst);
                return;
            }
        };
        #[cfg(windows)]
        end_with_app(&proc);
        let (stdin, stdout) = (proc.stdin.take().unwrap(), proc.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel::<Out>();
        let want_key = Arc::new(AtomicBool::new(true));
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        // Writer: everything for the process, in order.
        let q = queued.clone();
        std::thread::Builder::new()
            .name("video-out".into())
            .spawn(move || {
                let mut w = std::io::BufWriter::with_capacity(1 << 20, stdin);
                for m in rx {
                    let r = match m {
                        Out::Frame(f) => {
                            q.fetch_sub(1, Ordering::SeqCst);
                            videolink::write_frame(&mut w, &f)
                        }
                        Out::Json(j) => videolink::write_json(&mut w, &j),
                    };
                    if r.is_err() {
                        return;
                    }
                }
            })
            .unwrap();
        let pid = proc.id();
        eprintln!("video process: started (pid {pid})");
        // In place first, so a command from now on goes to it (or to the snapshot below),
        // and a close that came meanwhile ends it at once.
        *self.health.lock().unwrap() = Health { pid, ..Health::new() };
        {
            let mut c = self.child.lock().unwrap();
            if self.ending.load(Ordering::SeqCst) {
                let _ = proc.kill();
                let _ = proc.wait();
                return;
            }
            *c = Some(Child { proc, out: tx.clone(), queued, want_key, ready: ready.clone(), started: Instant::now() });
        }
        // What it should show, from the start: the window, then everything set on it so far.
        // Held until it is sent: a command sent meanwhile waits, and comes after it.
        let held = self.desired.lock().unwrap();
        let d = held.clone();
        // The window's state before the stream: building the decoder takes a while, and the
        // window should have its overlay and badge meanwhile.
        let mut first = vec![ToVideo::Start { parent: d.parent, title: d.title.clone() }];
        first.push(ToVideo::Audio { muted: d.audio.0, volume: d.audio.1 });
        first.push(ToVideo::Overlay { on: d.overlay });
        first.push(ToVideo::Badge { on: d.badge });
        first.push(ToVideo::Hidden { on: d.hidden });
        first.push(ToVideo::Topmost { on: d.topmost });
        if let Some((text, ok)) = d.badge_info {
            first.push(ToVideo::BadgeInfo { text, ok });
        }
        if !d.status.is_empty() {
            first.push(ToVideo::Status { text: d.status });
        }
        if !d.overlay_text.is_empty() {
            first.push(ToVideo::OverlayText { text: d.overlay_text });
        }
        first.push(ToVideo::Place { rect: d.place });
        if d.popped {
            allow_foreground(pid);
            first.push(ToVideo::Popped { on: true });
        }
        if d.fullscreen {
            allow_foreground(pid);
            first.push(ToVideo::Fullscreen { on: true });
        }
        // The stream last: building its decoder takes a while, and the window should be where
        // and how it was meanwhile (third review: a slow build let reports undo it).
        if let Some((caps, decoder)) = &d.stream {
            first.push(ToVideo::Stream { caps: caps.clone(), decoder: decoder.clone() });
        }
        // Commands sent since it went in place came before these; it ignores all until Start,
        // and these carry their values.
        for m in first {
            let _ = tx.send(Out::Json(m));
        }
        ready.store(true, Ordering::SeqCst);
        // Its first reports may not undo what was just sent (a slow start, third review).
        *self.last_command.lock().unwrap() = Instant::now();
        drop(held);
        self.ask_keyframe();
        // Reader: reports, sound and keyframe asks; its end is the process's end.
        let me = self.me.clone();
        std::thread::Builder::new()
            .name("video-in".into())
            .spawn(move || {
                let mut r = std::io::BufReader::new(stdout);
                while let Ok(Some(m)) = videolink::read::<FromVideo>(&mut r) {
                    let Some(me) = me.upgrade() else { return };
                    if let Msg::Json(m) = m {
                        // A fault here must not leave the video dead without a restart: the
                        // link is dropped and the process ended as for any other end.
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.on_message(m))).is_err() {
                            eprintln!("video process: a report could not be handled; it is ended");
                            break;
                        }
                    }
                }
                if let Some(me) = me.upgrade() {
                    me.ended(pid);
                }
            })
            .unwrap();
    }

    fn on_message(&self, m: FromVideo) {
        match m {
            FromVideo::Report { state, closed, frames, intervals, latencies, presented, skipped, max_present_us, max_lock_us, size, loops } => {
                {
                    let mut h = self.health.lock().unwrap();
                    // Its first report: the decoder is only now built (or being built), so its
                    // silence counts from here (fourth review: a slow start was ended at once).
                    if !h.reported {
                        h.decoded = Instant::now();
                        h.streak = None;
                    }
                    h.reported = true;
                    // The loop counter stands still: the presentation thread hangs.
                    if loops != h.loops {
                        h.loops = loops;
                        h.loops_moved = Instant::now();
                    }
                    if frames > 0 {
                        h.decoded = Instant::now();
                        h.key_since_picture = None;
                    }
                }
                // A new process's first reports show its own defaults until the state sent to
                // it is applied (its decoder is built first): they may not undo that state
                // (second review 2026-11-22: a restart turned the overlay off everywhere).
                let settled = self.health.lock().unwrap().started.elapsed() > Duration::from_secs(3);
                // F3 inside this window: every other window follows, as in one process.
                let (o, b) = (OVERLAY.load(Ordering::SeqCst), BADGE.load(Ordering::SeqCst));
                if settled && (state.overlay != o || state.badge != b) {
                    let mine = self.state.lock().unwrap().clone();
                    if state.overlay != mine.overlay || state.badge != mine.badge {
                        let (ov, ba) = (state.overlay, state.badge);
                        std::thread::spawn(move || set_all(|d| {
                            d.overlay = ov;
                            d.badge = ba;
                        }));
                    }
                }
                // What the window became by itself (F11, Esc, F3, closing a popped window), to
                // be set again after a restart; not from a report older than the app's last
                // command, which it may not have carried out yet.
                if settled && self.last_command.lock().unwrap().elapsed() > Duration::from_secs(1) {
                    let mut d = self.desired.lock().unwrap();
                    d.fullscreen = state.fullscreen;
                    d.popped = !state.embedded && d.parent != 0;
                    d.overlay = state.overlay;
                    d.badge = state.badge;
                }
                *self.state.lock().unwrap() = WindowStateMsg { alive: state.alive && !self.ending.load(Ordering::SeqCst), ..state };
                if closed {
                    self.closed.store(true, Ordering::SeqCst);
                }
                self.sink.frames.fetch_add(frames, Ordering::Relaxed);
                for i in intervals {
                    self.sink.frame_interval.push(i);
                }
                for l in latencies {
                    self.sink.video_latency.push(l);
                }
                if size.is_some() {
                    *self.sink.video_size.lock().unwrap() = size;
                }
                // STREAMER_DEBUG_VP_REPORT: each second, frames shown and where the window is.
                if std::env::var_os("STREAMER_DEBUG_VP_REPORT").is_some() {
                    let n = self.render.presented.load(Ordering::Relaxed) + presented;
                    static LAST: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
                    let mut l = LAST.lock().unwrap();
                    match *l {
                        Some((t, k)) if t.elapsed() >= Duration::from_secs(1) => {
                            let st = self.state.lock().unwrap().clone();
                            eprintln!("video process: shown {:.1} fps, embedded {}, fullscreen {}, size {:?}", (n - k) as f64 / t.elapsed().as_secs_f64(), st.embedded, st.fullscreen, *self.sink.video_size.lock().unwrap());
                            *l = Some((Instant::now(), n));
                        }
                        None => *l = Some((Instant::now(), n)),
                        _ => {}
                    }
                }
                self.render.presented.fetch_add(presented, Ordering::Relaxed);
                self.render.skipped.fetch_add(skipped, Ordering::Relaxed);
                self.render.max_present_us.fetch_max(max_present_us, Ordering::Relaxed);
                self.render.max_lock_us.fetch_max(max_lock_us, Ordering::Relaxed);
            }
            FromVideo::Audio { muted, volume } => {
                // The process already shows the new value; no echo back (an older echo would
                // move the bar back while it is dragged).
                let h = self.audio.lock().unwrap().clone();
                if let Some(h) = h {
                    let (m, v) = h(muted, volume);
                    self.desired.lock().unwrap().audio = (m, v);
                }
            }
            FromVideo::Keyframe => self.ask_keyframe(),
        }
    }

    /// The process is gone: by our close, or by a crash or hang, after which a new one starts
    /// (at most `MAX_RESTARTS` within `CRASH_WINDOW`).
    fn ended(&self, pid: u32) {
        let child = {
            let mut c = self.child.lock().unwrap();
            if c.as_ref().map(|c| c.proc.id()) != Some(pid) {
                return;
            }
            c.take()
        };
        let Some(mut c) = child else { return };
        // The link broke (or carried garbage) while the process may live on: end it, or the
        // wait below would hang with it.
        if !matches!(c.proc.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(200));
            if !matches!(c.proc.try_wait(), Ok(Some(_))) {
                let _ = c.proc.kill();
            }
        }
        let code = c.proc.wait().ok().and_then(|s| s.code());
        if self.ending.load(Ordering::SeqCst) {
            return;
        }
        let lived = c.started.elapsed();
        eprintln!("video process: ended (pid {pid}, code {}) after {:.1} s; the video starts again", code.map(|c| format!("{:#010x}", c as u32)).unwrap_or("?".into()), lived.as_secs_f64());
        let (too_many, recent) = {
            let mut cr = self.crashes.lock().unwrap();
            cr.push_back(Instant::now());
            while cr.front().is_some_and(|t| t.elapsed() > CRASH_WINDOW) {
                cr.pop_front();
            }
            (cr.len() > MAX_RESTARTS, cr.len())
        };
        // STREAMER_DEBUG_NO_RESTART: the video stays gone (the spoiled variant).
        if too_many || std::env::var_os("STREAMER_DEBUG_NO_RESTART").is_some() {
            eprintln!("video process: crashed {} times within a minute; the video stops", MAX_RESTARTS + 1);
            self.failed.store(true, Ordering::SeqCst);
            self.state.lock().unwrap().alive = false;
            return;
        }
        // At once the first time, then after 2 s and 4 s: a GPU reset of a few seconds does not
        // use up all the restarts and end the watch (second review 2026-11-22).
        let wait = Duration::from_secs(match recent { 0 | 1 => 0, 2 => 2, _ => 4 });
        if !wait.is_zero() {
            eprintln!("video process: starting again in {} s", wait.as_secs());
        }
        // Not when the watch ended or its window was closed meanwhile (third review).
        let t = Instant::now();
        loop {
            if self.ending.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
                return;
            }
            if t.elapsed() >= wait {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.spawn();
    }

    /// Kills a process that stopped reporting or whose presentation loop stopped turning.
    fn watchdog(&self) {
        let me = self.me.clone();
        std::thread::Builder::new()
            .name("video-watchdog".into())
            .spawn(move || {
                let mut last = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let Some(me) = me.upgrade() else { return };
                    if me.ending.load(Ordering::SeqCst) || me.failed.load(Ordering::SeqCst) {
                        return;
                    }
                    // Overslept (the computer slept, or this thread was starved): nobody turned
                    // meanwhile, so nothing stood still by its own fault; count from now. Measured
                    // from the end of the last turn, so a slow turn of its own does not count.
                    if last.elapsed() > Duration::from_millis(1500) {
                        let mut h = me.health.lock().unwrap();
                        let now = Instant::now();
                        h.loops_moved = now;
                        h.decoded = now;
                        if !h.reported {
                            h.started = now;
                        }
                        drop(h);
                        last = Instant::now();
                        continue;
                    }
                    let decode_watch = std::env::var_os("STREAMER_DEBUG_NO_DECODE_WATCH").is_none();
                    let (why, stood_still, ask, pid) = {
                        let mut h = me.health.lock().unwrap();
                        let stood_still = h.reported && h.loops_moved.elapsed() > hang_limit();
                        let why = if !h.reported {
                            (h.started.elapsed() > START_GRACE).then(|| format!("no report {:.1} s after the start", h.started.elapsed().as_secs_f64()))
                        } else if stood_still {
                            Some(format!("its window stood still for {:.1} s", h.loops_moved.elapsed().as_secs_f64()))
                        } else if decode_watch && h.sent.is_some_and(|s| s.elapsed() < Duration::from_secs(1)) && h.decoded.elapsed() > DECODE_HANG && (h.streak.is_some_and(|s| s.elapsed() > DECODE_HANG) || std::env::var_os("STREAMER_DEBUG_VP_NO_STREAK").is_some()) && (h.key_since_picture.is_some_and(|k| k.elapsed() > Duration::from_secs(3)) || (std::env::var_os("STREAMER_DEBUG_VP_WATCH_WITHOUT_KEY").is_some() && h.started.elapsed() > DECODE_HANG)) {
                            Some(format!("frames went in, none decoded for {:.1} s", h.decoded.elapsed().as_secs_f64()))
                        } else {
                            None
                        };
                        // Frames wait for a keyframe: ask again each second (an ask can get lost).
                        let waiting = me.child.lock().unwrap().as_ref().is_some_and(|c| c.want_key.load(Ordering::SeqCst));
                        let ask = waiting && h.key_asked.elapsed() > Duration::from_secs(1);
                        if ask {
                            h.key_asked = Instant::now();
                        }
                        (why, stood_still, ask, h.pid)
                    };
                    if ask {
                        me.ask_keyframe();
                    }
                    // Its window closed by its user: the watch ends; nothing to restart.
                    let mut why = why.filter(|_| !me.closed.load(Ordering::SeqCst));
                    // Its window waits on the app's own window when that is busy (a child window
                    // of another process's window): not its fault. Asked with no lock held, so
                    // the app's thread is never kept waiting by it (third review 2026-11-22).
                    // Only inside the app's window (a popped-out one has no parent to wait for),
                    // and never past 10 s: the app could as well be waiting for it (fourth review).
                    let embedded = me.state.lock().unwrap().embedded;
                    let long = me.health.lock().unwrap().loops_moved.elapsed() > Duration::from_secs(10);
                    if why.is_some() && stood_still && embedded && !long && parent_busy(me.parent) {
                        me.health.lock().unwrap().loops_moved = Instant::now();
                        why = None;
                    }
                    if let Some(why) = why {
                        // Only if it is still about that process (a new one may have its own by now).
                        {
                            let mut h = me.health.lock().unwrap();
                            if h.pid == pid {
                                *h = Health { pid, ..Health::new() };
                            }
                        }
                        let mut c = me.child.lock().unwrap();
                        // Only the process this was about (a new one may have started since).
                        if let Some(c) = c.as_mut().filter(|c| c.proc.id() == pid) {
                            eprintln!("video process: {why}; ended");
                            let _ = c.proc.kill();
                        }
                    }
                    last = Instant::now();
                }
            })
            .unwrap();
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.close();
    }
}

/// The app's own window (`hwnd`) does not answer within 300 ms: its thread is busy, and a
/// video window inside it may wait for it. 0 (no parent): never.
#[cfg(windows)]
fn parent_busy(hwnd: isize) -> bool {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG};
    if hwnd == 0 || !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as *mut std::ffi::c_void))) }.as_bool() {
        return false;
    }
    let mut r = 0usize;
    let ok = unsafe { SendMessageTimeoutW(HWND(hwnd as *mut std::ffi::c_void), 0, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 300, Some(&mut r)) };
    ok.0 == 0
}

#[cfg(not(windows))]
fn parent_busy(_hwnd: isize) -> bool {
    false
}

/// Puts a video process in a job that ends with the app: if the app dies (or is ended) while
/// its video process hangs, Windows ends that too (review 2026-11-22). The job's handle is
/// never closed by us; the app's end closes it.
#[cfg(windows)]
fn end_with_app(proc: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;
    static JOB: std::sync::OnceLock<Option<isize>> = std::sync::OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let j = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(j, JobObjectExtendedLimitInformation, &info as *const _ as *const std::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32).ok()?;
        Some(j.0 as isize)
    });
    match job {
        Some(j) => {
            if let Err(e) = unsafe { AssignProcessToJobObject(HANDLE(j as *mut std::ffi::c_void), HANDLE(proc.as_raw_handle())) } {
                eprintln!("video process: not tied to the app's life: {e}");
            }
        }
        None => eprintln!("video process: no job to tie it to the app's life"),
    }
}

#[cfg(windows)]
fn allow_foreground(pid: u32) {
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid) };
}

#[cfg(not(windows))]
fn allow_foreground(_pid: u32) {}

// ---------------------------------------------------------------------------------------
// The video process.

/// Becomes the video process and never returns when the command line asks for it; else
/// returns at once. Call first thing in `main`.
pub fn run_if_asked() {
    if !std::env::args().skip(1).any(|a| a == SWITCH) {
        return;
    }
    #[cfg(windows)]
    {
        let code = child_main();
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    std::process::exit(2);
}

#[cfg(windows)]
fn child_main() -> i32 {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    #[link(name = "kernel32")]
    extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    // A fault here ends this process quietly: no "stopped working" box (the app restarts it).
    unsafe { SetErrorMode(0x0001 | 0x0002) };
    // The link to the app is our stdout; anything else printed goes to stderr (the log).
    // C code (GStreamer, plugins, drivers) prints through the C runtime's descriptor 1, which
    // still is the pipe: the link takes a copy of it, and descriptor 1 becomes stderr
    // (review 2026-11-22: a stray printf would have broken the link).
    extern "C" {
        fn _dup(fd: i32) -> i32;
        fn _dup2(from: i32, to: i32) -> i32;
        fn _get_osfhandle(fd: i32) -> isize;
    }
    let link_out = unsafe {
        let own = _dup(1);
        let h = if own >= 0 { _get_osfhandle(own) } else { -1 };
        let h = if h != -1 && h != -2 { windows::Win32::Foundation::HANDLE(h as *mut std::ffi::c_void) } else { GetStdHandle(STD_OUTPUT_HANDLE).expect("stdout") };
        let _ = _dup2(2, 1);
        if let Ok(e) = GetStdHandle(STD_ERROR_HANDLE) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, e);
        }
        std::fs::File::from_raw_handle(h.0)
    };
    crate::init();
    crate::logfile::gst_warnings();
    let out = Arc::new(Mutex::new(std::io::BufWriter::new(link_out)));
    let mut input = std::io::BufReader::new(std::io::stdin().lock());
    let mut video: Option<ChildVideo> = None;
    // Until the app is gone or closes the link.
    // Tests only: STREAMER_DEBUG_VP_STALL_AFTER=N stops reading the app's link N s after the
    // start, for good, in the first video process of a watch (as a thread hung in the driver).
    let stall_at = std::env::var("STREAMER_DEBUG_VP_STALL_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).filter(|_| std::env::var("STREAMER_VIDEO_RUN").as_deref() == Ok("0"));
    let t0 = Instant::now();
    while let Ok(Some(m)) = videolink::read::<ToVideo>(&mut input) {
        if stall_at.is_some_and(|n| t0.elapsed().as_secs() >= n) {
            eprintln!("video process: test stall, the link is no longer read");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        match m {
            Msg::Frame(f) => {
                if let Some(v) = video.as_mut() {
                    v.push(f);
                }
            }
            Msg::Json(ToVideo::Start { parent, title }) if video.is_none() => {
                video = Some(ChildVideo::new(parent, &title, out.clone()));
            }
            Msg::Json(ToVideo::Close) => break,
            Msg::Json(m) => {
                if let Some(v) = video.as_mut() {
                    v.command(m);
                }
            }
        }
    }
    // The app is gone or said Close. Ending the window and pipeline may hang in the driver
    // (the very fault this process exists for): exit regardless after 2 s.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        std::process::exit(0);
    });
    if let Some(v) = video {
        v.end();
    }
    0
}

#[cfg(windows)]
struct ChildVideo {
    pipeline: gst::Pipeline,
    renderer: crate::present::Renderer,
    src: Option<gst_app::AppSrc>,
    stream: Option<(String, String)>,
    out: Arc<Mutex<std::io::BufWriter<std::fs::File>>>,
    /// Frame pts -> host capture time; the newest host-minus-counter offset.
    sent: Arc<Mutex<VecDeque<(u64, u64)>>>,
    offset: Arc<std::sync::atomic::AtomicI64>,
    acc: Arc<Mutex<Acc>>,
    audio: Arc<Mutex<(bool, f32)>>,
    stop: Arc<AtomicBool>,
    /// Its decoder fell behind and frames were dropped: the next ones wait for a keyframe.
    wait_key: bool,
    /// Since when, if so: after 3 s frames go on (a stream whose keyframes are not told).
    wait_since: Option<Instant>,
}

#[cfg(windows)]
#[derive(Default)]
struct Acc {
    frames: u64,
    intervals: Vec<u64>,
    latencies: Vec<u64>,
    size: Option<(u32, u32)>,
}

#[cfg(windows)]
impl ChildVideo {
    fn new(parent: isize, title: &str, out: Arc<Mutex<std::io::BufWriter<std::fs::File>>>) -> Self {
        crate::present::set_default_parent(parent);
        let renderer = crate::present::start();
        renderer.window.set_title(title);
        let pipeline = gst::Pipeline::with_name("video");
        crate::drain_bus(&pipeline, "video");
        pipeline.set_context(&renderer.context);
        let audio = Arc::new(Mutex::new((false, 1.0f32)));
        // The window's own sound controls: the app owns the sound; tell it, show its answer.
        let (a, o) = (audio.clone(), out.clone());
        renderer.window.set_audio_handler(Arc::new(move |muted, volume| {
            if muted.is_some() || volume.is_some() {
                let mut cur = a.lock().unwrap();
                if let Some(m) = muted {
                    cur.0 = m;
                }
                if let Some(v) = volume {
                    cur.1 = v.clamp(0.0, 1.0);
                }
                let _ = videolink::write_json(&mut *o.lock().unwrap(), &FromVideo::Audio { muted, volume });
            }
            *a.lock().unwrap()
        }));
        let v = ChildVideo {
            pipeline,
            renderer,
            src: None,
            stream: None,
            out,
            sent: Arc::default(),
            offset: Arc::default(),
            acc: Arc::default(),
            audio,
            stop: Arc::default(),
            // A new process starts at a keyframe (one may have come before it was ready).
            wait_key: true,
            wait_since: None,
        };
        v.reporter();
        v
    }

    /// The decoder chain for a stream; a new one replaces the old (caps or decoder changed).
    fn build(&mut self, caps: &str, decoder: &str) -> Result<(), String> {
        if self.stream.as_ref().is_some_and(|(c, d)| c == caps && d == decoder) {
            return Ok(());
        }
        if self.src.is_some() {
            let _ = self.pipeline.set_state(gst::State::Null);
            for e in self.pipeline.children() {
                let _ = self.pipeline.remove(&e);
            }
            // Gone: a failed build below must not look like the old one still in place.
            self.src = None;
            self.stream = None;
        }
        let caps_v: gst::Caps = caps.parse().map_err(|_| format!("caps {caps}"))?;
        let codec = match caps_v.structure(0).map(|s| s.name().as_str()) {
            Some("video/x-av1") => "av1",
            Some("video/x-h265") => "h265",
            Some("video/x-h264") => "h264",
            other => return Err(format!("unknown video {other:?}")),
        };
        let src = gst_app::AppSrc::builder().caps(&caps_v).is_live(true).format(gst::Format::Time).build();
        // A decoder that falls behind must not pile up frames without end: at most ~1 s at
        // 50 Mbit/s waits; past that new frames are dropped (the decoder then asks for a
        // keyframe), never the app blocked.
        src.set_property("block", false);
        src.set_max_bytes(8 << 20);
        if src.has_property("leaky-type") {
            src.set_property_from_str("leaky-type", "upstream");
        }
        let parse = crate::viewer::make(&format!("{codec}parse"))?;
        let mut chain: Vec<gst::Element> = vec![src.clone().upcast(), parse.clone()];
        let dec = crate::viewer::make(decoder)?;
        if decoder == "dav1ddec" {
            dec.set_property_from_str("max-frame-delay", "1");
            dec.set_property_from_str("inloop-filters", "deblock+cdef+restoration");
        }
        chain.push(dec.clone());
        if decoder == "dav1ddec" {
            chain.push(crate::viewer::make("d3d11upload")?);
        }
        chain.extend(crate::viewer::video_tail(Some(self.renderer.frames.clone()))?);
        let refs: Vec<&gst::Element> = chain.iter().collect();
        self.pipeline.add_many(&refs).map_err(|e| e.to_string())?;
        gst::Element::link_many(&refs).map_err(|e| e.to_string())?;
        if std::env::var_os("STREAMER_DEBUG_VP_TIMING").is_some() {
            debug_timing(&src, &parse, &dec);
        }
        // The decoder asks for a keyframe (it lost its reference): the app asks the streamer.
        let o = self.out.clone();
        src.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            if let Some(e) = info.event() {
                if e.structure().is_some_and(|s| s.name() == "GstForceKeyUnit") {
                    let _ = videolink::write_json(&mut *o.lock().unwrap(), &FromVideo::Keyframe);
                }
            }
            gst::PadProbeReturn::Ok
        });
        // Decoded frames: count, gaps, size, and the delay since the host captured them.
        let (acc, sent, offset) = (self.acc.clone(), self.sent.clone(), self.offset.clone());
        let started = Instant::now();
        let last = Mutex::new(None::<Instant>);
        dec.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) {
                if let Some(s) = c.caps().structure(0) {
                    let (w, h) = (s.get::<i32>("width").unwrap_or(0), s.get::<i32>("height").unwrap_or(0));
                    if w > 0 && h > 0 {
                        acc.lock().unwrap().size = Some((w as u32, h as u32));
                    }
                }
                return gst::PadProbeReturn::Ok;
            }
            let Some(b) = info.buffer() else { return gst::PadProbeReturn::Ok };
            let now = Instant::now();
            let mut a = acc.lock().unwrap();
            a.frames += 1;
            let warm = started.elapsed() >= Duration::from_secs(3);
            if let Some(prev) = last.lock().unwrap().replace(now) {
                if warm {
                    a.intervals.push(now.duration_since(prev).as_nanos() as u64);
                }
            }
            if let (true, Some(pts)) = (warm, b.pts().map(|p| p.nseconds())) {
                let host = videolink::qpc_ns() as i64 + offset.load(Ordering::Relaxed);
                if let Some(&(_, s)) = sent.lock().unwrap().iter().find(|(p, _)| *p == pts) {
                    if s != 0 && host > s as i64 {
                        a.latencies.push(host as u64 - s);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        self.pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        self.src = Some(src);
        self.stream = Some((caps.into(), decoder.into()));
        // A new decoder starts at a keyframe (the stream may change between keyframes).
        self.wait_key = true;
        self.wait_since = None;
        eprintln!("video process: decoding {codec} with {decoder}");
        Ok(())
    }

    fn push(&mut self, f: videolink::Frame) {
        let Some(src) = &self.src else { return };
        // Tests only: STREAMER_DEBUG_VP_DECODER_HANG_AFTER=N: from N s on, the first video
        // process of a watch reads its frames and decodes none (as a decoder stuck in the driver).
        static HANG: std::sync::OnceLock<Option<(Instant, u64)>> = std::sync::OnceLock::new();
        if let Some((t0, n)) = *HANG.get_or_init(|| std::env::var("STREAMER_DEBUG_VP_DECODER_HANG_AFTER").ok().and_then(|v| v.parse().ok()).filter(|_| std::env::var("STREAMER_VIDEO_RUN").as_deref() == Ok("0")).map(|n| (Instant::now(), n))) {
            if t0.elapsed().as_secs() >= n {
                return;
            }
        }
        // The decoder is far behind (its input near full): drop frames up to the next
        // keyframe here, rather than let the input drop some and feed the decoder frames
        // whose references are gone (second review 2026-11-22).
        if !self.wait_key && src.current_level_bytes() > (7 << 20) {
            eprintln!("video process: the decoder fell behind; frames wait for a keyframe");
            self.wait_key = true;
            let _ = videolink::write_json(&mut *self.out.lock().unwrap(), &FromVideo::Keyframe);
        }
        if self.wait_key {
            if f.delta && self.wait_since.get_or_insert_with(Instant::now).elapsed() < Duration::from_secs(3) {
                return;
            }
            // A keyframe into a still full input would be the one dropped: the next, then
            // (third review).
            if !f.delta && src.current_level_bytes() > (6 << 20) {
                return;
            }
            self.wait_key = false;
            self.wait_since = None;
        }
        if let Some(o) = f.host_minus_qpc {
            self.offset.store(o, Ordering::Relaxed);
        }
        if let (Some(p), Some(s)) = (f.pts, f.sent_host) {
            let mut q = self.sent.lock().unwrap();
            q.push_back((p, s));
            if q.len() > 600 {
                q.pop_front();
            }
        }
        let mut b = gst::Buffer::from_mut_slice(f.data);
        {
            let b = b.get_mut().unwrap();
            b.set_pts(f.pts.map(gst::ClockTime::from_nseconds));
            b.set_dts(f.dts.map(gst::ClockTime::from_nseconds));
            b.set_duration(f.duration.map(gst::ClockTime::from_nseconds));
            if f.delta {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            if f.discont {
                b.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        let _ = src.push_buffer(b);
    }

    fn command(&mut self, m: ToVideo) {
        let w = &self.renderer.window;
        match m {
            ToVideo::Stream { caps, decoder } => {
                // Without a decoder the picture would stay a loader for good: end, so the app
                // starts a new process (and after repeated failures says the video stopped).
                if let Err(e) = self.build(&caps, &decoder) {
                    eprintln!("video process: cannot decode: {e}");
                    std::process::exit(3);
                }
            }
            ToVideo::Place { rect } => w.place(rect),
            ToVideo::Hidden { on } => w.set_hidden(on),
            ToVideo::Popped { on } => w.set_popped(on),
            // As it comes: the window's own state lags the posted switch, and the switch does
            // nothing when already so (second review: a quick on-off lost the off).
            ToVideo::Fullscreen { on } => w.set_fullscreen(on),
            ToVideo::ToggleFullscreen => w.toggle_fullscreen(),
            ToVideo::Topmost { on } => w.set_topmost(on),
            ToVideo::Show => w.show(),
            ToVideo::Title { text } => w.set_title(&text),
            ToVideo::Overlay { on } => {
                if w.state().overlay != on {
                    w.set_overlay(on)
                }
            }
            ToVideo::OverlayText { text } => w.set_overlay_text(&text),
            ToVideo::Badge { on } => {
                if w.state().badge != on {
                    w.set_badge(on)
                }
            }
            ToVideo::BadgeInfo { text, ok } => w.set_badge_info(&text, ok),
            ToVideo::Status { text } => w.set_status(&text),
            ToVideo::PlaceAgain => w.place_again(),
            ToVideo::Audio { muted, volume } => *self.audio.lock().unwrap() = (muted, volume),
            ToVideo::Start { .. } | ToVideo::Close => {}
        }
    }

    /// Every 100 ms: what happened, for the app.
    fn reporter(&self) {
        let (out, acc, stats, closed, window, stop) = (self.out.clone(), self.acc.clone(), self.renderer.stats.clone(), self.renderer.closed.clone(), self.renderer.window, self.stop.clone());
        std::thread::Builder::new()
            .name("video-report".into())
            .spawn(move || {
                let mut last = (0u64, 0u64);
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                    let a = std::mem::take(&mut *acc.lock().unwrap());
                    let st = window.state();
                    let (p, s) = (stats.presented.load(Ordering::Relaxed), stats.skipped.load(Ordering::Relaxed));
                    let m = FromVideo::Report {
                        state: WindowStateMsg { fullscreen: st.fullscreen, topmost: st.topmost, overlay: st.overlay, badge: st.badge, embedded: st.embedded, alive: window.alive() },
                        closed: closed.load(Ordering::SeqCst),
                        frames: a.frames,
                        intervals: a.intervals,
                        latencies: a.latencies,
                        presented: p - last.0,
                        skipped: s - last.1,
                        max_present_us: stats.max_present_us.swap(0, Ordering::Relaxed),
                        max_lock_us: stats.max_lock_us.swap(0, Ordering::Relaxed),
                        size: a.size,
                        loops: stats.loops.load(Ordering::Relaxed) + crate::present::ticks(),
                    };
                    last = (p, s);
                    if videolink::write_json(&mut *out.lock().unwrap(), &m).is_err() {
                        return;
                    }
                }
            })
            .unwrap();
    }

    fn end(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.renderer.window.close();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Tests only (STREAMER_DEBUG_VP_TIMING): every 2 s, the median time from a frame's arrival
/// in the video process to the parser's output and to the decoder's output.
#[cfg(windows)]
fn debug_timing(src: &gst_app::AppSrc, parse: &gst::Element, dec: &gst::Element) {
    let arrived: Arc<Mutex<VecDeque<(u64, Instant)>>> = Arc::default();
    let a = arrived.clone();
    src.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(p) = info.buffer().and_then(|b| b.pts()) {
            let mut q = a.lock().unwrap();
            q.push_back((p.nseconds(), Instant::now()));
            if q.len() > 300 {
                q.pop_front();
            }
        }
        gst::PadProbeReturn::Ok
    });
    let probe = |e: &gst::Element, label: &'static str| {
        let a = arrived.clone();
        let samples = Mutex::new((Vec::<f64>::new(), Instant::now()));
        e.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(p) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
            let at = a.lock().unwrap().iter().rev().find(|(k, _)| *k == p.nseconds()).map(|(_, t)| *t);
            let mut s = samples.lock().unwrap();
            if let Some(at) = at {
                s.0.push(at.elapsed().as_secs_f64() * 1000.0);
            }
            if s.1.elapsed() > Duration::from_secs(2) && !s.0.is_empty() {
                s.0.sort_by(|x, y| x.partial_cmp(y).unwrap());
                eprintln!("video timing: arrival to {label} median {:.2} ms over {}", s.0[s.0.len() / 2], s.0.len());
                *s = (Vec::new(), Instant::now());
            }
            gst::PadProbeReturn::Ok
        });
    };
    probe(parse, "parser out");
    probe(dec, "decoder out");
}
