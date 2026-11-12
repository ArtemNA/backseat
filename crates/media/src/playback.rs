//! Everything this process plays goes out through one place: the voices, the sound of the
//! streams we watch, and the event sounds. One mixer, then the echo probe, then the speakers:
//! the voice chat's echo canceller hears all of it, so a stream played on speakers does not
//! come back to the others through our microphone.
//!
//! appsrc (one per input) -> audiomixer -> webrtcechoprobe -> speakers
//!
//! Inputs push raw audio as it comes (timestamped on arrival); each producer paces itself
//! (a viewer's appsink syncs to its stream). Windows only for the stream sound: macOS keeps
//! the viewer's own sink.

use gst::prelude::*;
use gst_app::AppSrc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The name webrtcdsp finds the echo probe by.
pub const ECHO_PROBE: &str = "voice-echo";
pub const RATE: i32 = 48_000;
pub const CHANNELS: i32 = 2;

/// The mixer's format: every input converts to it.
pub fn caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", RATE)
        .field("channels", CHANNELS)
        .field("layout", "interleaved")
        .build()
}

pub struct Playback {
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    /// The speakers chosen in the settings, None for the system's default.
    device: Mutex<Option<String>>,
    silent: bool,
}

static SHARED: OnceLock<Arc<Playback>> = OnceLock::new();
static SILENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WANT_DEVICE: Mutex<Option<String>> = Mutex::new(None);

/// Tests only: the shared output plays nothing (a fake sink at real time). Before `get`.
pub fn set_silent() {
    SILENT.store(true, Ordering::SeqCst);
}

/// The process's output, started on first use.
pub fn get() -> Result<Arc<Playback>, String> {
    if let Some(p) = SHARED.get() {
        return Ok(p.clone());
    }
    // Tests only: STREAMER_DEBUG_SILENT_PLAYBACK plays nothing.
    let silent = SILENT.load(Ordering::SeqCst) || std::env::var_os("STREAMER_DEBUG_SILENT_PLAYBACK").is_some();
    let p = Playback::new(silent, WANT_DEVICE.lock().unwrap().clone())?;
    Ok(SHARED.get_or_init(|| p).clone())
}

/// The speakers to play on (a device id from `devices`), None for the system's default.
/// Takes effect at once if the output already runs.
pub fn set_device(id: Option<String>) {
    *WANT_DEVICE.lock().unwrap() = id.clone();
    if let Some(p) = SHARED.get() {
        p.switch_device(id);
    }
}

/// An audio device the user can pick.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Microphones (`input`) or speakers, without the "default" entries and the loopback ones.
pub fn devices(input: bool) -> Vec<Device> {
    let m = gst::DeviceMonitor::new();
    let class = if input { "Audio/Source" } else { "Audio/Sink" };
    m.add_filter(Some(class), None);
    if m.start().is_err() {
        return vec![];
    }
    let out = m
        .devices()
        .into_iter()
        .filter_map(|d| {
            let props = d.properties()?;
            if props.get::<bool>("wasapi2.device.loopback").unwrap_or(false) {
                return None;
            }
            if props.get::<&str>("device.api").ok() != Some("wasapi2") {
                return None;
            }
            let id = props.get::<String>("device.id").ok()?;
            let name = d.display_name().to_string();
            // Our own "System default" entry stands for these.
            if name.starts_with("Default Audio") {
                return None;
            }
            Some(Device { id, name })
        })
        .collect();
    m.stop();
    out
}

fn sink_for(silent: bool, device: &Option<String>) -> Result<gst::Element, String> {
    if silent {
        return Ok(gst::ElementFactory::make("fakesink").name("pout").property("sync", true).build().unwrap());
    }
    let s = gst::ElementFactory::make(crate::platform::audio_sink()).name("pout").build().map_err(|e| e.to_string())?;
    if s.has_property("low-latency") {
        s.set_property("low-latency", true);
    }
    if s.has_property("buffer-time") {
        // As the viewer's own sink had: a small device buffer, latency before gap-free audio.
        s.set_property("buffer-time", 40_000i64);
        s.set_property("latency-time", 10_000i64);
    }
    if let (Some(d), true) = (device, s.has_property("device")) {
        s.set_property("device", d);
    }
    Ok(s)
}

impl Playback {
    fn new(silent: bool, device: Option<String>) -> Result<Arc<Self>, String> {
        let pipeline = gst::Pipeline::with_name("playback");
        // One fixed clock: devices (speakers, a microphone added with voice) come and go while
        // it plays, and each would otherwise offer its own clock.
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        let mk = |f: &str| gst::ElementFactory::make(f).build().map_err(|e| format!("{f}: {e}"));
        let filter = |c: gst::Caps| gst::ElementFactory::make("capsfilter").property("caps", c).build().unwrap();
        // Silence keeps the mixer (and the echo probe's timeline) running with nothing else in.
        let silence = gst::ElementFactory::make("audiotestsrc")
            .property_from_str("wave", "silence")
            .property("is-live", true)
            .property("samplesperbuffer", 480i32)
            .build()
            .map_err(|e| e.to_string())?;
        let mixer = gst::ElementFactory::make("audiomixer").name("pmix").build().map_err(|e| e.to_string())?;
        // Every input is stamped as it comes in (streams' sound, voices, sounds), so the mixer
        // needs no room for late ones: what it would add is a stream's sound coming later than
        // with its own sink. (5 ms pieces instead of 10 stopped the echo canceller's output.)
        mixer.set_property("latency", 0u64);
        // The tee: what goes to the speakers, for `tap` (tests: a simulated room).
        let tee = gst::ElementFactory::make("tee").name("ptee").property("allow-not-linked", true).build().unwrap();
        // As in the voice mixer: a queue lets the silence be late, so inputs with more latency
        // than 10 ms do not make the mixer's latency impossible.
        let hold = gst::ElementFactory::make("queue").property("max-size-time", 1_000_000_000u64).property("max-size-buffers", 0u32).property("max-size-bytes", 0u32).build().unwrap();
        // The echo probe, in the chain from the start: put in later while it played, it cancelled
        // nothing (see `Voice::new`).
        let pcaps = filter(caps());
        pcaps.set_property("name", "pcaps");
        let probe = gst::ElementFactory::make("webrtcechoprobe").name(ECHO_PROBE).build().map_err(|e| format!("webrtcechoprobe: {e}"))?;
        let chain = [silence, filter(caps()), hold, mixer.clone(), pcaps, probe, tee, mk("audioconvert")?, mk("audioresample")?, sink_for(silent, &device)?];
        pipeline.add_many(&chain).map_err(|e| e.to_string())?;
        gst::Element::link_many(&chain[..3]).map_err(|e| e.to_string())?;
        chain[2].link(&mixer).map_err(|e| e.to_string())?;
        gst::Element::link_many(&chain[3..]).map_err(|e| e.to_string())?;
        crate::drain_bus(&pipeline, "playback");
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("cannot start the speakers: {e}"))?;
        eprintln!("playback: started{}", if silent { " (silent)" } else { "" });
        Ok(Arc::new(Self { pipeline, mixer, device: Mutex::new(device), silent }))
    }

    /// The pipeline, for the voice chat that lives in it.
    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    /// How long after its running time a buffer reaches the speakers: the latency the
    /// speakers' branch asks for. (`Pipeline::latency` is only a manual override, none here;
    /// taking it for this placed a simulated echo before its sound.)
    pub fn render_latency(&self) -> gst::ClockTime {
        self.pipeline
            .by_name("pout")
            .and_then(|s| s.static_pad("sink"))
            .and_then(|pad| {
                let mut q = gst::query::Latency::new();
                pad.peer_query(&mut q).then(|| q.result().1)
            })
            .unwrap_or(gst::ClockTime::ZERO)
    }

    /// The mixer before the echo probe and the speakers.
    pub fn mixer(&self) -> &gst::Element {
        &self.mixer
    }

    /// A new input, stamped on arrival. It goes away when the returned handle is dropped.
    pub fn input(self: &Arc<Self>, label: &str) -> Result<Input, String> {
        let src = gst::ElementFactory::make("appsrc").build().map_err(|e| e.to_string())?.downcast::<AppSrc>().unwrap();
        src.set_caps(Some(&caps()));
        src.set_is_live(true);
        src.set_format(gst::Format::Time);
        src.set_do_timestamp(true);
        // Stated, not left to defaults: an appsrc said "at most 0 ms late", which the mixer
        // (20 ms) could not meet, so the output stalled now and then ("clock problem").
        src.set_min_latency(0);
        src.set_max_latency(1_000_000_000);
        // Never block a producer: a stalled output drops instead.
        src.set_property("block", false);
        src.set_property_from_str("leaky-type", "downstream");
        src.set_max_bytes((RATE * CHANNELS * 2 / 5) as u64); // 200 ms
        self.pipeline.add(&src).map_err(|e| e.to_string())?;
        let pad = self.mixer.request_pad_simple("sink_%u").ok_or("no mixer pad")?;
        src.static_pad("src").unwrap().link(&pad).map_err(|e| format!("{e:?}"))?;
        src.sync_state_with_parent().map_err(|e| e.to_string())?;
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        eprintln!("playback: input {n} ({label})");
        Ok(Input { src, pad, out: self.clone() })
    }

    /// Tests only: hands what goes to the speakers (the mixer's format) to `f` as it plays,
    /// its PTS turned into clock time (running time + this pipeline's base time).
    pub fn tap(&self, f: impl Fn(gst::Buffer) + Send + Sync + 'static) -> Result<(), String> {
        let tee = self.pipeline.by_name("ptee").ok_or("no tee")?;
        let q = gst::ElementFactory::make("queue").property_from_str("leaky", "downstream").build().unwrap();
        let sink = gst_app::AppSink::builder().sync(false).build();
        sink.set_property("async", false);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    if let Some(mut b) = s.pull_sample().ok().and_then(|x| x.buffer_owned()) {
                        let base = s.base_time().unwrap_or(gst::ClockTime::ZERO);
                        let pts = b.pts();
                        b.make_mut().set_pts(pts.map(|p| p + base));
                        f(b);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        self.pipeline.add_many([&q, sink.upcast_ref()]).map_err(|e| e.to_string())?;
        q.link(&sink).map_err(|e| e.to_string())?;
        tee.request_pad_simple("src_%u").unwrap().link(&q.static_pad("sink").unwrap()).map_err(|e| format!("{e:?}"))?;
        q.sync_state_with_parent().map_err(|e| e.to_string())?;
        sink.sync_state_with_parent().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn switch_device(&self, id: Option<String>) {
        let mut cur = self.device.lock().unwrap();
        if *cur == id || self.silent {
            return;
        }
        let Some(old) = self.pipeline.by_name("pout") else { return };
        let Some(before) = old.static_pad("sink").and_then(|p| p.peer()).and_then(|p| p.parent_element()) else { return };
        let new = match sink_for(false, &id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("playback: cannot open the speakers: {e}");
                return;
            }
        };
        let _ = old.set_state(gst::State::Null);
        before.unlink(&old);
        let _ = self.pipeline.remove(&old);
        if self.pipeline.add(&new).is_err() || before.link(&new).is_err() || new.sync_state_with_parent().is_err() {
            eprintln!("playback: switching the speakers failed");
        }
        eprintln!("playback: speakers {}", if id.is_some() { "chosen" } else { "system default" });
        *cur = id;
    }
}

/// One input of the shared output.
pub struct Input {
    src: AppSrc,
    pad: gst::Pad,
    out: Arc<Playback>,
}

impl Input {
    /// Pushes raw audio in the mixer's format (`caps`).
    pub fn push(&self, buf: gst::Buffer) {
        let mut buf = buf;
        // Timestamped on arrival by the appsrc.
        {
            let b = buf.make_mut();
            b.set_pts(gst::ClockTime::NONE);
            b.set_dts(gst::ClockTime::NONE);
        }
        let _ = self.src.push_buffer(buf);
    }

    /// An appsink callback that pushes each sample here.
    pub fn sink_callbacks(self: &Arc<Self>) -> gst_app::AppSinkCallbacks {
        let me = self.clone();
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(s) = sink.pull_sample() {
                    if let Some(b) = s.buffer_owned() {
                        me.push(b);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build()
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = self.src.set_state(gst::State::Null);
        if let Some(peer) = self.src.static_pad("src") {
            let _ = peer.unlink(&self.pad);
        }
        let _ = self.out.pipeline.remove(&self.src);
        self.out.mixer.release_request_pad(&self.pad);
    }
}

/// An appsink that hands what it gets to a new input of the shared output, converting to
/// its format first. Returns the elements to put at the end of a chain, and the input (keep
/// it as long as the chain lives).
pub fn tail(label: &str) -> Result<(Vec<gst::Element>, Arc<Input>), String> {
    let input = Arc::new(get()?.input(label)?);
    let mk = |f: &str| gst::ElementFactory::make(f).build().map_err(|e| format!("{f}: {e}"));
    let filter = gst::ElementFactory::make("capsfilter").property("caps", caps()).build().unwrap();
    // Not synced: the sound goes on as it comes and waits once, in the shared output. (Synced
    // here as well it waited twice: a stream's sound came 50 ms later than with its own sink.
    // Handing over the time to be heard at instead does not work either: the producing
    // pipeline sets no latency, so 98% of the buffers are past that time when they come.)
    let sink = gst_app::AppSink::builder().sync(false).max_buffers(10).drop(true).build();
    // Added to a pipeline that already plays: it need not preroll there.
    sink.set_property("async", false);
    sink.set_callbacks(input.sink_callbacks());
    Ok((vec![mk("audioconvert")?, mk("audioresample")?, filter, sink.upcast()], input))
}
