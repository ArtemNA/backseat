//! Voice chat in a room: everyone in voice talks with everyone else in voice, each pair over
//! its own WebRTC connection (a mesh: rooms are small, ~32 kbit/s per pair). One member's bad
//! link stays on its own pairs: its send queue drops, and the mixer does not wait for it.
//!
//! microphone -> webrtcdsp (echo, noise, gain) -> volume (mute) -> opusenc -> tee
//!     tee -> leaky queue -> rtpopuspay -> webrtcbin (one per peer)
//! webrtcbin -> depay -> opusdec -> volume (per peer) -> audiomixer
//!     audiomixer -> webrtcechoprobe (what the speakers play, for the echo canceller)
//!                -> volume (deafen) -> speakers
//!
//! The speakers are played by this process, and the stream's system-sound capture leaves
//! out our own process tree (host.rs), so the voices never go into a stream.

use crate::config::IceConfig;
use crate::signal::Outbox;
use gst::prelude::*;
use gst_webrtc::{WebRTCSDPType, WebRTCSessionDescription};
use protocol::PeerId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VOICE_PT: u32 = 111;
use crate::playback::ECHO_PROBE;
const RATE: i32 = 48_000;

#[derive(Clone, Debug)]
pub struct VoiceConfig {
    pub ice: IceConfig,
    /// Opus bitrate.
    pub kbit: u32,
    /// Receive jitter buffer, ms.
    pub latency_ms: u32,
    /// Test only: a steady tone at this frequency instead of the microphone; noise
    /// suppression and gain control off (they would take a steady tone for noise).
    pub test_tone: Option<f64>,
    /// Test only: no sound device out; levels are still measured.
    pub silent: bool,
    /// The microphone (a device id from `playback::devices(true)`), None for the system's.
    pub mic: Option<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self { ice: IceConfig::default(), kbit: 32, latency_ms: 60, test_tone: None, silent: false, mic: None }
    }
}

/// A sound level, updated every 100 ms: for "who is speaking" and the gates.
#[derive(Default)]
pub struct Level {
    acc: Mutex<(f64, u64, Option<Instant>)>,
    /// dBFS of the last 100 ms, -100 for silence, stored as bits.
    last: std::sync::atomic::AtomicU64,
    /// When `last` was measured, ms since `epoch()`.
    at: std::sync::atomic::AtomicU64,
    /// The loudest 100 ms since `take_max`, as bits (0: none).
    max: std::sync::atomic::AtomicU64,
}

fn epoch() -> Instant {
    static E: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *E.get_or_init(Instant::now)
}

impl Level {
    fn feed(&self, samples: &[u8]) {
        let mut a = self.acc.lock().unwrap();
        for s in samples.chunks_exact(2) {
            let v = i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0;
            a.0 += v * v;
            a.1 += 1;
        }
        let start = *a.2.get_or_insert_with(Instant::now);
        if start.elapsed() >= Duration::from_millis(100) && a.1 > 0 {
            let db = 20.0 * (a.0 / a.1 as f64).sqrt().max(1e-5).log10();
            self.last.store(db.to_bits(), Ordering::Relaxed);
            self.at.store(epoch().elapsed().as_millis() as u64, Ordering::Relaxed);
            let m = self.max.load(Ordering::Relaxed);
            if m == 0 || db > f64::from_bits(m) {
                self.max.store(db.to_bits(), Ordering::Relaxed);
            }
            *a = (0.0, 0, Some(Instant::now()));
        }
    }

    /// dBFS of the last 100 ms of sound (-100: nothing, or nothing for 300 ms).
    pub fn db(&self) -> f64 {
        if epoch().elapsed().as_millis() as u64 > self.at.load(Ordering::Relaxed) + 300 {
            return -100.0;
        }
        match self.last.load(Ordering::Relaxed) {
            0 => -100.0,
            b => f64::from_bits(b),
        }
    }

    /// The loudest 100 ms since the last call (-100: none).
    pub fn take_max(&self) -> f64 {
        match self.max.swap(0, Ordering::Relaxed) {
            0 => -100.0,
            b => f64::from_bits(b),
        }
    }

    /// Measures S16 mono buffers passing `pad`.
    fn watch(self: &Arc<Self>, pad: &gst::Pad) {
        let me = self.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                me.feed(map.as_slice());
            }
            gst::PadProbeReturn::Ok
        });
    }
}

struct Peer {
    webrtc: gst::Element,
    /// Send side: queue and payloader, and the tee pad feeding them.
    send: Vec<gst::Element>,
    tee_pad: gst::Pad,
    /// Receive side, made when the peer's audio arrives, and its pad on the shared mixer.
    recv: Arc<Mutex<Option<(Vec<gst::Element>, gst::Pad)>>>,
    level: Arc<Level>,
    link: Arc<dyn Outbox>,
}

pub struct Voice {
    /// The process's shared output pipeline: the voice lives inside it (one clock, the
    /// peers' sound straight into its mixer, the echo probe next to the canceller).
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    /// The microphone and its echo canceller (a pipeline of their own).
    mic_pipe: gst::Pipeline,
    /// The send chain in the shared pipeline (encoder, fan-out), removed when the voice stops.
    mic: Vec<gst::Element>,
    mute: gst::Element,
    tee: gst::Element,
    /// Stops the test room's feeding thread.
    room_stop: Arc<AtomicBool>,
    cfg: VoiceConfig,
    peers: Mutex<HashMap<PeerId, Peer>>,
    mic_level: Arc<Level>,
    /// The microphone before the echo canceller (gates: the echo is there to cancel).
    raw_level: Arc<Level>,
    stopped: AtomicBool,
    /// Sound off: every peer's volume element muted, now and for peers that come later.
    deafened: Arc<AtomicBool>,
    /// Microphone off by the user, and whether we may send now (push-to-talk: the key is
    /// down; otherwise always). Sound goes out only when not muted and allowed.
    mic_muted: AtomicBool,
    transmit: AtomicBool,
    /// Loudness per peer (1: as received), kept for a peer whose sound comes later.
    volumes: Arc<Mutex<HashMap<PeerId, f64>>>,
    /// Voice activity threshold (`Gate`): below it for a while, silence goes out.
    gate: Arc<Gate>,
}

/// Voice activity: the microphone (after noise suppression) opens above a threshold and
/// stays open 300 ms after the sound falls below it, so word ends are not cut.
#[derive(Default)]
pub struct Gate {
    /// Threshold dBFS as bits, 0 for none (always open).
    threshold: std::sync::atomic::AtomicU64,
    /// The level of each 10 ms before the gate, dBFS as bits (for the settings' meter).
    level: std::sync::atomic::AtomicU64,
    /// The loudest 10 ms before the gate since `take_max`, as bits (0: none).
    max: std::sync::atomic::AtomicU64,
    open_until: Mutex<Option<Instant>>,
}

const GATE_HOLD: Duration = Duration::from_millis(300);

impl Gate {
    fn pass(&self, data: &mut [u8]) {
        let mut sum = 0f64;
        let n = data.len() / 2;
        for s in data.chunks_exact(2) {
            let v = i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0;
            sum += v * v;
        }
        let db = if n > 0 { 20.0 * (sum / n as f64).sqrt().max(1e-5).log10() } else { -100.0 };
        self.level.store(db.to_bits(), Ordering::Relaxed);
        let m = self.max.load(Ordering::Relaxed);
        if m == 0 || db > f64::from_bits(m) {
            self.max.store(db.to_bits(), Ordering::Relaxed);
        }
        let t = self.threshold.load(Ordering::Relaxed);
        if t == 0 {
            return;
        }
        let mut until = self.open_until.lock().unwrap();
        if db >= f64::from_bits(t) {
            *until = Some(Instant::now() + GATE_HOLD);
        }
        if !until.is_some_and(|u| Instant::now() < u) {
            data.fill(0);
        }
    }

    pub fn level_db(&self) -> f64 {
        match self.level.load(Ordering::Relaxed) {
            0 => -100.0,
            b => f64::from_bits(b),
        }
    }
}

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory).build().map_err(|e| format!("{factory}: {e}"))
}

fn mono_caps() -> gst::Element {
    let caps = gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("rate", RATE).field("channels", 1i32).field("layout", "interleaved").build();
    gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap()
}

impl Voice {
    pub fn new(cfg: VoiceConfig) -> Result<Arc<Self>, String> {
        // The peers (their connections, the sound to the speakers) live in the shared output's
        // pipeline: one clock, their sound straight into its mixer. The microphone with the
        // echo canceller has a pipeline of its own and hands what it sends over: in the shared
        // pipeline, behind the mixer, the canceller cancelled nothing (stream echo -34.9 dBFS
        // left of -32), while in a pipeline of its own it does (-60..-85, the gate).
        if cfg.silent {
            crate::playback::set_silent();
        }
        let out = crate::playback::get()?;
        let pipeline = out.pipeline().clone();
        let mixer = out.mixer().clone();
        let room_stop = Arc::new(AtomicBool::new(false));
        let mic_pipe = gst::Pipeline::with_name("voice-mic");
        mic_pipe.use_clock(Some(&gst::SystemClock::obtain()));

        // In.
        // Tests only: STREAMER_DEBUG_VOICE_ROOM makes the microphone hear what our speakers
        // play, 40 ms later and 6 dB down, as a room would (and nothing else). Timestamped as a
        // real microphone would be: when the sound reached it, 40 ms after it was played.
        // "<delay ms>:<gain>" (default "40:0.5"); "1000:1" is an echo bot for a person to
        // hear themselves.
        let room_spec = std::env::var("STREAMER_DEBUG_VOICE_ROOM").ok();
        let room = room_spec.is_some();
        let (room_ms, room_gain) = room_spec
            .as_deref()
            .and_then(|v| v.split_once(':'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<f64>().ok()?)))
            .unwrap_or((40, 0.5));
        let src = if room {
            // What the speakers play, heard `room_ms` after it is played: a buffer plays at
            // its running time plus the pipeline's latency (when the sink renders it), so the
            // microphone hears it then plus the room's delay, stamped with that time, and is
            // handed on when the clock gets there. (Stamping on arrival from the mixer made
            // the echo come before its sound once the voice added latency to the pipeline.)
            let a = gst_app::AppSrc::builder().caps(&crate::playback::caps()).is_live(true).format(gst::Format::Time).min_latency(0).max_latency(1_000_000_000).build();
            let line: Arc<Mutex<std::collections::VecDeque<gst::Buffer>>> = Arc::default();
            let l = line.clone();
            // `tap` gives clock time (running time + base time).
            crate::playback::get()?.tap(move |b| l.lock().unwrap().push_back(b))?;
            let (feed, stop, delay, pw, mw) = (a.clone(), room_stop.clone(), gst::ClockTime::from_mseconds(room_ms), pipeline.downgrade(), mic_pipe.downgrade());
            std::thread::Builder::new()
                .name("voice-room".into())
                .spawn(move || {
                    let (mut latency, mut asked) = (gst::ClockTime::ZERO, Instant::now() - Duration::from_secs(1));
                    loop {
                    std::thread::sleep(Duration::from_millis(1));
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let (Some(_p), Some(m)) = (pw.upgrade(), mw.upgrade()) else { return };
                    // Played at the shared pipeline's running time + the speakers' latency;
                    // stamped in the microphone pipeline's time (same clock, own base time).
                    let (Some(clock), Some(base)) = (m.clock(), m.base_time()) else { continue };
                    if asked.elapsed() > Duration::from_millis(500) {
                        if let Ok(out) = crate::playback::get() {
                            latency = out.render_latency();
                        }
                        asked = Instant::now();
                    }
                    let now = clock.time();
                    loop {
                        let due = {
                            let mut q = line.lock().unwrap();
                            match q.front().and_then(|b| b.pts()) {
                                Some(at) if now >= at + latency + delay => q.pop_front(),
                                Some(_) => None,
                                // No stamp: cannot place it; dropped.
                                None => {
                                    q.pop_front();
                                    None
                                }
                            }
                        };
                        match due {
                            Some(mut b) => {
                                // Stamped `delay` after the buffer's own stamp. (Adding the speakers'
                                // latency, as a room hears it, the canceller cancelled nothing; the
                                // real microphone is measured in the voice log instead: device vs
                                // processed level.)
                                let heard = b.pts().unwrap() + delay;
                                let _ = latency;
                                {
                                    let b = b.make_mut();
                                    b.set_pts(heard.checked_sub(base));
                                    b.set_dts(gst::ClockTime::NONE);
                                }
                                let _ = feed.push_buffer(b);
                            }
                            None => break,
                        }
                    }
                    }
                })
                .unwrap();
            let bin = gst::Bin::new();
            let v = gst::ElementFactory::make("volume").property("volume", room_gain).build().unwrap();
            bin.add_many([a.upcast_ref(), &v]).unwrap();
            gst::Element::link_many([a.upcast_ref(), &v]).unwrap();
            bin.add_pad(&gst::GhostPad::with_target(&v.static_pad("src").unwrap()).unwrap()).unwrap();
            bin.upcast()
        } else {
            match cfg.test_tone {
            Some(f) => gst::ElementFactory::make("audiotestsrc")
                .property_from_str("wave", "sine")
                .property("freq", f)
                .property("volume", 0.3f64)
                .property("is-live", true)
                .property("samplesperbuffer", 480i32)
                .build()
                .map_err(|e| e.to_string())?,
            None => {
                let s = make("wasapi2src")?;
                if s.has_property("low-latency") {
                    s.set_property("low-latency", true);
                }
                if let (Some(d), true) = (&cfg.mic, s.has_property("device")) {
                    s.set_property("device", d);
                }
                s
            }
            }
        };
        let dsp = make("webrtcdsp")?;
        dsp.set_property("probe", ECHO_PROBE);
        dsp.set_property("echo-cancel", std::env::var_os("STREAMER_DEBUG_VOICE_NO_AEC").is_none());
        let tested = cfg.test_tone.is_some() || room;
        dsp.set_property("noise-suppression", !tested);
        dsp.set_property("gain-control", !tested);
        dsp.set_property("high-pass-filter", true);
        let mute = gst::ElementFactory::make("volume").build().unwrap();
        let opus = make("opusenc")?;
        opus.set_property_from_str("audio-type", "voice");
        opus.set_property_from_str("frame-size", "20");
        opus.set_property("bitrate", (cfg.kbit * 1000) as i32);
        opus.set_property("inband-fec", true);
        opus.set_property("packet-loss-percentage", 10i32);
        let tee = gst::ElementFactory::make("tee").property("allow-not-linked", true).build().unwrap();
        // Capture side, in its own pipeline, up to an appsink...
        let handoff = gst_app::AppSink::builder().sync(false).build();
        handoff.set_property("async", false);
        let inp = [src, make("audioconvert")?, make("audioresample")?, mono_caps(), dsp, mute.clone(), handoff.clone().upcast()];
        mic_pipe.add_many(&inp).map_err(|e| e.to_string())?;
        gst::Element::link_many(&inp).map_err(|e| e.to_string())?;
        // ...and on in the shared pipeline: encoder and the fan-out to the peers.
        let mono = gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("rate", RATE).field("channels", 1i32).field("layout", "interleaved").build();
        let send_src = gst_app::AppSrc::builder().caps(&mono).is_live(true).format(gst::Format::Time).do_timestamp(true).min_latency(0).max_latency(1_000_000_000).build();
        send_src.set_property("block", false);
        send_src.set_property_from_str("leaky-type", "downstream");
        send_src.set_max_bytes((RATE * 2 / 5) as u64); // 200 ms
        let feed = send_src.clone();
        handoff.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    if let Some(mut b) = sink.pull_sample().ok().and_then(|s| s.buffer_owned()) {
                        {
                            let b = b.make_mut();
                            b.set_pts(gst::ClockTime::NONE);
                            b.set_dts(gst::ClockTime::NONE);
                        }
                        let _ = feed.push_buffer(b);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let send = [send_src.upcast(), opus, tee.clone()];
        pipeline.add_many(&send).map_err(|e| e.to_string())?;
        gst::Element::link_many(&send).map_err(|e| e.to_string())?;
        let mic_level = Arc::new(Level::default());
        mic_level.watch(&mute.static_pad("src").unwrap());
        // The voice activity gate, between the canceller (its noise suppression) and mute.
        let gate = Arc::new(Gate::default());
        let g = gate.clone();
        inp[4].static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(ref mut b)) = info.data {
                let b = b.make_mut();
                if let Ok(mut m) = b.map_writable() {
                    g.pass(m.as_mut_slice());
                }
            }
            gst::PadProbeReturn::Ok
        });
        let raw_level = Arc::new(Level::default());
        raw_level.watch(&inp[4].static_pad("sink").unwrap());

        // The echo probe is in the output from its start (`Playback::new`). A probe put in only
        // for the voice (to avoid the backlog below) cancelled nothing in the gate, cause not
        // found. Known, open: after watching a while before joining voice the probe is behind
        // and nothing is cancelled (gate -VoiceAfter 20: -34.9 dBFS left of -32).
        // The shared pipeline already plays: start the send chain downstream first.
        for e in send.iter().rev() {
            let _ = e.sync_state_with_parent();
        }
        if let Err(e) = mic_pipe.set_state(gst::State::Playing) {
            let _ = mic_pipe.set_state(gst::State::Null);
            for e in &send {
                let _ = e.set_state(gst::State::Null);
                let _ = pipeline.remove(e);
            }
            return Err(format!("cannot open the microphone: {e}"));
        }
        crate::drain_bus(&mic_pipe, "voice-mic");
        Ok(Arc::new(Self { pipeline, mixer, mic_pipe, mic: send.to_vec(), mute, tee, room_stop, cfg, peers: Mutex::default(), mic_level, raw_level, stopped: AtomicBool::new(false), deafened: Arc::default(), mic_muted: AtomicBool::new(false), transmit: AtomicBool::new(true), volumes: Arc::default(), gate }))
    }

    pub fn has(&self, id: PeerId) -> bool {
        self.peers.lock().unwrap().contains_key(&id)
    }

    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.peers.lock().unwrap().keys().copied().collect()
    }

    /// Opens a connection to `id`. The side with the lower id offers.
    pub fn add_peer(&self, id: PeerId, link: Arc<dyn Outbox>, offerer: bool) -> Result<(), String> {
        // Held while the connection is made: the session may ask for the same peer from two
        // threads at once (our join meeting their "hi"), and the second one must find it.
        let mut peers = self.peers.lock().unwrap();
        if self.stopped.load(Ordering::SeqCst) || peers.contains_key(&id) {
            return Ok(());
        }
        let webrtc = gst::ElementFactory::make("webrtcbin").name(format!("voice-{id}")).build().map_err(|e| e.to_string())?;
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        webrtc.set_property("latency", self.cfg.latency_ms);
        // The jitter buffers kept rtpbin's own 200 ms whatever webrtcbin was told, and in the
        // shared output that latency is everyone's: a watched stream's sound came 170 ms later
        // while anyone was in voice (output latency 210 ms against 40). Set on each one.
        if let Some(rtpbin) = webrtc.downcast_ref::<gst::Bin>().and_then(|b| b.by_name("rtpbin")) {
            let ms = self.cfg.latency_ms;
            rtpbin.connect("new-jitterbuffer", false, move |v| {
                if let Ok(jb) = v[1].get::<gst::Element>() {
                    jb.set_property("latency", ms);
                }
                None
            });
        }
        crate::ice::configure(&webrtc, &self.cfg.ice);
        crate::ice::watch(&webrtc, format!("voice {id}"), self.cfg.ice.turn.is_some(), || {});
        let l = link.clone();
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let cand = v[2].get::<String>().unwrap();
            l.send("ice", &format!("{mline} {cand}"));
            None
        });
        if offerer {
            let (offered, l) = (Arc::new(AtomicBool::new(false)), link.clone());
            webrtc.connect("on-negotiation-needed", false, move |v| {
                if offered.swap(true, Ordering::SeqCst) {
                    return None;
                }
                let w = v[0].get::<gst::Element>().unwrap();
                let (w2, l) = (w.clone(), l.clone());
                let promise = gst::Promise::with_change_func(move |reply| {
                    let Ok(Some(reply)) = reply else { return };
                    let Ok(offer) = reply.get::<WebRTCSessionDescription>("offer") else { return };
                    w2.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                    l.send("offer", &offer.sdp().as_text().unwrap());
                });
                w.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
                None
            });
        }
        let level = Arc::new(Level::default());
        let recv: Arc<Mutex<Option<(Vec<gst::Element>, gst::Pad)>>> = Arc::default();
        let (pw, mixer, r, lv, vols, deaf) = (self.pipeline.downgrade(), self.mixer.clone(), recv.clone(), level.clone(), self.volumes.clone(), self.deafened.clone());
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(p) = pw.upgrade() else { return };
            let volume = vols.lock().unwrap().get(&id).copied().unwrap_or(1.0);
            match receive(&p, &mixer, pad, id, &lv, volume, deaf.load(Ordering::SeqCst)) {
                Ok(chain) => *r.lock().unwrap() = Some(chain),
                Err(e) => eprintln!("voice: cannot play {id}: {e}"),
            }
        });

        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 5u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .unwrap();
        let pay = make("rtpopuspay")?;
        pay.set_property("pt", VOICE_PT);
        self.pipeline.add_many([&webrtc, &queue, &pay]).map_err(|e| e.to_string())?;
        gst::Element::link_many([&queue, &pay, &webrtc]).map_err(|e| e.to_string())?;
        // The format up front: webrtcbin offers only once it knows what the pad carries, and
        // waiting for the first sound lost the offer now and then (a microphone that is slow
        // to open, the echo bot's delay): the two sides never connected.
        let rtp_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48000i32)
            .field("payload", VOICE_PT as i32)
            .build();
        if let Some(t) = pay.static_pad("src").and_then(|p| p.peer()).and_then(|p| p.property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver")) {
            t.set_property("codec-preferences", &rtp_caps);
        }
        for e in [&webrtc, &pay, &queue] {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        let tee_pad = self.tee.request_pad_simple("src_%u").unwrap();
        // Spoiled variant for the gate: nothing is sent.
        if std::env::var_os("STREAMER_DEBUG_VOICE_NO_SEND").is_none() {
            tee_pad.link(&queue.static_pad("sink").unwrap()).map_err(|e| format!("voice {id}: {e:?}"))?;
        }
        peers.insert(id, Peer { webrtc, send: vec![queue, pay], tee_pad, recv, level, link });
        eprintln!("voice: connecting with {id} ({})", if offerer { "offering" } else { "answering" });
        Ok(())
    }

    /// Closes the connection to `id`.
    pub fn remove_peer(&self, id: PeerId) {
        let Some(p) = self.peers.lock().unwrap().remove(&id) else { return };
        let tee = &self.tee;
        // Unlink the tee pad between buffers.
        if let Some(peer) = p.tee_pad.peer() {
            let (tx, rx) = std::sync::mpsc::channel();
            p.tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                let _ = pad.unlink(&peer);
                let _ = tx.send(());
                gst::PadProbeReturn::Remove
            });
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
        tee.release_request_pad(&p.tee_pad);
        let _ = p.webrtc.set_state(gst::State::Null);
        for e in &p.send {
            let _ = e.set_state(gst::State::Null);
        }
        let _ = self.pipeline.remove(&p.webrtc);
        for e in &p.send {
            let _ = self.pipeline.remove(e);
        }
        if let Some((chain, mix_pad)) = p.recv.lock().unwrap().take() {
            for e in &chain {
                let _ = e.set_state(gst::State::Null);
                let _ = self.pipeline.remove(e);
            }
            self.mixer.release_request_pad(&mix_pad);
        }
        eprintln!("voice: {id} gone");
    }

    /// One signaling message from `id`: offer, answer, ice.
    pub fn on_message(&self, id: PeerId, kind: &str, payload: &str) {
        let Some((webrtc, link)) = self.peers.lock().unwrap().get(&id).map(|p| (p.webrtc.clone(), p.link.clone())) else { return };
        match kind {
            "offer" => {
                let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(payload.as_bytes()) else { return };
                webrtc.emit_by_name::<()>("set-remote-description", &[&WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp), &None::<gst::Promise>]);
                let w = webrtc.clone();
                let promise = gst::Promise::with_change_func(move |reply| {
                    let Ok(Some(reply)) = reply else { return };
                    let Ok(answer) = reply.get::<WebRTCSessionDescription>("answer") else { return };
                    w.emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                    link.send("answer", &answer.sdp().as_text().unwrap());
                });
                webrtc.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
            }
            "answer" => {
                if let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(payload.as_bytes()) {
                    webrtc.emit_by_name::<()>("set-remote-description", &[&WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp), &None::<gst::Promise>]);
                }
            }
            "ice" => crate::host::add_ice(&webrtc, payload),
            _ => {}
        }
    }

    /// Our microphone off (silence is sent) or on.
    pub fn set_mic_muted(&self, on: bool) {
        self.mic_muted.store(on, Ordering::SeqCst);
        self.apply_mic();
    }

    /// Push-to-talk: send only while `on` (the key is down). Without push-to-talk, on.
    pub fn set_transmit(&self, on: bool) {
        self.transmit.store(on, Ordering::SeqCst);
        self.apply_mic();
    }

    /// Voice activity threshold in dBFS, None to send all the time.
    pub fn set_threshold(&self, db: Option<f64>) {
        self.gate.threshold.store(db.map_or(0, f64::to_bits), Ordering::Relaxed);
    }

    /// The microphone's level before the threshold (after noise suppression), last 10 ms.
    pub fn input_db(&self) -> f64 {
        self.gate.level_db()
    }

    /// For the log: the loudest moments since the last call of what the device gave, of it
    /// after echo/noise/gain processing, and of what was sent (after threshold and mute);
    /// dBFS, -100 for nothing.
    pub fn level_report(&self) -> (f64, f64, f64) {
        let g = match self.gate.max.swap(0, Ordering::Relaxed) {
            0 => -100.0,
            b => f64::from_bits(b),
        };
        (self.raw_level.take_max(), g, self.mic_level.take_max())
    }

    pub fn transmitting(&self) -> bool {
        self.transmit.load(Ordering::SeqCst) && !self.mic_muted.load(Ordering::SeqCst)
    }

    fn apply_mic(&self) {
        self.mute.set_property("mute", !self.transmitting());
    }

    /// Nobody heard (and, as in other voice apps, the microphone stays as it is).
    pub fn set_deafened(&self, on: bool) {
        self.deafened.store(on, Ordering::SeqCst);
        for id in self.peer_ids() {
            if let Some(v) = self.pipeline.by_name(&format!("vvol-{id}")) {
                v.set_property("mute", on);
            }
        }
    }

    /// One member's loudness, 0..2 (1: as received).
    pub fn set_peer_volume(&self, id: PeerId, v: f64) {
        let v = v.clamp(0.0, 2.0);
        self.volumes.lock().unwrap().insert(id, v);
        if let Some(e) = self.pipeline.by_name(&format!("vvol-{id}")) {
            e.set_property("volume", v);
        }
    }

    pub fn peer_volume(&self, id: PeerId) -> f64 {
        self.volumes.lock().unwrap().get(&id).copied().unwrap_or(1.0)
    }

    /// dBFS of our microphone (after the echo canceller) over the last 100 ms.
    pub fn mic_db(&self) -> f64 {
        self.mic_level.db()
    }

    /// The loudest 100 ms of the microphone before the echo canceller since the last call.
    pub fn raw_max_db(&self) -> f64 {
        self.raw_level.take_max()
    }

    /// The loudest 100 ms of our microphone since the last call.
    pub fn mic_max_db(&self) -> f64 {
        self.mic_level.take_max()
    }

    /// dBFS of what each peer sends, over the last 100 ms.
    pub fn peer_db(&self) -> Vec<(PeerId, f64)> {
        self.peers.lock().unwrap().iter().map(|(id, p)| (*id, p.level.db())).collect()
    }

    /// Leaves voice: the peers' connections and the microphone chain go out of the shared
    /// pipeline (which keeps playing the streams and sounds).
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        for id in self.peer_ids() {
            self.remove_peer(id);
        }
        self.room_stop.store(true, Ordering::SeqCst);
        let _ = self.mic_pipe.set_state(gst::State::Null);
        for e in &self.mic {
            let _ = e.set_state(gst::State::Null);
        }
        for e in &self.mic {
            let _ = self.pipeline.remove(e);
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A peer's audio: depay -> decode -> its volume -> the shared mixer (same pipeline).
fn receive(pipeline: &gst::Pipeline, mixer: &gst::Element, pad: &gst::Pad, id: PeerId, level: &Arc<Level>, volume: f64, deaf: bool) -> Result<(Vec<gst::Element>, gst::Pad), String> {
    let dec = make("opusdec")?;
    dec.set_property("plc", true);
    // No in-band FEC: to use it the decoder waits for the next packet and reports 120 ms for
    // that (measured: 60 ms after the jitter buffer, 180 after the decoder), and in the shared
    // output every input waits for the slowest: a watched stream's sound came 170 ms later
    // while anyone was in voice. Loss concealment (plc) stays, as for a stream's sound.
    dec.set_property("use-inband-fec", false);
    let vol = gst::ElementFactory::make("volume").name(format!("vvol-{id}")).property("volume", volume).property("mute", deaf).build().unwrap();
    let to_mix = gst::ElementFactory::make("capsfilter").property("caps", crate::playback::caps()).build().unwrap();
    let chain = vec![make("rtpopusdepay")?, dec, make("audioconvert")?, make("audioresample")?, mono_caps(), vol.clone(), make("audioconvert")?, to_mix.clone()];
    pipeline.add_many(&chain).map_err(|e| e.to_string())?;
    gst::Element::link_many(&chain).map_err(|e| e.to_string())?;
    level.watch(&vol.static_pad("src").unwrap());
    // Into the mixer as it comes, stamped then, with no latency of its own: the mixer waits
    // for its slowest input, and with the voice's jitter buffer (60 ms) as that, a watched
    // stream's sound came that much later while anyone was in voice. The jitter buffer still
    // evens out the network before this point. STREAMER_DEBUG_VOICE_OWN_LATENCY: the old way.
    if std::env::var_os("STREAMER_DEBUG_VOICE_OWN_LATENCY").is_none() {
        let out = to_mix.static_pad("src").unwrap();
        let pw = pipeline.downgrade();
        out.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(p) = pw.upgrade() else { return gst::PadProbeReturn::Ok };
            let (Some(clock), Some(base)) = (p.clock(), p.base_time()) else { return gst::PadProbeReturn::Ok };
            if let Some(gst::PadProbeData::Buffer(ref mut b)) = info.data {
                if let Some(now) = clock.time().checked_sub(base) {
                    let b = b.make_mut();
                    b.set_pts(now);
                    b.set_dts(gst::ClockTime::NONE);
                }
            }
            gst::PadProbeReturn::Ok
        });
        out.add_probe(gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL, |_, info| {
            if let Some(gst::PadProbeData::Query(ref mut q)) = info.data {
                if let gst::QueryViewMut::Latency(l) = q.view_mut() {
                    let (live, _min, max) = l.result();
                    l.set(live, gst::ClockTime::ZERO, max);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    let mix_pad = mixer.request_pad_simple("sink_%u").ok_or("no mixer pad")?;
    to_mix.static_pad("src").unwrap().link(&mix_pad).map_err(|e| format!("{e:?}"))?;
    pad.link(&chain[0].static_pad("sink").unwrap()).map_err(|e| format!("{e:?}"))?;
    for e in chain.iter().rev() {
        e.sync_state_with_parent().map_err(|e| e.to_string())?;
    }
    eprintln!("voice: hearing {id}");
    // STREAMER_DEBUG_VOICE_LATENCY: the latency each step of the receive chain reports.
    if std::env::var_os("STREAMER_DEBUG_VOICE_LATENCY").is_some() {
        let steps: Vec<(String, gst::Pad)> = std::iter::once(("webrtcbin".to_string(), pad.clone()))
            .chain(chain.iter().filter_map(|e| e.static_pad("src").map(|p| (e.factory().map(|f| f.name().to_string()).unwrap_or_default(), p))))
            .collect();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            for (name, p) in steps {
                let mut q = gst::query::Latency::new();
                let ok = p.query(&mut q);
                eprintln!("voice latency after {name}: ok {ok} min {} ms", q.result().1.mseconds());
            }
        });
    }
    Ok((chain, mix_pad))
}
