//! Viewer: webrtcbin, hardware decode, presentation, audio playback, latency measurement.

use crate::clock::HostClock;
use crate::config::ViewerConfig;
use crate::host::add_ice;
use crate::platform;
use crate::signal::Outbox;
use crate::stats::Series;
use gst::prelude::*;
use gst_webrtc::{WebRTCSDPType, WebRTCSessionDescription};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Default)]
pub struct ViewerStats {
    pub frame_interval: Series,
    /// Encoder input to decoder output, video.
    pub video_latency: Series,
    /// Encoder input to depayloader output: network and jitterbuffer only.
    pub video_before_decoder: Series,
    /// Encoder input to decoder output, audio.
    pub audio_latency: Series,
    pub frames: Arc<AtomicU64>,
    /// Packet transit per received stream: local arrival minus RTP time, offset by
    /// TRANSIT_BIAS. Only its spread means anything: how unevenly packets arrive.
    pub transit: Arc<Mutex<Vec<Series>>>,
    /// The decoded video's size (the stream's own, before fitting to the window).
    pub video_size: Arc<Mutex<Option<(u32, u32)>>>,
}

/// Keeps transit values positive for `Series`.
pub const TRANSIT_BIAS: u64 = 1 << 62;

/// Jitterbuffer counters since start, summed over all streams.
#[derive(Debug, Default, Clone, Copy)]
pub struct JitterReport {
    pub pushed: u64,
    pub lost: u64,
    /// Packets that arrived after their deadline and were thrown away.
    pub late: u64,
}

pub struct Viewer {
    pub pipeline: gst::Pipeline,
    pub webrtc: gst::Element,
    pub stats: ViewerStats,
    #[cfg(windows)]
    pub window_closed: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(windows)]
    render: Arc<crate::present::RenderStats>,
    /// The video window, unless headless.
    #[cfg(windows)]
    pub window: Option<crate::videoproc::VideoWindow>,
    /// The video process crashed too often and gave up.
    #[cfg(windows)]
    pub video_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Test-only loss on the path from the host.
    pub relay: Option<Arc<crate::netsim::LossRelay>>,
    /// Set when no direct connection could be made.
    pub ice_failed: Arc<std::sync::atomic::AtomicBool>,
    /// The streamer's offer arrived. Failed without it: the streamer never answered the
    /// request to watch, so the network was never tried.
    pub offered: Arc<std::sync::atomic::AtomicBool>,
    /// Sound off for this stream (the stream itself keeps coming).
    audio: Arc<Audio>,
    /// The video codec and the decoder in use ("AV1", "dav1ddec"), once video arrives.
    pub video_codec: Arc<Mutex<Option<(String, String)>>>,
    jitterbuffers: Arc<Mutex<Vec<gst::Element>>>,
    /// Video RTP bytes and packets out of webrtcbin, counted here: webrtcbin's own statistics
    /// lose the stream after a network stall of a few seconds (the session drops its sources).
    video_rtp: Arc<(AtomicU64, AtomicU64)>,
    /// How far behind the video is, and catch-up requests sent.
    pub lag: Arc<crate::catchup::LagWatch>,
    never_answer: bool,
    no_ice: bool,
}

/// Receive-side RTP counters for video, since the session started.
#[derive(Debug, Default, Clone, Copy)]
pub struct Inbound {
    pub received: i64,
    pub lost: i64,
    pub repaired: i64,
    pub nack: i64,
    pub pli: i64,
    /// Video payload bytes received.
    pub bytes: i64,
}

/// Receive counters since the watch started, from the viewer's own counting (see
/// [`Viewer::net`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct NetCounters {
    /// Video RTP bytes received.
    pub bytes: u64,
    /// RTP packets that came out of the jitter buffers, all streams.
    pub received: u64,
    /// RTP packets the jitter buffers gave up on, all streams.
    pub lost: u64,
}

impl NetCounters {
    /// Mbit/s and loss in percent between `before` and now, `secs` apart.
    pub fn rate(&self, before: &NetCounters, secs: f64) -> (f64, f64) {
        let (dr, dl) = (self.received.saturating_sub(before.received) as f64, self.lost.saturating_sub(before.lost) as f64);
        (self.bytes.saturating_sub(before.bytes) as f64 * 8.0 / secs.max(0.001) / 1e6, if dr + dl > 0.0 { dl * 100.0 / (dr + dl) } else { 0.0 })
    }
}

/// Presentation counters since start, and the longest Present and lock hold since last call.
pub struct RenderReport {
    pub presented: u64,
    pub skipped: u64,
    pub max_present_ms: f64,
    pub max_lock_ms: f64,
}

impl Viewer {
    pub fn new(cfg: ViewerConfig, clock: HostClock, link: Arc<dyn Outbox>) -> Result<Self, String> {
        let pipeline = gst::Pipeline::with_name("viewer");
        crate::drain_bus(&pipeline, "viewer");
        let webrtc = gst::ElementFactory::make("webrtcbin").build().map_err(|e| e.to_string())?;
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        crate::ice::configure(&webrtc, &cfg.ice);
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let offered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (f, o) = (failed.clone(), offered.clone());
        crate::ice::watch(&webrtc, "the streamer".into(), cfg.ice.turn.is_some(), move || {
            if !o.load(Ordering::SeqCst) {
                eprintln!("viewer: the streamer never answered the request to watch; the network was not tried");
            }
            f.store(true, Ordering::SeqCst)
        });
        webrtc.set_property("latency", cfg.latency_ms);
        pipeline.add(&webrtc).unwrap();

        webrtc.connect("on-new-transceiver", false, |v| {
            if let Ok(t) = v[1].get::<gst_webrtc::WebRTCRTPTransceiver>() {
                t.set_property("do-nack", true);
            }
            None
        });
        let relay = cfg.impair.active().then(|| crate::netsim::LossRelay::start(cfg.impair));
        let (l, r, no_ice) = (link.clone(), relay.clone(), cfg.debug_no_ice);
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let mut cand = v[2].get::<String>().unwrap();
            if no_ice {
                return None;
            }
            if let Some(r) = &r {
                match r.rewrite_local(&cand) {
                    Some(c) => cand = c,
                    None => return None,
                }
            }
            l.send("ice", &format!("{mline} {cand}"));
            None
        });
        let stats = ViewerStats::default();
        let jitterbuffers = Arc::new(Mutex::new(Vec::new()));
        let lag = Arc::new(crate::catchup::LagWatch::default());
        let ask: Arc<dyn Fn(std::time::Duration) + Send + Sync> = {
            let link = link.clone();
            // Spoiled control for the catch-up gate: measure, never ask.
            let off = std::env::var_os("STREAMER_DEBUG_NO_CATCHUP").is_some();
            let log = std::env::var_os("STREAMER_DEBUG_CATCHUP").is_some();
            let t0 = Instant::now();
            Arc::new(move |d| {
                if log {
                    eprintln!("catchup {:>8.3} s: ask, lag {} ms", t0.elapsed().as_secs_f64(), d.as_millis());
                }
                if !off {
                    link.send(crate::catchup::MSG, &d.as_millis().to_string());
                }
            })
        };
        tune_jitterbuffer(&webrtc, stats.transit.clone(), jitterbuffers.clone(), (lag.clone(), ask));

        // The video in a process of its own (decoder and window): a fault in the GPU driver
        // ends only that process. STREAMER_VIDEO_IN_PROCESS: here, as before.
        #[cfg(windows)]
        let remote = (!cfg.headless && crate::videoproc::use_process()).then(|| {
            crate::videoproc::Remote::start(
                crate::present::default_parent(),
                "",
                crate::videoproc::StatsSink {
                    frames: stats.frames.clone(),
                    frame_interval: stats.frame_interval.clone(),
                    video_latency: stats.video_latency.clone(),
                    video_size: stats.video_size.clone(),
                },
            )
        });
        #[cfg(windows)]
        // Headless (tests only): no window, no D3D device, no decoding.
        let renderer = (!cfg.headless && remote.is_none()).then(|| {
            let r = crate::present::start();
            pipeline.set_context(&r.context);
            r
        });

        let started = Instant::now();
        let pipeline_weak = pipeline.downgrade();
        let (stats2, cfg2) = (stats.clone(), cfg.clone());
        let audio = Arc::new(Audio::default());
        let audio2 = audio.clone();
        let video_codec: Arc<Mutex<Option<(String, String)>>> = Arc::default();
        let video_codec2 = video_codec.clone();
        let video_rtp: Arc<(AtomicU64, AtomicU64)> = Arc::default();
        let video_rtp2 = video_rtp.clone();
        #[cfg(windows)]
        let frames_tx = renderer.as_ref().map(|r| r.frames.clone());
        #[cfg(windows)]
        let window = match &remote {
            Some(r) => Some(crate::videoproc::VideoWindow::Remote(r.clone())),
            None => renderer.as_ref().map(|r| crate::videoproc::VideoWindow::Local(r.window)),
        };
        #[cfg(windows)]
        let remote2 = remote.clone();
        #[cfg(windows)]
        if let Some(w) = &window {
            // The video window's own sound controls (fullscreen, popped out) act here.
            let (a, p) = (audio.clone(), pipeline.downgrade());
            w.set_audio_handler(Arc::new(move |muted, volume| {
                if let Some(m) = muted {
                    a.muted.store(m, Ordering::SeqCst);
                }
                if let Some(v) = volume {
                    a.volume.store((v.clamp(0.0, 1.0) * 100.0).round() as u32, Ordering::SeqCst);
                }
                if let Some(p) = p.upgrade() {
                    a.apply(&p);
                }
                if muted.is_some() || volume.is_some() {
                    eprintln!("viewer: sound {} at {}%", if a.muted.load(Ordering::SeqCst) { "muted" } else { "on" }, a.volume.load(Ordering::SeqCst));
                    sound_changed(a.muted.load(Ordering::SeqCst), a.volume.load(Ordering::SeqCst));
                }
                (a.muted.load(Ordering::SeqCst), a.volume.load(Ordering::SeqCst) as f32 / 100.0)
            }));
            // Its controls start as the saved sound, not at full volume.
            w.sync_audio(audio.muted.load(Ordering::SeqCst), audio.volume.load(Ordering::SeqCst) as f32 / 100.0);
        }
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipeline) = pipeline_weak.upgrade() else { return };
            let Some(caps) = pad.current_caps() else { return };
            let s = caps.structure(0).unwrap();
            let media = s.get::<&str>("media").unwrap_or("");
            let encoding = s.get::<&str>("encoding-name").unwrap_or("").to_ascii_lowercase();
            if media == "video" {
                let n = video_rtp2.clone();
                pad.add_probe(gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST, move |_, info| {
                    let (bytes, packets) = match &info.data {
                        Some(gst::PadProbeData::Buffer(b)) => (b.size() as u64, 1),
                        Some(gst::PadProbeData::BufferList(l)) => (l.iter().map(|b| b.size() as u64).sum(), l.len() as u64),
                        _ => (0, 0),
                    };
                    n.0.fetch_add(bytes, Ordering::Relaxed);
                    n.1.fetch_add(packets, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            }
            let result = match media {
                "video" if cfg2.headless => add_headless_video(&pipeline, pad, &encoding, &stats2),
                #[cfg(windows)]
                "video" if remote2.is_some() => {
                    let c = crate::config::Codec::parse(&encoding);
                    let dec = c.as_ref().map(|c| cfg2.decoder.clone().unwrap_or_else(|| platform::video_decoder(*c))).unwrap_or_default();
                    if let Ok(c) = &c {
                        *video_codec2.lock().unwrap() = Some((c.rtp_name().to_string(), dec.clone()));
                    }
                    add_remote_video(&pipeline, pad, &encoding, &cfg2, &clock, &stats2, started, remote2.as_ref().unwrap(), &dec)
                }
                "video" => {
                    #[cfg(windows)]
                    let tail = video_tail(frames_tx.clone());
                    #[cfg(not(windows))]
                    let tail = video_tail();
                    if let Ok(c) = crate::config::Codec::parse(&encoding) {
                        let dec = cfg2.decoder.clone().unwrap_or_else(|| platform::video_decoder(c));
                        *video_codec2.lock().unwrap() = Some((c.rtp_name().to_string(), dec));
                    }
                    add_video(&pipeline, pad, &encoding, &cfg2, &clock, &stats2, started, tail)
                }
                "audio" if cfg2.audio => add_audio(&pipeline, pad, &clock, &stats2, started, cfg2.audio_sync, cfg2.shared_audio, &audio2),
                _ => add_fakesink(&pipeline, pad),
            };
            if let Err(e) = result {
                eprintln!("viewer: cannot build {media} branch: {e}");
            }
        });

        Ok(Self {
            pipeline,
            webrtc,
            stats,
            #[cfg(windows)]
            window_closed: match &remote {
                Some(r) => r.closed.clone(),
                None => renderer.as_ref().map(|r| r.closed.clone()).unwrap_or_default(),
            },
            #[cfg(windows)]
            video_failed: remote.as_ref().map(|r| r.failed.clone()).unwrap_or_default(),
            #[cfg(windows)]
            render: match &remote {
                Some(r) => r.render.clone(),
                None => renderer.map(|r| r.stats).unwrap_or_default(),
            },
            #[cfg(windows)]
            window,
            relay,
            ice_failed: failed,
            offered,
            audio,
            video_codec,
            jitterbuffers,
            video_rtp,
            lag,
            never_answer: cfg.debug_never_answer,
            no_ice: cfg.debug_no_ice,
        })
    }

    /// Transit since the last call for the busiest stream, which is the video.
    pub fn take_video_transit(&self) -> crate::stats::Summary {
        let all: Vec<_> = self.stats.transit.lock().unwrap().iter().map(|s| s.take()).collect();
        all.into_iter().max_by_key(|s| s.len()).unwrap_or_else(|| Series::default().take())
    }

    pub fn jitter_report(&self) -> JitterReport {
        let mut r = JitterReport::default();
        for jb in self.jitterbuffers.lock().unwrap().iter() {
            let s = jb.property::<gst::Structure>("stats");
            r.pushed += s.get::<u64>("num-pushed").unwrap_or(0);
            r.lost += s.get::<u64>("num-lost").unwrap_or(0);
            r.late += s.get::<u64>("num-late").unwrap_or(0);
        }
        r
    }

    /// Receive counters that survive a network stall: video bytes counted on webrtcbin's
    /// output, packets and losses from the jitter buffers (a new one per source comes along).
    /// STREAMER_DEBUG_NET_FROM_STATS: from webrtcbin's statistics, as before (the spoiled
    /// variant: after a stall of a few seconds they show nothing, the badge 0 Mbit/s).
    pub fn net(&self) -> Option<NetCounters> {
        if std::env::var_os("STREAMER_DEBUG_NET_FROM_STATS").is_some() {
            return self.inbound().map(|i| NetCounters { bytes: i.bytes.max(0) as u64, received: i.received.max(0) as u64, lost: i.lost.max(0) as u64 });
        }
        let j = self.jitter_report();
        let bytes = self.video_rtp.0.load(Ordering::Relaxed);
        (bytes > 0).then_some(NetCounters { bytes, received: j.pushed, lost: j.lost })
    }

    /// Video receive counters from webrtcbin, or None before the stream starts.
    pub fn inbound(&self) -> Option<Inbound> {
        let promise = gst::Promise::new();
        self.webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        if promise.wait() != gst::PromiseResult::Replied {
            return None;
        }
        let reply = promise.get_reply()?;
        if std::env::var_os("STREAMER_DEBUG_STATS").is_some() {
            eprintln!("stats reply {reply}");
        }
        let num = |s: &gst::StructureRef, f: &str| {
            s.get::<u64>(f)
                .map(|v| v as i64)
                .or_else(|_| s.get::<i64>(f))
                .or_else(|_| s.get::<u32>(f).map(i64::from))
                .or_else(|_| s.get::<i32>(f).map(i64::from))
                .unwrap_or(0)
        };
        reply.iter().filter_map(|(_, v)| v.get::<gst::Structure>().ok()).find_map(|s| {
            let video = s.get::<&str>("kind").map(|k| k == "video").unwrap_or(true);
            (video && s.has_field("packets-received") && s.has_field("packets-lost") && s.has_field("nack-count")).then(|| Inbound {
                received: num(&s, "packets-received"),
                lost: num(&s, "packets-lost"),
                repaired: num(&s, "packets-repaired"),
                nack: num(&s, "nack-count"),
                pli: num(&s, "pli-count"),
                bytes: num(&s, "bytes-received"),
            })
        })
    }

    pub fn render_report(&self) -> Option<RenderReport> {
        #[cfg(windows)]
        {
            let r = &self.render;
            Some(RenderReport {
                presented: r.presented.load(Ordering::Relaxed),
                skipped: r.skipped.load(Ordering::Relaxed),
                max_present_ms: r.max_present_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
                max_lock_ms: r.max_lock_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
            })
        }
        #[cfg(not(windows))]
        None
    }

    pub fn start(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|e| format!("cannot start viewer pipeline: {e}"))
    }

    /// Turns this stream's sound off or on, now and for an audio branch made later.
    pub fn set_muted(&self, on: bool) {
        self.audio.muted.store(on, Ordering::SeqCst);
        sound_changed(on, self.audio.volume.load(Ordering::SeqCst));
        match self.pipeline.by_name(AUDIO_VOLUME) {
            Some(v) => v.set_property("mute", on || std::env::var_os("STREAMER_DEBUG_AUDIO_SINK_PROBE").is_some()),
            None => eprintln!("viewer: no audio branch to mute yet"),
        }
        // The video window's own controls show it (in a video process, they keep a copy).
        #[cfg(windows)]
        if let Some(w) = &self.window {
            w.sync_audio(on, self.volume());
        }
    }

    /// The video process crashed too often and gave up.
    pub fn video_failed(&self) -> bool {
        #[cfg(windows)]
        return self.video_failed.load(Ordering::Relaxed);
        #[cfg(not(windows))]
        return false;
    }

    pub fn muted(&self) -> bool {
        self.audio.muted.load(Ordering::SeqCst)
    }

    /// Sound level, 0..1 (1: as received).
    pub fn volume(&self) -> f32 {
        self.audio.volume.load(Ordering::SeqCst) as f32 / 100.0
    }

    pub fn window_closed(&self) -> bool {
        #[cfg(windows)]
        return self.window_closed.load(Ordering::Relaxed);
        #[cfg(not(windows))]
        return false;
    }

    /// Applies one signaling message from the host.
    pub fn on_message(&self, link: &Arc<dyn Outbox>, clock: &HostClock, kind: &str, payload: &str) {
        if (self.never_answer && kind == "offer") || (self.no_ice && kind == "ice") {
            return;
        }
        match kind {
            "offer" => {
                self.offered.store(true, Ordering::SeqCst);
                let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(payload.as_bytes()) else { return };
                let offer = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);
                self.webrtc.emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);
                let (w, l) = (self.webrtc.clone(), link.clone());
                let promise = gst::Promise::with_change_func(move |reply| {
                    let Ok(Some(reply)) = reply else { return };
                    let Ok(answer) = reply.get::<WebRTCSessionDescription>("answer") else { return };
                    w.emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                    l.send("answer", &answer.sdp().as_text().unwrap());
                });
                self.webrtc.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
            }
            "ice" => match &self.relay {
                None => add_ice(&self.webrtc, payload),
                Some(r) => {
                    if let Some((mline, cand)) = payload.split_once(' ') {
                        if let Some(c) = r.rewrite_remote(cand) {
                            add_ice(&self.webrtc, &format!("{mline} {c}"));
                        }
                    }
                }
            },
            "pong" => {
                clock.on_pong(payload);
            }
            _ => {}
        }
    }
}

/// The receiving RTP session does not know the stream's bitrate, so its RTCP budget was tiny
/// and retransmission requests waited past their deadline: only 72-87% of 5% losses were
/// repaired. A session bandwidth at the top of our range (a cap, not traffic) fixed it.
const SESSION_DEFAULTS: &str = "bandwidth=50000000,rtcp-fraction=0.05";

/// By default the jitterbuffer also requests a packet that is merely later than the packet
/// spacing predicts. On a link slower than the stream every packet is late, so a viewer sent
/// ~16000 needless requests in 40 s and the resends filled its link further. Requesting only
/// real gaps gave 0 there and still recovered all of 5% random loss.
const JB_DEFAULTS: &str = "rtx-next-seqnum=false";

/// Defaults above, plus experiment overrides: STREAMER_JB_PROPS for rtpjitterbuffer
/// and STREAMER_SESSION_PROPS for RTPSession, both as "name=value,...".
type LagHook = (Arc<crate::catchup::LagWatch>, Arc<dyn Fn(std::time::Duration) + Send + Sync>);

fn tune_jitterbuffer(webrtc: &gst::Element, transit: Arc<Mutex<Vec<Series>>>, jbs: Arc<Mutex<Vec<gst::Element>>>, lag: LagHook) {
    let jb_props = format!("{JB_DEFAULTS},{}", std::env::var("STREAMER_JB_PROPS").unwrap_or_default());
    let session_props = format!("{SESSION_DEFAULTS},{}", std::env::var("STREAMER_SESSION_PROPS").unwrap_or_default());
    let Some(rtpbin) = webrtc.downcast_ref::<gst::Bin>().and_then(|b| b.by_name("rtpbin")) else {
        eprintln!("viewer: no rtpbin inside webrtcbin, tuning ignored");
        return;
    };
    let apply = |obj: &gst::glib::Object, props: &str| {
        for kv in props.split(',').filter(|s| !s.is_empty()) {
            if let Some((k, val)) = kv.split_once('=') {
                obj.set_property_from_str(k.trim(), val.trim());
            }
        }
    };
    rtpbin.connect("new-jitterbuffer", false, move |v| {
        let rtpbin = v[0].get::<gst::Element>().unwrap();
        let jb = v[1].get::<gst::Element>().unwrap();
        let session = v[2].get::<u32>().unwrap();
        apply(jb.upcast_ref(), &jb_props);
        jbs.lock().unwrap().push(jb.clone());
        let series = Series::default();
        transit.lock().unwrap().push(series.clone());
        watch_transit(&jb, series, lag.clone());
        if !session_props.is_empty() {
            if let Some(s) = rtpbin.emit_by_name::<Option<gst::glib::Object>>("get-internal-session", &[&session]) {
                apply(&s, &session_props);
            }
        }
        None
    });
}

/// Records each packet's transit (arrival on our clock minus its RTP time) at the
/// jitterbuffer input, which is right after the network. A constant clock offset cancels out
/// in the spread.
fn watch_transit(jb: &gst::Element, transit: Series, (lag, ask): LagHook) {
    let Some(pad) = jb.static_pad("sink") else { return };
    let epoch = Instant::now();
    // (last RTP timestamp, unwrapped high bits); None until the first video packet.
    let state = Mutex::new(None::<(u32, i64)>);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(buf) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Ok(rtp) = gst_rtp::RTPBuffer::from_buffer_readable(buf) else { return gst::PadProbeReturn::Ok };
        let ts = rtp.timestamp();
        let video = rtp.payload_type() as u32 == crate::host::VIDEO_PT;
        let mut st = state.lock().unwrap();
        let (last, wraps) = st.get_or_insert((ts, 0));
        // Packets may arrive out of order; only a big jump counts as a wrap.
        if ts < *last && *last - ts > u32::MAX / 2 {
            *wraps += 1;
        } else if ts > *last && ts - *last > u32::MAX / 2 {
            *wraps -= 1;
        }
        *last = ts;
        // 90 kHz is the video clock; audio comes out wrong but is never reported.
        let rtp_ns = ((*wraps << 32) + ts as i64) as i128 * 1_000_000_000 / 90_000;
        let local_ns = epoch.elapsed().as_nanos() as i128;
        transit.push((local_ns - rtp_ns + TRANSIT_BIAS as i128) as u64);
        if video {
            if let Some(d) = lag.packet(Instant::now(), local_ns - rtp_ns) {
                drop(st);
                ask(d);
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Test-only video branch: depayloads and asks for keyframes on loss, like a decoder would,
/// but never touches the GPU.
fn add_headless_video(pipeline: &gst::Pipeline, pad: &gst::Pad, encoding: &str, stats: &ViewerStats) -> Result<(), String> {
    let codec = crate::config::Codec::parse(encoding)?;
    let depay = make(&format!("rtp{}depay", codec.short()))?;
    depay.set_property("request-keyframe", true);
    // Whole frames out of the depayloader count as frames (nothing is decoded here), so the
    // sound watch sees the picture going on.
    let frames = stats.frames.clone();
    depay.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        frames.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    link_branch(pipeline, pad, &[depay, make("fakesink")?])
}

pub(crate) fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|e| format!("{factory}: {e}"))
}

/// Elements after the decoder, ending in a sink. Windows: our own swapchain.
#[cfg(windows)]
pub(crate) fn video_tail(frames: Option<std::sync::mpsc::Sender<(gst_app::AppSink, gst::Element)>>) -> Result<Vec<gst::Element>, String> {
    let caps = gst::Caps::builder("video/x-raw")
        .features(["memory:D3D11Memory"])
        .field("format", "BGRA")
        .build();
    let sink = gst_app::AppSink::builder().caps(&caps).max_buffers(1).drop(true).sync(false).build();
    // The renderer sets the frame size to its window's here; the converter then scales on
    // the GPU and adds borders to keep the aspect ratio.
    let fit = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().map_err(|e| e.to_string())?;
    if let Some(tx) = frames {
        tx.send((sink.clone(), fit.clone())).map_err(|_| "renderer gone".to_string())?;
    }
    // NV12 to BGRA on the GPU.
    Ok(vec![make("d3d11convert")?, fit, sink.upcast()])
}

#[cfg(not(windows))]
fn video_tail() -> Result<Vec<gst::Element>, String> {
    let name = if cfg!(target_os = "macos") { "glimagesink" } else { "autovideosink" };
    let sink = make(name)?;
    sink.set_property("sync", false);
    Ok(vec![sink])
}

#[allow(clippy::too_many_arguments)]
fn add_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    encoding: &str,
    cfg: &ViewerConfig,
    clock: &HostClock,
    stats: &ViewerStats,
    started: Instant,
    tail: Result<Vec<gst::Element>, String>,
) -> Result<(), String> {
    let codec = crate::config::Codec::parse(encoding)?;
    let depay = make(&format!("rtp{}depay", codec.short()))?;
    if cfg.wait_keyframe {
        depay.set_property("wait-for-keyframe", true);
    }
    let parse = make(&format!("{}parse", codec.short()))?;
    let decoder_name = cfg.decoder.clone().unwrap_or_else(|| platform::video_decoder(codec));
    let decoder = make(&decoder_name)?;
    let mut chain = vec![depay.clone(), parse, decoder.clone()];
    if decoder_name == "dav1ddec" {
        // Software AV1: no frames held back (one frame of delay), all in-loop filters (a
        // correct picture whatever the plugin's default), then one copy up to the GPU for the
        // D3D11 presenter.
        decoder.set_property_from_str("max-frame-delay", "1");
        decoder.set_property_from_str("inloop-filters", "deblock+cdef+restoration");
        #[cfg(windows)]
        chain.push(make("d3d11upload")?);
    }
    let debug = std::env::var_os("STREAMER_DEBUG_DECODER").is_some();
    if debug {
        eprintln!("viewer: video decoder {decoder_name}");
    }
    let video_size = stats.video_size.clone();
    decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) {
            if let Some(s) = c.caps().structure(0) {
                let (w, h) = (s.get::<i32>("width").unwrap_or(0), s.get::<i32>("height").unwrap_or(0));
                if debug {
                    eprintln!("viewer: decoded frames {w}x{h}");
                }
                if w > 0 && h > 0 {
                    *video_size.lock().unwrap() = Some((w as u32, h as u32));
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    chain.extend(tail?);
    link_branch(pipeline, pad, &chain)?;

    measure(&depay, &decoder, 90_000, clock, started, Some(stats.video_before_decoder.clone()), stats.video_latency.clone());
    let (interval, frames) = (stats.frame_interval.clone(), stats.frames.clone());
    let last = Mutex::new(None::<Instant>);
    decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        let now = Instant::now();
        frames.fetch_add(1, Ordering::Relaxed);
        if let Some(prev) = last.lock().unwrap().replace(now) {
            if now.duration_since(started).as_secs() >= 3 {
                interval.push(now.duration_since(prev).as_nanos() as u64);
            }
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// The video branch when a video process decodes and shows it: depayloader and parser here,
/// each whole frame (compressed) to the process with the host time it was captured at. The
/// process asks for keyframes through `remote`; they go to the streamer as from a decoder.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn add_remote_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    encoding: &str,
    cfg: &ViewerConfig,
    clock: &HostClock,
    stats: &ViewerStats,
    started: Instant,
    remote: &Arc<crate::videoproc::Remote>,
    decoder: &str,
) -> Result<(), String> {
    let codec = crate::config::Codec::parse(encoding)?;
    let depay = make(&format!("rtp{}depay", codec.short()))?;
    if cfg.wait_keyframe {
        depay.set_property("wait-for-keyframe", true);
    }
    depay.set_property("request-keyframe", true);
    // Frames go to the video process straight from the depayloader; its own parser runs
    // there. A parser here held each frame until the next one began (one frame, 16.7 ms at
    // 60 fps: 34.3 ms from capture to sent against 17.5 ms out of the depayloader,
    // 2026-11-22). STREAMER_DEBUG_VP_PARSE_HERE: the parser here (the spoiled variant).
    let sink = gst_app::AppSink::builder().sync(false).max_buffers(30).drop(false).build();
    let mut chain = vec![depay.clone()];
    if std::env::var_os("STREAMER_DEBUG_VP_PARSE_HERE").is_some() {
        chain.push(make(&format!("{}parse", codec.short()))?);
    }
    chain.push(sink.clone().upcast());
    link_branch(pipeline, pad, &chain)?;
    let by_pts = measure_in(&depay, 90_000, clock, started, Some(stats.video_before_decoder.clone()));
    // A keyframe for the video process: the same ask a decoder makes, up to the streamer.
    let sink_pad = sink.static_pad("sink").unwrap().downgrade();
    remote.set_keyframe_request(Arc::new(move || {
        if let Some(p) = sink_pad.upgrade() {
            let s = gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build();
            p.push_event(gst::event::CustomUpstream::new(s));
        }
    }));
    let (r, clock, decoder) = (Arc::downgrade(remote), clock.clone(), decoder.to_string());
    let is_av1 = codec.short() == "av1";
    // Test switches, read once (not per frame).
    let pause = std::env::var("STREAMER_DEBUG_VP_PAUSE").ok().and_then(|v| { let p: Vec<f64> = v.split_whitespace().filter_map(|x| x.parse().ok()).collect(); (p.len() == 2).then(|| (p[0], p[1])) });
    let drop_keys = std::env::var("STREAMER_DEBUG_VP_DROP_KEYS_SECS").ok().and_then(|v| v.parse::<u64>().ok());
    let (dbg_timing, dbg_delta, av1_by_flag) = (std::env::var_os("STREAMER_DEBUG_VP_TIMING").is_some(), std::env::var_os("STREAMER_DEBUG_VP_DELTA").is_some(), std::env::var_os("STREAMER_DEBUG_VP_AV1_FLAGS").is_some());
    let caps_sent = Mutex::new(String::new());
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let Some(remote) = r.upgrade() else { return Err(gst::FlowError::Flushing) };
                if let Some(c) = sample.caps().map(|c| c.to_string()) {
                    let mut sent = caps_sent.lock().unwrap();
                    if *sent != c {
                        remote.set_stream(&c, &decoder);
                        *sent = c;
                    }
                }
                let Some(b) = sample.buffer() else { return Ok(gst::FlowSuccess::Ok) };
                let pts = b.pts().map(|p| p.nseconds());
                let sent_host = pts.and_then(|p| by_pts.lock().unwrap().iter().rev().find(|(k, _)| *k == p).map(|(_, v)| *v));
                let host_minus_qpc = clock.host_now().map(|h| h as i64 - crate::videolink::qpc_ns() as i64);
                // STREAMER_DEBUG_VP_TIMING: capture to sent to the video process, each 2 s.
                if dbg_timing {
                    static ACC: Mutex<Vec<f64>> = Mutex::new(Vec::new());
                    if let (Some(s), Some(h)) = (sent_host, clock.host_now()) {
                        let mut a = ACC.lock().unwrap();
                        a.push(h.saturating_sub(s) as f64 / 1e6);
                        if a.len() >= 120 {
                            a.sort_by(|x, y| x.partial_cmp(y).unwrap());
                            eprintln!("video timing: capture to sent median {:.2} ms", a[a.len() / 2]);
                            a.clear();
                        }
                    }
                }
                let data = b.map_readable().map(|m| m.to_vec()).unwrap_or_default();
                // Tests only: STREAMER_DEBUG_VP_DROP_KEYS_SECS=N loses every keyframe for the first N s
                // of the watch, as a bad network can.
                // Tests only: STREAMER_DEBUG_VP_PAUSE="FROM TO" (s): no frame at all meanwhile, as a
                // stream that stops for a while.
                if let Some((a, z)) = pause {
                    let t = started.elapsed().as_secs_f64();
                    if t >= a && t < z {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                if let Some(n) = drop_keys {
                    let key = if is_av1 { crate::videolink::av1_has_sequence_header(&data) } else { !b.flags().contains(gst::BufferFlags::DELTA_UNIT) };
                    if key && started.elapsed().as_secs() < n {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                // STREAMER_DEBUG_VP_DELTA: each 120 frames, how many the depayloader marked
                // as not keyframes, and the sizes of those it did not (keyframes are big).
                if dbg_delta {
                    static ACC: Mutex<(u32, Vec<usize>)> = Mutex::new((0, Vec::new()));
                    let mut a = ACC.lock().unwrap();
                    let d = if is_av1 { !crate::videolink::av1_has_sequence_header(&data) } else { b.flags().contains(gst::BufferFlags::DELTA_UNIT) };
                    if d {
                        a.0 += 1;
                    } else {
                        a.1.push(data.len());
                    }
                    if a.0 as usize + a.1.len() >= 120 {
                        eprintln!("video delta: {} marked delta, {} not, their sizes {:?}", a.0, a.1.len(), a.1);
                        *a = (0, Vec::new());
                    }
                }
                remote.push(crate::videolink::Frame {
                    pts,
                    dts: b.dts().map(|p| p.nseconds()),
                    duration: b.duration().map(|p| p.nseconds()),
                    // rtpav1depay marks nothing as delta: AV1 keyframes are told by their
                    // sequence header. STREAMER_DEBUG_VP_AV1_FLAGS: by the flag (the spoiled variant).
                    delta: if is_av1 && !av1_by_flag { !crate::videolink::av1_has_sequence_header(&data) } else { b.flags().contains(gst::BufferFlags::DELTA_UNIT) },
                    discont: b.flags().contains(gst::BufferFlags::DISCONT),
                    sent_host,
                    host_minus_qpc,
                    data,
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}

/// The element that mutes a viewer's sound and sets its level.
const AUDIO_VOLUME: &str = "avolume";

/// A viewer's sound: muted and level in percent, kept for an audio branch made later.
struct Audio {
    muted: std::sync::atomic::AtomicBool,
    volume: std::sync::atomic::AtomicU32,
    /// The stream's input of the shared output (Windows), kept while the branch lives.
    output: Mutex<Option<Arc<crate::playback::Input>>>,
}

impl Default for Audio {
    /// As the user last left a stream's sound (kept between runs by the app).
    fn default() -> Self {
        let (muted, volume) = *SOUND.lock().unwrap();
        Self { muted: muted.into(), volume: volume.into(), output: Mutex::new(None) }
    }
}

/// A new watch's sound: muted, volume in percent. The last change on any stream (2026-11-23:
/// muted once, every stream should stay muted, also after a restart).
static SOUND: Mutex<(bool, u32)> = Mutex::new((false, 100));
static ON_SOUND: std::sync::OnceLock<Box<dyn Fn(bool, u32) + Send + Sync>> = std::sync::OnceLock::new();

/// The sound new watches start with (the app sets what it saved).
pub fn set_sound_default(muted: bool, volume: u32) {
    *SOUND.lock().unwrap() = (muted, volume.min(100));
}

/// Called after every change of a stream's sound, to save it. Set once.
pub fn on_sound_change(f: Box<dyn Fn(bool, u32) + Send + Sync>) {
    let _ = ON_SOUND.set(f);
}

fn sound_changed(muted: bool, volume: u32) {
    *SOUND.lock().unwrap() = (muted, volume);
    if let Some(f) = ON_SOUND.get() {
        f(muted, volume);
    }
}

impl Audio {
    fn apply(&self, pipeline: &gst::Pipeline) {
        if let Some(v) = pipeline.by_name(AUDIO_VOLUME) {
            v.set_property("mute", self.muted.load(Ordering::SeqCst) || std::env::var_os("STREAMER_DEBUG_AUDIO_SINK_PROBE").is_some());
            v.set_property("volume", self.volume.load(Ordering::SeqCst) as f64 / 100.0);
        }
    }
}

fn add_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    clock: &HostClock,
    stats: &ViewerStats,
    started: Instant,
    audio_sync: bool,
    shared_audio: bool,
    audio: &Audio,
) -> Result<(), String> {
    let depay = make("rtpopusdepay")?;
    let decoder = make("opusdec")?;
    decoder.set_property("plc", true);
    // Tests: STREAMER_DEBUG_SILENT_PLAYBACK plays nothing here either (a viewer of its own,
    // not through the shared output).
    let sink = make(if std::env::var_os("STREAMER_DEBUG_SILENT_PLAYBACK").is_some() { "fakesink" } else { platform::audio_sink() })?;
    // Mute by zeroing samples before the device, not by the sink's own mute: wasapi2sink's
    // mute acts on the Windows session, which a check through process loopback could not
    // see change. The stream keeps coming, so unmuting is instant and stays in sync.
    let volume = make("volume")?;
    volume.set_property("name", AUDIO_VOLUME);
    // STREAMER_DEBUG_AUDIO_SINK_PROBE: muted from the first sample, nothing is heard.
    volume.set_property("mute", audio.muted.load(Ordering::SeqCst) || std::env::var_os("STREAMER_DEBUG_AUDIO_SINK_PROBE").is_some());
    volume.set_property("volume", audio.volume.load(Ordering::SeqCst) as f64 / 100.0);
    // With sync the sink follows the stream timing and corrects clock drift between the
    // machines; without it audio piles up in front of the device (stage 1 on macOS: ~300 ms).
    sink.set_property("sync", audio_sync);
    if sink.has_property("low-latency") {
        sink.set_property("low-latency", true);
    }
    // Speakers unplugged: the stream goes on without its sound instead of stopping.
    if sink.has_property("continue-on-error") {
        sink.set_property("continue-on-error", true);
    }
    // On the speakers chosen for voice, and moved with them. STREAMER_DEBUG_STREAM_DEFAULT_SPEAKERS:
    // always the system's default, as before (the spoiled variant).
    // (Through the shared output the sound is already on voice's speakers.)
    let follow = crate::playback::device_by_id(&sink) && !(shared_audio && cfg!(windows)) && std::env::var_os("STREAMER_DEBUG_STREAM_DEFAULT_SPEAKERS").is_none();
    if follow {
        if let Some(d) = crate::playback::present_device(crate::playback::device()) {
            sink.set_property("device", d);
        }
    }
    if sink.has_property("buffer-time") {
        // Microseconds: a small device buffer, as latency matters more than gap-free audio.
        sink.set_property("buffer-time", 40_000i64);
        sink.set_property("latency-time", 10_000i64);
    }
    // Test only: STREAMER_DEBUG_AUDIO_LEVEL prints the received level once a second and plays
    // nothing, to check what the host captured without recording it.
    let debug_level = std::env::var_os("STREAMER_DEBUG_AUDIO_LEVEL").is_some();
    let sink = if debug_level { make("fakesink")? } else { sink };
    if debug_level {
        let acc = std::sync::Mutex::new((0f64, 0u64, Instant::now()));
        decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) else { return gst::PadProbeReturn::Ok };
            // opusdec gives interleaved S16LE.
            let mut a = acc.lock().unwrap();
            for s in map.as_slice().as_chunks::<2>().0 {
                let v = i16::from_le_bytes(*s) as f64 / 32768.0;
                a.0 += v * v;
                a.1 += 1;
            }
            if a.2.elapsed() >= std::time::Duration::from_secs(1) && a.1 > 0 {
                let rms = (a.0 / a.1 as f64).sqrt();
                eprintln!("audio level: {:.1} dB", 20.0 * rms.max(1e-9).log10());
                *a = (0.0, 0, Instant::now());
            }
            gst::PadProbeReturn::Ok
        });
    }
    // Only when asked (`ViewerConfig::shared_audio`, Windows): to the speakers through the
    // process's shared output, so a voice chat's echo canceller hears the stream too, at
    // ~20 ms more delay. Otherwise the stream's own sink, exactly as before voice chat.
    let shared = shared_audio && cfg!(windows) && !debug_level;
    // If the shared output cannot start (a missing plugin, no device), the stream keeps its
    // sound through its own sink, as before.
    let shared_tail = if shared {
        crate::playback::tail("stream").inspect_err(|e| eprintln!("viewer: shared sound output unavailable ({e}); own sink")).ok()
    } else {
        None
    };
    let own_sink = shared_tail.is_none().then(|| sink.clone());
    if let (Some(s), true) = (&own_sink, follow) {
        crate::playback::follow(s);
    }
    let chain = if let Some((tail, input)) = shared_tail {
        *audio.output.lock().unwrap() = Some(input);
        let mut c = vec![depay.clone(), decoder.clone(), make("audioconvert")?, volume];
        c.extend(tail);
        c
    } else {
        vec![depay.clone(), decoder.clone(), make("audioconvert")?, volume, make("audioresample")?, sink]
    };
    link_branch(pipeline, pad, &chain)?;
    if let (Some(s), true) = (&own_sink, std::env::var_os("STREAMER_DEBUG_AUDIO_SINK_PROBE").is_some()) {
        debug_sink_probe(pipeline, &decoder, s);
    }
    measure(&depay, &decoder, 48_000, clock, started, None, stats.audio_latency.clone());
    eprintln!(
        "viewer: sound branch ready ({}, sync {audio_sync}, {} at {}%{})",
        if own_sink.is_some() { "own output" } else { "shared output" },
        if audio.muted.load(Ordering::SeqCst) { "muted" } else { "on" },
        audio.volume.load(Ordering::SeqCst),
        if follow { if crate::playback::device().is_some() { ", on the speakers chosen for voice" } else { ", on the default speakers as voice" } } else { "" }
    );
    // Tests only: STREAMER_DEBUG_SOUND_GONE_AFTER=N drops the decoded sound after N s (the
    // sound watch must notice).
    if let Some(n) = std::env::var("STREAMER_DEBUG_SOUND_GONE_AFTER").ok().and_then(|v| v.parse::<u64>().ok()) {
        let t0 = Instant::now();
        decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| if t0.elapsed().as_secs() >= n { gst::PadProbeReturn::Drop } else { gst::PadProbeReturn::Ok });
    }
    watch_sound(&depay, &decoder, own_sink, stats.frames.clone());
    Ok(())
}

/// Test only (STREAMER_DEBUG_AUDIO_SINK_PROBE): the real sink with the stream muted at the
/// volume element (nothing heard), printing once a second the decoded level, how late each
/// buffer reaches the sink against the pipeline clock (min/max ms; positive = late) and the
/// sink's rendered/dropped counters, to see whether the device takes the sound in time.
fn debug_sink_probe(pipeline: &gst::Pipeline, decoder: &gst::Element, sink: &gst::Element) {
    let level = Arc::new(Mutex::new((0f64, 0u64)));
    let l = level.clone();
    decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
            let mut a = l.lock().unwrap();
            for s in map.as_slice().as_chunks::<2>().0 {
                let v = i16::from_le_bytes(*s) as f64 / 32768.0;
                a.0 += v * v;
                a.1 += 1;
            }
        }
        gst::PadProbeReturn::Ok
    });
    let late = Arc::new(Mutex::new((i64::MAX, i64::MIN, 0u64)));
    let lt = late.clone();
    let el = sink.downgrade();
    sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let (Some(b), Some(e)) = (info.buffer(), el.upgrade()) else { return gst::PadProbeReturn::Ok };
        let seg = pad.sticky_event::<gst::event::Segment>(0);
        let rt = seg.and_then(|s| s.segment().downcast_ref::<gst::ClockTime>().and_then(|s| b.pts().and_then(|p| s.to_running_time(p))));
        if let (Some(rt), Some(now)) = (rt, e.current_running_time()) {
            let d = (now.nseconds() as i64 - rt.nseconds() as i64) / 1_000_000;
            let mut a = lt.lock().unwrap();
            a.0 = a.0.min(d);
            a.1 = a.1.max(d);
            a.2 += 1;
        }
        gst::PadProbeReturn::Ok
    });
    // The sink in use now (it is replaced when the speakers change), by its factory.
    let (pw, factory) = (pipeline.downgrade(), sink.factory().map(|f| f.name().to_string()).unwrap_or_default());
    let _ = std::thread::Builder::new().name("sink-probe".into()).spawn(move || {
        let mut last = (0u64, 0u64);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let Some(p) = pw.upgrade() else { return };
            let Some(s) = p.iterate_recurse().into_iter().flatten().find(|e| e.factory().is_some_and(|f| f.name() == factory.as_str())) else { continue };
            let st = s.property::<gst::Structure>("stats");
            let (r, d) = (st.get::<u64>("rendered").unwrap_or(0), st.get::<u64>("dropped").unwrap_or(0));
            let (sum, n) = std::mem::take(&mut *level.lock().unwrap());
            let db = if n > 0 { 20.0 * (sum / n as f64).sqrt().max(1e-9).log10() } else { f64::NAN };
            let (lo, hi, k) = std::mem::replace(&mut *late.lock().unwrap(), (i64::MAX, i64::MIN, 0));
            eprintln!("audio sink: level {db:.1} dB, {k} buffers late {lo}..{hi} ms, rendered {}/s dropped {}/s", r.saturating_sub(last.0), d.saturating_sub(last.1));
            last = (r, d);
        }
    });
}

/// Logs a stream's sound going missing while its picture goes on (reported 2026-11-22: a
/// stream sometimes had no sound until watched again), with where it stops: packets in,
/// decoded, played or dropped late by the output (own output only). Once when it goes, once
/// when it comes back.
fn watch_sound(depay: &gst::Element, decoder: &gst::Element, sink: Option<gst::Element>, frames: Arc<AtomicU64>) {
    let counter = |e: &gst::Element, pad: &str| {
        let n = Arc::new(AtomicU64::new(0));
        let c = n.clone();
        e.static_pad(pad).unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        n
    };
    let (packets, decoded) = (counter(depay, "sink"), counter(decoder, "src"));
    // The decoded level over each minute, for the log: a stream that sends silence shows here.
    let level = Arc::new(Mutex::new((0f64, 0u64)));
    let lv = level.clone();
    decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
            let (mut sum, mut n) = (0f64, 0u64);
            // opusdec gives interleaved S16LE.
            for s in map.as_slice().as_chunks::<2>().0 {
                let v = i16::from_le_bytes(*s) as f64 / 32768.0;
                sum += v * v;
                n += 1;
            }
            let mut a = lv.lock().unwrap();
            a.0 += sum;
            a.1 += n;
        }
        gst::PadProbeReturn::Ok
    });
    let (dw, sw) = (depay.downgrade(), sink.map(|s| s.downgrade()));
    let stats = move || -> Option<(u64, u64)> {
        let s = sw.as_ref()?.upgrade()?;
        let st = s.property::<gst::Structure>("stats");
        Some((st.get::<u64>("rendered").unwrap_or(0), st.get::<u64>("dropped").unwrap_or(0)))
    };
    let _ = std::thread::Builder::new().name("sound-watch".into()).spawn(move || {
        let mut last = (packets.load(Ordering::Relaxed), decoded.load(Ordering::Relaxed), stats(), frames.load(Ordering::Relaxed));
        let (mut quiet, mut told, mut first, mut ticks) = (0u32, false, true, 0u64);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if dw.upgrade().is_none() {
                return;
            }
            let now = (packets.load(Ordering::Relaxed), decoded.load(Ordering::Relaxed), stats(), frames.load(Ordering::Relaxed));
            let (p, d) = (now.0 - last.0, now.1 - last.1);
            let (r, dr) = match (now.2, last.2) {
                (Some(a), Some(b)) => (Some(a.0.saturating_sub(b.0)), a.1.saturating_sub(b.1)),
                _ => (None, 0),
            };
            let video = now.3 > last.3;
            if first && d > 0 {
                eprintln!("viewer: sound coming");
                first = false;
            }
            // Played: by the own output's count, or (shared output) by what was decoded.
            let playing = r.map_or(d > 0, |r| r > 0);
            quiet = if playing || !video { 0 } else { quiet + 1 };
            if quiet == 3 && !told {
                told = true;
                eprintln!(
                    "viewer: no sound for 3 s while the picture goes on: audio packets in {p}/s, decoded {d}/s, played {}, dropped by the output {dr}/s",
                    r.map(|r| format!("{r}/s")).unwrap_or("? (shared output)".into())
                );
            }
            if playing && told {
                told = false;
                eprintln!("viewer: sound back");
            }
            last = now;
            ticks += 1;
            if ticks % 60 == 0 {
                let (sum, n) = std::mem::take(&mut *level.lock().unwrap());
                let db = if n > 0 && sum > 0.0 { format!("{:.0} dB", 10.0 * (sum / n as f64).log10()) } else if n > 0 { "silent".into() } else { "nothing decoded".into() };
                eprintln!("viewer: sound over the last minute: {db}, audio packets in {p}/s, decoded {d}/s");
            }
        }
    });
}

fn add_fakesink(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<(), String> {
    let sink = make("fakesink")?;
    link_branch(pipeline, pad, &[sink])
}

fn link_branch(pipeline: &gst::Pipeline, pad: &gst::Pad, chain: &[gst::Element]) -> Result<(), String> {
    pipeline.add_many(chain).map_err(|e| e.to_string())?;
    gst::Element::link_many(chain).map_err(|e| e.to_string())?;
    pad.link(&chain[0].static_pad("sink").unwrap()).map_err(|e| format!("{e:?}"))?;
    for e in chain.iter().rev() {
        e.sync_state_with_parent().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Latency from the host's encoder input (carried as RTP time) to the decoder output,
/// matched by PTS because decoders drop frames before the first keyframe.
fn measure(
    depay: &gst::Element,
    decoder: &gst::Element,
    clock_rate: u64,
    clock: &HostClock,
    started: Instant,
    before_decoder: Option<Series>,
    after_decoder: Series,
) {
    let by_pts = measure_in(depay, clock_rate, clock, started, before_decoder);
    let clock = clock.clone();
    decoder.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if started.elapsed().as_secs() < 3 {
            return gst::PadProbeReturn::Ok;
        }
        let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds()) else {
            return gst::PadProbeReturn::Ok;
        };
        let sent = by_pts.lock().unwrap().iter().rev().find(|(k, _)| *k == pts).map(|(_, v)| *v);
        if let (Some(sent), Some(now)) = (sent, clock.host_now()) {
            after_decoder.push(now.saturating_sub(sent));
        }
        gst::PadProbeReturn::Ok
    });
}

/// The depayloader's side of the latency measurement: network and jitterbuffer delay, and
/// each frame's pts with the host time it was captured at.
fn measure_in(depay: &gst::Element, clock_rate: u64, clock: &HostClock, started: Instant, before_decoder: Option<Series>) -> Arc<Mutex<VecDeque<(u64, u64)>>> {
    let last_rtp = Arc::new(AtomicU64::new(0));
    let rtp_in = last_rtp.clone();
    depay.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(b) = info.buffer() {
            if let Ok(rtp) = gst_rtp::RTPBuffer::from_buffer_readable(b) {
                rtp_in.store(rtp.timestamp() as u64 * 1_000_000_000 / clock_rate, Ordering::Relaxed);
            }
        }
        gst::PadProbeReturn::Ok
    });

    let by_pts: Arc<Mutex<VecDeque<(u64, u64)>>> = Arc::default();
    let (map_in, clock_in) = (by_pts.clone(), clock.clone());
    depay.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
            let sent = last_rtp.load(Ordering::Relaxed);
            if let (Some(series), Some(now)) = (&before_decoder, clock_in.host_now()) {
                if started.elapsed().as_secs() >= 3 {
                    series.push(now.saturating_sub(sent));
                }
            }
            let mut q = map_in.lock().unwrap();
            q.push_back((pts.nseconds(), sent));
            if q.len() > 600 {
                q.pop_front();
            }
        }
        gst::PadProbeReturn::Ok
    });
    by_pts
}
