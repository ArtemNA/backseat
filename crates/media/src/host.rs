//! Host: capture, one encoder per stream, fan-out through `tee` to one webrtcbin per viewer.

use crate::clock;
use crate::config::{HostConfig, Source};
use crate::platform;
use crate::signal::Outbox;
use gst::prelude::*;
use gst_webrtc::{WebRTCRTPTransceiver, WebRTCSDPType, WebRTCSessionDescription};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const VIDEO_PT: u32 = 96;
const AUDIO_PT: u32 = 111;

#[derive(Default)]
pub struct HostStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub key_frames: AtomicU64,
    pub key_bytes: AtomicU64,
    pub max_frame_bytes: AtomicU64,
    pub key_requests_dropped: AtomicU64,
}

pub struct Host {
    pub pipeline: gst::Pipeline,
    pub stats: Arc<HostStats>,
    pub encoder: String,
    cfg: HostConfig,
}

fn video_source(cfg: &HostConfig) -> String {
    let m = cfg.mode;
    match cfg.source {
        Source::Capture { monitor } => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true monitor-index={monitor} ! \
             video/x-raw(memory:D3D11Memory),framerate={}/1",
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

fn audio_source(cfg: &HostConfig) -> Result<gst::Element, String> {
    match cfg.source {
        Source::Bars | Source::Noise => gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property_from_str("wave", "ticks")
            .property("volume", 0.3f64)
            .build()
            .map_err(|e| e.to_string()),
        Source::Capture { .. } => {
            let src = gst::ElementFactory::make("wasapi2src")
                .property("loopback", true)
                .property("low-latency", true)
                .build()
                .map_err(|e| format!("system audio capture unavailable: {e}"))?;
            // Excludes our own playback, so a host who also watches does not echo it back.
            if src.has_property("loopback-mode") {
                src.set_property_from_str("loopback-mode", "exclude-process-tree");
                src.set_property("loopback-target-pid", std::process::id());
            } else {
                eprintln!("warning: this Windows build cannot exclude our own audio from capture");
            }
            Ok(src)
        }
    }
}

impl Host {
    pub fn new(cfg: HostConfig) -> Result<Self, String> {
        let (encoder, kind) = platform::encoder_candidates(cfg.codec)
            .into_iter()
            .find(|(name, _)| gst::ElementFactory::find(name).is_some())
            .ok_or("no hardware encoder found for this codec")?;
        let m = cfg.mode;
        let gop = m.fps * cfg.keyframe_secs;
        let c = cfg.codec.short();
        let desc = format!(
            "{src} ! d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12,width={w},height={h} ! \
             queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! \
             {encoder} name=venc {props} ! {c}parse config-interval=-1 ! video/x-{c},alignment=au ! \
             tee name=vtee allow-not-linked=true",
            src = video_source(&cfg),
            w = m.width,
            h = m.height,
            props = platform::encoder_properties(kind, cfg.kbit, m.fps, gop),
        );
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
            let chain = [audio_source(&cfg)?, mk("audioconvert")?, mk("audioresample")?, filter, queue, opus, tee];
            pipeline.add_many(&chain).unwrap();
            gst::Element::link_many(&chain).map_err(|e| format!("audio link: {e}"))?;
        }

        let stats = Arc::new(HostStats::default());
        let tee_sink = pipeline.by_name("vtee").unwrap().static_pad("sink").unwrap();
        let s = stats.clone();
        tee_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                s.frames.fetch_add(1, Ordering::Relaxed);
                s.bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
                s.max_frame_bytes.fetch_max(b.size() as u64, Ordering::Relaxed);
                if !b.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    s.key_frames.fetch_add(1, Ordering::Relaxed);
                    s.key_bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
        // Decision D1: a viewer's keyframe request never reaches the shared encoder.
        let s = stats.clone();
        tee_sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            let is_request = info
                .event()
                .and_then(|e| e.structure())
                .is_some_and(|st| st.name() == "GstForceKeyUnit");
            if is_request {
                s.key_requests_dropped.fetch_add(1, Ordering::Relaxed);
                return gst::PadProbeReturn::Drop;
            }
            gst::PadProbeReturn::Ok
        });

        Ok(Self { pipeline, stats, encoder, cfg })
    }

    pub fn start(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|e| format!("cannot start host pipeline: {e}"))
    }

    pub fn running_time(&self) -> Option<u64> {
        clock::running_time(&self.pipeline)
    }

    /// Adds one viewer branch and starts negotiation over the link. Returns its webrtcbin.
    pub fn add_viewer(&self, index: usize, link: Arc<dyn Outbox>) -> gst::Element {
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(format!("viewer-{index}"))
            .build()
            .unwrap();
        webrtc.set_property_from_str("bundle-policy", "max-bundle");

        let l = link.clone();
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap();
            let cand = v[2].get::<String>().unwrap();
            l.send("ice", &format!("{mline} {cand}"));
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
        let mut added = vec![];
        let video_pay = make_pay(&format!("rtp{}pay", self.cfg.codec.short()), VIDEO_PT);
        video_pay.set_property("config-interval", -1i32);
        video_pay.set_property_from_str("aggregate-mode", "zero-latency");
        added.extend(self.branch("vtee", video_pay, &webrtc));
        if self.cfg.audio {
            added.extend(self.branch("atee", make_pay("rtpopuspay", AUDIO_PT), &webrtc));
        }
        webrtc.sync_state_with_parent().unwrap();
        for e in added {
            e.sync_state_with_parent().unwrap();
        }
        webrtc
    }

    fn branch(&self, tee: &str, pay: gst::Element, webrtc: &gst::Element) -> Vec<gst::Element> {
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 4u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .unwrap();
        self.pipeline.add_many([&queue, &pay]).unwrap();
        gst::Element::link_many([&queue, &pay, webrtc]).unwrap();
        let tee = self.pipeline.by_name(tee).unwrap();
        tee.request_pad_simple("src_%u").unwrap().link(&queue.static_pad("sink").unwrap()).unwrap();
        vec![pay, queue]
    }

    /// Applies one signaling message for the viewer behind `webrtc`.
    pub fn on_message(&self, webrtc: &gst::Element, link: &dyn Outbox, kind: &str, payload: &str) {
        match kind {
            "answer" => {
                if let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(payload.as_bytes()) {
                    let answer = WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp);
                    webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
                }
            }
            "ice" => add_ice(webrtc, payload),
            "ping" => {
                let now = self.running_time().unwrap_or(0);
                link.send("pong", &format!("{payload} {now}"));
            }
            _ => {}
        }
    }
}

fn make_pay(factory: &str, pt: u32) -> gst::Element {
    let pay = gst::ElementFactory::make(factory).build().unwrap();
    pay.set_property("pt", pt);
    pay.set_property("mtu", 1200u32);
    // RTP time equals the host running time, which lets the viewer measure latency.
    pay.set_property("timestamp-offset", 0u32);
    pay
}

pub fn add_ice(webrtc: &gst::Element, payload: &str) {
    if let Some((mline, cand)) = payload.split_once(' ') {
        if let Ok(mline) = mline.parse::<u32>() {
            webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &cand.to_string()]);
        }
    }
}
