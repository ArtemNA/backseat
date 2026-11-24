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
/// Room a mixer leaves for input that comes a little late (ms): with none, a mixer put
/// silence in place of such pieces, and a stream's sound or a voice broke up on a busy
/// machine (2026-11-24). Only in the shared output: in the streamer's system sound mix it
/// halved the sound sent (0.2.24).
pub const MIX_ROOM_MS: u64 = 30;
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
    /// What each input brought this minute and what the mixer gave (`watch_level`).
    levels: Arc<Mutex<Levels>>,
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
/// Takes effect at once if the output already runs, and for the streams' own outputs.
pub fn set_device(id: Option<String>) {
    *WANT_DEVICE.lock().unwrap() = id.clone();
    if let Some(p) = SHARED.get() {
        p.switch_device(id.clone());
    }
    switch_followers(id);
}

/// Moves every stream's own output to the speakers wanted (`id` only for the spoiled test).
/// Off the caller's thread (the UI's): each switch opens a device and waits for a gap
/// between buffers. One at a time, to the speakers wanted by then and on the outputs as
/// they are by then: quick changes in a row end on the last one chosen.
fn switch_followers(id: Option<String>) {
    if !FOLLOWERS.lock().unwrap().is_empty() {
        // STREAMER_DEBUG_SWITCH_PARALLEL: each change on its own, to the id it was given (the
        // spoiled variant: quick changes could end on an earlier one).
        let parallel = std::env::var_os("STREAMER_DEBUG_SWITCH_PARALLEL").is_some();
        std::thread::spawn(move || {
            let _one = if parallel { None } else { Some(SWITCHING.lock().unwrap()) };
            // The speakers wanted by then, or the default ones if those are not there.
            let id = if parallel { id } else { present_device(device()) };
            let followers: Vec<gst::Element> = {
                let mut f = FOLLOWERS.lock().unwrap();
                f.retain(|w| w.upgrade().is_some());
                f.iter().filter_map(|w| w.upgrade()).collect()
            };
            for s in followers {
                switch_sink(&s, &id);
            }
        });
    }
}

/// The speakers chosen for voice (None: the system's default), which streams play on too.
pub fn device() -> Option<String> {
    WANT_DEVICE.lock().unwrap().clone()
}

/// The sink takes a device id as text (wasapi2sink; macOS's sink takes a number: left alone).
pub fn device_by_id(sink: &gst::Element) -> bool {
    sink.find_property("device").is_some_and(|p| p.value_type() == String::static_type())
}

/// `id` if Windows has those speakers active now, else None (the system's default): chosen
/// speakers unplugged would leave a new stream silent. One lookup by id (milliseconds), not
/// a listing of every device (~0.7 s on a streaming thread). STREAMER_DEBUG_NO_PRESENCE_CHECK:
/// the id as it is (the spoiled variant).
pub fn present_device(id: Option<String>) -> Option<String> {
    let id = id?;
    if std::env::var_os("STREAMER_DEBUG_NO_PRESENCE_CHECK").is_some() || speaker_active(&id) {
        Some(id)
    } else {
        eprintln!("viewer: the speakers chosen for voice are not there; the stream plays on the default ones");
        None
    }
}

#[cfg(windows)]
fn speaker_active(id: &str) -> bool {
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Cannot ask: trust the choice.
        let Ok(e) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return true };
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        e.GetDevice(windows::core::PCWSTR(wide.as_ptr())).and_then(|d| d.GetState()).is_ok_and(|s| s == DEVICE_STATE_ACTIVE)
    }
}

#[cfg(not(windows))]
fn speaker_active(_: &str) -> bool {
    true
}

/// Streams' own sound outputs, moved with the voice's speakers (2026-11-23: voice played on
/// the chosen speakers, streams on the system default).
static FOLLOWERS: Mutex<Vec<gst::glib::WeakRef<gst::Element>>> = Mutex::new(Vec::new());

static SWITCHING: Mutex<()> = Mutex::new(());

/// Tests: the device each stream's own output is set to (None: the system's default).
pub fn followers_devices() -> Vec<Option<String>> {
    FOLLOWERS.lock().unwrap().iter().filter_map(|w| w.upgrade()).filter(|e| e.parent().is_some() && device_by_id(e)).map(|e| e.property::<Option<String>>("device")).collect()
}

/// Tests: whether every stream's own output is on the speakers wanted now (None: no outputs).
pub fn followers_on_wanted() -> Option<bool> {
    let want = device();
    let f: Vec<gst::Element> = FOLLOWERS.lock().unwrap().iter().filter_map(|w| w.upgrade()).filter(|e| e.parent().is_some()).collect();
    (!f.is_empty()).then(|| f.iter().all(|e| device_by_id(e) && e.property::<Option<String>>("device") == want))
}

/// A stream's own output (made with `device()`), switched along with the voice's speakers.
pub fn follow(sink: &gst::Element) {
    let mut f = FOLLOWERS.lock().unwrap();
    if !f.iter().any(|w| w.upgrade().as_ref() == Some(sink)) {
        f.push(sink.downgrade());
    }
}

/// `follow` for a new stream's output, set to `set_to` before: if the speakers changed
/// meanwhile (before it was in the list), it is moved now.
pub fn follow_new(sink: &gst::Element, set_to: &Option<String>) {
    follow(sink);
    let want = device();
    if want != *set_to && present_device(want.clone()) != *set_to {
        switch_followers(want);
    }
}

/// Moves a stream's own output to other speakers: a new sink like it opens first, the sound
/// moves over between buffers, then the old one closes (as the shared output does).
fn switch_sink(old: &gst::Element, id: &Option<String>) {
    if !device_by_id(old) || old.property::<Option<String>>("device") == *id {
        return;
    }
    let (Some(parent), Some(factory)) = (old.parent().and_then(|p| p.downcast::<gst::Bin>().ok()), old.factory()) else { return };
    let Some(before) = old.static_pad("sink").and_then(|p| p.peer()) else { return };
    let Ok(new) = factory.create().build() else { return };
    for p in ["sync", "low-latency", "continue-on-error", "buffer-time", "latency-time"] {
        if new.has_property(p) {
            new.set_property_from_value(p, &old.property_value(p));
        }
    }
    new.set_property("device", id.clone());
    crate::diag::watch_ts(&new.static_pad("sink").unwrap(), "stream sound to its speakers (after a change of speakers)".into());
    if parent.add(&new).is_err() {
        return;
    }
    if new.set_state(gst::State::Playing) == Err(gst::StateChangeError) {
        let _ = new.set_state(gst::State::Null);
        let _ = parent.remove(&new);
        eprintln!("viewer: the chosen speakers do not open for the stream; it stays where it was");
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let (o, n) = (old.clone(), new.clone());
    let probe = before.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
        if let (Some(os), Some(ns)) = (o.static_pad("sink"), n.static_pad("sink")) {
            let _ = pad.unlink(&os);
            let ok = pad.link(&ns).is_ok();
            // The new one would not take it: back to the old speakers.
            if !ok {
                let _ = pad.link(&os);
            }
            let _ = tx.send(ok);
        }
        gst::PadProbeReturn::Remove
    });
    let linked = match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(l) => l,
        // No gap in 2 s: the probe goes, so it cannot move the sound later onto a sink that
        // is gone by then (it could have run meanwhile: then its answer is waiting).
        Err(_) => {
            if let Some(id) = probe {
                before.remove_probe(id);
            }
            rx.try_recv().unwrap_or(false)
        }
    };
    let (keep, drop) = if linked { (&new, old) } else { (old, &new) };
    let _ = drop.set_state(gst::State::Null);
    let _ = parent.remove(drop);
    follow(keep);
    // The new device's latency counts from now on.
    let mut up = keep.parent();
    while let Some(o) = up {
        if let Some(p) = o.downcast_ref::<gst::Pipeline>() {
            let _ = p.recalculate_latency();
            break;
        }
        up = o.parent();
    }
    eprintln!("viewer: stream sound moved to the {} speakers{}", if id.is_some() { "chosen" } else { "default" }, if linked { "" } else { " (failed)" });
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
    let out: Vec<Device> = m
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
    let s = sink_for_inner(silent, device)?;
    crate::diag::watch_ts(&s.static_pad("sink").unwrap(), "shared output to the speakers".into());
    Ok(s)
}

fn sink_for_inner(silent: bool, device: &Option<String>) -> Result<gst::Element, String> {
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
    // Speakers unplugged: the output goes on without them instead of stopping the whole
    // shared pipeline (the voice's sending side lives in it too).
    if s.has_property("continue-on-error") && std::env::var_os("STREAMER_DEBUG_SINK_STOPS_ON_ERROR").is_none() {
        s.set_property("continue-on-error", true);
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
        // Every input is stamped as it comes in (streams' sound, voices, sounds), so only a
        // busy machine makes one late: `MIX_ROOM_MS` for that (none before 0.2.24, when a
        // voice could break up). Everything here plays that much later. (5 ms pieces instead
        // of 10 stopped the echo canceller's output.) STREAMER_DEBUG_PMIX_LATENCY_MS: another.
        let room = std::env::var("STREAMER_DEBUG_PMIX_LATENCY_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(MIX_ROOM_MS);
        mixer.set_property("latency", room * 1_000_000);
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
        let levels: Arc<Mutex<Levels>> = Arc::default();
        let l = levels.clone();
        mixer.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(m) = info.buffer().and_then(|b| b.map_readable().ok()) {
                let (sum, n) = sum_squares_s16(m.as_slice());
                let mut l = l.lock().unwrap();
                l.out.0 += sum;
                l.out.1 += n;
            }
            gst::PadProbeReturn::Ok
        });
        log_levels(mixer.downgrade(), levels.clone());
        Ok(Arc::new(Self { pipeline, mixer, levels, device: Mutex::new(device), silent }))
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

    /// Counts the level of what comes into the mixer through `pad` (one of its inputs), for
    /// the log once a minute, under `label`. `forget_level` when the pad goes.
    pub fn watch_level(&self, pad: &gst::Pad, label: String) {
        let (l, key) = (self.levels.clone(), pad.name().to_string());
        l.lock().unwrap().inputs.insert(key.clone(), (label, 0.0, 0));
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(m) = info.buffer().and_then(|b| b.map_readable().ok()) {
                let (sum, n) = sum_squares_s16(m.as_slice());
                if let Some(e) = l.lock().unwrap().inputs.get_mut(&key) {
                    e.1 += sum;
                    e.2 += n;
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// The input on `pad` left: no longer in the log.
    pub fn forget_level(&self, pad: &gst::Pad) {
        self.levels.lock().unwrap().inputs.remove(pad.name().as_str());
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
        self.watch_level(&pad, label.to_string());
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
        // The new speakers start first; only once they play does the mixer's output move to
        // them, between buffers (an idle probe). Stopping the old sink while the mixer still
        // pushed into it stopped the mixer, and with it every sound of the app; and speakers
        // that would not open left no sink at all. The new sink takes the old one's name.
        // STREAMER_DEBUG_SWITCH_OLD_WAY: the old order (the spoiled variant).
        if std::env::var_os("STREAMER_DEBUG_SWITCH_OLD_WAY").is_some() {
            let _ = old.set_state(gst::State::Null);
            before.unlink(&old);
            let _ = self.pipeline.remove(&old);
            new.set_property("name", "pout");
            if self.pipeline.add(&new).is_err() || before.link(&new).is_err() || new.sync_state_with_parent().is_err() {
                eprintln!("playback: switching the speakers failed");
            }
            *cur = id;
            return;
        }
        new.set_property("name", "pout-next");
        if self.pipeline.add(&new).is_err() {
            eprintln!("playback: switching the speakers failed");
            return;
        }
        if new.set_state(gst::State::Playing) == Err(gst::StateChangeError) {
            let _ = new.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&new);
            eprintln!("playback: the chosen speakers do not open; staying on the current ones");
            return;
        }
        let Some(out) = before.static_pad("src") else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        let (o, n) = (old.clone(), new.clone());
        out.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
            if let (Some(os), Some(ns)) = (o.static_pad("sink"), n.static_pad("sink")) {
                let _ = pad.unlink(&os);
                let _ = tx.send(pad.link(&ns).is_ok());
            }
            gst::PadProbeReturn::Remove
        });
        let linked = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false);
        let _ = old.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&old);
        new.set_property("name", "pout");
        if !linked {
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
        self.out.forget_level(&self.pad);
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

/// The shared output's levels over a minute: each input's (label, sum of squares, samples),
/// keyed by its mixer pad, and the mixer's output.
#[derive(Default)]
struct Levels {
    inputs: std::collections::HashMap<String, (String, f64, u64)>,
    out: (f64, u64),
}

fn sum_squares_s16(bytes: &[u8]) -> (f64, u64) {
    let (mut sum, mut n) = (0f64, 0u64);
    for c in bytes.chunks_exact(2) {
        let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
        sum += v * v;
        n += 1;
    }
    (sum, n)
}

/// What the log says about a minute, or None when no input was heard (> -70 dB): each
/// input's level and the output's, and a mark when the output is 10 dB quieter than its
/// loudest input (the mixer dropped sound, as when a voice broke up 2026-11-24). Every level
/// is over the output's time (same format): a voice silent most of the minute sends nothing
/// then, and is not louder for it. The counts start over. STREAMER_DEBUG_PMIX_NO_MARK: never
/// marked (the spoiled variant).
fn levels_line(l: &mut Levels) -> Option<String> {
    let db = |sum: f64, n: u64| if n > 0 && sum > 0.0 { 10.0 * (sum / n as f64).log10() } else { -150.0 };
    let span = l.out.1;
    let mut rows: Vec<(String, f64)> = l.inputs.values_mut().filter(|e| e.2 > 0).map(|e| (e.0.clone(), db(std::mem::take(&mut e.1), std::mem::take(&mut e.2).max(span)))).collect();
    let out = db(l.out.0, l.out.1);
    l.out = (0.0, 0);
    let loudest = rows.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
    if loudest < -70.0 {
        return None;
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let list = rows.iter().map(|(n, d)| format!("{n} {d:.0}")).collect::<Vec<_>>().join(", ");
    let marked = out < loudest - 10.0 && std::env::var_os("STREAMER_DEBUG_PMIX_NO_MARK").is_none();
    let cut = if marked { " (quieter than an input: the mixer dropped sound)" } else { "" };
    Some(format!("playback: sound over the last minute, dB: out {out:.0}{cut}; {list}"))
}

/// Once a minute while the mixer lives: `levels_line`, when there is one.
fn log_levels(mixer: gst::glib::WeakRef<gst::Element>, levels: Arc<Mutex<Levels>>) {
    let _ = std::thread::Builder::new().name("playback-levels".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        if mixer.upgrade().is_none() {
            return;
        }
        let line = levels_line(&mut levels.lock().unwrap());
        if let Some(line) = line {
            eprintln!("{line}");
        }
    });
}

#[cfg(test)]
mod level_tests {
    use super::*;

    fn feed(l: &mut Levels, key: &str, label: &str, db: f64) {
        let e = l.inputs.entry(key.into()).or_insert((label.into(), 0.0, 0));
        e.1 += 10f64.powf(db / 10.0) * 1000.0;
        e.2 += 1000;
    }

    /// The minute's line: nothing when no input was heard; the mark only when the output is
    /// 10 dB quieter than its loudest input; the counts start over after each line.
    /// Spoiled: STREAMER_DEBUG_PMIX_NO_MARK.
    #[test]
    fn the_level_line_marks_a_mixer_that_drops_sound() {
        let mut l = Levels::default();
        feed(&mut l, "sink_1", "voice 22", -90.0);
        l.out = (10f64.powf(-9.0) * 1000.0, 1000);
        assert_eq!(levels_line(&mut l), None, "nobody was heard");
        feed(&mut l, "sink_1", "voice 22", -30.0);
        feed(&mut l, "sink_2", "stream", -50.0);
        l.out = (10f64.powf(-3.0) * 1000.0, 1000);
        let line = levels_line(&mut l).unwrap();
        assert!(line.contains("out -30;") && line.contains("stream -50, voice 22 -30") && !line.contains("dropped"), "{line}");
        feed(&mut l, "sink_1", "voice 22", -30.0);
        l.out = (10f64.powf(-4.5) * 1000.0, 1000);
        let line = levels_line(&mut l).unwrap();
        assert!(line.contains("dropped sound"), "cut by 15 dB: {line}");
        assert_eq!(levels_line(&mut l), None, "the next minute starts from nothing");
        // A voice heard a tenth of the minute at -30 dB in a -40 dB output: not a drop.
        let e = l.inputs.entry("sink_1".into()).or_insert(("voice 22".into(), 0.0, 0));
        e.1 += 10f64.powf(-3.0) * 100.0;
        e.2 += 100;
        l.out = (10f64.powf(-4.0) * 1000.0, 1000);
        let line = levels_line(&mut l).unwrap();
        assert!(!line.contains("dropped"), "a voice that spoke a little: {line}");
    }
}
