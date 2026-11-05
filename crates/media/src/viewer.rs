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
        webrtc.set_property("latency", cfg.latency_ms);
        pipeline.add(&webrtc).unwrap();

        webrtc.connect("on-new-transceiver", false, |v| {
            if let Ok(t) = v[1].get::<gst_webrtc::WebRTCRTPTransceiver>() {
                t.set_property("do-nack", true);
            }
            None
        });
        let l = link.clone();
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let cand = v[2].get::<String>().unwrap();
            l.send("ice", &format!("{mline} {cand}"));
            None
        });

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
                "video" if cfg2.headless => add_fakesink(&pipeline, pad),
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
            "ice" => add_ice(&self.webrtc, payload),
            "pong" => {
                clock.on_pong(payload);
            }
            _ => {}
        }
    }
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
