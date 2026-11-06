//! Host: one capture, one encoder per quality that some viewer watches (a layer), fan-out
//! through each layer's `tee` to one webrtcbin per viewer. The host's settings are the
//! ceiling; viewers may ask for less and move between layers at keyframes.

use crate::clock;
use crate::config::{HostConfig, Quality, Source};
use crate::platform;
use crate::signal::Outbox;
use gst::prelude::*;
use gst_webrtc::{WebRTCRTPTransceiver, WebRTCSDPType, WebRTCSessionDescription};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const VIDEO_PT: u32 = 96;
/// Signaling message kind: a viewer asks for a quality, as "720p30 4000" (kbit/s optional).
pub const QUALITY_MSG: &str = "quality";
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
    /// Statistics of the layer at the host's own quality.
    pub stats: Arc<HostStats>,
    pub encoder: String,
    kind: platform::EncoderKind,
    cfg: HostConfig,
    /// The host's own quality: the most any viewer gets.
    pub ceiling: Quality,
    /// Viewer branches by viewer id. The lock also serializes start, stop and teardown.
    /// Lock order: `viewers`, then `layers`. Streaming threads take neither.
    viewers: Mutex<HashMap<u64, Branch>>,
    layers: Mutex<HashMap<Quality, Layer>>,
}

/// One encoder at one quality, fed from the shared capture.
struct Layer {
    bin: gst::Bin,
    tee: gst::Element,
    /// Request pad on the capture tee feeding this layer.
    feed: gst::Pad,
    stats: Arc<HostStats>,
    /// No viewer watches it: its input is dropped and the encoder idles. Stopping an
    /// encoder while the pipeline plays hung the whole host within 60-135 s of churn
    /// (3 runs of 4), so a layer is only torn down with the pipeline.
    parked: Arc<AtomicBool>,
}

/// Which layer feeds a viewer's video, and the one it is moving to.
struct VideoLink {
    quality: Quality,
    tee: gst::Element,
    pad: gst::Pad,
    /// Target layer, its tee pad and the probe waiting there for a keyframe.
    pending: Option<(Quality, gst::Element, gst::Pad, gst::PadProbeId)>,
    /// Tee pads already unlinked from this viewer, to release off the streaming thread.
    retired: Vec<(gst::Element, gst::Pad)>,
}

/// One row of the per-layer report.
pub struct LayerReport {
    pub quality: Quality,
    /// Kept with an idle encoder because no viewer watches it.
    pub parked: bool,
    pub viewers: usize,
    pub stats: Arc<HostStats>,
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
    /// Catch-ups this viewer asked for, and frames skipped for it, since it joined.
    pub catchups: u64,
    pub skipped_frames: u64,
}

/// Everything one viewer adds to the pipeline.
struct Branch {
    webrtc: gst::Element,
    /// Queues and payloaders, one pair per tee.
    elements: Vec<gst::Element>,
    /// Tee request pads feeding this branch, audio only; video is in `video`.
    tee_pads: Vec<(gst::Element, gst::Pad)>,
    /// Skips this viewer's video while its network queue drains.
    gate: Arc<crate::catchup::Gate>,
    video: Arc<Mutex<VideoLink>>,
    /// Input of the branch's video queue, where a layer's tee pad links.
    video_in: gst::Pad,
    /// Tee pads and the branch inputs they feed, linked only once the branch plays: a tee
    /// pushing into a branch still in NULL gets FLUSHING and passes it up, which stopped
    /// the layer, the capture tee and the capture (stress test, 2026-11-06).
    unlinked: Vec<(gst::Pad, gst::Pad)>,
}

fn video_source(cfg: &HostConfig) -> String {
    let m = cfg.mode;
    match cfg.source {
        Source::Capture { monitor } => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true monitor-index={monitor} ! \
             video/x-raw(memory:D3D11Memory),framerate={}/1",
            m.fps
        ),
        // The frame rate must be set: WGC and DXGI otherwise negotiate 30 (stage 6 probe A).
        Source::Monitor { handle } => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true monitor-handle={handle} ! \
             video/x-raw(memory:D3D11Memory),framerate={}/1",
            m.fps
        ),
        Source::Window { hwnd } => format!(
            "d3d11screencapturesrc capture-api=wgc show-cursor=true window-handle={hwnd} ! \
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
        Source::Capture { .. } | Source::Monitor { .. } | Source::Window { .. } => {
            let src = gst::ElementFactory::make("wasapi2src")
                .property("loopback", true)
                .property("low-latency", true)
                .build()
                .map_err(|e| format!("system audio capture unavailable: {e}"))?;
            if !src.has_property("loopback-mode") {
                if cfg.audio_pid.is_some() {
                    return Err("this Windows build cannot capture one program's audio".into());
                }
                eprintln!("warning: this Windows build cannot exclude our own audio from capture");
                return Ok(src);
            }
            match cfg.audio_pid {
                // One program only.
                Some(pid) => {
                    src.set_property_from_str("loopback-mode", "include-process-tree");
                    src.set_property("loopback-target-pid", pid);
                }
                // Everything but our own playback, so a host who also watches does not echo it.
                None => {
                    src.set_property_from_str("loopback-mode", "exclude-process-tree");
                    src.set_property("loopback-target-pid", std::process::id());
                }
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
        let desc = format!("{} ! tee name=srctee allow-not-linked=true", video_source(&cfg));
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

        crate::drain_bus(&pipeline, "host");
        let ceiling = Quality::new(cfg.mode, cfg.kbit);
        Ok(Self {
            pipeline,
            stats: Arc::new(HostStats::default()),
            encoder: encoder.to_string(),
            kind,
            cfg,
            ceiling,
            viewers: Mutex::default(),
            layers: Mutex::default(),
        })
    }

    /// Builds the encoder chain for one quality and starts it with the pipeline.
    fn make_layer(&self, q: Quality) -> Result<Layer, String> {
        let c = self.cfg.codec.short();
        // A leaky queue first: a slow encoder drops its own frames and never holds up the
        // capture or the other layers.
        let rate = if q.fps < self.cfg.mode.fps { format!("videorate drop-only=true max-rate={} ! ", q.fps) } else { String::new() };
        let desc = format!(
            "queue name=lqueue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! {rate}\
             d3d11convert ! capsfilter name=vsize caps=\"video/x-raw(memory:D3D11Memory),format=NV12,width={w},height={h}\" ! \
             {enc} name=venc {props} ! {c}parse config-interval=-1 ! video/x-{c},alignment=au ! \
             tee name=ltee allow-not-linked=true",
            w = q.width,
            h = q.height,
            enc = self.encoder,
            props = platform::encoder_properties(self.kind, q.kbit, q.fps, q.fps * self.cfg.keyframe_secs),
        );
        let bin = gst::parse::bin_from_description(&desc, true).map_err(|e| format!("layer {q}: {e}"))?;
        bin.set_property("name", format!("layer-{}x{}-{}-{}", q.width, q.height, q.fps, q.kbit));
        let tee = bin.by_name("ltee").unwrap();
        if q.fps < self.cfg.mode.fps && std::env::var_os("STREAMER_DEBUG_VIDEORATE_ONLY").is_none() {
            thin_frames(&bin.by_name("lqueue").unwrap(), q.fps);
        }
        let stats = if q == self.ceiling { self.stats.clone() } else { Arc::new(HostStats::default()) };
        watch_layer(&tee, stats.clone());
        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        let feed = self.pipeline.by_name("srctee").unwrap().request_pad_simple("src_%u").unwrap();
        let parked = Arc::new(AtomicBool::new(false));
        let p = parked.clone();
        feed.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if p.load(Ordering::Relaxed) {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        });
        feed.link(&bin.static_pad("sink").unwrap()).map_err(|e| format!("layer {q}: {e}"))?;
        bin.sync_state_with_parent().map_err(|e| format!("layer {q}: {e}"))?;
        if q == self.ceiling {
            debug_switch_size(&bin, q.width, q.height);
        }
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: layer {q} started", since_start_ms());
        }
        Ok(Layer { bin, tee, feed, stats, parked })
    }

    /// The tee of the layer at quality `q`, started (or unparked) if nobody watched it.
    /// An unparked encoder continues its stream; viewers move to it at its next periodic
    /// keyframe and keep their current layer until then.
    fn layer_tee(&self, q: Quality) -> Result<gst::Element, String> {
        let mut layers = self.layers.lock().unwrap();
        if let Some(l) = layers.get(&q) {
            if l.parked.swap(false, Ordering::SeqCst) && debug_timeline() {
                eprintln!("timeline {:>9.1} ms: layer {q} unparked", since_start_ms());
            }
            return Ok(l.tee.clone());
        }
        let l = self.make_layer(q)?;
        let tee = l.tee.clone();
        layers.insert(q, l);
        Ok(tee)
    }

    /// Releases tee pads that viewers left and parks layers nobody watches or waits for.
    /// Must not run on a streaming thread; takes the viewers lock.
    pub fn housekeeping(&self) {
        let viewers = self.viewers.lock().unwrap();
        self.housekeeping_locked(&viewers);
    }

    fn housekeeping_locked(&self, viewers: &HashMap<u64, Branch>) {
        trace("housekeeping enter");
        let mut used = std::collections::HashSet::new();
        for b in viewers.values() {
            let mut v = b.video.lock().unwrap();
            for (tee, pad) in v.retired.drain(..) {
                release_tee_pad(&tee, &pad);
            }
            used.insert(v.quality);
            if let Some((q, ..)) = &v.pending {
                used.insert(*q);
            }
        }
        // While viewers remain the pipeline plays: park idle layers. Once it has stopped
        // (no viewers) tearing them down is safe. STREAMER_DEBUG_DESTROY_LAYERS restores the
        // tear-down while playing, the spoiled variant for the churn gate.
        let destroy = viewers.is_empty() || std::env::var_os("STREAMER_DEBUG_DESTROY_LAYERS").is_some();
        let idle: Vec<Quality> = self.layers.lock().unwrap().keys().filter(|q| !used.contains(q)).copied().collect();
        if !destroy {
            for q in idle {
                if let Some(l) = self.layers.lock().unwrap().get(&q) {
                    if !l.parked.swap(true, Ordering::SeqCst) && debug_timeline() {
                        eprintln!("timeline {:>9.1} ms: layer {q} parked", since_start_ms());
                    }
                }
            }
            trace("housekeeping exit");
            return;
        }
        for q in idle {
            let Some(l) = self.layers.lock().unwrap().remove(&q) else { continue };
            trace("housekeeping: detach layer");
            // Detach from the capture between buffers, then stop it on its own.
            let (tx, rx) = mpsc::channel();
            let sink = l.bin.static_pad("sink").unwrap();
            l.feed.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                let _ = pad.unlink(&sink);
                let _ = tx.send(());
                gst::PadProbeReturn::Remove
            });
            if rx.recv_timeout(Duration::from_secs(2)).is_err() {
                if let Some(peer) = l.feed.peer() {
                    let _ = l.feed.unlink(&peer);
                }
            }
            trace("housekeeping: layer to NULL");
            let _ = l.bin.set_state(gst::State::Null);
            trace("housekeeping: remove layer");
            let _ = self.pipeline.remove(&l.bin);
            self.pipeline.by_name("srctee").unwrap().release_request_pad(&l.feed);
            if debug_timeline() {
                eprintln!("timeline {:>9.1} ms: layer {q} stopped", since_start_ms());
            }
        }
        trace("housekeeping exit");
    }

    /// The quality each viewer currently receives.
    pub fn viewer_layers(&self) -> Vec<(u64, Quality)> {
        self.viewers.lock().unwrap().iter().map(|(id, b)| (*id, b.video.lock().unwrap().quality)).collect()
    }

    /// Per layer: quality, viewers on it, statistics.
    pub fn layers(&self) -> Vec<LayerReport> {
        let viewers = self.viewers.lock().unwrap();
        let mut out: Vec<LayerReport> = self
            .layers
            .lock()
            .unwrap()
            .iter()
            .map(|(q, l)| LayerReport {
                quality: *q,
                parked: l.parked.load(Ordering::Relaxed),
                viewers: viewers.values().filter(|b| b.video.lock().unwrap().quality == *q).count(),
                stats: l.stats.clone(),
            })
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse((r.quality.height, r.quality.fps, r.quality.kbit)));
        out
    }

    /// Moves viewer `id` to the layer at `want`, clamped to the host's quality. It keeps the
    /// current layer until the new one sends a keyframe, so the picture never stops.
    pub fn set_quality(&self, id: u64, want: Quality) -> Result<Quality, String> {
        // STREAMER_DEBUG_NO_CLAMP is the spoiled variant for the layers gate.
        let q = if std::env::var_os("STREAMER_DEBUG_NO_CLAMP").is_some() { want } else { want.clamp_to(self.ceiling) };
        trace("set_quality enter");
        let viewers = self.viewers.lock().unwrap();
        let Some(b) = viewers.get(&id) else { return Err(format!("no viewer {id}")) };
        {
            let mut v = b.video.lock().unwrap();
            if let Some((_, tee, pad, probe)) = v.pending.take() {
                pad.remove_probe(probe);
                v.retired.push((tee, pad));
            }
            if v.quality == q {
                drop(v);
                self.housekeeping_locked(&viewers);
                return Ok(q);
            }
        }
        trace("set_quality: layer_tee");
        let tee = self.layer_tee(q)?;
        trace("set_quality: request pad");
        let pad = request_tee_pad(&tee);
        let (link, video_in) = (b.video.clone(), b.video_in.clone());
        let probe = pad
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let Some(buf) = info.buffer() else { return gst::PadProbeReturn::Ok };
                if buf.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    return gst::PadProbeReturn::Drop;
                }
                // The first keyframe of the new layer: swap inputs. Linking before the push
                // continues sends this keyframe, with the new caps, to the viewer.
                trace("handover enter");
                let mut v = link.lock().unwrap();
                let Some((q, tee, ..)) = v.pending.take() else { return gst::PadProbeReturn::Remove };
                let _ = v.pad.unlink(&video_in);
                let _ = pad.link(&video_in);
                let old = (std::mem::replace(&mut v.tee, tee), std::mem::replace(&mut v.pad, pad.clone()));
                v.retired.push(old);
                v.quality = q;
                trace("handover swapped");
                if debug_timeline() {
                    eprintln!("timeline {:>9.1} ms: viewer moved to {q}", since_start_ms());
                }
                gst::PadProbeReturn::Remove
            })
            .ok_or("cannot watch the new layer")?;
        b.video.lock().unwrap().pending = Some((q, tee, pad, probe));
        trace("set_quality: pending set");
        // The previous target, if any, may now be unused.
        self.housekeeping_locked(&viewers);
        Ok(q)
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
        let webrtcs: Vec<(u64, gst::Element, Arc<crate::catchup::Gate>)> =
            self.viewers.lock().unwrap().iter().map(|(id, b)| (*id, b.webrtc.clone(), b.gate.clone())).collect();
        webrtcs
            .into_iter()
            .map(|(id, w, gate)| {
                let mut q = ViewerQuality {
                    id,
                    catchups: gate.pauses.load(Ordering::Relaxed),
                    skipped_frames: gate.skipped_frames.load(Ordering::Relaxed),
                    ..Default::default()
                };
                let promise = gst::Promise::new();
                w.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
                if promise.wait() == gst::PromiseResult::Replied {
                    if let Some(reply) = promise.get_reply() {
                        if std::env::var_os("STREAMER_DEBUG_STATS").is_some() {
                            eprintln!("stats reply {reply}");
                        }
                        for s in reply.iter().filter_map(|(_, v)| v.get::<gst::Structure>().ok()) {
                            if std::env::var_os("STREAMER_DEBUG_STATS").is_some() {
                                eprintln!("stats {s}");
                            }
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
    /// `want`: the quality to start at, clamped to the host's; None is the host's own.
    pub fn add_viewer(&self, id: u64, link: Arc<dyn Outbox>, want: Option<Quality>) -> Result<(), String> {
        let mut viewers = self.viewers.lock().unwrap();
        if viewers.contains_key(&id) {
            return Ok(());
        }
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: add viewer {id}", since_start_ms());
        }
        let start = want.map_or(self.ceiling, |w| w.clamp_to(self.ceiling));
        let mut branch = self.build_branch(id, link, start)?;
        if viewers.is_empty() {
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| format!("cannot start capture and encoding: {e}"))?;
        }
        for e in std::iter::once(&branch.webrtc).chain(&branch.elements) {
            e.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        // STREAMER_DEBUG_LINK_FIRST links before the branch plays: the spoiled variant.
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_none() {
            for (src, sink) in branch.unlinked.drain(..) {
                src.link(&sink).map_err(|e| format!("viewer {id}: {e}"))?;
            }
        }
        viewers.insert(id, branch);
        Ok(())
    }

    /// Removes a viewer branch while the others keep playing. The last viewer stops capture
    /// and encoding. Must not run on a streaming thread.
    pub fn remove_viewer(&self, id: u64) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(mut branch) = viewers.remove(&id) else { return };
        if debug_timeline() {
            eprintln!("timeline {:>9.1} ms: remove viewer {id}", since_start_ms());
        }
        {
            let mut v = branch.video.lock().unwrap();
            if let Some((_, tee, pad, probe)) = v.pending.take() {
                pad.remove_probe(probe);
                release_tee_pad(&tee, &pad);
            }
            for (tee, pad) in v.retired.drain(..) {
                release_tee_pad(&tee, &pad);
            }
            branch.tee_pads.push((v.tee.clone(), v.pad.clone()));
        }

        trace("remove_viewer: wait idle");
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
        trace("remove_viewer: to NULL");
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
        trace("remove_viewer: release pads");
        for (tee, pad) in &branch.tee_pads {
            if let Some(peer) = pad.peer() {
                let _ = pad.unlink(&peer);
            }
            release_tee_pad(tee, pad);
        }
        if viewers.is_empty() {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
        self.housekeeping_locked(&viewers);
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

    fn build_branch(&self, id: u64, link: Arc<dyn Outbox>, start: Quality) -> Result<Branch, String> {
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
        let video_pay = make_pay(&format!("rtp{}pay", self.cfg.codec.short()), VIDEO_PT);
        video_pay.set_property("config-interval", -1i32);
        video_pay.set_property_from_str("aggregate-mode", "zero-latency");
        // A viewer starts on the quality it subscribed with, and may change it later.
        let tee = self.layer_tee(start)?;
        let video_queue = branch_queue();
        self.pipeline.add_many([&video_queue, &video_pay]).unwrap();
        gst::Element::link_many([&video_queue, &video_pay, &webrtc]).unwrap();
        let video_in = video_queue.static_pad("sink").unwrap();
        let pad = request_tee_pad(&tee);
        let mut unlinked = vec![(pad.clone(), video_in.clone())];
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_some() {
            pad.link(&video_in).map_err(|e| format!("viewer {id}: {e}"))?;
            unlinked.clear();
        }
        let video = VideoLink { quality: start, tee, pad, pending: None, retired: vec![] };
        let mut branch = Branch {
            webrtc,
            elements: vec![video_pay, video_queue.clone()],
            tee_pads: vec![],
            gate: Default::default(),
            video: Arc::new(Mutex::new(video)),
            video_in,
            unlinked,
        };
        // Between the queue and the payloader, so dropped frames leave no gaps in RTP
        // sequence numbers.
        let gate = branch.gate.clone();
        let queue_src = video_queue.static_pad("src").unwrap();
        queue_src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buf) = info.buffer() else { return gst::PadProbeReturn::Ok };
            if gate.pass(!buf.flags().contains(gst::BufferFlags::DELTA_UNIT)) {
                gst::PadProbeReturn::Ok
            } else {
                gst::PadProbeReturn::Drop
            }
        });
        if self.cfg.audio {
            self.feed(&mut branch, "atee", make_pay("rtpopuspay", AUDIO_PT));
        }
        Ok(branch)
    }

    /// Links tee -> leaky queue -> payloader -> the branch's webrtcbin.
    fn feed(&self, branch: &mut Branch, tee: &str, pay: gst::Element) {
        let queue = branch_queue();
        self.pipeline.add_many([&queue, &pay]).unwrap();
        gst::Element::link_many([&queue, &pay, &branch.webrtc]).unwrap();
        let tee = self.pipeline.by_name(tee).unwrap();
        let pad = tee.request_pad_simple("src_%u").unwrap();
        let sink = queue.static_pad("sink").unwrap();
        if std::env::var_os("STREAMER_DEBUG_LINK_FIRST").is_some() {
            pad.link(&sink).unwrap();
        } else {
            branch.unlinked.push((pad.clone(), sink));
        }
        branch.elements.extend([pay, queue]);
        branch.tee_pads.push((tee, pad));
    }

    /// Applies one signaling message from viewer `id`.
    pub fn on_message(&self, id: u64, link: &dyn Outbox, kind: &str, payload: &str) {
        let Some((webrtc, gate)) = self.viewers.lock().unwrap().get(&id).map(|b| (b.webrtc.clone(), b.gate.clone())) else {
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
            QUALITY_MSG => match Quality::parse_request(payload, self.ceiling).and_then(|want| self.set_quality(id, want)) {
                Ok(q) => println!("host: viewer {id} asked for {}, gets {q}", payload.trim()),
                Err(e) => eprintln!("host: viewer {id} quality request {}: {e}", payload.trim()),
            },
            crate::catchup::MSG => {
                if let Ok(ms) = payload.trim().parse::<u64>() {
                    gate.request(std::time::Duration::from_millis(ms));
                }
            }
            "ping" => {
                let now = self.running_time().unwrap_or(0);
                link.send("pong", &format!("{payload} {now}"));
            }
            _ => {}
        }
    }
}

/// Probe for viewer-chosen modes: STREAMER_DEBUG_SWITCH_SECS=N alternates the encoded size
/// between the mode and 1280x720 every N seconds, so decoders meet a new resolution mid-stream
/// exactly as when a viewer moves to another encoder.
fn debug_switch_size(layer: &gst::Bin, w: u32, h: u32) {
    let Some(secs) = std::env::var("STREAMER_DEBUG_SWITCH_SECS").ok().and_then(|v| v.parse::<u64>().ok()) else {
        return;
    };
    let filter = layer.by_name("vsize").unwrap();
    std::thread::spawn(move || {
        let mut small = false;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            small = !small;
            let (cw, ch) = if small { (1280, 720) } else { (w, h) };
            let caps = gst::Caps::builder("video/x-raw")
                .features(["memory:D3D11Memory"])
                .field("format", "NV12")
                .field("width", cw as i32)
                .field("height", ch as i32)
                .build();
            eprintln!("switch {:>9.1} ms: encode {cw}x{ch}", since_start_ms());
            filter.set_property("caps", &caps);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let enc = filter.parent().and_downcast::<gst::Bin>().and_then(|b| b.by_name("venc"));
            let out = enc.and_then(|e| e.static_pad("src")).and_then(|p| p.current_caps());
            let size = out.as_ref().and_then(|c| c.structure(0)).map(|s| {
                format!("{}x{}", s.get::<i32>("width").unwrap_or(0), s.get::<i32>("height").unwrap_or(0))
            });
            eprintln!("switch {:>9.1} ms: encoder output now {}", since_start_ms(), size.unwrap_or_default());
        }
    });
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// STREAMER_DEBUG_TIMELINE prints frame gaps and viewer changes on one time axis.
fn debug_timeline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STREAMER_DEBUG_TIMELINE").is_some())
}

/// Step trace for the timeline, to find where a stall happens.
fn trace(what: &str) {
    if debug_timeline() {
        eprintln!("timeline {:>9.1} ms: [{}] {what}", since_start_ms(), std::thread::current().name().unwrap_or("?"));
    }
}

fn since_start_ms() -> f64 {
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1e3
}

/// A new output of a layer's tee, as a ghost pad on the layer's bin so it can link to a
/// viewer branch in the pipeline. Pads of tees directly in the pipeline are returned as is.
fn request_tee_pad(tee: &gst::Element) -> gst::Pad {
    let pad = tee.request_pad_simple("src_%u").unwrap();
    match tee.parent().and_downcast::<gst::Bin>() {
        Some(bin) if !bin.is::<gst::Pipeline>() => {
            let ghost = gst::GhostPad::with_target(&pad).unwrap();
            ghost.set_active(true).unwrap();
            bin.add_pad(&ghost).unwrap();
            ghost.upcast()
        }
        _ => pad,
    }
}

/// Undoes `request_tee_pad`.
fn release_tee_pad(tee: &gst::Element, pad: &gst::Pad) {
    match pad.downcast_ref::<gst::GhostPad>() {
        Some(ghost) => {
            // The tee first, so it stops pushing here; only then the ghost. Deactivating the
            // ghost while the tee still pushed returned FLUSHING into the layer, which paused
            // its queue and, through the capture tee, the capture itself: the whole host
            // stopped (stress test, 2026-11-06). STREAMER_DEBUG_GHOST_FIRST is that order.
            let target = ghost.target();
            let ghost_first = std::env::var_os("STREAMER_DEBUG_GHOST_FIRST").is_some();
            if ghost_first {
                let _ = ghost.set_active(false);
            }
            if let Some(t) = &target {
                tee.release_request_pad(t);
            }
            let _ = ghost.set_target(None::<&gst::Pad>);
            if !ghost_first {
                let _ = ghost.set_active(false);
            }
            if let Some(bin) = ghost.parent().and_downcast::<gst::Element>() {
                let _ = bin.remove_pad(ghost);
            }
        }
        None => tee.release_request_pad(pad),
    }
}

/// Keeps at most `fps` frames per second by timestamp, before the layer's videorate.
/// videorate alone (drop-only, max-rate) let a 720p30 layer through at 60 fps after its
/// first seconds, and the encoder, configured for 30, then sent 6.9 instead of 4 Mbit/s.
/// STREAMER_DEBUG_VIDEORATE_ONLY leaves it to videorate: the spoiled variant.
fn thin_frames(queue: &gst::Element, fps: u32) {
    let period = 1_000_000_000u64 / fps as u64;
    // Half a frame of slack, so capture jitter does not drop a frame that is due.
    let slack = period / 2;
    let next = Mutex::new(None::<u64>);
    queue.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|t| t.nseconds()) else {
            return gst::PadProbeReturn::Ok;
        };
        let mut next = next.lock().unwrap();
        match *next {
            Some(n) if pts + slack < n => gst::PadProbeReturn::Drop,
            Some(n) if pts < n + period => {
                *next = Some(n + period);
                gst::PadProbeReturn::Ok
            }
            // First frame, or a gap (parked layer, capture stall): start again from here.
            _ => {
                *next = Some(pts + period);
                gst::PadProbeReturn::Ok
            }
        }
    });
}

/// Leaky, so a viewer that stalls (or never negotiates) cannot block the tee.
fn branch_queue() -> gst::Element {
    gst::ElementFactory::make("queue")
        .property("max-size-buffers", 4u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .unwrap()
}

/// Frame statistics at a layer's fan-out point, and decision D1: a viewer's keyframe request
/// never reaches the shared encoder. STREAMER_DEBUG_FORWARD_KEY_REQUESTS is the spoiled
/// variant for the isolation gate.
fn watch_layer(tee: &gst::Element, s: Arc<HostStats>) {
    let tee_sink = tee.static_pad("sink").unwrap();
    let stats = s.clone();
    let last = Mutex::new(None::<std::time::Instant>);
    tee_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(b) = info.buffer() {
            let now = std::time::Instant::now();
            if let Some(prev) = last.lock().unwrap().replace(now) {
                let gap = now.duration_since(prev);
                stats.frame_interval.push(gap.as_nanos() as u64);
                if debug_timeline() && gap.as_millis() > 25 {
                    eprintln!("timeline {:>9.1} ms: encoded frame gap {:.1} ms", since_start_ms(), gap.as_secs_f64() * 1e3);
                }
            }
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
            stats.max_frame_bytes.fetch_max(b.size() as u64, Ordering::Relaxed);
            if !b.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                stats.key_frames.fetch_add(1, Ordering::Relaxed);
                stats.key_bytes.fetch_add(b.size() as u64, Ordering::Relaxed);
            }
        }
        gst::PadProbeReturn::Ok
    });
    let forward = std::env::var_os("STREAMER_DEBUG_FORWARD_KEY_REQUESTS").is_some();
    tee_sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        let is_request = info.event().and_then(|e| e.structure()).is_some_and(|st| st.name() == "GstForceKeyUnit");
        if is_request {
            s.key_requests_dropped.fetch_add(1, Ordering::Relaxed);
            if !forward {
                return gst::PadProbeReturn::Drop;
            }
        }
        gst::PadProbeReturn::Ok
    });
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
