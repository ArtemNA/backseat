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
}

pub struct Viewer {
    pub pipeline: gst::Pipeline,
    pub webrtc: gst::Element,
    pub stats: ViewerStats,
    #[cfg(windows)]
    pub window_closed: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(windows)]
    render: Arc<crate::present::RenderStats>,
    /// Test-only loss on the path from the host.
    pub relay: Option<Arc<crate::netsim::LossRelay>>,
    /// Set when no direct connection could be made.
    pub ice_failed: Arc<std::sync::atomic::AtomicBool>,
}

/// Receive-side RTP counters for video, since the session started.
#[derive(Debug, Default, Clone, Copy)]
pub struct Inbound {
    pub received: i64,
    pub lost: i64,
    pub repaired: i64,
    pub nack: i64,
    pub pli: i64,
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
        let f = failed.clone();
        crate::ice::watch(&webrtc, "the streamer".into(), cfg.ice.turn.is_some(), move || {
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
        let (l, r) = (link.clone(), relay.clone());
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let mut cand = v[2].get::<String>().unwrap();
            if let Some(r) = &r {
                match r.rewrite_local(&cand) {
                    Some(c) => cand = c,
                    None => return None,
                }
            }
            l.send("ice", &format!("{mline} {cand}"));
            None
        });
        tune_jitterbuffer(&webrtc);

        #[cfg(windows)]
        // Headless (tests only): no window, no D3D device, no decoding.
        let renderer = (!cfg.headless).then(|| {
            let r = crate::present::start();
            pipeline.set_context(&r.context);
            r
        });

        let stats = ViewerStats::default();
        let started = Instant::now();
        let pipeline_weak = pipeline.downgrade();
        let (stats2, cfg2) = (stats.clone(), cfg.clone());
        #[cfg(windows)]
        let frames_tx = renderer.as_ref().map(|r| r.frames.clone());
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipeline) = pipeline_weak.upgrade() else { return };
            let Some(caps) = pad.current_caps() else { return };
            let s = caps.structure(0).unwrap();
            let media = s.get::<&str>("media").unwrap_or("");
            let encoding = s.get::<&str>("encoding-name").unwrap_or("").to_ascii_lowercase();
            let result = match media {
                "video" if cfg2.headless => add_headless_video(&pipeline, pad, &encoding),
                "video" => {
                    #[cfg(windows)]
                    let tail = video_tail(frames_tx.clone());
                    #[cfg(not(windows))]
                    let tail = video_tail();
                    add_video(&pipeline, pad, &encoding, &cfg2, &clock, &stats2, started, tail)
                }
                "audio" if cfg2.audio => add_audio(&pipeline, pad, &clock, &stats2, started, cfg2.audio_sync),
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
            window_closed: renderer.as_ref().map(|r| r.closed.clone()).unwrap_or_default(),
            #[cfg(windows)]
            render: renderer.map(|r| r.stats).unwrap_or_default(),
            relay,
            ice_failed: failed,
        })
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

    pub fn window_closed(&self) -> bool {
        #[cfg(windows)]
        return self.window_closed.load(Ordering::Relaxed);
        #[cfg(not(windows))]
        return false;
    }

    /// Applies one signaling message from the host.
    pub fn on_message(&self, link: &Arc<dyn Outbox>, clock: &HostClock, kind: &str, payload: &str) {
        match kind {
            "offer" => {
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
fn tune_jitterbuffer(webrtc: &gst::Element) {
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
        if !session_props.is_empty() {
            if let Some(s) = rtpbin.emit_by_name::<Option<gst::glib::Object>>("get-internal-session", &[&session]) {
                apply(&s, &session_props);
            }
        }
        None
    });
}

/// Test-only video branch: depayloads and asks for keyframes on loss, like a decoder would,
/// but never touches the GPU.
fn add_headless_video(pipeline: &gst::Pipeline, pad: &gst::Pad, encoding: &str) -> Result<(), String> {
    let codec = crate::config::Codec::parse(encoding)?;
    let depay = make(&format!("rtp{}depay", codec.short()))?;
    depay.set_property("request-keyframe", true);
    link_branch(pipeline, pad, &[depay, make("fakesink")?])
}

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|e| format!("{factory}: {e}"))
}

/// Elements after the decoder, ending in a sink. Windows: our own swapchain.
#[cfg(windows)]
fn video_tail(frames: Option<std::sync::mpsc::Sender<gst_app::AppSink>>) -> Result<Vec<gst::Element>, String> {
    let caps = gst::Caps::builder("video/x-raw")
        .features(["memory:D3D11Memory"])
        .field("format", "BGRA")
        .build();
    let sink = gst_app::AppSink::builder().caps(&caps).max_buffers(1).drop(true).sync(false).build();
    if let Some(tx) = frames {
        tx.send(sink.clone()).map_err(|_| "renderer gone".to_string())?;
    }
    // NV12 to BGRA on the GPU, at the stream's own size.
    Ok(vec![make("d3d11convert")?, sink.upcast()])
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
    let decoder = make(cfg.decoder.as_deref().unwrap_or(&platform::video_decoder(codec)))?;
    let mut chain = vec![depay.clone(), parse, decoder.clone()];
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

fn add_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    clock: &HostClock,
    stats: &ViewerStats,
    started: Instant,
    audio_sync: bool,
) -> Result<(), String> {
    let depay = make("rtpopusdepay")?;
    let decoder = make("opusdec")?;
    decoder.set_property("plc", true);
    let sink = make(platform::audio_sink())?;
    // With sync the sink follows the stream timing and corrects clock drift between the
    // machines; without it audio piles up in front of the device (stage 1 on macOS: ~300 ms).
    sink.set_property("sync", audio_sync);
    if sink.has_property("low-latency") {
        sink.set_property("low-latency", true);
    }
    if sink.has_property("buffer-time") {
        // Microseconds: a small device buffer, as latency matters more than gap-free audio.
        sink.set_property("buffer-time", 40_000i64);
        sink.set_property("latency-time", 10_000i64);
    }
    let chain = vec![depay.clone(), decoder.clone(), make("audioconvert")?, make("audioresample")?, sink];
    link_branch(pipeline, pad, &chain)?;
    measure(&depay, &decoder, 48_000, clock, started, None, stats.audio_latency.clone());
    Ok(())
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
