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
/// No report, or a presentation loop that stands still, for this long: the process hangs.
const HANG: Duration = Duration::from_secs(5);
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
static BADGE: AtomicBool = AtomicBool::new(false);

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
    out: mpsc::SyncSender<Out>,
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
    last_report: Mutex<(Instant, u64)>,
    /// Video processes started for this watch so far.
    runs: AtomicU64,
    me: Weak<Remote>,
}

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
            last_report: Mutex::new((Instant::now(), 0)),
            runs: AtomicU64::new(0),
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
        self.desired.lock().unwrap().stream = Some((caps.into(), decoder.into()));
        self.send(Out::Json(ToVideo::Stream { caps: caps.into(), decoder: decoder.into() }));
    }

    /// A compressed frame for the decoder. Never blocks: a full queue drops frames until the
    /// next keyframe, which is asked for.
    pub fn push(&self, f: videolink::Frame) {
        let guard = self.child.lock().unwrap();
        let Some(c) = guard.as_ref() else { return };
        if c.want_key.load(Ordering::SeqCst) {
            if f.delta {
                return;
            }
            c.want_key.store(false, Ordering::SeqCst);
        }
        if c.out.try_send(Out::Frame(f)).is_err() {
            c.want_key.store(true, Ordering::SeqCst);
            drop(guard);
            self.ask_keyframe();
        }
    }

    fn command(&self, m: ToVideo) {
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
        }
        self.send(Out::Json(m));
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

    fn send(&self, m: Out) {
        if let Some(c) = self.child.lock().unwrap().as_ref() {
            // Commands wait for room; only frames are dropped.
            let _ = c.out.send(m);
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
        let (stdin, stdout) = (proc.stdin.take().unwrap(), proc.stdout.take().unwrap());
        let (tx, rx) = mpsc::sync_channel::<Out>(QUEUE);
        let want_key = Arc::new(AtomicBool::new(true));
        // Writer: everything for the process, in order.
        std::thread::Builder::new()
            .name("video-out".into())
            .spawn(move || {
                let mut w = std::io::BufWriter::with_capacity(1 << 20, stdin);
                for m in rx {
                    let r = match m {
                        Out::Frame(f) => videolink::write_frame(&mut w, &f),
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
        // What it should show, from the start: the window, then everything set on it so far.
        let d = self.desired.lock().unwrap().clone();
        let mut first = vec![ToVideo::Start { parent: d.parent, title: d.title.clone() }];
        if let Some((caps, decoder)) = &d.stream {
            first.push(ToVideo::Stream { caps: caps.clone(), decoder: decoder.clone() });
        }
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
        first.push(ToVideo::Place { rect: d.place });
        if d.popped {
            allow_foreground(pid);
            first.push(ToVideo::Popped { on: true });
        }
        if d.fullscreen {
            allow_foreground(pid);
            first.push(ToVideo::Fullscreen { on: true });
        }
        for m in first {
            let _ = tx.send(Out::Json(m));
        }
        *self.last_report.lock().unwrap() = (Instant::now(), 0);
        *self.child.lock().unwrap() = Some(Child { proc, out: tx, want_key, started: Instant::now() });
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
                        me.on_message(m);
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
                    let mut lr = self.last_report.lock().unwrap();
                    // The loop counter stands still: the presentation thread hangs.
                    if loops != lr.1 {
                        *lr = (Instant::now(), loops);
                    }
                }
                // F3 inside this window: every other window follows, as in one process.
                let (o, b) = (OVERLAY.load(Ordering::SeqCst), BADGE.load(Ordering::SeqCst));
                if state.overlay != o || state.badge != b {
                    let mine = self.state.lock().unwrap().clone();
                    if state.overlay != mine.overlay || state.badge != mine.badge {
                        let (ov, ba) = (state.overlay, state.badge);
                        std::thread::spawn(move || set_all(|d| {
                            d.overlay = ov;
                            d.badge = ba;
                        }));
                    }
                }
                {
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
                let h = self.audio.lock().unwrap().clone();
                if let Some(h) = h {
                    let (m, v) = h(muted, volume);
                    self.command(ToVideo::Audio { muted: m, volume: v });
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
        let code = c.proc.wait().ok().and_then(|s| s.code());
        if self.ending.load(Ordering::SeqCst) {
            return;
        }
        let lived = c.started.elapsed();
        eprintln!("video process: ended (pid {pid}, code {}) after {:.1} s; the video starts again", code.map(|c| format!("{:#010x}", c as u32)).unwrap_or("?".into()), lived.as_secs_f64());
        let too_many = {
            let mut cr = self.crashes.lock().unwrap();
            cr.push_back(Instant::now());
            while cr.front().is_some_and(|t| t.elapsed() > CRASH_WINDOW) {
                cr.pop_front();
            }
            cr.len() > MAX_RESTARTS
        };
        // STREAMER_DEBUG_NO_RESTART: the video stays gone (the spoiled variant).
        if too_many || std::env::var_os("STREAMER_DEBUG_NO_RESTART").is_some() {
            eprintln!("video process: crashed {} times within a minute; the video stops", MAX_RESTARTS + 1);
            self.failed.store(true, Ordering::SeqCst);
            self.state.lock().unwrap().alive = false;
            return;
        }
        self.spawn();
    }

    /// Kills a process that stopped reporting or whose presentation loop stopped turning.
    fn watchdog(&self) {
        let me = self.me.clone();
        std::thread::Builder::new()
            .name("video-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(500));
                let Some(me) = me.upgrade() else { return };
                if me.ending.load(Ordering::SeqCst) || me.failed.load(Ordering::SeqCst) {
                    return;
                }
                let since = me.last_report.lock().unwrap().0.elapsed();
                if since > HANG {
                    let mut c = me.child.lock().unwrap();
                    if let Some(c) = c.as_mut() {
                        eprintln!("video process: no sign of life for {:.1} s; ended", since.as_secs_f64());
                        let _ = c.proc.kill();
                    }
                    drop(c);
                    *me.last_report.lock().unwrap() = (Instant::now(), 0);
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
    let link_out = unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE).expect("stdout");
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
    while let Ok(Some(m)) = videolink::read::<ToVideo>(&mut input) {
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
        }
        let caps_v: gst::Caps = caps.parse().map_err(|_| format!("caps {caps}"))?;
        let codec = match caps_v.structure(0).map(|s| s.name().as_str()) {
            Some("video/x-av1") => "av1",
            Some("video/x-h265") => "h265",
            Some("video/x-h264") => "h264",
            other => return Err(format!("unknown video {other:?}")),
        };
        let src = gst_app::AppSrc::builder().caps(&caps_v).is_live(true).format(gst::Format::Time).build();
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
        eprintln!("video process: decoding {codec} with {decoder}");
        Ok(())
    }

    fn push(&mut self, f: videolink::Frame) {
        let Some(src) = &self.src else { return };
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
                if let Err(e) = self.build(&caps, &decoder) {
                    eprintln!("video process: cannot decode: {e}");
                }
            }
            ToVideo::Place { rect } => w.place(rect),
            ToVideo::Hidden { on } => w.set_hidden(on),
            ToVideo::Popped { on } => w.set_popped(on),
            ToVideo::Fullscreen { on } => {
                if w.state().fullscreen != on {
                    w.set_fullscreen(on)
                }
            }
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
                        loops: stats.loops.load(Ordering::Relaxed),
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