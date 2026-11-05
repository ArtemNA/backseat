//! Host: capture, one encoder per stream, fan-out through `tee` to one webrtcbin per viewer.

use crate::clock;
use crate::config::{HostConfig, Source};
use crate::platform;
use crate::signal::Outbox;
use gst::prelude::*;
use gst_webrtc::{WebRTCRTPTransceiver, WebRTCSDPType, WebRTCSessionDescription};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

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
    /// Time between encoded frames reaching the fan-out point, in ns.
    pub frame_interval: crate::stats::Series,
}

pub struct Host {
    pub pipeline: gst::Pipeline,
    pub stats: Arc<HostStats>,
    pub encoder: String,
    cfg: HostConfig,
    /// Viewer branches by viewer id. The lock also serializes start, stop and teardown.
    viewers: Mutex<HashMap<u64, Branch>>,
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
}

/// Everything one viewer adds to the pipeline.
struct Branch {
    webrtc: gst::Element,
    /// Queues and payloaders, one pair per tee.
    elements: Vec<gst::Element>,
    /// Tee request pads feeding this branch.
    tee_pads: Vec<(gst::Element, gst::Pad)>,
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
        let last = Mutex::new(None::<std::time::Instant>);
        tee_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                let now = std::time::Instant::now();
                if let Some(prev) = last.lock().unwrap().replace(now) {
                    let gap = now.duration_since(prev);
                    s.frame_interval.push(gap.as_nanos() as u64);
                    if debug_timeline() && gap.as_millis() > 25 {
                        eprintln!("timeline {:>9.1} ms: encoded frame gap {:.1} ms", since_start_ms(), gap.as_secs_f64() * 1e3);
                    }
                }
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
        // STREAMER_DEBUG_FORWARD_KEY_REQUESTS is the spoiled variant for the isolation gate.
        let s = stats.clone();
        let forward = std::env::var_os("STREAMER_DEBUG_FORWARD_KEY_REQUESTS").is_some();
        tee_sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            let is_request = info
                .event()
                .and_then(|e| e.structure())
                .is_some_and(|st| st.name() == "GstForceKeyUnit");
            if is_request {
                s.key_requests_dropped.fetch_add(1, Ordering::Relaxed);
                if !forward {
                    return gst::PadProbeReturn::Drop;
                }
            }
            gst::PadProbeReturn::Ok
        });

        crate::drain_bus(&pipeline, "host");
        Ok(Self { pipeline, stats, encoder, cfg, viewers: Mutex::default() })
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
        let webrtcs: Vec<(u64, gst::Element)> =
            self.viewers.lock().unwrap().iter().map(|(id, b)| (*id, b.webrtc.clone())).collect();
        webrtcs
            .into_iter()
            .map(|(id, w)| {
                let mut q = ViewerQuality { id, ..Default::default() };
                let promise = gst::Promise::new();
                w.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
                if promise.wait() == gst::PromiseResult::Replied {
                    if let Some(reply) = promise.get_reply() {
                        for s in reply.iter().filter_map(|(_, v)| v.get::<gst::Structure>().ok()) {
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
    pub fn add_viewer(&self, id: u64, link: Arc<dyn Outbox>) -> Result<(), String> {
        let mut viewers = self.viewers.lock().unwrap();
        if viewers.contains_key(&id) {
            return Ok(());
        }
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: add viewer {id}", since_start_ms());
        }
        let branch = self.build_branch(id, link);
        if viewers.is_empty() {
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| format!("cannot start capture and encoding: {e}"))?;
        }
        for e in std::iter::once(&branch.webrtc).chain(&branch.elements) {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        viewers.insert(id, branch);
        Ok(())
    }

    /// Removes a viewer branch while the others keep playing. The last viewer stops capture
    /// and encoding. Must not run on a streaming thread.
    pub fn remove_viewer(&self, id: u64) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(branch) = viewers.remove(&id) else { return };
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: remove viewer {id}", since_start_ms());
        }

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
        for (tee, pad) in &branch.tee_pads {
            if let Some(peer) = pad.peer() {
                let _ = pad.unlink(&peer);
            }
            tee.release_request_pad(pad);
        }
        if viewers.is_empty() {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
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

    fn build_branch(&self, id: u64, link: Arc<dyn Outbox>) -> Branch {
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(format!("viewer-{id}"))
            .build()
            .unwrap();
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        crate::ice::configure(&webrtc, &self.cfg.ice);
        crate::ice::watch(&webrtc, format!("viewer {id}"), self.cfg.ice.turn.is_some(), || {});

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
        let mut branch = Branch { webrtc, elements: vec![], tee_pads: vec![] };
        let video_pay = make_pay(&format!("rtp{}pay", self.cfg.codec.short()), VIDEO_PT);
        video_pay.set_property("config-interval", -1i32);
        video_pay.set_property_from_str("aggregate-mode", "zero-latency");
        self.feed(&mut branch, "vtee", video_pay);
        if self.cfg.audio {
            self.feed(&mut branch, "atee", make_pay("rtpopuspay", AUDIO_PT));
        }
        branch
    }

    /// Links tee -> leaky queue -> payloader -> the branch's webrtcbin.
    fn feed(&self, branch: &mut Branch, tee: &str, pay: gst::Element) {
        // Leaky, so a viewer that stalls (or never negotiates) cannot block the tee.
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 4u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .unwrap();
        self.pipeline.add_many([&queue, &pay]).unwrap();
        gst::Element::link_many([&queue, &pay, &branch.webrtc]).unwrap();
        let tee = self.pipeline.by_name(tee).unwrap();
        let pad = tee.request_pad_simple("src_%u").unwrap();
        pad.link(&queue.static_pad("sink").unwrap()).unwrap();
        branch.elements.extend([pay, queue]);
        branch.tee_pads.push((tee, pad));
    }

    /// Applies one signaling message from viewer `id`.
    pub fn on_message(&self, id: u64, link: &dyn Outbox, kind: &str, payload: &str) {
        let Some(webrtc) = self.viewers.lock().unwrap().get(&id).map(|b| b.webrtc.clone()) else {
            return;
        };
        let webrtc = &webrtc;
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

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// STREAMER_DEBUG_TIMELINE prints frame gaps and viewer changes on one time axis.
fn debug_timeline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STREAMER_DEBUG_TIMELINE").is_some())
}

fn since_start_ms() -> f64 {
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3
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
