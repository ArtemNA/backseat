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
/// The name webrtcdsp finds its echo probe by: one voice per process.
const ECHO_PROBE: &str = "voice-echo";
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
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self { ice: IceConfig::default(), kbit: 32, latency_ms: 60, test_tone: None, silent: false }
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
    stopped: AtomicBool,
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
        let probe = gst::ElementFactory::make("webrtcechoprobe").name(ECHO_PROBE).build().map_err(|e| format!("webrtcechoprobe: {e}"))?;
        let deafen = gst::ElementFactory::make("volume").name("vout").build().unwrap();
        let sink = if cfg.silent {
            gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap()
        } else {
            let s = make(crate::platform::audio_sink())?;
            if s.has_property("low-latency") {
                s.set_property("low-latency", true);
            }
            s
        };
        let out = [silence.clone(), mono_caps(), mixer.clone(), mono_caps(), probe, deafen, make("audioconvert")?, make("audioresample")?, sink];
        pipeline.add_many(&out).map_err(|e| e.to_string())?;
        gst::Element::link_many(&out[..2]).map_err(|e| e.to_string())?;
        out[1].link(&mixer).map_err(|e| e.to_string())?;
        gst::Element::link_many(&out[2..]).map_err(|e| e.to_string())?;

        // In.
        let src = match cfg.test_tone {
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
                s
            }
        };
        let dsp = make("webrtcdsp")?;
        dsp.set_property("probe", ECHO_PROBE);
        dsp.set_property("echo-cancel", std::env::var_os("STREAMER_DEBUG_VOICE_NO_AEC").is_none());
        let tested = cfg.test_tone.is_some();
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

        bus(&pipeline);
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("cannot start voice (microphone or speakers): {e}"))?;
        Ok(Arc::new(Self { pipeline, cfg, peers: Mutex::default(), mic_level, stopped: AtomicBool::new(false) }))
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
        let (pw, r, lv) = (self.pipeline.downgrade(), recv.clone(), level.clone());
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(p) = pw.upgrade() else { return };
            match receive(&p, pad, id, &lv) {
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
        if let Some(v) = self.pipeline.by_name("vmic") {
            v.set_property("mute", on);
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
        if let Some(e) = self.pipeline.by_name(&format!("vvol-{id}")) {
            e.set_property("volume", v.clamp(0.0, 2.0));
        }
    }

    /// dBFS of our microphone (after the echo canceller) over the last 100 ms.
    pub fn mic_db(&self) -> f64 {
        self.mic_level.db()
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
fn receive(pipeline: &gst::Pipeline, pad: &gst::Pad, id: PeerId, level: &Arc<Level>) -> Result<(Vec<gst::Element>, gst::Pad), String> {
    let dec = make("opusdec")?;
    dec.set_property("plc", true);
    dec.set_property("use-inband-fec", true);
    let vol = gst::ElementFactory::make("volume").name(format!("vvol-{id}")).build().unwrap();
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
