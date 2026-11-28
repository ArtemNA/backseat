//! Host: one capture, one encoder per quality that some viewer watches (a layer), fan-out
//! through each layer's `tee` to one webrtcbin per viewer. The host's settings are the
//! ceiling; viewers may ask for less and move between layers at keyframes.

use crate::clock;
use crate::config::{HostConfig, Quality, Source};
use crate::platform;
use crate::signal::Outbox;
use gst::prelude::*;
use gst_webrtc::{WebRTCRTPTransceiver, WebRTCSDPType, WebRTCSessionDescription};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const VIDEO_PT: u32 = 96;
/// Signaling message kind: a viewer asks for a quality, as "720p30 4000" (kbit/s optional).
pub const QUALITY_MSG: &str = "quality";
const AUDIO_PT: u32 = 111;

#[derive(Default)]
pub struct HostStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub key_frames: AtomicU64,
    pub key_bytes: AtomicU64,
    pub max_frame_bytes: AtomicU64,
    pub key_requests_dropped: AtomicU64,
    /// Time between encoded frames reaching the fan-out point, in ns.
    pub frame_interval: crate::stats::Series,
}

pub struct Host {
    pub pipeline: gst::Pipeline,
    /// Statistics of the layer at the host's own quality.
    pub stats: Arc<HostStats>,
    pub encoder: String,
    kind: platform::EncoderKind,
    /// Changes while live (`reconfigure`): source, quality, one program's audio.
    cfg: Mutex<HostConfig>,
    /// The host's own quality: the most any viewer gets.
    ceiling: Mutex<Quality>,
    /// The source was replaced while live: layer sizes come from the new one's description.
    source_changed: AtomicBool,
    /// Viewer branches by viewer id. The lock also serializes start, stop and teardown.
    /// Lock order: `viewers`, then `layers`. Streaming threads take neither.
    viewers: Mutex<HashMap<u64, Branch>>,
    layers: Mutex<HashMap<Quality, Layer>>,
    /// The streamer's own small picture (`set_preview`), made on first use.
    preview: Mutex<Option<Preview>>,
    /// The capture failed for good (its window closed).
    lost: Arc<SourceLost>,
    /// Holes punched for viewers, for their next connection (good for a moment only).
    pinned: Mutex<HashMap<u64, (crate::punch::Pinned, std::time::Instant)>>,
    on_viewer_failed: Arc<Mutex<Option<ViewerFailHook>>>,
}

/// The capture failed (its window was closed): the stream cannot go on with it. Told once,
/// to whoever listens (`Host::on_source_lost`), even if the failure came first.
type Tell = Box<dyn Fn(String) + Send>;

#[derive(Default)]
struct SourceLost {
    reason: Mutex<Option<String>>,
    tell: Mutex<Option<Tell>>,
}

impl SourceLost {
    fn fail(&self, reason: String) {
        let mut r = self.reason.lock().unwrap();
        if r.is_some() {
            return;
        }
        *r = Some(reason.clone());
        drop(r);
        if let Some(f) = self.tell.lock().unwrap().as_ref() {
            f(reason);
        }
    }
}

/// A small copy of what is captured, for the streamer to see what goes out. Its input is
/// dropped while it is off, so it costs nothing then.
struct Preview {
    sink: gst_app::AppSink,
    on: Arc<AtomicBool>,
}

/// The preview's size (the picture keeps its shape inside, with black bars) and rate.
pub const PREVIEW_W: u32 = 320;
pub const PREVIEW_H: u32 = 180;
const PREVIEW_FPS: u32 = 5;

/// One encoder at one quality, fed from the shared capture.
struct Layer {
    bin: gst::Bin,
    tee: gst::Element,
    /// Request pad on the capture tee feeding this layer.
    feed: gst::Pad,
    stats: Arc<HostStats>,
    /// No viewer watches it: its input is dropped and the encoder idles. Stopping an
    /// encoder while the pipeline plays hung the whole host within 60-135 s of churn
    /// (3 runs of 4), so a layer is only torn down with the pipeline.
    parked: Arc<AtomicBool>,
}

/// Which layer feeds a viewer's video, and the one it is moving to.
struct VideoLink {
    quality: Quality,
    tee: gst::Element,
    pad: gst::Pad,
    /// Target layer, its tee pad and the probe waiting there for a keyframe.
    pending: Option<(Quality, gst::Element, gst::Pad, gst::PadProbeId)>,
    /// Tee pads already unlinked from this viewer, to release off the streaming thread.
    retired: Vec<(gst::Element, gst::Pad)>,
}

/// One row of the per-layer report.
pub struct LayerReport {
    pub quality: Quality,
    /// Kept with an idle encoder because no viewer watches it.
    pub parked: bool,
    pub viewers: usize,
    pub stats: Arc<HostStats>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ViewerQuality {
    pub id: u64,
    /// Share of packets lost in the viewer's last receiver report, 0..1.
    pub fraction_lost: f64,
    pub rtt_ms: f64,
    pub nack: u64,
    pub pli: u64,
    /// Retransmission requests received, and packets actually resent.
    pub rtx_requests: u64,
    pub rtx_packets: u64,
    /// Catch-ups this viewer asked for, and frames skipped for it, since it joined.
    pub catchups: u64,
    pub skipped_frames: u64,
    /// RTP payload bytes sent to this viewer since it joined, video and audio. Headers
    /// (RTP, SRTP, UDP/IP) add about 4% on the wire.
    pub bytes_sent: u64,
}

/// Everything one viewer adds to the pipeline.
struct Branch {
    webrtc: gst::Element,
    /// Queues and payloaders, one pair per tee.
    elements: Vec<gst::Element>,
    /// Tee request pads feeding this branch, audio only; video is in `video`.
    tee_pads: Vec<(gst::Element, gst::Pad)>,
    /// Skips this viewer's video while its network queue drains.
    gate: Arc<crate::catchup::Gate>,
    video: Arc<Mutex<VideoLink>>,
    /// Input of the branch's video queue, where a layer's tee pad links.
    video_in: gst::Pad,
    /// The quality the viewer asked for; None: the host's own, whatever it becomes.
    want: Option<Quality>,
    /// Tee pads and the branch inputs they feed, linked only once the branch plays: a tee
    /// pushing into a branch still in NULL gets FLUSHING and passes it up, which stopped
    /// the layer, the capture tee and the capture (stress test, 2026-11-06).
    unlinked: Vec<(gst::Pad, gst::Pad)>,
    /// The punched hole this branch's connection uses, if any (told to ICE with the answer).
    hole: Option<crate::punch::Pinned>,
    /// Its connection's failure is being (or was) handled.
    handled: Arc<AtomicBool>,
    /// A punch for its connection is running.
    punching: Arc<AtomicBool>,
}

/// Told when the connection `w` with viewer `id` fails (`was_up`: it had worked).
pub type ViewerFailHook = Arc<dyn Fn(u64, gst::Element, bool) + Send + Sync>;

/// The capture as a pipeline description. The caps end in a named capsfilter, not bare caps:
/// a description that ends in caps parses inside the whole pipeline (`Host::new`) but not as
/// a bin on its own (`swap_video_source`: "no element D3D11Memory", so a running stream could
/// not change to another window or monitor, nor change its frame rate).
/// STREAMER_DEBUG_SOURCE_BARE_CAPS: the old description, the spoiled variant.
fn video_source(cfg: &HostConfig) -> String {
    let desc = video_source_desc(cfg);
    if std::env::var_os("STREAMER_DEBUG_SOURCE_BARE_CAPS").is_some() {
        return desc.replace("capsfilter caps=\"", "").replace("/1\"", "/1");
    }
    desc
}

fn video_source_desc(cfg: &HostConfig) -> String {
    // Test only: STREAMER_DEBUG_SOURCE replaces the capture with any pipeline description
    // that ends in D3D11 memory (e.g. a raw clip from disk played in real time).
    if let Ok(desc) = std::env::var("STREAMER_DEBUG_SOURCE") {
        return desc;
    }
    let m = cfg.mode;
    match cfg.source {
        Source::Capture { monitor } => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true monitor-index={monitor} ! \
             capsfilter caps=\"video/x-raw(memory:D3D11Memory),framerate={}/1\"",
            m.fps
        ),
        // The frame rate must be set: WGC and DXGI otherwise negotiate 30 (stage 6 probe A).
        Source::Monitor { handle } => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true monitor-handle={handle} ! \
             capsfilter caps=\"video/x-raw(memory:D3D11Memory),framerate={}/1\"",
            m.fps
        ),
        Source::Window { hwnd } => format!(
            "d3d11screencapturesrc capture-api=wgc show-cursor=true window-handle={hwnd} ! \
             capsfilter caps=\"video/x-raw(memory:D3D11Memory),framerate={}/1\"",
            m.fps
        ),
        Source::Bars => format!(
            "videotestsrc is-live=true pattern=smpte horizontal-speed=4 ! \
             video/x-raw,format=NV12,width={},height={},framerate={}/1 ! d3d11upload",
            m.width, m.height, m.fps
        ),
        Source::Noise => format!(
            "videotestsrc is-live=true pattern=snow ! \
             video/x-raw,format=NV12,width={},height={},framerate={}/1 ! d3d11upload",
            m.width, m.height, m.fps
        ),
    }
}

/// The whole system's sound, one program at a time (`crate::audio_apps`): each program that
/// has sound is captured on its own and mixed, leaving out Backseat, sound routers (they play
/// our voice chat over again as their own) and the programs the user excludes. Checked again
/// every 2 s, so programs that start or end their sound mid-stream come and go.
#[cfg(windows)]
fn system_sound(exclude: &[String]) -> Result<gst::Element, String> {
    let bin = gst::Bin::with_name("audiosrc");
    let mk = |f: &str| gst::ElementFactory::make(f).build().map_err(|e| format!("{f}: {e}"));
    let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", 48_000i32).field("channels", 2i32).build();
    // Silence keeps the mixer running when no program plays.
    let silence = gst::ElementFactory::make("audiotestsrc").property_from_str("wave", "silence").property("is-live", true).property("samplesperbuffer", 480i32).build().map_err(|e| e.to_string())?;
    let scaps = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
    // No latency of its own: with 30 ms (0.2.24) only every other 10 ms piece reached the
    // viewers (50 sound packets a second instead of 100: half the sound gone; rate-probe.ps1).
    // The captures are stamped on arrival (below), so they are not late anyway.
    // STREAMER_DEBUG_SYSMIX_LATENCY_MS: another (30: the spoiled variant).
    let mixer = gst::ElementFactory::make("audiomixer").name("sysmix").property("latency", std::env::var("STREAMER_DEBUG_SYSMIX_LATENCY_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1_000_000).build().map_err(|e| e.to_string())?;
    if std::env::var_os("STREAMER_DEBUG_SYSMIX_NO_SILENCE").is_some() {
        bin.add(&mixer).unwrap();
    } else {
        bin.add_many([&silence, &scaps, &mixer]).unwrap();
        gst::Element::link_many([&silence, &scaps, &mixer]).map_err(|e| e.to_string())?;
    }
    // The mix stays in the inputs' format (F32): left to choose, it took the encoder's S16 and
    // sent silence (0.2.13, 0.2.14). STREAMER_DEBUG_SYSMIX_ANY_FORMAT: as then.
    let outcaps = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
    let out = if std::env::var_os("STREAMER_DEBUG_SYSMIX_ANY_FORMAT").is_some() {
        mixer.clone()
    } else {
        bin.add(&outcaps).unwrap();
        mixer.link(&outcaps).map_err(|e| e.to_string())?;
        outcaps.clone()
    };
    crate::diag::watch_ts(&mixer.static_pad("src").unwrap(), "system sound mix".into());
    bin.add_pad(&gst::GhostPad::with_target(&out.static_pad("src").unwrap()).unwrap()).unwrap();
    // Tests: STREAMER_DEBUG_SYSMIX_LEVEL logs the mix's level once a second (packets alone
    // flow with silence too: 0.2.13 passed that check and sent silence).
    if std::env::var_os("STREAMER_DEBUG_SYSMIX_LEVEL").is_some() {
        let acc = std::sync::Mutex::new((0f64, 0usize));
        let mixer_dbg = mixer.clone();
        let pad_rt = |e: &gst::Element| e.current_running_time();
        eprintln!("host: mixer caps check later");
        mixer.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(c) = pad.current_caps() { let _ = c; }
            if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                let mut a = acc.lock().unwrap();
                for c in map.as_slice().chunks_exact(4) {
                    let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64;
                    a.0 += v * v;
                    a.1 += 1;
                }
                if a.1 >= 96_000 {
                    let db = if a.0 > 0.0 { 10.0 * (a.0 / a.1 as f64).log10() } else { -100.0 };
                    eprintln!("host: system sound level {db:.0} dB, pts {:?}, running {:?}", info.buffer().and_then(|b| b.pts()), pad_rt(&mixer_dbg));
                    *a = (0.0, 0);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    // Tests: STREAMER_DEBUG_SYSMIX_DUMP=<file> writes the mix (F32 stereo 48 kHz, raw).
    if let Some(path) = std::env::var_os("STREAMER_DEBUG_SYSMIX_DUMP") {
        if let Ok(f) = std::fs::File::create(path) {
            let f = Mutex::new(std::io::BufWriter::new(f));
            mixer.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                    use std::io::Write;
                    let _ = f.lock().unwrap().write_all(map.as_slice());
                }
                gst::PadProbeReturn::Ok
            });
        }
    }
    let exclude: Vec<String> = exclude.iter().map(|e| e.to_ascii_lowercase()).collect();
    // Test only: STREAMER_DEBUG_SYSMIX_FAKE="A_FROM A_TO;B_FROM B_TO" (seconds since start)
    // replaces the programs with test tones (pid 1: 440 Hz, pid 2: 880 Hz, ...) present only in
    // their windows, so programs come and go without any real sound being captured or played.
    let fake: Option<Vec<(f64, f64)>> = std::env::var("STREAMER_DEBUG_SYSMIX_FAKE").ok().map(|v| {
        v.split(';')
            .filter_map(|w| {
                let mut it = w.split_whitespace().filter_map(|x| x.parse::<f64>().ok());
                Some((it.next()?, it.next()?))
            })
            .collect()
    });
    let fake_started = std::time::Instant::now();
    let is_fake = fake.is_some();
    let pid_secs = |name: &str| -> Option<(u32, f64)> {
        let v = std::env::var(name).ok()?;
        let mut it = v.split_whitespace();
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    };
    // Test only: STREAMER_DEBUG_SYSMIX_FAKE_STUCK="PID SECS": that fake program's first capture
    // gives silence from SECS s on (as a real program's capture did, reported 2026-11-23: the
    // stream went quiet until a viewer joined again and the capture started anew).
    let stuck = pid_secs("STREAMER_DEBUG_SYSMIX_FAKE_STUCK");
    // Test only: STREAMER_DEBUG_SYSMIX_FAKE_LATE="PID SECS": that fake program's first capture
    // stamps its sound 1 s in the past from SECS s on, so the mixer drops it as late (the mix
    // goes silent while the capture is heard).
    let late = pid_secs("STREAMER_DEBUG_SYSMIX_FAKE_LATE");
    // STREAMER_DEBUG_SYSMIX_FAKE_LATE_MS: by that much instead of 1 s (a little late: the
    // mixer cuts the start of every buffer instead of dropping it whole).
    let late_by = gst::ClockTime::from_mseconds(std::env::var("STREAMER_DEBUG_SYSMIX_FAKE_LATE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000));
    // STREAMER_DEBUG_SYSMIX_FAKE_LATE_JITTER_MS: plus 0..that much, different for each buffer.
    let late_jitter: u64 = std::env::var("STREAMER_DEBUG_SYSMIX_FAKE_LATE_JITTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // STREAMER_DEBUG_SYSMIX_FAKE_LATE_GROW_MS: plus that much more for every second since
    // SECS (timestamps that drift away from the clock, as a real capture's did 2026-11-24).
    let late_grow: f64 = std::env::var("STREAMER_DEBUG_SYSMIX_FAKE_LATE_GROW_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    // Test only: STREAMER_DEBUG_SYSMIX_FAKE_CUT="PID SECS": from SECS s on, that fake program's
    // first capture reaches the mixer with 19 of 20 pieces silenced (its capture's level stays):
    // a mix much quieter than what it mixes, as the mixer made it 2026-11-24.
    let cut = pid_secs("STREAMER_DEBUG_SYSMIX_FAKE_CUT");
    let cut_used = Arc::new(AtomicBool::new(false));
    let pause_used = Arc::new(AtomicBool::new(false));
    let restamp = std::env::var_os("STREAMER_DEBUG_SYSMIX_NO_RESTAMP").is_none();
    // Test only: STREAMER_DEBUG_SYSMIX_FAKE_CLOCKED="PID": that fake program is a real capture
    // (of this process's own sound: none), which can give the pipeline its clock.
    let clocked: Option<u32> = std::env::var("STREAMER_DEBUG_SYSMIX_FAKE_CLOCKED").ok().and_then(|v| v.trim().parse().ok());
    let (stuck_used, late_used) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    // What each program's capture gave since the last look, and the mix: (sum of squares, samples).
    let levels: Arc<Mutex<HashMap<u32, (f64, u64)>>> = Arc::default();
    let levels_in = levels.clone();
    let mix_level: Arc<Mutex<(f64, u64)>> = Arc::default();
    let ml = mix_level.clone();
    mixer.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
            let (sum, n) = sum_squares_f32(map.as_slice());
            let mut a = ml.lock().unwrap();
            a.0 += sum;
            a.1 += n;
        }
        gst::PadProbeReturn::Ok
    });
    let add = move |bin: &gst::Bin, app: &crate::audio_apps::AudioApp| -> Result<(), String> {
        let src = if is_fake && clocked != Some(app.pid) {
            gst::ElementFactory::make("audiotestsrc")
                .property("is-live", true)
                .property("freq", 440.0 * app.pid as f64)
                .property("volume", 0.3f64)
                .property("samplesperbuffer", 480i32)
                .build()
                .map_err(|e| e.to_string())?
        } else {
            let src = mk("wasapi2src")?;
            src.set_property("loopback", true);
            src.set_property("low-latency", true);
            src.set_property("loopback-target-pid", if is_fake { std::process::id() } else { app.pid });
            src.set_property_from_str("loopback-mode", "include-process-tree");
            // Not the pipeline's clock: a program's capture leaves when the program ends (or
            // starts again, below), and a pipeline whose clock left goes on a dead clock.
            // STREAMER_DEBUG_SYSMIX_PROVIDE_CLOCK: as in 0.2.19 (the spoiled variant).
            if std::env::var_os("STREAMER_DEBUG_SYSMIX_PROVIDE_CLOCK").is_none() && src.has_property("provide-clock") {
                src.set_property("provide-clock", false);
            }
            src
        };
        let conv = mk("audioconvert")?;
        let res = mk("audioresample")?;
        let cf = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
        crate::diag::watch_ts(&cf.static_pad("src").unwrap(), format!("capture of {}", app.exe));
        let branch = gst::Bin::with_name(&format!("app{}", app.pid));
        branch.add_many([&src, &conv, &res, &cf]).unwrap();
        gst::Element::link_many([&src, &conv, &res, &cf]).map_err(|e| e.to_string())?;
        branch.add_pad(&gst::GhostPad::with_target(&cf.static_pad("src").unwrap()).unwrap()).unwrap();
        if let Some((pid, secs)) = stuck.filter(|(p, _)| is_fake && *p == app.pid) {
            if !stuck_used.swap(true, Ordering::SeqCst) {
                let t0 = std::time::Instant::now();
                eprintln!("host: test: the capture of fake{pid}.exe goes silent in {secs} s");
                src.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if t0.elapsed().as_secs_f64() >= secs {
                        if let Some(b) = info.buffer_mut() {
                            if let Ok(mut m) = b.make_mut().map_writable() {
                                m.as_mut_slice().fill(0);
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        if let Some((pid, secs)) = late.filter(|(p, _)| is_fake && *p == app.pid) {
            if !late_used.swap(true, Ordering::SeqCst) {
                let t0 = std::time::Instant::now();
                eprintln!("host: test: the capture of fake{pid}.exe is stamped late in {secs} s");
                let seed_cell = std::sync::atomic::AtomicU32::new(0x9e37_79b9);
                cf.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    let mut seed = seed_cell.load(Ordering::Relaxed);
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed_cell.store(seed, Ordering::Relaxed);
                    let jitter = if late_jitter > 0 { gst::ClockTime::from_mseconds(seed as u64 % (late_jitter + 1)) } else { gst::ClockTime::ZERO };
                    let since = t0.elapsed().as_secs_f64() - secs;
                    let grow = gst::ClockTime::from_nseconds((late_grow * since.max(0.0) * 1e6) as u64);
                    if since >= 0.0 {
                        let jitter = jitter + grow;
                        if let Some(b) = info.buffer_mut() {
                            let b = b.make_mut();
                            if let Some(p) = b.pts() {
                                b.set_pts(p.saturating_sub(late_by + jitter));
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        // Test only: STREAMER_DEBUG_SYSMIX_FAKE_PAUSE="PID SECS": that fake program's first
        // capture sends nothing for 500 ms from SECS s on (a capture that paused or stalled).
        if let Some((pid, secs)) = pid_secs("STREAMER_DEBUG_SYSMIX_FAKE_PAUSE").filter(|(p, _)| is_fake && *p == app.pid) {
            if !pause_used.swap(true, Ordering::SeqCst) {
                let t0 = std::time::Instant::now();
                eprintln!("host: test: the capture of fake{pid}.exe pauses in {secs} s");
                cf.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    let t = t0.elapsed().as_secs_f64();
                    if t >= secs && t < secs + 0.5 {
                        gst::PadProbeReturn::Drop
                    } else {
                        gst::PadProbeReturn::Ok
                    }
                });
            }
        }
        // Each piece of a program's sound is stamped with the stream's clock as it comes, as
        // the voices are in the shared output: the capture's own timestamps drifted away from
        // that clock over a stream, until the mixer took more and more of the sound for late
        // and put silence in its place (2026-11-24: 85-99% of it, only a new capture helped).
        // The mixer evens out the small unevenness of arrival (its pads' alignment threshold).
        // STREAMER_DEBUG_SYSMIX_NO_RESTAMP: the capture's own timestamps (the spoiled variant).
        // A pause in the capture needs no discontinuity flag: the mixer took the sound after
        // a 500 ms pause in its place at once with or without one (STREAMER_DEBUG_SYSMIX_FAKE_PAUSE,
        // 2026-11-24).
        // The clock and base time are the whole pipeline's, and only while it plays (a viewer
        // leaving pauses it, and the base time changes when the next one comes).
        if restamp {
            let cfw = cf.downgrade();
            cf.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(el) = cfw.upgrade() else { return gst::PadProbeReturn::Ok };
                let mut top: gst::Element = el;
                while let Some(p) = top.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                    top = p;
                }
                if top.current_state() != gst::State::Playing {
                    return gst::PadProbeReturn::Ok;
                }
                let (Some(clock), Some(base)) = (top.clock(), top.base_time()) else { return gst::PadProbeReturn::Ok };
                if let (Some(now), Some(b)) = (clock.time().checked_sub(base), info.buffer_mut()) {
                    let b = b.make_mut();
                    b.set_pts(now);
                    b.set_dts(gst::ClockTime::NONE);
                }
                gst::PadProbeReturn::Ok
            });
        }
        // The capture's level (F32 after the caps filter), for the watch below; its entry is
        // made once the branch is in the mix.
        let (lv, pid) = (levels_in.clone(), app.pid);
        cf.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                let (sum, n) = sum_squares_f32(map.as_slice());
                if let Some(e) = lv.lock().unwrap().get_mut(&pid) {
                    e.0 += sum;
                    e.1 += n;
                }
            }
            gst::PadProbeReturn::Ok
        });
        if std::env::var_os("STREAMER_DEBUG_SYSMIX_LEVEL").is_some() {
            let (acc, exe, n) = (std::sync::Mutex::new((0f64, 0usize)), app.exe.clone(), std::sync::atomic::AtomicU64::new(0));
            src.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let k = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if k == 0 {
                    eprintln!("host: sound of {exe}: first buffer, caps {:?}", pad.current_caps().map(|c| c.to_string()));
                }
                if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                    let mut a = acc.lock().unwrap();
                    for c in map.as_slice().chunks_exact(2) {
                        let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
                        a.0 += v * v;
                        a.1 += 1;
                    }
                    if k % 100 == 99 {
                        let db = if a.0 > 0.0 { 10.0 * (a.0 / a.1 as f64).log10() } else { -100.0 };
                        eprintln!("host: sound of {exe}: buffers {} level {db:.0} dB", k + 1);
                        *a = (0.0, 0);
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
        bin.add(&branch).map_err(|e| e.to_string())?;
        levels_in.lock().unwrap().insert(pid, (0.0, 0));
        let linked = (|| -> Result<(), String> {
            let mixer = bin.by_name("sysmix").ok_or("no mixer")?;
            let pad = mixer.request_pad_simple("sink_%u").ok_or("mixer pad")?;
            if let Err(e) = branch.static_pad("src").unwrap().link(&pad) {
                mixer.release_request_pad(&pad);
                return Err(e.to_string());
            }
            if let Some((pid, secs)) = cut.filter(|(p, _)| is_fake && *p == app.pid) {
                if !cut_used.swap(true, Ordering::SeqCst) {
                    let (t0, k) = (std::time::Instant::now(), std::sync::atomic::AtomicU64::new(0));
                    eprintln!("host: test: the mixer gets fake{pid}.exe cut in {secs} s");
                    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                        if t0.elapsed().as_secs_f64() >= secs && k.fetch_add(1, Ordering::Relaxed) % 20 != 0 {
                            if let Some(b) = info.buffer_mut() {
                                if let Ok(mut m) = b.make_mut().map_writable() {
                                    m.as_mut_slice().fill(0);
                                }
                            }
                        }
                        gst::PadProbeReturn::Ok
                    });
                }
            }
            if std::env::var_os("STREAMER_DEBUG_SYSMIX_LEVEL").is_some() {
                let (exe, n) = (app.exe.clone(), std::sync::atomic::AtomicU64::new(0));
                pad.add_probe(gst::PadProbeType::BUFFER, move |p, info| {
                    let k = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k % 100 == 0 {
                        let nz = info.buffer().and_then(|b| b.map_readable().ok()).map(|m| m.as_slice().iter().filter(|x| **x != 0).count()).unwrap_or(0);
                        eprintln!("host: mixer input {exe}: buffer {k}, nonzero bytes {nz}, caps {:?}, pts {:?}", p.current_caps().map(|c| c.to_string()), info.buffer().and_then(|b| b.pts()));
                    }
                    gst::PadProbeReturn::Ok
                });
            }
            branch.sync_state_with_parent().map_err(|e| e.to_string())?;
            Ok(())
        })();
        // A half-made branch would stay under the program's name, and nothing would add it again.
        if linked.is_err() {
            levels_in.lock().unwrap().remove(&pid);
            remove_app_branch_in_time(bin, branch.upcast_ref::<gst::Element>());
        }
        linked
    };
    // The programs to capture, and every program with sound (their meters).
    let wanted = move || -> (Vec<crate::audio_apps::AudioApp>, Vec<crate::audio_apps::AudioApp>) {
        if let Some(f) = &fake {
            let t = fake_started.elapsed().as_secs_f64();
            let v: Vec<_> = f
                .iter()
                .enumerate()
                .filter(|(_, (a, b))| (*a..*b).contains(&t))
                .map(|(i, _)| crate::audio_apps::AudioApp { pid: i as u32 + 1, exe: format!("fake{}.exe", i + 1), active: true, peak: 0.5 })
                .collect();
            return (v.clone(), v);
        }
        let all = crate::audio_apps::audio_apps();
        (crate::audio_apps::to_capture(&all).into_iter().filter(|a| !exclude.contains(&a.exe.to_ascii_lowercase())).collect(), all)
    };
    let (first, _) = wanted();
    eprintln!("host: system sound from {}", first.iter().map(|a| a.exe.as_str()).collect::<Vec<_>>().join(", "));
    let mut watch = CaptureWatch::default();
    for a in &first {
        match add(&bin, a) {
            Ok(()) => watch.born(a.pid),
            Err(e) => eprintln!("host: sound of {}: {e}", a.exe),
        }
    }
    let weak = bin.downgrade();
    std::thread::Builder::new()
        .name("system-sound".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let Some(bin) = weak.upgrade() else { return };
            if bin.parent().is_none() && bin.current_state() == gst::State::Null {
                return;
            }
            let (now, all) = wanted();
            let heard: HashMap<u32, (f64, u64)> = levels.lock().unwrap().iter_mut().map(|(p, v)| (*p, std::mem::take(v))).collect();
            let mix = std::mem::take(&mut *mix_level.lock().unwrap());
            let live = bin.current_state() == gst::State::Playing;
            if !live {
                watch.pause();
            } else if watch.resumed(bin.base_time()) {
                // Every capture starts anew with the pipeline (also when a viewer left and
                // another came between two looks): each gets its grace again.
                for a in &now {
                    watch.regrace(a.pid);
                }
            }
            let parents = if is_fake { HashMap::new() } else { crate::audio_apps::parents() };
            // A program that plays while its capture gives silence: that capture starts anew.
            let mut again: Vec<&crate::audio_apps::AudioApp> = Vec::new();
            // Each capture's level over the same time as the mix's (the samples the mixer gave,
            // same format): a capture that sent sound only in bursts is not louder for it.
            let mut loudest = f64::NEG_INFINITY;
            for a in &now {
                let peak = if is_fake { a.peak } else { crate::audio_apps::tree_peak(&all, &parents, a.pid) };
                let (sum, n) = heard.get(&a.pid).copied().unwrap_or_default();
                let db = level_db(sum, n);
                watch.minute_add(a.pid, sum, n, peak);
                if watch.born_while(a.pid, CaptureWatch::GRACE) {
                    continue;
                }
                loudest = loudest.max(level_db(sum, n.max(mix.1)));
                if live && watch.restart_due(a.pid, peak, db) {
                    eprintln!("host: system sound starts the capture of {} again: it plays (meter {:.0} dB) but the capture gave {db:.0} dB", a.exe, 20.0 * (peak as f64).max(1e-9).log10());
                    again.push(a);
                }
            }
            // Programs heard while the mix is silent, or much quieter than they are: the mixer
            // drops their sound, whole or in pieces (its timing went wrong); every capture
            // starts anew, with fresh timestamps. No buffers out of the mixer counts as silent.
            let mix_db = level_db(mix.0, mix.1);
            watch.mix_add(mix.0, mix.1);
            if live && watch.mix_silent_due(loudest, mix_db) {
                eprintln!("host: system sound: the mix gave {mix_db:.0} dB while a program's capture gave {loudest:.0} dB (the mixer drops its sound); every capture starts again");
                again = now.iter().filter(|a| !watch.is_abandoned(a.pid) && watch.may_add(a.pid)).collect();
            }
            for a in again {
                if let Some(el) = bin.by_name(&format!("app{}", a.pid)) {
                    if !remove_app_branch_in_time(&bin, &el) {
                        eprintln!("host: system sound: the capture of {} did not stop in 3 s; left as it is", a.exe);
                        watch.abandon(a.pid);
                        continue;
                    }
                }
                match add(&bin, a) {
                    Ok(()) => watch.born(a.pid),
                    Err(e) => {
                        eprintln!("host: sound of {}: {e}", a.exe);
                        watch.add_failed(a.pid);
                        // Its half-made branch did not leave in time: not touched again.
                        if bin.by_name(&format!("app{}", a.pid)).is_some() {
                            watch.abandon(a.pid);
                        }
                    }
                }
            }
            watch.log_levels(live, &now);
            for a in &now {
                if bin.by_name(&format!("app{}", a.pid)).is_none() && watch.may_add(a.pid) {
                    eprintln!("host: system sound adds {}", a.exe);
                    match add(&bin, a) {
                        Ok(()) => watch.born(a.pid),
                        Err(e) => {
                            eprintln!("host: sound of {}: {e}", a.exe);
                            watch.add_failed(a.pid);
                            // Its half-made branch did not leave in time: not touched again.
                            if bin.by_name(&format!("app{}", a.pid)).is_some() {
                                watch.abandon(a.pid);
                            }
                        }
                    }
                }
            }
            // A program gone (or now excluded): its branch leaves.
            for el in bin.children() {
                let name = el.name();
                let Some(pid) = name.strip_prefix("app").and_then(|p| p.parse::<u32>().ok()) else { continue };
                if now.iter().any(|a| a.pid == pid) {
                    continue;
                }
                // Already asked to stop and hanging there: not again (each ask is a thread).
                if watch.is_abandoned(pid) {
                    continue;
                }
                eprintln!("host: system sound drops a program ({pid})");
                levels.lock().unwrap().remove(&pid);
                if remove_app_branch_in_time(&bin, &el) {
                    watch.forget(pid);
                } else {
                    eprintln!("host: system sound: the capture of a program that left ({pid}) did not stop in 3 s");
                    watch.abandon(pid);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(bin.upcast())
}

/// Sum of squares and count of F32LE samples.
#[cfg(windows)]
fn sum_squares_f32(bytes: &[u8]) -> (f64, u64) {
    let (mut sum, mut n) = (0f64, 0u64);
    for c in bytes.chunks_exact(4) {
        let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64;
        sum += v * v;
        n += 1;
    }
    (sum, n)
}

/// RMS level in dB, or -150 for nothing or digital silence.
#[cfg(windows)]
fn level_db(sum: f64, n: u64) -> f64 {
    if n > 0 && sum > 0.0 {
        10.0 * (sum / n as f64).log10()
    } else {
        -150.0
    }
}

/// Takes a program's capture out of the system sound mix.
#[cfg(windows)]
fn remove_app_branch(bin: &gst::Bin, el: &gst::Element) {
    let peer = el.static_pad("src").and_then(|p| p.peer());
    let _ = el.set_state(gst::State::Null);
    if let (Some(peer), Some(mixer)) = (peer, bin.by_name("sysmix")) {
        let _ = el.static_pad("src").unwrap().unlink(&peer);
        mixer.release_request_pad(&peer);
    }
    let _ = bin.remove(el);
}

/// As [`remove_app_branch`], on a thread of its own: false when it did not finish in 3 s (a
/// capture that hangs in its stop must not stop the watch of all the others).
#[cfg(windows)]
fn remove_app_branch_in_time(bin: &gst::Bin, el: &gst::Element) -> bool {
    let (tx, rx) = mpsc::channel();
    let (b, e) = (bin.clone(), el.clone());
    let spawned = std::thread::Builder::new().name("sound-branch-stop".into()).spawn(move || {
        remove_app_branch(&b, &e);
        let _ = tx.send(());
    });
    spawned.is_ok() && rx.recv_timeout(Duration::from_secs(3)).is_ok()
}

/// Watches each program's capture in the system sound mix against what Windows says the
/// program plays, and the mix against its inputs: a stream went quiet until a viewer joined
/// again and every capture started anew (reported 2026-11-23). STREAMER_DEBUG_SYSMIX_NO_WATCH
/// turns the restarts off (the spoiled variant).
#[cfg(windows)]
#[derive(Default)]
struct CaptureWatch {
    /// Looks in a row (2 s each) a program played while its capture was silent.
    silent: HashMap<u32, u32>,
    /// Restarts of each program's capture since it was last heard, and the last one.
    restarts: HashMap<u32, (u32, std::time::Instant)>,
    /// When each program's capture (re)started.
    born: HashMap<u32, std::time::Instant>,
    /// Captures that did not stop when asked: never touched again.
    abandoned: std::collections::HashSet<u32>,
    /// Looks in a row the mix was silent while a capture was heard, restarts of all since the
    /// mix was last heard, and the last one.
    mix_silent: u32,
    mix_restarts: (u32, Option<std::time::Instant>),
    /// Looks in a row the mix was fine (the backoff starts over only after `GOOD_LOOKS`).
    mix_good: u32,
    /// This minute's sums: each capture's (sum of squares, samples, loudest meter), the mix's.
    minute: HashMap<u32, (f64, u64, f32)>,
    mix_minute: (f64, u64),
    last_log: Option<std::time::Instant>,
    /// The pipeline was not playing at the last look.
    paused: bool,
    /// When adding a program's capture last failed (not again for a while).
    failed: HashMap<u32, std::time::Instant>,
    /// The pipeline's base time at the last look while playing.
    base: Option<gst::ClockTime>,
}

#[cfg(windows)]
impl CaptureWatch {
    /// Louder than this on its meter, a program plays (-50 dB).
    const PLAYS: f32 = 0.003;
    /// Quieter than this, a capture or the mix is silent.
    const SILENT_DB: f64 = -80.0;
    /// Louder than this, a capture is heard (for the mix check). -70, not -60: a quiet
    /// browser tab (-66 dB) had its sound cut to -83 dB in the mix (2026-11-24).
    const HEARD_DB: f64 = -70.0;
    /// The mix this much quieter than its loudest input: the mixer drops sound (a mix is
    /// never quieter than one of its inputs, short of sounds that cancel out).
    const CUT_DB: f64 = 10.0;
    /// Looks in a row the mix must be fine before restarts count from the start again: 30 s
    /// (a mix cut now and then would otherwise restart every capture every few seconds).
    const GOOD_LOOKS: u32 = 15;
    /// Looks in a row before a restart: 4 s.
    const LOOKS: u32 = 2;
    /// A capture just started is left alone this long (its first sound may take a while).
    const GRACE: std::time::Duration = std::time::Duration::from_secs(6);

    /// Wait before the next restart after `n` in a row that did not help: 20 s, 40 s, 80 s,
    /// then every 2 min (each restart costs only that program's sound, already silent).
    fn backoff(n: u32) -> std::time::Duration {
        std::time::Duration::from_secs((20u64 << n.saturating_sub(1).min(3)).min(120))
    }

    fn born(&mut self, pid: u32) {
        self.born.insert(pid, std::time::Instant::now());
        self.silent.remove(&pid);
        self.abandoned.remove(&pid);
        self.failed.remove(&pid);
    }

    fn is_abandoned(&self, pid: u32) -> bool {
        self.abandoned.contains(&pid)
    }

    fn add_failed(&mut self, pid: u32) {
        self.failed.insert(pid, std::time::Instant::now());
    }

    /// A program whose capture could not be added is tried again after 6 s, not every look.
    fn may_add(&self, pid: u32) -> bool {
        self.failed.get(&pid).is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(6))
    }

    /// Playing again after a pause, or started anew between two looks (a new base time):
    /// true once.
    fn resumed(&mut self, base: Option<gst::ClockTime>) -> bool {
        let new_start = self.base.is_some() && base != self.base;
        self.base = base;
        std::mem::replace(&mut self.paused, false) || new_start
    }

    /// A fresh grace period, as at a start; an abandoned capture stays abandoned.
    fn regrace(&mut self, pid: u32) {
        self.born.insert(pid, std::time::Instant::now());
        self.silent.remove(&pid);
    }

    fn born_while(&self, pid: u32, d: std::time::Duration) -> bool {
        self.born.get(&pid).is_some_and(|t| t.elapsed() < d)
    }

    fn abandon(&mut self, pid: u32) {
        self.abandoned.insert(pid);
    }

    fn forget(&mut self, pid: u32) {
        self.silent.remove(&pid);
        self.restarts.remove(&pid);
        self.born.remove(&pid);
        self.abandoned.remove(&pid);
        self.minute.remove(&pid);
        self.failed.remove(&pid);
    }

    /// Not live (no viewer): nothing flows, the counts start over.
    fn pause(&mut self) {
        self.silent.clear();
        self.restarts.clear();
        self.mix_silent = 0;
        self.mix_restarts = (0, None);
        self.mix_good = 0;
        self.paused = true;
    }

    fn due(restarts: Option<(u32, std::time::Instant)>) -> bool {
        restarts.is_none_or(|(n, at)| at.elapsed() >= Self::backoff(n))
    }

    fn restart_due(&mut self, pid: u32, peak: f32, db: f64) -> bool {
        if db >= Self::SILENT_DB {
            // Heard: any restarts so far helped, the count starts over.
            self.silent.remove(&pid);
            self.restarts.remove(&pid);
            return false;
        }
        if peak < Self::PLAYS || self.abandoned.contains(&pid) {
            self.silent.remove(&pid);
            return false;
        }
        let k = self.silent.entry(pid).or_default();
        *k += 1;
        if *k < Self::LOOKS || std::env::var_os("STREAMER_DEBUG_SYSMIX_NO_WATCH").is_some() || !Self::due(self.restarts.get(&pid).copied()) {
            return false;
        }
        let n = self.restarts.get(&pid).map_or(0, |r| r.0);
        self.restarts.insert(pid, (n + 1, std::time::Instant::now()));
        self.silent.remove(&pid);
        true
    }

    /// The mix silent, or `CUT_DB` quieter than its loudest input, while that input is heard:
    /// two looks in a row (with the backoff after restarts that did not help).
    /// STREAMER_DEBUG_SYSMIX_WATCH_SILENT_ONLY: only a silent mix counts (as in 0.2.23).
    fn mix_silent_due(&mut self, loudest_input_db: f64, mix_db: f64) -> bool {
        let silent_only = std::env::var_os("STREAMER_DEBUG_SYSMIX_WATCH_SILENT_ONLY").is_some();
        // Nothing heard: nothing to judge (a pause in a program's sound neither helps nor
        // hurts the backoff).
        if loudest_input_db < Self::HEARD_DB {
            self.mix_silent = 0;
            return false;
        }
        let cut = mix_db < Self::SILENT_DB || (!silent_only && mix_db < loudest_input_db - Self::CUT_DB);
        if !cut {
            self.mix_silent = 0;
            self.mix_good += 1;
            if self.mix_good >= Self::GOOD_LOOKS {
                self.mix_restarts = (0, None);
            }
            return false;
        }
        self.mix_good = 0;
        self.mix_silent += 1;
        let (n, at) = self.mix_restarts;
        if self.mix_silent < Self::LOOKS || std::env::var_os("STREAMER_DEBUG_SYSMIX_NO_WATCH").is_some() || !Self::due(at.map(|t| (n, t))) {
            return false;
        }
        self.mix_restarts = (n + 1, Some(std::time::Instant::now()));
        self.mix_silent = 0;
        true
    }

    fn minute_add(&mut self, pid: u32, sum: f64, n: u64, peak: f32) {
        let e = self.minute.entry(pid).or_default();
        e.0 += sum;
        e.1 += n;
        e.2 = e.2.max(peak);
    }

    fn mix_add(&mut self, sum: f64, n: u64) {
        self.mix_minute.0 += sum;
        self.mix_minute.1 += n;
    }

    /// Once a minute while live: each program's capture level over the minute and the loudest
    /// its meter showed, and the mix's level.
    fn log_levels(&mut self, live: bool, now: &[crate::audio_apps::AudioApp]) {
        if !live {
            self.minute.clear();
            self.mix_minute = (0.0, 0);
            self.last_log = None;
            return;
        }
        let Some(t) = self.last_log else {
            self.last_log = Some(std::time::Instant::now());
            return;
        };
        if t.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }
        self.last_log = Some(std::time::Instant::now());
        let db = |sum: f64, n: u64| if n == 0 { "nothing".to_string() } else if sum > 0.0 { format!("{:.0}", 10.0 * (sum / n as f64).log10()) } else { "silent".into() };
        let rows: Vec<String> = now
            .iter()
            .map(|a| {
                let (sum, n, peak) = self.minute.get(&a.pid).copied().unwrap_or_default();
                let name = if a.exe.is_empty() { format!("pid {}", a.pid) } else { a.exe.clone() };
                format!("{name} {} (meter up to {:.0})", db(sum, n), 20.0 * (peak as f64).max(1e-9).log10())
            })
            .collect();
        eprintln!("host: system sound over the last minute, dB: mix {}; {}", db(self.mix_minute.0, self.mix_minute.1), rows.join(", "));
        self.minute.clear();
        self.mix_minute = (0.0, 0);
    }
}

fn audio_source(cfg: &HostConfig) -> Result<gst::Element, String> {
    match cfg.source {
        // Tests only: STREAMER_DEBUG_TEST_AUDIO=<audiotestsrc wave> instead of ticks (the
        // echo gate needs sound all the time, as speech or a game has).
        // Tests: STREAMER_DEBUG_BARS_SYSTEM_SOUND gives the test pattern the system's real sound.
        Source::Bars | Source::Noise if std::env::var_os("STREAMER_DEBUG_BARS_SYSTEM_SOUND").is_none() => gst::ElementFactory::make("audiotestsrc")
            .name("audiosrc")
            .property("is-live", true)
            .property_from_str("wave", &std::env::var("STREAMER_DEBUG_TEST_AUDIO").unwrap_or_else(|_| "ticks".into()))
            .property("volume", 0.3f64)
            .build()
            .map_err(|e| e.to_string()),
        Source::Capture { .. } | Source::Monitor { .. } | Source::Window { .. } | Source::Bars | Source::Noise => {
            let src = gst::ElementFactory::make("wasapi2src")
                .name("audiosrc")
                .property("loopback", true)
                .property("low-latency", true)
                .build()
                .map_err(|e| format!("system audio capture unavailable: {e}"))?;
            if !src.has_property("loopback-mode") {
                if cfg.audio_pid.is_some() {
                    return Err("this Windows build cannot capture one program's audio".into());
                }
                eprintln!("warning: this Windows build cannot exclude our own audio from capture");
                return Ok(src);
            }
            match cfg.audio_pid {
                // One program only.
                Some(pid) => {
                    src.set_property_from_str("loopback-mode", "include-process-tree");
                    src.set_property("loopback-target-pid", pid);
                    // Not the pipeline's clock either: the program may end mid-stream.
                    // STREAMER_DEBUG_SYSMIX_PROVIDE_CLOCK: as in 0.2.19 (the spoiled variant).
                    if std::env::var_os("STREAMER_DEBUG_SYSMIX_PROVIDE_CLOCK").is_none() && src.has_property("provide-clock") {
                        src.set_property("provide-clock", false);
                    }
                }
                // Everything but our own playback, one program at a time (sound routers would
                // bring ours back). STREAMER_DEBUG_SYSTEM_EXCLUDE_SELF: one capture of all but
                // our process, as before (a router's copy of our sound gets in).
                #[cfg(windows)]
                None if std::env::var_os("STREAMER_DEBUG_SYSTEM_EXCLUDE_SELF").is_none() => return system_sound(&cfg.audio_exclude),
                None => {
                    src.set_property_from_str("loopback-mode", "exclude-process-tree");
                    src.set_property("loopback-target-pid", std::process::id());
                }
            }
            Ok(src)
        }
    }
}

impl Host {
    pub fn new(cfg: HostConfig) -> Result<Self, String> {
        let (encoder, kind) = platform::working_encoder(cfg.codec).ok_or("no working hardware encoder for this codec")?;
        let desc = format!("{} ! tee name=srctee allow-not-linked=true", video_source(&cfg));
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| format!("video pipeline: {e}"))?
            .downcast::<gst::Pipeline>()
            .unwrap();

        if cfg.audio {
            let mk = |f: &str| gst::ElementFactory::make(f).build().map_err(|e| format!("{f}: {e}"));
            let caps = gst::Caps::builder("audio/x-raw").field("rate", 48000i32).field("channels", 2i32).build();
            let filter = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
            let queue = gst::ElementFactory::make("queue")
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 100_000_000u64)
                .property_from_str("leaky", "downstream")
                .build()
                .unwrap();
            let opus = mk("opusenc")?;
            opus.set_property_from_str("audio-type", "restricted-lowdelay");
            opus.set_property_from_str("frame-size", "10");
            opus.set_property("bitrate", 128_000i32);
            opus.set_property_from_str("bitrate-type", "cbr");
            let tee = gst::ElementFactory::make("tee")
                .name("atee")
                .property("allow-not-linked", true)
                .build()
                .unwrap();
            crate::diag::watch_ts(&opus.static_pad("sink").unwrap(), "sound to the encoder".into());
            crate::diag::watch_queue_drops(&queue, "sound before the encoder".into());
            let chain = [audio_source(&cfg)?, mk("audioconvert")?, mk("audioresample")?, filter, queue, opus, tee];
            pipeline.add_many(&chain).unwrap();
            gst::Element::link_many(&chain).map_err(|e| format!("audio link: {e}"))?;
        }

        debug_source_error(&pipeline);
        debug_source_stuck(&pipeline);
        // An error from the capture itself (not an encoder or a viewer's branch) ends it:
        // WGC says "Capture item was closed" and sends EOS on, so nothing more comes.
        let lost = Arc::new(SourceLost::default());
        let (l, weak) = (lost.clone(), pipeline.downgrade());
        crate::drain_bus_with(
            &pipeline,
            "host",
            Some(Box::new(move |e: String, src: Option<gst::Object>| {
                let (Some(p), Some(src)) = (weak.upgrade(), src) else { return };
                let ours = capture_elements(&p).iter().any(|c| src == *c.upcast_ref::<gst::Object>() || src.has_as_ancestor(c));
                // STREAMER_DEBUG_KEEP_LOST_SOURCE: the stream goes on without it (the spoiled variant).
                if ours && std::env::var_os("STREAMER_DEBUG_KEEP_LOST_SOURCE").is_none() {
                    l.fail(e);
                }
            })),
        );
        let ceiling = Quality::new(cfg.mode, cfg.kbit);
        Ok(Self {
            pipeline,
            stats: Arc::new(HostStats::default()),
            encoder: encoder.to_string(),
            kind,
            cfg: Mutex::new(cfg),
            ceiling: Mutex::new(ceiling),
            source_changed: AtomicBool::new(false),
            viewers: Mutex::default(),
            layers: Mutex::default(),
            preview: Mutex::default(),
            lost,
            pinned: Mutex::default(),
            on_viewer_failed: Arc::default(),
        })
    }

    /// `f` is told, once, when the capture fails for good (its window closed); at once if it
    /// already did. Called on the pipeline's bus thread.
    pub fn on_source_lost(&self, f: Box<dyn Fn(String) + Send>) {
        // Under `reason`'s lock: a failure cannot slip in between the check and the setting.
        let reason = self.lost.reason.lock().unwrap();
        match reason.clone() {
            Some(r) => {
                drop(reason);
                f(r)
            }
            None => *self.lost.tell.lock().unwrap() = Some(f),
        }
    }

    pub fn cfg(&self) -> HostConfig {
        self.cfg.lock().unwrap().clone()
    }

    /// The host's own quality: the most any viewer gets.
    pub fn ceiling(&self) -> Quality {
        *self.ceiling.lock().unwrap()
    }

    /// Changes the stream while it runs: the picture source, the quality, which program's
    /// sound. Viewers stay connected. Each viewer's quality follows: one that watches at the
    /// host's own quality moves with it, up or down; one that asked for less keeps that,
    /// unless the new quality is lower still. The codec, and sound on or off, cannot change
    /// this way (they change what was negotiated with each viewer).
    pub fn reconfigure(&self, new: HostConfig) -> Result<(), String> {
        let old = self.cfg();
        if new.codec != old.codec {
            return Err("the codec cannot change while live".into());
        }
        if new.audio != old.audio {
            return Err("sound on or off cannot change while live".into());
        }
        eprintln!("host: reconfigure from {} to {}", self.ceiling(), Quality::new(new.mode, new.kbit));
        let viewers = self.viewers.lock().unwrap();
        if new.source != old.source || new.mode.fps != old.mode.fps {
            self.swap_video_source(&new)?;
            self.source_changed.store(new.source != old.source, Ordering::SeqCst);
        }
        if new.audio && (new.audio_pid != old.audio_pid || new.audio_exclude != old.audio_exclude) {
            self.swap_audio_source(&new)?;
        }
        let ceiling = Quality::new(new.mode, new.kbit);
        *self.cfg.lock().unwrap() = new;
        *self.ceiling.lock().unwrap() = ceiling;
        // Layers keep their quality; their picture takes the new source's shape.
        for (q, l) in self.layers.lock().unwrap().iter() {
            if let Some(f) = l.bin.by_name("vsize") {
                let w = self.layer_width(*q);
                let caps = gst::Caps::builder("video/x-raw").features(["memory:D3D11Memory"]).field("format", "NV12").field("width", w as i32).field("height", q.height as i32).build();
                f.set_property("caps", &caps);
            }
        }
        let moves: Vec<(u64, Quality)> = viewers
            .iter()
            .map(|(id, b)| (*id, b.want.map_or(ceiling, |w| w.clamp_to(ceiling))))
            .collect();
        drop(viewers);
        // STREAMER_DEBUG_NO_FOLLOW: viewers keep their layer (the spoiled variant).
        let moves = if std::env::var_os("STREAMER_DEBUG_NO_FOLLOW").is_some() { vec![] } else { moves };
        for (id, q) in moves {
            self.move_viewer(id, q)?;
        }
        eprintln!("host: reconfigured to {ceiling}");
        Ok(())
    }

    /// Replaces the capture in front of the capture tee; the layers and viewers stay.
    /// Neither the old capture's stop nor the new one's start may hold the stream (and the
    /// app, which waits on the same locks): a window capture (WGC) can hang in either
    /// (2026-11-21: the app froze for good on the second change of window).
    fn swap_video_source(&self, cfg: &HostConfig) -> Result<(), String> {
        const START_WAIT: Duration = Duration::from_secs(5);
        let tee = self.pipeline.by_name("srctee").ok_or("no capture tee")?;
        let sink = tee.static_pad("sink").ok_or("no tee sink")?;
        let old = capture_elements(&self.pipeline);
        let bin = gst::parse::bin_from_description(&video_source(cfg), true).map_err(|e| format!("new source: {e}"))?;
        // The new capture starts on its own first (with the pipeline's GPU device), so one
        // that cannot start leaves the old one running, and one that never answers is left
        // behind after START_WAIT. A live source sends nothing before it plays.
        for c in self.pipeline.contexts() {
            bin.set_context(&c);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let b = bin.clone();
        std::thread::spawn(move || {
            let _ = tx.send(b.set_state(gst::State::Paused));
        });
        match rx.recv_timeout(START_WAIT) {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                let _ = bin.set_state(gst::State::Null);
                return Err("new source: it cannot be captured".into());
            }
            Err(_) => {
                eprintln!("host: the new source did not start in {} s; left behind", START_WAIT.as_secs());
                return Err("new source: it did not start".into());
            }
        }
        debug_source_stuck_now();
        // The old one: off the tee and out of the pipeline at once, stopped on its own thread.
        // STREAMER_DEBUG_SWAP_INLINE_STOP: stopped here first, as before (the spoiled variant).
        if std::env::var_os("STREAMER_DEBUG_SWAP_INLINE_STOP").is_some() {
            for e in &old {
                let _ = e.set_state(gst::State::Null);
            }
        }
        if let Some(peer) = sink.peer() {
            let _ = peer.unlink(&sink);
        }
        for e in &old {
            let _ = self.pipeline.remove(e);
        }
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            for e in &old {
                let _ = e.set_state(gst::State::Null);
            }
            eprintln!("host: old source stopped in {} ms", t.elapsed().as_millis());
        });
        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        bin.static_pad("src").ok_or("new source has no output")?.link(&sink).map_err(|e| format!("new source: {e}"))?;
        bin.sync_state_with_parent().map_err(|e| format!("new source: {e}"))?;
        eprintln!("host: new source {:?} in place", cfg.source);
        Ok(())
    }

    /// Replaces the sound capture (whole system or one program).
    fn swap_audio_source(&self, cfg: &HostConfig) -> Result<(), String> {
        let src = self.pipeline.by_name("audiosrc").ok_or("no sound capture")?;
        let next = src.static_pad("src").and_then(|p| p.peer()).ok_or("sound capture not linked")?;
        let new = audio_source(cfg)?;
        let _ = src.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&src);
        self.pipeline.add(&new).map_err(|e| e.to_string())?;
        new.static_pad("src").ok_or("no sound output")?.link(&next).map_err(|e| format!("sound: {e}"))?;
        new.sync_state_with_parent().map_err(|e| format!("sound: {e}"))?;
        Ok(())
    }

    /// Turns the streamer's own small picture on or off. Off, its frames are dropped at the
    /// capture tee and nothing downstream runs.
    pub fn set_preview(&self, on: bool) -> Result<(), String> {
        // Before the first viewer nothing runs: the preview starts the capture (no encoder:
        // layers come with viewers) and stops it again when it turns off, if still nobody
        // came. Once a viewer started it, the capture stays as before.
        // Lock order: `viewers`, then `layers`, then `preview`. Called by the window every
        // tick: never waits for a change in progress (`reconfigure` holds `viewers`); the
        // next tick asks again.
        let viewers = match self.viewers.try_lock() {
            Ok(v) => v,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let idle = viewers.is_empty() && self.layers.lock().unwrap().is_empty();
        let mut p = self.preview.lock().unwrap();
        let r = self.switch_preview(&mut p, on);
        if idle && std::env::var_os("STREAMER_DEBUG_PREVIEW_NO_CAPTURE").is_none() {
            let target = if on && r.is_ok() { gst::State::Playing } else { gst::State::Null };
            if self.pipeline.current_state() != target && self.pipeline.pending_state() != target {
                eprintln!("host: capture {} for the preview", if target == gst::State::Playing { "started" } else { "stopped" });
                self.pipeline.set_state(target).map_err(|e| format!("preview capture: {e}"))?;
            }
        }
        drop(viewers);
        r
    }

    fn switch_preview(&self, p: &mut Option<Preview>, on: bool) -> Result<(), String> {
        if p.is_none() {
            if !on {
                return Ok(());
            }
            *p = Some(self.make_preview()?);
        }
        let p = p.as_ref().unwrap();
        // STREAMER_DEBUG_PREVIEW_STUCK: it never turns on (the spoiled variant).
        let on = on && std::env::var_os("STREAMER_DEBUG_PREVIEW_STUCK").is_none();
        if p.on.swap(on, Ordering::SeqCst) != on {
            eprintln!("host: preview {}", if on { "on" } else { "off" });
        }
        Ok(())
    }

    /// The latest small picture since the last call: width, height and BGRA rows.
    pub fn preview_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        let p = self.preview.lock().unwrap();
        let p = p.as_ref().filter(|p| p.on.load(Ordering::Relaxed))?;
        let sample = p.sink.try_pull_sample(gst::ClockTime::ZERO)?;
        let s = sample.caps()?.structure(0)?;
        let (w, h) = (s.get::<i32>("width").ok()? as usize, s.get::<i32>("height").ok()? as usize);
        let map = sample.buffer()?.map_readable().ok()?;
        let stride = map.len().checked_div(h).unwrap_or(0);
        if w == 0 || stride < w * 4 {
            return None;
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for row in map.chunks_exact(stride).take(h) {
            out.extend_from_slice(&row[..w * 4]);
        }
        Some((w as u32, h as u32, out))
    }

    fn make_preview(&self) -> Result<Preview, String> {
        let desc = format!(
            "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! \
             videorate drop-only=true max-rate={PREVIEW_FPS} ! d3d11convert ! \
             capsfilter caps=\"video/x-raw(memory:D3D11Memory),format=BGRA,width={PREVIEW_W},height={PREVIEW_H}\" ! \
             d3d11download ! capsfilter caps=\"video/x-raw,format=BGRA\" ! \
             appsink name=psink max-buffers=1 drop=true sync=false enable-last-sample=false"
        );
        let bin = gst::parse::bin_from_description(&desc, true).map_err(|e| format!("preview: {e}"))?;
        bin.set_property("name", "preview");
        let sink = bin.by_name("psink").unwrap().downcast::<gst_app::AppSink>().map_err(|_| "preview: no appsink")?;
        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        let feed = self.pipeline.by_name("srctee").ok_or("no capture tee")?.request_pad_simple("src_%u").ok_or("preview: no tee pad")?;
        let on = Arc::new(AtomicBool::new(false));
        let o = on.clone();
        // Buffers only: caps and other events still pass, so it is ready the moment it turns on.
        feed.add_probe(gst::PadProbeType::BUFFER, move |_, _| if o.load(Ordering::Relaxed) { gst::PadProbeReturn::Ok } else { gst::PadProbeReturn::Drop });
        feed.link(&bin.static_pad("sink").unwrap()).map_err(|e| format!("preview: {e}"))?;
        bin.sync_state_with_parent().map_err(|e| format!("preview: {e}"))?;
        Ok(Preview { sink, on })
    }

    /// The encoded width for a quality: its height times the source's shape, even, at most
    /// 4096 (AMD's limit). Modes are named by height; a 21:9 screen at 1440p stays
    /// 3440x1440 instead of being squeezed into 2560x1440. A 16:9 or unknown source gives
    /// the mode's own width.
    fn layer_width(&self, q: Quality) -> u32 {
        let src = self
            .pipeline
            .by_name("srctee")
            .and_then(|t| t.static_pad("sink"))
            .and_then(|p| p.current_caps())
            .and_then(|c| {
                let s = c.structure(0)?;
                Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
            });
        // Before the first frame the tee has no caps yet: take the size from the source.
        // Right after the source changed (`reconfigure`) the tee still has the old one's.
        let src = if self.source_changed.load(Ordering::SeqCst) { None } else { src };
        let src = src.or_else(|| source_size(&self.cfg().source));
        let w = match src {
            Some((sw, sh)) if sw > 0 && sh > 0 => {
                let exact = q.height as f64 * sw as f64 / sh as f64;
                ((exact / 2.0).round() as u32 * 2).clamp(2, 4096)
            }
            _ => q.width,
        };
        if std::env::var_os("STREAMER_DEBUG_LAYER_SIZE").is_some() {
            eprintln!("layer {q}: source {src:?} -> encoded {w}x{}", q.height);
        }
        w
    }

    /// Builds the encoder chain for one quality and starts it with the pipeline.
    fn make_layer(&self, q: Quality) -> Result<Layer, String> {
        let c = self.cfg().codec.short();
        // A leaky queue first: a slow encoder drops its own frames and never holds up the
        // capture or the other layers.
        let rate = if q.fps < self.cfg().mode.fps { format!("videorate drop-only=true max-rate={} ! ", q.fps) } else { String::new() };
        let desc = format!(
            "queue name=lqueue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! {rate}\
             d3d11convert ! capsfilter name=vsize caps=\"video/x-raw(memory:D3D11Memory),format=NV12,width={w},height={h}\" ! \
             {enc} name=venc {props} ! {parse} ! \
             tee name=ltee allow-not-linked=true",
            w = self.layer_width(q),
            h = q.height,
            enc = self.encoder,
            props = platform::encoder_properties(self.kind, self.cfg().codec, q.kbit, q.fps, q.fps * self.cfg().keyframe_secs),
            parse = if self.cfg().codec == crate::config::Codec::Av1 {
                "av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu".to_string()
            } else {
                format!("{c}parse config-interval=-1 ! video/x-{c},alignment=au")
            },
        );
        let bin = gst::parse::bin_from_description(&desc, true).map_err(|e| format!("layer {q}: {e}"))?;
        bin.set_property("name", format!("layer-{}x{}-{}-{}", q.width, q.height, q.fps, q.kbit));
        let tee = bin.by_name("ltee").unwrap();
        if q.fps < self.cfg().mode.fps && std::env::var_os("STREAMER_DEBUG_VIDEORATE_ONLY").is_none() {
            thin_frames(&bin.by_name("lqueue").unwrap(), q.fps);
        }
        let stats = if q == self.ceiling() { self.stats.clone() } else { Arc::new(HostStats::default()) };
        watch_layer(&tee, stats.clone());
        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        let feed = self.pipeline.by_name("srctee").unwrap().request_pad_simple("src_%u").unwrap();
        let parked = Arc::new(AtomicBool::new(false));
        let p = parked.clone();
        feed.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if p.load(Ordering::Relaxed) {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        });
        feed.link(&bin.static_pad("sink").unwrap()).map_err(|e| format!("layer {q}: {e}"))?;
        bin.sync_state_with_parent().map_err(|e| format!("layer {q}: {e}"))?;
        if q == self.ceiling() {
            debug_switch_size(&bin, q.width, q.height);
        }
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: layer {q} started", since_start_ms());
        }
        Ok(Layer { bin, tee, feed, stats, parked })
    }

    /// The tee of the layer at quality `q`, started (or unparked) if nobody watched it.
    /// An unparked encoder continues its stream; viewers move to it at its next periodic
    /// keyframe and keep their current layer until then.
    fn layer_tee(&self, q: Quality) -> Result<gst::Element, String> {
        let mut layers = self.layers.lock().unwrap();
        if let Some(l) = layers.get(&q) {
            if l.parked.swap(false, Ordering::SeqCst) && debug_timeline() {
                eprintln!("timeline {:>9.1} ms: layer {q} unparked", since_start_ms());
            }
            return Ok(l.tee.clone());
        }
        let l = self.make_layer(q)?;
        let tee = l.tee.clone();
        layers.insert(q, l);
        Ok(tee)
    }

    /// Releases tee pads that viewers left and parks layers nobody watches or waits for.
    /// Must not run on a streaming thread; takes the viewers lock.
    pub fn housekeeping(&self) {
        let viewers = self.viewers.lock().unwrap();
        self.housekeeping_locked(&viewers);
    }

    fn housekeeping_locked(&self, viewers: &HashMap<u64, Branch>) {
        trace("housekeeping enter");
        let mut used = std::collections::HashSet::new();
        for b in viewers.values() {
            let mut v = b.video.lock().unwrap();
            for (tee, pad) in v.retired.drain(..) {
                release_tee_pad(&tee, &pad);
            }
            used.insert(v.quality);
            if let Some((q, ..)) = &v.pending {
                used.insert(*q);
            }
        }
        // While viewers remain the pipeline plays: park idle layers. Once it has stopped
        // (no viewers) tearing them down is safe. STREAMER_DEBUG_DESTROY_LAYERS restores the
        // tear-down while playing, the spoiled variant for the churn gate.
        let destroy = viewers.is_empty() || std::env::var_os("STREAMER_DEBUG_DESTROY_LAYERS").is_some();
        let idle: Vec<Quality> = self.layers.lock().unwrap().keys().filter(|q| !used.contains(q)).copied().collect();
        if !destroy {
            for q in idle {
                if let Some(l) = self.layers.lock().unwrap().get(&q) {
                    if !l.parked.swap(true, Ordering::SeqCst) && debug_timeline() {
                        eprintln!("timeline {:>9.1} ms: layer {q} parked", since_start_ms());
                    }
                }
            }
            trace("housekeeping exit");
            return;
        }
        for q in idle {
            let Some(l) = self.layers.lock().unwrap().remove(&q) else { continue };
            trace("housekeeping: detach layer");
            // Detach from the capture between buffers, then stop it on its own.
            let (tx, rx) = mpsc::channel();
            let sink = l.bin.static_pad("sink").unwrap();
            l.feed.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                let _ = pad.unlink(&sink);
                let _ = tx.send(());
                gst::PadProbeReturn::Remove
            });
            if rx.recv_timeout(Duration::from_secs(2)).is_err() {
                if let Some(peer) = l.feed.peer() {
                    let _ = l.feed.unlink(&peer);
                }
            }
            trace("housekeeping: layer to NULL");
            let _ = l.bin.set_state(gst::State::Null);
            trace("housekeeping: remove layer");
            let _ = self.pipeline.remove(&l.bin);
            self.pipeline.by_name("srctee").unwrap().release_request_pad(&l.feed);
            if debug_timeline() {
                eprintln!("timeline {:>9.1} ms: layer {q} stopped", since_start_ms());
            }
        }
        trace("housekeeping exit");
    }

    /// The quality each viewer currently receives.
    pub fn viewer_layers(&self) -> Vec<(u64, Quality)> {
        self.viewers.lock().unwrap().iter().map(|(id, b)| (*id, b.video.lock().unwrap().quality)).collect()
    }

    /// Per layer: quality, viewers on it, statistics.
    pub fn layers(&self) -> Vec<LayerReport> {
        let viewers = self.viewers.lock().unwrap();
        let mut out: Vec<LayerReport> = self
            .layers
            .lock()
            .unwrap()
            .iter()
            .map(|(q, l)| LayerReport {
                quality: *q,
                parked: l.parked.load(Ordering::Relaxed),
                viewers: viewers.values().filter(|b| b.video.lock().unwrap().quality == *q).count(),
                stats: l.stats.clone(),
            })
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse((r.quality.height, r.quality.fps, r.quality.kbit)));
        out
    }

    /// Moves viewer `id` to the layer at `want`, clamped to the host's quality. It keeps the
    /// current layer until the new one sends a keyframe, so the picture never stops.
    pub fn set_quality(&self, id: u64, want: Quality) -> Result<Quality, String> {
        // STREAMER_DEBUG_NO_CLAMP is the spoiled variant for the layers gate.
        let q = if std::env::var_os("STREAMER_DEBUG_NO_CLAMP").is_some() { want } else { want.clamp_to(self.ceiling()) };
        if let Some(b) = self.viewers.lock().unwrap().get_mut(&id) {
            // Asking for the host's own quality means following it.
            b.want = (want != self.ceiling()).then_some(want);
        }
        self.move_viewer(id, q)
    }

    fn move_viewer(&self, id: u64, q: Quality) -> Result<Quality, String> {
        trace("set_quality enter");
        let viewers = self.viewers.lock().unwrap();
        let Some(b) = viewers.get(&id) else { return Err(format!("no viewer {id}")) };
        {
            let mut v = b.video.lock().unwrap();
            if let Some((_, tee, pad, probe)) = v.pending.take() {
                pad.remove_probe(probe);
                v.retired.push((tee, pad));
            }
            if v.quality == q {
                drop(v);
                self.housekeeping_locked(&viewers);
                return Ok(q);
            }
        }
        trace("set_quality: layer_tee");
        let tee = self.layer_tee(q)?;
        trace("set_quality: request pad");
        let pad = request_tee_pad(&tee);
        let (link, video_in) = (b.video.clone(), b.video_in.clone());
        let probe = pad
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let Some(buf) = info.buffer() else { return gst::PadProbeReturn::Ok };
                if buf.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    return gst::PadProbeReturn::Drop;
                }
                // The first keyframe of the new layer: swap inputs. Linking before the push
                // continues sends this keyframe, with the new caps, to the viewer.
                trace("handover enter");
                let mut v = link.lock().unwrap();
                let Some((q, tee, ..)) = v.pending.take() else { return gst::PadProbeReturn::Remove };
                let _ = v.pad.unlink(&video_in);
                let _ = pad.link(&video_in);
                let old = (std::mem::replace(&mut v.tee, tee), std::mem::replace(&mut v.pad, pad.clone()));
                v.retired.push(old);
                v.quality = q;
                trace("handover swapped");
                if debug_timeline() {
                    eprintln!("timeline {:>9.1} ms: viewer moved to {q}", since_start_ms());
                }
                gst::PadProbeReturn::Remove
            })
            .ok_or("cannot watch the new layer")?;
        b.video.lock().unwrap().pending = Some((q, tee, pad, probe));
        trace("set_quality: pending set");
        // The previous target, if any, may now be unused.
        self.housekeeping_locked(&viewers);
        Ok(q)
    }


    pub fn running_time(&self) -> Option<u64> {
        clock::running_time(&self.pipeline)
    }

    pub fn viewer_count(&self) -> usize {
        self.viewers.lock().unwrap().len()
    }

    /// What each viewer reports back about the video it receives (RTCP), for marking a
    /// viewer with problems. Only that viewer is affected by them (decision D1).
    pub fn viewer_quality(&self) -> Vec<ViewerQuality> {
        let webrtcs: Vec<(u64, gst::Element, Arc<crate::catchup::Gate>)> =
            self.viewers.lock().unwrap().iter().map(|(id, b)| (*id, b.webrtc.clone(), b.gate.clone())).collect();
        webrtcs
            .into_iter()
            .map(|(id, w, gate)| {
                let mut q = ViewerQuality {
                    id,
                    catchups: gate.pauses.load(Ordering::Relaxed),
                    skipped_frames: gate.skipped_frames.load(Ordering::Relaxed),
                    ..Default::default()
                };
                let promise = gst::Promise::new();
                w.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
                if promise.wait() == gst::PromiseResult::Replied {
                    if let Some(reply) = promise.get_reply() {
                        if std::env::var_os("STREAMER_DEBUG_STATS").is_some() {
                            eprintln!("stats reply {reply}");
                        }
                        for s in reply.iter().filter_map(|(_, v)| v.get::<gst::Structure>().ok()) {
                            if std::env::var_os("STREAMER_DEBUG_STATS").is_some() {
                                eprintln!("stats {s}");
                            }
                            if s.name() == "outbound-rtp" {
                                q.bytes_sent += s.get::<u64>("bytes-sent").unwrap_or(0);
                            }
                            let kind = s.get::<&str>("kind").unwrap_or("video");
                            if kind != "video" {
                                continue;
                            }
                            if s.has_field("round-trip-time") && s.has_field("fraction-lost") {
                                q.fraction_lost = s.get::<f64>("fraction-lost").unwrap_or(0.0);
                                q.rtt_ms = s.get::<f64>("round-trip-time").unwrap_or(0.0) * 1000.0;
                            }
                            if s.has_field("packets-sent") && s.has_field("nack-count") {
                                q.nack = s.get::<u32>("nack-count").map(u64::from).unwrap_or(0);
                                q.pli = s.get::<u32>("pli-count").map(u64::from).unwrap_or(0);
                            }
                        }
                    }
                }
                // Retransmission requests received and packets resent, from rtprtxsend.
                if let Some(bin) = w.downcast_ref::<gst::Bin>() {
                    for e in bin.iterate_recurse().into_iter().flatten() {
                        if e.factory().is_some_and(|f| f.name() == "rtprtxsend") {
                            q.rtx_requests += e.property::<u32>("num-rtx-requests") as u64;
                            q.rtx_packets += e.property::<u32>("num-rtx-packets") as u64;
                        }
                    }
                }
                q
            })
            .collect()
    }

    /// Adds one viewer branch and starts negotiation over the link. The first viewer starts
    /// capture and encoding; until then the host uses no encoder and sends nothing.
    /// `want`: the quality to start at, clamped to the host's; None is the host's own.
    pub fn add_viewer(&self, id: u64, link: Arc<dyn Outbox>, want: Option<Quality>) -> Result<(), String> {
        let mut viewers = self.viewers.lock().unwrap();
        if viewers.contains_key(&id) {
            return Ok(());
        }
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: add viewer {id}", since_start_ms());
        }
        let start = want.map_or(self.ceiling(), |w| w.clamp_to(self.ceiling()));
        let mut branch = self.build_branch(id, link, start)?;
        branch.want = want.filter(|w| *w != self.ceiling());
        if viewers.is_empty() {
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| format!("cannot start capture and encoding: {e}"))?;
            let p = self.pipeline.downgrade();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                if let Some(p) = p.upgrade() {
                    let c = p.clock();
                    eprintln!("host: pipeline clock {} ({})", c.as_ref().map(|c| c.name().to_string()).unwrap_or("none".into()), c.as_ref().map(|c| c.type_().name()).unwrap_or(""));
                }
            });
        }
        for e in std::iter::once(&branch.webrtc).chain(&branch.elements) {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        // STREAMER_DEBUG_LINK_FIRST links before the branch plays: the spoiled variant.
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_none() {
            for (src, sink) in branch.unlinked.drain(..) {
                src.link(&sink).map_err(|e| format!("viewer {id}: {e}"))?;
            }
        }
        // A keyframe now, so the new viewer (who gets nothing before one) need not wait up
        // to a GOP. Marked, so the filter that keeps viewers' own requests away lets it pass.
        if !join_key_off() {
            let pad = branch.video.lock().unwrap().pad.clone();
            let s = gst::Structure::builder("GstForceKeyUnit")
                .field("running-time", u64::MAX)
                .field("all-headers", true)
                .field("count", 0u32)
                .field(HOST_KEY_REQUEST, true)
                .build();
            pad.send_event(gst::event::CustomUpstream::new(s));
        }
        viewers.insert(id, branch);
        Ok(())
    }

    /// `f(viewer, connection, was_up)` when a connection with a viewer fails.
    pub fn on_viewer_failed(&self, f: ViewerFailHook) {
        *self.on_viewer_failed.lock().unwrap() = Some(f);
    }

    /// Takes the failure of connection `w` with `id` to handle: its "punching" flag, or None
    /// when `w` is no longer that viewer's connection or its failure is handled already.
    pub fn claim_viewer(&self, id: u64, w: &gst::Element) -> Option<Arc<AtomicBool>> {
        let viewers = self.viewers.lock().unwrap();
        let b = viewers.get(&id).filter(|b| b.webrtc == *w)?;
        (!b.handled.swap(true, Ordering::SeqCst)).then(|| b.punching.clone())
    }

    /// The connection with viewer `id` now, and whether it is up.
    pub fn viewer_connection(&self, id: u64) -> Option<(gst::Element, bool)> {
        use gst_webrtc::WebRTCICEConnectionState as State;
        let w = self.viewers.lock().unwrap().get(&id).map(|b| b.webrtc.clone())?;
        let up = matches!(w.property::<State>("ice-connection-state"), State::Connected | State::Completed);
        Some((w, up))
    }

    /// Keeps a hole punched for viewer `id` for its next connection (`restart_viewer`).
    pub fn set_pinned(&self, id: u64, hole: crate::punch::Pinned) {
        let mut p = self.pinned.lock().unwrap();
        p.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(15));
        p.insert(id, (hole, std::time::Instant::now()));
    }

    /// Builds viewer `id`'s branch again (a new connection, on a hole punched for it if one
    /// is kept), at the quality it has; the viewer asked for it ("restart"), having made a
    /// new connection itself. Only for a connection that never came up.
    pub fn restart_viewer(&self, id: u64, link: Arc<dyn Outbox>) -> Result<(), String> {
        use gst_webrtc::WebRTCICEConnectionState as State;
        let want = {
            let viewers = self.viewers.lock().unwrap();
            let Some(b) = viewers.get(&id) else { return Err(format!("no viewer {id}")) };
            if matches!(b.webrtc.property::<State>("ice-connection-state"), State::Connected | State::Completed) {
                return Err(format!("viewer {id}: the connection is up"));
            }
            b.want
        };
        eprintln!("host: viewer {id} asked to start over");
        self.remove_viewer(id);
        self.add_viewer(id, link, want)
    }

    /// How each viewer's connection stands. STREAMER_DEBUG_LINK_NO_HOLE: a hole is not told.
    pub fn viewer_links(&self) -> Vec<(u64, crate::viewer::Link)> {
        use crate::viewer::Link;
        use gst_webrtc::WebRTCICEConnectionState as State;
        let no_hole = std::env::var_os("STREAMER_DEBUG_LINK_NO_HOLE").is_some();
        self.viewers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, b)| {
                let up = matches!(b.webrtc.property::<State>("ice-connection-state"), State::Connected | State::Completed);
                let link = if up {
                    if b.hole.is_some() && !no_hole { Link::Punched } else { Link::Direct }
                } else if b.punching.load(Ordering::SeqCst) {
                    Link::Punching
                } else {
                    Link::Connecting
                };
                (*id, link)
            })
            .collect()
    }

    /// Removes a viewer branch while the others keep playing. The last viewer stops capture
    /// and encoding. Must not run on a streaming thread.
    pub fn remove_viewer(&self, id: u64) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(mut branch) = viewers.remove(&id) else { return };
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: remove viewer {id}", since_start_ms());
        }
        {
            let mut v = branch.video.lock().unwrap();
            if let Some((_, tee, pad, probe)) = v.pending.take() {
                pad.remove_probe(probe);
                release_tee_pad(&tee, &pad);
            }
            for (tee, pad) in v.retired.drain(..) {
                release_tee_pad(&tee, &pad);
            }
            branch.tee_pads.push((v.tee.clone(), v.pad.clone()));
        }

        trace("remove_viewer: wait idle");
        // Unlink each tee pad only when no buffer is passing through it.
        let (done_tx, done_rx) = mpsc::channel();
        for (_, pad) in &branch.tee_pads {
            let tx = done_tx.clone();
            pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                if let Some(peer) = pad.peer() {
                    let _ = pad.unlink(&peer);
                }
                let _ = tx.send(());
                gst::PadProbeReturn::Remove
            });
        }
        for _ in &branch.tee_pads {
            if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                eprintln!("host: tee pad of viewer {id} did not go idle; unlinking anyway");
            }
        }
        trace("remove_viewer: to NULL");
        // webrtcbin first: a branch that never finished negotiating blocks its input, and
        // stopping the queue first would deadlock on that push.
        let _ = branch.webrtc.set_state(gst::State::Null);
        for e in &branch.elements {
            let _ = e.set_state(gst::State::Null);
        }
        // Links inside the branch and webrtcbin's request pads keep every element alive,
        // with webrtcbin's ICE sockets (stage 3: ~0.6 MB and 2 handles leaked per viewer).
        for e in std::iter::once(&branch.webrtc).chain(&branch.elements) {
            for pad in e.pads() {
                if let Some(peer) = pad.peer() {
                    let _ = if pad.direction() == gst::PadDirection::Src { pad.unlink(&peer) } else { peer.unlink(&pad) };
                }
            }
        }
        for pad in branch.webrtc.sink_pads() {
            branch.webrtc.release_request_pad(&pad);
        }
        let _ = self.pipeline.remove(&branch.webrtc);
        for e in &branch.elements {
            let _ = self.pipeline.remove(e);
        }
        trace("remove_viewer: release pads");
        for (tee, pad) in &branch.tee_pads {
            if let Some(peer) = pad.peer() {
                let _ = pad.unlink(&peer);
            }
            release_tee_pad(tee, pad);
        }
        if viewers.is_empty() {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
        self.housekeeping_locked(&viewers);
        if std::env::var_os("STREAMER_DEBUG_LEAKS").is_some() {
            for e in std::iter::once(&branch.webrtc).chain(&branch.elements) {
                eprintln!(
                    "leak check: {} refcount {} parent {:?} state {:?}",
                    e.name(),
                    e.ref_count(),
                    e.parent().map(|p| p.name()),
                    e.current_state()
                );
            }
            let weak = branch.webrtc.downgrade();
            let others: Vec<_> = branch.elements.iter().map(|e| e.downgrade()).collect();
            drop(branch);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                let alive = others.iter().filter(|w| w.upgrade().is_some()).count();
                match weak.upgrade() {
                    Some(w) => eprintln!("leak check: webrtcbin of viewer {id} still alive, refcount {}", w.ref_count() - 1),
                    None => eprintln!("leak check: webrtcbin of viewer {id} freed"),
                }
                eprintln!("leak check: {alive} of {} queue/payloader elements still alive", others.len());
            });
        }
    }

    /// Stops everything: all viewers and the pipeline.
    pub fn stop(&self) {
        let ids: Vec<u64> = self.viewers.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.remove_viewer(id);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    fn build_branch(&self, id: u64, link: Arc<dyn Outbox>, start: Quality) -> Result<Branch, String> {
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(format!("viewer-{id}"))
            .build()
            .unwrap();
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        // A hole punched for this viewer (its connection failed before): its port, once.
        let mut ice = self.cfg().ice;
        ice.pinned = self.pinned.lock().unwrap().remove(&id).filter(|(_, at)| at.elapsed() < Duration::from_secs(15)).map(|(p, _)| p);
        if let Some(p) = ice.pinned {
            eprintln!("host: connecting with viewer {id} through a punched hole (local port {})", p.port);
        }
        crate::ice::configure(&webrtc, &ice);
        let (hook, w) = (self.on_viewer_failed.clone(), webrtc.downgrade());
        let _ = crate::ice::watch(&webrtc, format!("viewer {id}"), ice.turn.is_some(), move |was_up| {
            let f = hook.lock().unwrap().clone();
            if let (Some(f), Some(w)) = (f, w.upgrade()) {
                f(id, w, was_up);
            }
        });

        let l = link.clone();
        // Tests only: STREAMER_DEBUG_ICE_NO_CANDIDATES: our candidates are not sent (a
        // connection without a punched hole fails, as between two NATs).
        let send_candidates = std::env::var_os("STREAMER_DEBUG_ICE_NO_CANDIDATES").is_none();
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let cand = v[2].get::<String>().unwrap();
            if send_candidates {
                l.send("ice", &format!("{mline} {cand}"));
            }
            None
        });

        let offered = Arc::new(AtomicBool::new(false));
        webrtc.connect("on-negotiation-needed", false, move |v| {
            if offered.swap(true, Ordering::SeqCst) {
                return None;
            }
            let w = v[0].get::<gst::Element>().unwrap();
            if let Some(t) = w.emit_by_name::<Option<WebRTCRTPTransceiver>>("get-transceiver", &[&0i32]) {
                t.set_property("do-nack", true);
            }
            let (w2, l) = (w.clone(), link.clone());
            let promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(reply)) = reply else { return };
                let Ok(offer) = reply.get::<WebRTCSessionDescription>("offer") else { return };
                w2.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                l.send("offer", &offer.sdp().as_text().unwrap());
            });
            w.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            None
        });

        self.pipeline.add(&webrtc).unwrap();
        let video_pay = make_pay(&format!("rtp{}pay", self.cfg().codec.short()), VIDEO_PT);
        // H.26x payloaders: parameter sets with every key frame, no waiting to aggregate.
        if video_pay.has_property("config-interval") {
            video_pay.set_property("config-interval", -1i32);
        }
        if video_pay.has_property("aggregate-mode") {
            video_pay.set_property_from_str("aggregate-mode", "zero-latency");
        }
        // A viewer starts on the quality it subscribed with, and may change it later.
        let tee = self.layer_tee(start)?;
        let video_queue = branch_queue();
        self.pipeline.add_many([&video_queue, &video_pay]).unwrap();
        gst::Element::link_many([&video_queue, &video_pay, &webrtc]).unwrap();
        let video_in = video_queue.static_pad("sink").unwrap();
        let pad = request_tee_pad(&tee);
        let mut unlinked = vec![(pad.clone(), video_in.clone())];
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_some() {
            pad.link(&video_in).map_err(|e| format!("viewer {id}: {e}"))?;
            unlinked.clear();
        }
        let video = VideoLink { quality: start, tee, pad, pending: None, retired: vec![] };
        let mut branch = Branch {
            want: None,
            webrtc,
            elements: vec![video_pay, video_queue.clone()],
            tee_pads: vec![],
            // STREAMER_DEBUG_NO_JOIN_KEY: deltas go out from the start (the spoiled variant).
            gate: Arc::new(if join_key_off() { Default::default() } else { crate::catchup::Gate::starting() }),
            video: Arc::new(Mutex::new(video)),
            video_in,
            unlinked,
            hole: ice.pinned,
            handled: Arc::default(),
            punching: Arc::default(),
        };
        // Between the queue and the payloader, so dropped frames leave no gaps in RTP
        // sequence numbers.
        let gate = branch.gate.clone();
        let queue_src = video_queue.static_pad("src").unwrap();
        queue_src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buf) = info.buffer() else { return gst::PadProbeReturn::Ok };
            if gate.pass(!buf.flags().contains(gst::BufferFlags::DELTA_UNIT)) {
                gst::PadProbeReturn::Ok
            } else {
                gst::PadProbeReturn::Drop
            }
        });
        if self.cfg().audio {
            self.feed(&mut branch, "atee", make_pay("rtpopuspay", AUDIO_PT));
        }
        Ok(branch)
    }

    /// Links tee -> leaky queue -> payloader -> the branch's webrtcbin.
    fn feed(&self, branch: &mut Branch, tee: &str, pay: gst::Element) {
        let queue = branch_queue();
        self.pipeline.add_many([&queue, &pay]).unwrap();
        gst::Element::link_many([&queue, &pay, &branch.webrtc]).unwrap();
        let tee = self.pipeline.by_name(tee).unwrap();
        let pad = tee.request_pad_simple("src_%u").unwrap();
        let sink = queue.static_pad("sink").unwrap();
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_some() {
            pad.link(&sink).unwrap();
        } else {
            branch.unlinked.push((pad.clone(), sink));
        }
        branch.elements.extend([pay, queue]);
        branch.tee_pads.push((tee, pad));
    }

    /// Applies one signaling message from viewer `id`.
    pub fn on_message(&self, id: u64, link: &dyn Outbox, kind: &str, payload: &str) {
        let Some((webrtc, gate, hole)) = self.viewers.lock().unwrap().get(&id).map(|b| (b.webrtc.clone(), b.gate.clone(), b.hole)) else {
            return;
        };
        let webrtc = &webrtc;
        match kind {
            "answer" => {
                if let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(payload.as_bytes()) {
                    let answer = WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp);
                    webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
                    // A punched hole: the viewer's address there as a candidate.
                    crate::ice::add_pinned(webrtc, hole);
                }
            }
            "ice" => add_ice(webrtc, payload),
            QUALITY_MSG => match Quality::parse_request(payload, self.ceiling()).and_then(|want| self.set_quality(id, want)) {
                Ok(q) => println!("host: viewer {id} asked for {}, gets {q}", payload.trim()),
                Err(e) => eprintln!("host: viewer {id} quality request {}: {e}", payload.trim()),
            },
            crate::catchup::MSG => {
                if let Ok(ms) = payload.trim().parse::<u64>() {
                    gate.request(std::time::Duration::from_millis(ms));
                }
            }
            "ping" => {
                let now = self.running_time().unwrap_or(0);
                link.send("pong", &format!("{payload} {now}"));
            }
            _ => {}
        }
    }
}

/// Probe for viewer-chosen modes: STREAMER_DEBUG_SWITCH_SECS=N alternates the encoded size
/// between the mode and 1280x720 every N seconds, so decoders meet a new resolution mid-stream
/// exactly as when a viewer moves to another encoder.
/// The captured size of a screen or window source, as far as it is known before capture
/// starts; None for test sources (they are made at the mode's size).
/// Test only: STREAMER_DEBUG_SOURCE_ERROR_SECS=N fails the first capture N seconds after its
/// first frame, as a closed window does ("Capture item was closed": the source posts an
/// error and sends EOS downstream).
fn debug_source_error(pipeline: &gst::Pipeline) {
    let Some(secs) = std::env::var("STREAMER_DEBUG_SOURCE_ERROR_SECS").ok().and_then(|v| v.parse::<f64>().ok()) else { return };
    let Some(pad) = pipeline.by_name("srctee").and_then(|t| t.static_pad("sink")).and_then(|p| p.peer()) else { return };
    let first = std::sync::OnceLock::new();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if first.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() < secs {
            return gst::PadProbeReturn::Ok;
        }
        // Handled, not Drop: only then does the push return the probe's flow result.
        info.data.take();
        info.flow_res = Err(gst::FlowError::Error);
        gst::PadProbeReturn::Handled
    });
}

/// The capture: every element upstream of the capture tee.
fn capture_elements(pipeline: &gst::Pipeline) -> Vec<gst::Element> {
    let mut out = Vec::new();
    let mut pad = pipeline.by_name("srctee").and_then(|t| t.static_pad("sink")).and_then(|s| s.peer());
    while let Some(p) = pad {
        let Some(e) = p.parent_element() else { break };
        pad = e.sink_pads().first().and_then(|s| s.peer());
        out.push(e);
    }
    out
}

/// Test only: STREAMER_DEBUG_SOURCE_STUCK makes the first capture's streaming thread hang for
/// good once a change of source begins, so stopping it never returns (as a window capture
/// that hung on stop).
static DEBUG_STUCK: AtomicBool = AtomicBool::new(false);

fn debug_source_stuck(pipeline: &gst::Pipeline) {
    if std::env::var_os("STREAMER_DEBUG_SOURCE_STUCK").is_none() {
        return;
    }
    let Some(pad) = pipeline.by_name("srctee").and_then(|t| t.static_pad("sink")).and_then(|p| p.peer()) else { return };
    pad.add_probe(gst::PadProbeType::BUFFER, |_, _| {
        while DEBUG_STUCK.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
        gst::PadProbeReturn::Ok
    });
}

/// Hangs the first capture's streaming thread (STREAMER_DEBUG_SOURCE_STUCK) and waits until
/// it is caught there.
fn debug_source_stuck_now() {
    if std::env::var_os("STREAMER_DEBUG_SOURCE_STUCK").is_some() && !DEBUG_STUCK.swap(true, Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn source_size(source: &Source) -> Option<(i32, i32)> {
    let monitor = |pick: &dyn Fn(&platform::MonitorInfo) -> bool| {
        platform::monitors().into_iter().find(|m| pick(m)).map(|m| (m.width as i32, m.height as i32))
    };
    match source {
        Source::Monitor { handle } => monitor(&|m| m.handle == *handle),
        Source::Capture { monitor: i } if *i < 0 => monitor(&|m| m.primary),
        Source::Capture { monitor: i } => platform::monitors().into_iter().nth(*i as usize).map(|m| (m.width as i32, m.height as i32)),
        Source::Window { hwnd } => platform::windows()
            .into_iter()
            .find(|w| w.hwnd == *hwnd && w.width > 0 && w.height > 0)
            .map(|w| (w.width as i32, w.height as i32)),
        _ => None,
    }
}

fn debug_switch_size(layer: &gst::Bin, w: u32, h: u32) {
    let Some(secs) = std::env::var("STREAMER_DEBUG_SWITCH_SECS").ok().and_then(|v| v.parse::<u64>().ok()) else {
        return;
    };
    let filter = layer.by_name("vsize").unwrap();
    std::thread::spawn(move || {
        let mut small = false;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            small = !small;
            let (cw, ch) = if small { (1280, 720) } else { (w, h) };
            let caps = gst::Caps::builder("video/x-raw")
                .features(["memory:D3D11Memory"])
                .field("format", "NV12")
                .field("width", cw as i32)
                .field("height", ch as i32)
                .build();
            eprintln!("switch {:>9.1} ms: encode {cw}x{ch}", since_start_ms());
            filter.set_property("caps", &caps);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let enc = filter.parent().and_downcast::<gst::Bin>().and_then(|b| b.by_name("venc"));
            let out = enc.and_then(|e| e.static_pad("src")).and_then(|p| p.current_caps());
            let size = out.as_ref().and_then(|c| c.structure(0)).map(|s| {
                format!("{}x{}", s.get::<i32>("width").unwrap_or(0), s.get::<i32>("height").unwrap_or(0))
            });
            eprintln!("switch {:>9.1} ms: encoder output now {}", since_start_ms(), size.unwrap_or_default());
        }
    });
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// STREAMER_DEBUG_TIMELINE prints frame gaps and viewer changes on one time axis.
fn debug_timeline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STREAMER_DEBUG_TIMELINE").is_some())
}

/// Step trace for the timeline, to find where a stall happens.
fn trace(what: &str) {
    if debug_timeline() {
        eprintln!("timeline {:>9.1} ms: [{}] {what}", since_start_ms(), std::thread::current().name().unwrap_or("?"));
    }
}

fn since_start_ms() -> f64 {
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3
}

/// A new output of a layer's tee, as a ghost pad on the layer's bin so it can link to a
/// viewer branch in the pipeline. Pads of tees directly in the pipeline are returned as is.
fn request_tee_pad(tee: &gst::Element) -> gst::Pad {
    let pad = tee.request_pad_simple("src_%u").unwrap();
    match tee.parent().and_downcast::<gst::Bin>() {
        Some(bin) if !bin.is::<gst::Pipeline>() => {
            let ghost = gst::GhostPad::with_target(&pad).unwrap();
            ghost.set_active(true).unwrap();
            bin.add_pad(&ghost).unwrap();
            ghost.upcast()
        }
        _ => pad,
    }
}

/// Undoes `request_tee_pad`.
fn release_tee_pad(tee: &gst::Element, pad: &gst::Pad) {
    match pad.downcast_ref::<gst::GhostPad>() {
        Some(ghost) => {
            // The tee first, so it stops pushing here; only then the ghost. Deactivating the
            // ghost while the tee still pushed returned FLUSHING into the layer, which paused
            // its queue and, through the capture tee, the capture itself: the whole host
            // stopped (stress test, 2026-11-06). STREAMER_DEBUG_GHOST_FIRST is that order.
            let target = ghost.target();
            let ghost_first = std::env::var_os("STREAMER_DEBUG_GHOST_FIRST").is_some();
            if ghost_first {
                let _ = ghost.set_active(false);
            }
            if let Some(t) = &target {
                tee.release_request_pad(t);
            }
            let _ = ghost.set_target(None::<&gst::Pad>);
            if !ghost_first {
                let _ = ghost.set_active(false);
            }
            if let Some(bin) = ghost.parent().and_downcast::<gst::Element>() {
                let _ = bin.remove_pad(ghost);
            }
        }
        None => tee.release_request_pad(pad),
    }
}

/// Keeps at most `fps` frames per second by timestamp, before the layer's videorate.
/// videorate alone (drop-only, max-rate) let a 720p30 layer through at 60 fps after its
/// first seconds, and the encoder, configured for 30, then sent 6.9 instead of 4 Mbit/s.
/// STREAMER_DEBUG_VIDEORATE_ONLY leaves it to videorate: the spoiled variant.
fn thin_frames(queue: &gst::Element, fps: u32) {
    let period = 1_000_000_000u64 / fps as u64;
    // Half a frame of slack, so capture jitter does not drop a frame that is due.
    let slack = period / 2;
    let next = Mutex::new(None::<u64>);
    queue.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|t| t.nseconds()) else {
            return gst::PadProbeReturn::Ok;
        };
        let mut next = next.lock().unwrap();
        match *next {
            Some(n) if pts + slack < n => gst::PadProbeReturn::Drop,
            Some(n) if pts < n + period => {
                *next = Some(n + period);
                gst::PadProbeReturn::Ok
            }
            // First frame, or a gap (parked layer, capture stall): start again from here.
            _ => {
                *next = Some(pts + period);
                gst::PadProbeReturn::Ok
            }
        }
    });
}

/// Leaky, so a viewer that stalls (or never negotiates) cannot block the tee.
fn branch_queue() -> gst::Element {
    gst::ElementFactory::make("queue")
        .property("max-size-buffers", 4u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .unwrap()
}

/// Frame statistics at a layer's fan-out point, and decision D1: a viewer's keyframe request
/// never reaches the shared encoder. STREAMER_DEBUG_FORWARD_KEY_REQUESTS is the spoiled
/// variant for the isolation gate.
fn watch_layer(tee: &gst::Element, s: Arc<HostStats>) {
    let tee_sink = tee.static_pad("sink").unwrap();
    let stats = s.clone();
    let last = Mutex::new(None::<std::time::Instant>);
    tee_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(b) = info.buffer() {
            let now = std::time::Instant::now();
            if let Some(prev) = last.lock().unwrap().replace(now) {
                let gap = now.duration_since(prev);
                stats.frame_interval.push(gap.as_nanos() as u64);
                if debug_timeline() && gap.as_millis() > 25 {
                    eprintln!("timeline {:>9.1} ms: encoded frame gap {:.1} ms", since_start_ms(), gap.as_secs_f64() * 1e3);
                }
            }
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
            stats.max_frame_bytes.fetch_max(b.size() as u64, Ordering::Relaxed);
            if !b.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                stats.key_frames.fetch_add(1, Ordering::Relaxed);
                stats.key_bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
            }
        }
        gst::PadProbeReturn::Ok
    });
    let forward = std::env::var_os("STREAMER_DEBUG_FORWARD_KEY_REQUESTS").is_some();
    tee_sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        let st = info.event().and_then(|e| e.structure());
        let is_request = st.is_some_and(|st| st.name() == "GstForceKeyUnit" && !st.has_field(HOST_KEY_REQUEST));
        if is_request {
            s.key_requests_dropped.fetch_add(1, Ordering::Relaxed);
            if !forward {
                return gst::PadProbeReturn::Drop;
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Marks the host's own keyframe request for a joining viewer (`add_viewer`).
const HOST_KEY_REQUEST: &str = "backseat-join";

fn join_key_off() -> bool {
    std::env::var_os("STREAMER_DEBUG_NO_JOIN_KEY").is_some()
}

fn make_pay(factory: &str, pt: u32) -> gst::Element {
    let pay = gst::ElementFactory::make(factory).build().unwrap();
    pay.set_property("pt", pt);
    pay.set_property("mtu", 1200u32);
    // RTP time equals the host running time, which lets the viewer measure latency. The
    // Rust payloaders (AV1) take it as i64, the C ones as u32.
    if pay.property_type("timestamp-offset") == Some(i64::static_type()) {
        pay.set_property("timestamp-offset", 0i64);
    } else {
        pay.set_property("timestamp-offset", 0u32);
    }
    pay
}

pub fn add_ice(webrtc: &gst::Element, payload: &str) {
    if let Some((mline, cand)) = payload.split_once(' ') {
        if let Ok(mline) = mline.parse::<u32>() {
            webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &cand.to_string()]);
        }
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::config::{Codec, IceConfig, VideoMode};

    fn cfg(source: Source) -> HostConfig {
        HostConfig { mode: VideoMode::parse("1080p60").unwrap(), kbit: 8000, codec: Codec::H264, keyframe_secs: 2, audio: false, audio_pid: None, audio_exclude: vec![], source, ice: IceConfig::default() }
    }

    /// The watch restarts the captures when the mix is silent or much quieter than a capture
    /// that is heard (two looks in a row), not when the mix is as loud, nor for a capture
    /// that is barely heard. Spoiled: STREAMER_DEBUG_SYSMIX_WATCH_SILENT_ONLY.
    #[cfg(windows)]
    #[test]
    fn a_mix_much_quieter_than_a_capture_restarts_them() {
        let looks = |input: f64, mix: f64| {
            let mut w = CaptureWatch::default();
            (w.mix_silent_due(input, mix), w.mix_silent_due(input, mix))
        };
        assert_eq!(looks(-66.0, -83.0), (false, true), "a quiet tab cut by 17 dB (2026-11-24)");
        assert_eq!(looks(-50.0, -70.0), (false, true), "cut by 20 dB, the mix not silent");
        assert_eq!(looks(-44.0, -60.0), (false, true), "cut by 16 dB");
        assert_eq!(looks(-40.0, -100.0), (false, true), "a silent mix");
        assert_eq!(looks(-44.0, -44.0), (false, false), "a mix as loud as its input");
        assert_eq!(looks(-44.0, -50.0), (false, false), "6 dB: the edges of a look");
        assert_eq!(looks(-75.0, -100.0), (false, false), "an input barely heard");
        // After a restart, a mix fine for a moment does not let the next restart come at once.
        let mut w = CaptureWatch::default();
        assert!(!w.mix_silent_due(-40.0, -60.0) && w.mix_silent_due(-40.0, -60.0));
        assert!(!w.mix_silent_due(-40.0, -40.0));
        assert!(!w.mix_silent_due(-40.0, -60.0) && !w.mix_silent_due(-40.0, -60.0), "the backoff held");
        // A pause in the program's sound (nothing heard, mix silent) does not count against
        // the good looks: 14 good, a pause, 1 good clear the backoff.
        for _ in 0..14 {
            assert!(!w.mix_silent_due(-40.0, -40.0));
        }
        assert!(!w.mix_silent_due(-150.0, -150.0));
        assert!(!w.mix_silent_due(-40.0, -40.0));
        assert!(!w.mix_silent_due(-40.0, -60.0) && w.mix_silent_due(-40.0, -60.0), "the backoff cleared");
    }

    /// A running stream changes its window or monitor by building the new capture as a bin
    /// on its own (`swap_video_source`). Only parsed here: nothing is captured.
    #[test]
    fn every_capture_parses_as_a_bin_on_its_own() {
        crate::init();
        for s in [Source::Capture { monitor: 0 }, Source::Monitor { handle: 1 }, Source::Window { hwnd: 1 }, Source::Bars, Source::Noise] {
            let desc = video_source(&cfg(s.clone()));
            let bin = gst::parse::bin_from_description(&desc, true);
            assert!(bin.is_ok(), "{s:?}: {desc}: {:?}", bin.err());
        }
    }
}
