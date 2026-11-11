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
    /// Receive side, made when the peer's audio arrives, and its mixer pad.
    recv: Arc<Mutex<Option<(Vec<gst::Element>, gst::Pad)>>>,
    level: Arc<Level>,
    link: Arc<dyn Outbox>,
}

pub struct Voice {
    pipeline: gst::Pipeline,
    cfg: VoiceConfig,
    peers: Mutex<HashMap<PeerId, Peer>>,
    mic_level: Arc<Level>,
    /// The microphone before the echo canceller (gates: the echo is there to cancel).
    raw_level: Arc<Level>,
    stopped: AtomicBool,
    /// Our input of the shared output, kept while the voice lives.
    _output: Arc<crate::playback::Input>,
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
        let pipeline = gst::Pipeline::with_name("voice");

        // Out: silence keeps the mixer (and the echo probe) running with nobody talking.
        let silence = gst::ElementFactory::make("audiotestsrc")
            .property_from_str("wave", "silence")
            .property("is-live", true)
            .property("samplesperbuffer", 480i32)
            .build()
            .map_err(|e| e.to_string())?;
        let mixer = gst::ElementFactory::make("audiomixer").name("vmix").build().map_err(|e| e.to_string())?;
        let deafen = gst::ElementFactory::make("volume").name("vout").build().unwrap();
        // To the speakers through the process's shared output, whose echo probe the
        // canceller below listens to (it hears the streams we watch there too).
        if cfg.silent {
            crate::playback::set_silent();
        }
        let (tail, output) = crate::playback::tail("voice", true)?;
        let mut out = vec![silence.clone(), mono_caps(), mixer.clone(), mono_caps(), deafen];
        out.extend(tail);
        pipeline.add_many(&out).map_err(|e| e.to_string())?;
        gst::Element::link_many(&out[..2]).map_err(|e| e.to_string())?;
        out[1].link(&mixer).map_err(|e| e.to_string())?;
        gst::Element::link_many(&out[2..]).map_err(|e| e.to_string())?;

        // In.
        // Tests only: STREAMER_DEBUG_VOICE_ROOM makes the microphone hear what our speakers
        // play, 40 ms later and 6 dB down, as a room would (and nothing else). Timestamped as a
        // real microphone would be: when the sound reached it, 40 ms after it was played.
        let room = std::env::var_os("STREAMER_DEBUG_VOICE_ROOM").is_some();
        let src = if room {
            let a = gst_app::AppSrc::builder().caps(&crate::playback::caps()).is_live(true).format(gst::Format::Time).min_latency(0).max_latency(1_000_000_000).build();
            let (feed, pw) = (a.clone(), pipeline.downgrade());
            crate::playback::get()?.tap(move |mut b| {
                let (Some(p), Some(at)) = (pw.upgrade(), b.pts()) else { return };
                let Some(base) = p.base_time() else { return };
                let Some(rt) = (at + gst::ClockTime::from_mseconds(40)).checked_sub(base) else { return };
                {
                    let b = b.make_mut();
                    b.set_pts(rt);
                    b.set_dts(gst::ClockTime::NONE);
                }
                let _ = feed.push_buffer(b);
            })?;
            let bin = gst::Bin::new();
            let q = gst::ElementFactory::make("queue").property("max-size-time", 1_000_000_000u64).property("max-size-buffers", 0u32).property("max-size-bytes", 0u32).build().unwrap();
            let v = gst::ElementFactory::make("volume").property("volume", 0.5f64).build().unwrap();
            bin.add_many([a.upcast_ref(), &q, &v]).unwrap();
            gst::Element::link_many([a.upcast_ref(), &q, &v]).unwrap();
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
        let mute = gst::ElementFactory::make("volume").name("vmic").build().unwrap();
        let opus = make("opusenc")?;
        opus.set_property_from_str("audio-type", "voice");
        opus.set_property_from_str("frame-size", "20");
        opus.set_property("bitrate", (cfg.kbit * 1000) as i32);
        opus.set_property("inband-fec", true);
        opus.set_property("packet-loss-percentage", 10i32);
        let tee = gst::ElementFactory::make("tee").name("vtee").property("allow-not-linked", true).build().unwrap();
        let inp = [src, make("audioconvert")?, make("audioresample")?, mono_caps(), dsp, mute.clone(), opus, tee];
        pipeline.add_many(&inp).map_err(|e| e.to_string())?;
        gst::Element::link_many(&inp).map_err(|e| e.to_string())?;
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

        bus(&pipeline);
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("cannot start voice (microphone or speakers): {e}"))?;
        Ok(Arc::new(Self { pipeline, cfg, peers: Mutex::default(), mic_level, raw_level, stopped: AtomicBool::new(false), _output: output, mic_muted: AtomicBool::new(false), transmit: AtomicBool::new(true), volumes: Arc::default(), gate }))
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
        let (pw, r, lv, vols) = (self.pipeline.downgrade(), recv.clone(), level.clone(), self.volumes.clone());
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(p) = pw.upgrade() else { return };
            let volume = vols.lock().unwrap().get(&id).copied().unwrap_or(1.0);
            match receive(&p, pad, id, &lv, volume) {
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
        for e in [&webrtc, &pay, &queue] {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        let tee = self.pipeline.by_name("vtee").unwrap();
        let tee_pad = tee.request_pad_simple("src_%u").unwrap();
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
        let tee = self.pipeline.by_name("vtee").unwrap();
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
            if let Some(m) = self.pipeline.by_name("vmix") {
                m.release_request_pad(&mix_pad);
            }
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

    pub fn transmitting(&self) -> bool {
        self.transmit.load(Ordering::SeqCst) && !self.mic_muted.load(Ordering::SeqCst)
    }

    fn apply_mic(&self) {
        if let Some(v) = self.pipeline.by_name("vmic") {
            v.set_property("mute", !self.transmitting());
        }
    }

    /// Nobody heard (and, as in other voice apps, the microphone stays as it is).
    pub fn set_deafened(&self, on: bool) {
        if let Some(v) = self.pipeline.by_name("vout") {
            v.set_property("mute", on);
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

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        for id in self.peer_ids() {
            self.remove_peer(id);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// A peer's audio: depay -> decode -> its volume -> the mixer.
fn receive(pipeline: &gst::Pipeline, pad: &gst::Pad, id: PeerId, level: &Arc<Level>, volume: f64) -> Result<(Vec<gst::Element>, gst::Pad), String> {
    let dec = make("opusdec")?;
    dec.set_property("plc", true);
    dec.set_property("use-inband-fec", true);
    let vol = gst::ElementFactory::make("volume").name(format!("vvol-{id}")).property("volume", volume).build().unwrap();
    let chain = vec![make("rtpopusdepay")?, dec, make("audioconvert")?, make("audioresample")?, mono_caps(), vol.clone()];
    pipeline.add_many(&chain).map_err(|e| e.to_string())?;
    gst::Element::link_many(&chain).map_err(|e| e.to_string())?;
    level.watch(&vol.static_pad("src").unwrap());
    let mixer = pipeline.by_name("vmix").ok_or("no mixer")?;
    let mix_pad = mixer.request_pad_simple("sink_%u").ok_or("no mixer pad")?;
    vol.static_pad("src").unwrap().link(&mix_pad).map_err(|e| format!("{e:?}"))?;
    pad.link(&chain[0].static_pad("sink").unwrap()).map_err(|e| format!("{e:?}"))?;
    for e in chain.iter().rev() {
        e.sync_state_with_parent().map_err(|e| e.to_string())?;
    }
    eprintln!("voice: hearing {id}");
    Ok((chain, mix_pad))
}

/// Errors and latency changes (a new peer's branch changes it).
fn bus(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();
    let weak = pipeline.downgrade();
    std::thread::Builder::new()
        .name("voice-bus".into())
        .spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    gst::MessageView::Latency(_) => {
                        if let Some(p) = weak.upgrade() {
                            let _ = p.recalculate_latency();
                        }
                    }
                    gst::MessageView::Error(e) => {
                        eprintln!("voice: error from {}: {}", e.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error());
                    }
                    _ => {}
                }
                if weak.upgrade().is_none() {
                    return;
                }
            }
        })
        .unwrap();
}
