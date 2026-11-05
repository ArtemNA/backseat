//! Stage 0 probe: one host and N viewers in one process, connected through real
//! WebRTC sessions over the local network stack. Measures frame pacing, capture-to-decode
//! latency, actual bitrate, keyframe count and keyframe requests. No display path.

use gst::prelude::*;
use gst_webrtc::{WebRTCRTPTransceiver, WebRTCRTPTransceiverDirection, WebRTCSDPType, WebRTCSessionDescription};
mod net;
#[cfg(windows)]
mod swapchain;

/// Stand-in so the shared viewer code compiles where the own swapchain does not exist.
#[cfg(not(windows))]
mod swapchain {
    use std::sync::{mpsc, Arc};
    pub const WIDTH: u32 = 1920;
    pub const HEIGHT: u32 = 1080;
    pub type OnPresent = Arc<dyn Fn(Option<u64>) + Send + Sync>;
    pub struct Renderer {
        pub context: gst::Context,
        pub sinks: mpsc::Sender<(gst_app::AppSink, OnPresent)>,
        pub tearing: bool,
    }
    pub fn start() -> Renderer {
        panic!("--display own is Windows only")
    }
}

/// Host running time in ns, as seen by the viewer. None until the clock is known.
type HostClock = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

fn decoder_name(codec: &str) -> String {
    if cfg!(target_os = "macos") {
        "vtdec_hw".into()
    } else {
        format!("d3d11{codec}dec")
    }
}

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WARMUP: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
struct Args {
    codec: String,
    src: String,
    kbit: u32,
    fps: u32,
    width: u32,
    height: u32,
    gop: i32,
    viewers: usize,
    stuck: usize,
    seconds: u64,
    latency_ms: u32,
    forward_key_requests: bool,
    delay_ms: u64,
    loss_pct: f64,
    wait_keyframe: bool,
    /// "fake", "d3d11", "d3d12" or "own": the sink of viewer 0. Other viewers use fakesink.
    display: String,
    /// "local" (host and viewers in one process), "host" or "viewer".
    role: String,
    peer: Option<String>,
    listen: Option<String>,
}

/// Lossy relay between one host branch and viewer 0. Drops RTP only (host to viewer):
/// STUN, DTLS and RTCP pass, so the session stays up while media suffers.
struct LossProxy {
    host_side: UdpSocket,
    viewer_side: UdpSocket,
    host_addr: Mutex<Option<SocketAddr>>,
    viewer_addr: Mutex<Option<SocketAddr>>,
    loss_pct: f64,
    dropped: AtomicU64,
    forwarded: AtomicU64,
}

#[derive(Clone, Copy)]
enum From_ {
    Host,
    Viewer,
}

impl LossProxy {
    fn start(loss_pct: f64) -> Arc<Self> {
        let open = || {
            let s = UdpSocket::bind("0.0.0.0:0").unwrap();
            s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            s
        };
        let p = Arc::new(Self {
            host_side: open(),
            viewer_side: open(),
            host_addr: Mutex::new(None),
            viewer_addr: Mutex::new(None),
            loss_pct,
            dropped: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
        });
        let a = p.clone();
        std::thread::spawn(move || a.pump_host_to_viewer());
        let b = p.clone();
        std::thread::spawn(move || b.pump_viewer_to_host());
        p
    }

    fn pump_host_to_viewer(&self) {
        let mut buf = vec![0u8; 2048];
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        loop {
            let Ok(n) = self.host_side.recv(&mut buf) else { continue };
            let is_rtp = n > 1 && (128..192).contains(&buf[0]) && !(200..=204).contains(&buf[1]);
            if is_rtp {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if ((rng % 100_000) as f64) < self.loss_pct * 1000.0 {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                self.forwarded.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(to) = *self.viewer_addr.lock().unwrap() {
                self.viewer_side.send_to(&buf[..n], to).ok();
            }
        }
    }

    fn pump_viewer_to_host(&self) {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok(n) = self.viewer_side.recv(&mut buf) else { continue };
            if let Some(to) = *self.host_addr.lock().unwrap() {
                self.host_side.send_to(&buf[..n], to).ok();
            }
        }
    }

    /// Rewrites the first IPv4 UDP candidate so both peers only see the relay.
    /// Every other candidate is dropped. Returns None to swallow a candidate.
    fn rewrite(&self, from: From_, candidate: &str) -> Option<String> {
        if candidate.is_empty() {
            return Some(String::new());
        }
        let mut parts: Vec<String> = candidate.split(' ').map(str::to_string).collect();
        if parts.len() < 6 || !parts[2].eq_ignore_ascii_case("udp") || !parts[4].contains('.') {
            return None;
        }
        let addr: SocketAddr = format!("{}:{}", parts[4], parts[5]).parse().ok()?;
        let (slot, relay) = match from {
            From_::Host => (&self.host_addr, &self.viewer_side),
            From_::Viewer => (&self.viewer_addr, &self.host_side),
        };
        let mut slot = slot.lock().unwrap();
        if slot.is_some() {
            return None;
        }
        *slot = Some(addr);
        parts[5] = relay.local_addr().unwrap().port().to_string();
        Some(parts.join(" "))
    }
}

fn parse_args() -> Args {
    let mut a = Args {
        codec: "h265".into(),
        src: "snow".into(),
        kbit: 20000,
        fps: 60,
        width: 1920,
        height: 1080,
        gop: 120,
        viewers: 1,
        stuck: 0,
        seconds: 20,
        latency_ms: 20,
        forward_key_requests: false,
        delay_ms: 0,
        loss_pct: 0.0,
        wait_keyframe: false,
        display: "fake".into(),
        role: "local".into(),
        peer: None,
        listen: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let key = argv[i].as_str();
        if key == "--forward-key-requests" {
            a.forward_key_requests = true;
            i += 1;
            continue;
        }
        if key == "--wait-keyframe" {
            a.wait_keyframe = true;
            i += 1;
            continue;
        }
        let val = argv.get(i + 1).unwrap_or_else(|| panic!("missing value for {key}"));
        match key {
            "--codec" => a.codec = val.clone(),
            "--src" => a.src = val.clone(),
            "--kbit" => a.kbit = val.parse().unwrap(),
            "--fps" => a.fps = val.parse().unwrap(),
            "--width" => a.width = val.parse().unwrap(),
            "--height" => a.height = val.parse().unwrap(),
            "--gop" => a.gop = val.parse().unwrap(),
            "--viewers" => a.viewers = val.parse().unwrap(),
            "--stuck" => a.stuck = val.parse().unwrap(),
            "--seconds" => a.seconds = val.parse().unwrap(),
            "--latency" => a.latency_ms = val.parse().unwrap(),
            "--delay-ms" => a.delay_ms = val.parse().unwrap(),
            "--loss" => a.loss_pct = val.parse().unwrap(),
            "--display" => a.display = val.clone(),
            "--role" => a.role = val.clone(),
            "--peer" => a.peer = Some(val.clone()),
            "--listen" => a.listen = Some(val.clone()),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    assert!(a.codec == "h265" || a.codec == "h264", "codec must be h264 or h265");
    a
}

#[derive(Default)]
struct HostStats {
    key_frames: AtomicU64,
    frames: AtomicU64,
    bytes: AtomicU64,
    key_requests: AtomicU64,
    key_bytes: AtomicU64,
    max_key_bytes: AtomicU64,
}

fn host_pipeline(a: &Args, stats: Arc<HostStats>) -> gst::Pipeline {
    let source = match a.src.as_str() {
        "capture" => format!(
            "d3d11screencapturesrc capture-api=dxgi show-cursor=true ! video/x-raw(memory:D3D11Memory),framerate={}/1",
            a.fps
        ),
        "snow" => format!(
            "videotestsrc is-live=true pattern=snow ! video/x-raw,format=NV12,width={},height={},framerate={}/1 ! d3d11upload",
            a.width, a.height, a.fps
        ),
        // Known colours and layout, moving: checks colour order and geometry by eye.
        "bars" => format!(
            "videotestsrc is-live=true pattern=smpte horizontal-speed=4 ! video/x-raw,format=NV12,width={},height={},framerate={}/1 ! d3d11upload",
            a.width, a.height, a.fps
        ),
        other => panic!("unknown source {other}"),
    };
    // Control for the latency measurement: a known delay must appear in the result.
    let delay = if a.delay_ms > 0 {
        format!(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=0 min-threshold-time={} ! ",
            a.delay_ms * 1_000_000
        )
    } else {
        String::new()
    };
    let desc = format!(
        "{source} ! {delay}queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! \
         nvd3d11{codec}enc name=enc preset=p4 tune=ultra-low-latency rc-mode=cbr bitrate={kbit} vbv-buffer-size={vbv} \
         gop-size={gop} bframes=0 zerolatency=true repeat-sequence-header=true aud=false ! \
         {codec}parse config-interval=-1 ! video/x-{codec},alignment=au ! tee name=t allow-not-linked=true",
        codec = a.codec,
        kbit = a.kbit,
        vbv = a.kbit / a.fps,
        gop = a.gop,
    );
    let pipeline = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
    let tee = pipeline.by_name("t").unwrap();
    let sink = tee.static_pad("sink").unwrap();

    let s = stats.clone();
    sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer() {
            s.frames.fetch_add(1, Ordering::Relaxed);
            s.bytes.fetch_add(buffer.size() as u64, Ordering::Relaxed);
            if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                s.key_frames.fetch_add(1, Ordering::Relaxed);
                s.key_bytes.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                s.max_key_bytes.fetch_max(buffer.size() as u64, Ordering::Relaxed);
            }
        }
        gst::PadProbeReturn::Ok
    });

    // Decision D1: viewers never reach the shared encoder.
    let forward = a.forward_key_requests;
    sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        if let Some(event) = info.event() {
            let is_key_request = event.structure().is_some_and(|st| st.name() == "GstForceKeyUnit");
            if is_key_request {
                stats.key_requests.fetch_add(1, Ordering::Relaxed);
                if !forward {
                    return gst::PadProbeReturn::Drop;
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    pipeline
}

fn make(factory: &str) -> gst::Element {
    gst::ElementFactory::make(factory)
        .build()
        .unwrap_or_else(|e| panic!("cannot create {factory}: {e}"))
}

fn make_webrtc(name: String) -> gst::Element {
    let w = gst::ElementFactory::make("webrtcbin").name(name).build().unwrap();
    w.set_property_from_str("bundle-policy", "max-bundle");
    w
}

fn add_ice_forward(from: &gst::Element, to: &gst::Element, relay: Option<(Arc<LossProxy>, From_)>) {
    let to = to.downgrade();
    from.connect("on-ice-candidate", false, move |values| {
        let mline = values[1].get::<u32>().unwrap();
        let mut candidate = values[2].get::<String>().unwrap();
        if let Some((proxy, side)) = &relay {
            match proxy.rewrite(*side, &candidate) {
                Some(c) => candidate = c,
                None => return None,
            }
        }
        if let Some(to) = to.upgrade() {
            to.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
        }
        None
    });
}

enum Peer {
    /// Viewer in this process, optionally behind the lossy relay.
    Local(gst::Element, Option<Arc<LossProxy>>),
    /// Viewer in another process, reached over the signaling link.
    Remote(Arc<net::Link>),
    /// Offer is never answered.
    Stuck,
}

/// Adds one host branch for one viewer.
fn add_host_branch(a: &Args, pipeline: &gst::Pipeline, index: usize, peer: Peer) -> gst::Element {
    let (viewer, relay, link) = match peer {
        Peer::Local(v, r) => (Some(v), r, None),
        Peer::Remote(l) => (None, None, Some(l)),
        Peer::Stuck => (None, None, None),
    };
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 4u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .unwrap();
    queue.set_property_from_str("leaky", "downstream");

    let pay = make(&format!("rtp{}pay", a.codec));
    pay.set_property("config-interval", -1i32);
    pay.set_property_from_str("aggregate-mode", "zero-latency");
    pay.set_property("pt", 96u32);
    pay.set_property("mtu", 1200u32);
    // RTP time equals the host running time, which gives the latency measurement.
    pay.set_property("timestamp-offset", 0u32);

    let webrtc = make_webrtc(format!("host-webrtc-{index}"));
    if let Some(viewer) = &viewer {
        add_ice_forward(&webrtc, viewer, relay.clone().map(|r| (r, From_::Host)));
        add_ice_forward(viewer, &webrtc, relay.map(|r| (r, From_::Viewer)));
    }
    if let Some(link) = &link {
        let link = link.clone();
        webrtc.connect("on-ice-candidate", false, move |values| {
            let mline = values[1].get::<u32>().unwrap();
            let candidate = values[2].get::<String>().unwrap();
            link.send("ice", &format!("{mline} {candidate}"));
            None
        });
    }

    let viewer_for_nego = viewer.clone();
    let link_for_nego = link.clone();
    webrtc.connect("on-negotiation-needed", false, move |values| {
        let host = values[0].get::<gst::Element>().unwrap();
        let t = host.emit_by_name::<Option<WebRTCRTPTransceiver>>("get-transceiver", &[&0i32]);
        if let Some(t) = t {
            t.set_property("do-nack", true);
        }
        let viewer = viewer_for_nego.clone();
        let link = link_for_nego.clone();
        let host2 = host.clone();
        let promise = gst::Promise::with_change_func(move |reply| {
            let reply = reply.expect("offer reply").expect("offer structure");
            let offer = reply.get::<WebRTCSessionDescription>("offer").expect("offer");
            host2.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            if let Some(viewer) = viewer {
                answer(&host2, &viewer, &offer);
            }
            if let Some(link) = link {
                link.send("offer", &offer.sdp().as_text().unwrap());
            }
        });
        host.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        None
    });

    pipeline.add_many([&queue, &pay, &webrtc]).unwrap();
    gst::Element::link_many([&queue, &pay, &webrtc]).unwrap();
    let tee = pipeline.by_name("t").unwrap();
    let tee_pad = tee.request_pad_simple("src_%u").unwrap();
    tee_pad.link(&queue.static_pad("sink").unwrap()).unwrap();
    for e in [&webrtc, &pay, &queue] {
        e.sync_state_with_parent().unwrap();
    }
    webrtc
}

fn answer(host: &gst::Element, viewer: &gst::Element, offer: &WebRTCSessionDescription) {
    // The SDP is serialized on purpose, as it will cross a signaling server later.
    let text = offer.sdp().as_text().unwrap();
    let sdp = gst_sdp::SDPMessage::parse_buffer(text.as_bytes()).unwrap();
    let remote = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);
    viewer.emit_by_name::<()>("set-remote-description", &[&remote, &None::<gst::Promise>]);
    let (host, viewer2) = (host.clone(), viewer.clone());
    let promise = gst::Promise::with_change_func(move |reply| {
        let reply = reply.expect("answer reply").expect("answer structure");
        let ans = reply.get::<WebRTCSessionDescription>("answer").expect("answer");
        viewer2.emit_by_name::<()>("set-local-description", &[&ans, &None::<gst::Promise>]);
        host.emit_by_name::<()>("set-remote-description", &[&ans, &None::<gst::Promise>]);
    });
    viewer.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
}

#[derive(Default)]
struct ViewerStats {
    intervals_ns: Vec<u64>,
    latency_ns: Vec<u64>,
    before_decoder_ns: Vec<u64>,
    last: Option<Instant>,
}

struct Viewer {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    stats: Arc<Mutex<ViewerStats>>,
    presents: Arc<Mutex<Vec<u64>>>,
    present_latency: Arc<Mutex<Vec<u64>>>,
}

fn build_viewer(a: &Args, index: usize, clock: HostClock) -> Viewer {
    let pipeline = gst::Pipeline::with_name(&format!("viewer-{index}"));
    let webrtc = make_webrtc(format!("viewer-webrtc-{index}"));
    webrtc.set_property("latency", a.latency_ms);
    pipeline.add(&webrtc).unwrap();

    let encoding = a.codec.to_uppercase();
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", encoding.as_str())
        .field("clock-rate", 90000i32)
        .field("payload", 96i32)
        .build();
    let t = webrtc.emit_by_name::<WebRTCRTPTransceiver>(
        "add-transceiver",
        &[&WebRTCRTPTransceiverDirection::Recvonly, &caps],
    );
    t.set_property("do-nack", true);

    let stats = Arc::new(Mutex::new(ViewerStats::default()));
    let codec = a.codec.clone();
    let wait_keyframe = a.wait_keyframe;
    let display = a.display.clone();
    let present_ns: Arc<Mutex<Vec<u64>>> = Arc::default();
    let presents_out = present_ns.clone();
    let present_lat: Arc<Mutex<Vec<u64>>> = Arc::default();
    let present_lat_out = present_lat.clone();
    let render_tx = if index == 0 && a.display == "own" {
        let r = swapchain::start();
        println!("own swapchain: tearing supported = {}", r.tearing);
        pipeline.set_context(&r.context);
        Some(r.sinks)
    } else {
        None
    };
    let pipeline_weak = pipeline.downgrade();
    let host_weak = clock;
    let stats2 = stats.clone();
    let started = Instant::now();
    webrtc.connect_pad_added(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let Some(pipeline) = pipeline_weak.upgrade() else { return };
        let depay = make(&format!("rtp{codec}depay"));
        if wait_keyframe {
            // After a gap nothing reaches the decoder until the next keyframe: a clean freeze.
            depay.set_property("wait-for-keyframe", true);
        }
        let parse = make(&format!("{codec}parse"));
        let dec = make(&decoder_name(&codec));
        let sink = match (index, display.as_str()) {
            (0, "d3d11") => {
                let s = gst::ElementFactory::make("d3d11videosink")
                    .property("sync", false)
                    .property("emit-present", true)
                    .build()
                    .unwrap();
                let presents = present_ns.clone();
                let last = Arc::new(Mutex::new(None::<Instant>));
                s.connect("present", false, move |_| {
                    let now = Instant::now();
                    let mut l = last.lock().unwrap();
                    if now.duration_since(started) > WARMUP {
                        if let Some(prev) = *l {
                            presents.lock().unwrap().push(now.duration_since(prev).as_nanos() as u64);
                        }
                    }
                    *l = Some(now);
                    None
                });
                s
            }
            (0, "d3d12") => gst::ElementFactory::make("d3d12videosink").property("sync", false).build().unwrap(),
            (0, "own") => {
                let caps = gst::Caps::builder("video/x-raw")
                    .features(["memory:D3D11Memory"])
                    .field("format", "BGRA")
                    .field("width", swapchain::WIDTH as i32)
                    .field("height", swapchain::HEIGHT as i32)
                    .build();
                gst_app::AppSink::builder().caps(&caps).max_buffers(1).drop(true).sync(false).build().upcast()
            }
            _ => gst::ElementFactory::make("fakesink").property("sync", false).build().unwrap(),
        };
        let dec = if display == "d3d12" && index == 0 { make(&format!("d3d12{codec}dec")) } else { dec };
        let mut chain = vec![depay.clone(), parse, dec.clone()];
        if index == 0 && display == "own" {
            // NV12 decoder output to BGRA at the back buffer size, still on the GPU.
            chain.push(make("d3d11convert"));
        }
        chain.push(sink.clone());
        pipeline.add_many(&chain).unwrap();
        gst::Element::link_many(&chain).unwrap();
        pad.link(&depay.static_pad("sink").unwrap()).unwrap();

        // Map viewer PTS to the host running time carried by the RTP timestamp. The decoder
        // drops frames before the first keyframe, so matching by order would be wrong.
        let last_rtp_ns = Arc::new(AtomicU64::new(0));
        let rtp_in = last_rtp_ns.clone();
        depay.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                if let Ok(rtp) = gst_rtp::RTPBuffer::from_buffer_readable(buffer) {
                    let ns = rtp.timestamp() as u64 * 1_000_000_000 / 90_000;
                    rtp_in.store(ns, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
        let by_pts: Arc<Mutex<VecDeque<(u64, u64)>>> = Arc::default();
        let by_pts_in = by_pts.clone();
        let by_pts_present = by_pts.clone();
        let host_weak_present = host_weak.clone();
        let (clock_net, stats_net) = (host_weak.clone(), stats2.clone());
        depay.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                let rtp_ns = last_rtp_ns.load(Ordering::Relaxed);
                // Latency before the decoder: network, jitterbuffer and depayloading only.
                if Instant::now().duration_since(started) > WARMUP {
                    if let Some(now) = clock_net() {
                        stats_net.lock().unwrap().before_decoder_ns.push(now.saturating_sub(rtp_ns));
                    }
                }
                let mut q = by_pts_in.lock().unwrap();
                q.push_back((pts.nseconds(), rtp_ns));
                if q.len() > 600 {
                    q.pop_front();
                }
            }
            gst::PadProbeReturn::Ok
        });

        let stats = stats2.clone();
        let host_weak = host_weak.clone();
        dec.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let now = Instant::now();
            let pts = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds());
            let sent = pts.and_then(|p| {
                let q = by_pts.lock().unwrap();
                q.iter().rev().find(|(k, _)| *k == p).map(|(_, v)| *v)
            });
            let mut s = stats.lock().unwrap();
            if now.duration_since(started) > WARMUP {
                if let Some(last) = s.last {
                    s.intervals_ns.push(now.duration_since(last).as_nanos() as u64);
                }
                if let (Some(sent), Some(running)) = (sent, host_weak()) {
                    s.latency_ns.push(running.saturating_sub(sent));
                }
            }
            s.last = Some(now);
            gst::PadProbeReturn::Ok
        });

        if let (Some(tx), Ok(appsink)) = (render_tx.clone(), sink.clone().downcast::<gst_app::AppSink>()) {
            let by_pts = by_pts_present.clone();
            let host_weak = host_weak_present.clone();
            let presents = present_ns.clone();
            let lat = present_lat.clone();
            let last = Mutex::new(None::<Instant>);
            let on_present: swapchain::OnPresent = Arc::new(move |pts| {
                let now = Instant::now();
                if now.duration_since(started) <= WARMUP {
                    *last.lock().unwrap() = Some(now);
                    return;
                }
                if let Some(prev) = last.lock().unwrap().replace(now) {
                    presents.lock().unwrap().push(now.duration_since(prev).as_nanos() as u64);
                }
                let sent = pts.and_then(|p| by_pts.lock().unwrap().iter().rev().find(|(k, _)| *k == p).map(|(_, v)| *v));
                if let (Some(sent), Some(running)) = (sent, host_weak()) {
                    lat.lock().unwrap().push(running.saturating_sub(sent));
                }
            });
            tx.send((appsink, on_present)).unwrap();
        }
        for e in chain.iter().rev() {
            e.sync_state_with_parent().unwrap();
        }
    });
    Viewer { pipeline, webrtc, stats, presents: presents_out, present_latency: present_lat_out }
}

fn watch_bus(pipeline: &gst::Pipeline, name: String, failed: Arc<AtomicBool>) {
    let bus = pipeline.bus().unwrap();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            if let gst::MessageView::Error(e) = msg.view() {
                eprintln!("ERROR [{name}] {}: {:?}", e.error(), e.debug());
                failed.store(true, Ordering::SeqCst);
            }
        }
    });
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i] as f64 / 1e6
}

fn get_stats(webrtc: &gst::Element) -> Vec<gst::Structure> {
    let promise = gst::Promise::new();
    webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    let mut out = vec![];
    if promise.wait() == gst::PromiseResult::Replied {
        if let Some(reply) = promise.get_reply() {
            for (_, value) in reply.iter() {
                if let Ok(s) = value.get::<gst::Structure>() {
                    out.push(s);
                }
            }
        }
    }
    out
}

fn field_u64(s: &gst::StructureRef, name: &str) -> i64 {
    s.get::<u64>(name)
        .map(|v| v as i64)
        .or_else(|_| s.get::<i64>(name))
        .or_else(|_| s.get::<u32>(name).map(i64::from))
        .or_else(|_| s.get::<i32>(name).map(i64::from))
        .unwrap_or(-1)
}

fn host_running(host: &gst::Pipeline) -> Option<u64> {
    Some(host.clock()?.time().nseconds() - host.base_time()?.nseconds())
}

fn sdp(kind: WebRTCSDPType, text: &str) -> WebRTCSessionDescription {
    WebRTCSessionDescription::new(kind, gst_sdp::SDPMessage::parse_buffer(text.as_bytes()).unwrap())
}

fn add_remote_ice(webrtc: &gst::Element, payload: &str) {
    if let Some((mline, cand)) = payload.split_once(' ') {
        webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline.parse::<u32>().unwrap(), &cand.to_string()]);
    }
}

fn print_host(stats: &HostStats, begin: (u64, u64, u64, Instant)) {
    let elapsed = begin.3.elapsed().as_secs_f64();
    let frames = stats.frames.load(Ordering::Relaxed) - begin.0;
    let bytes = stats.bytes.load(Ordering::Relaxed) - begin.1;
    println!(
        "host: {:.1} fps, {:.2} Mbit/s, key frames {}, key requests seen {}",
        frames as f64 / elapsed,
        bytes as f64 * 8.0 / elapsed / 1e6,
        stats.key_frames.load(Ordering::Relaxed) - begin.2,
        stats.key_requests.load(Ordering::Relaxed),
    );
}

fn snapshot(s: &HostStats) -> (u64, u64, u64, Instant) {
    (
        s.frames.load(Ordering::Relaxed),
        s.bytes.load(Ordering::Relaxed),
        s.key_frames.load(Ordering::Relaxed),
        Instant::now(),
    )
}

fn print_outbound(branch: &gst::Element) {
    for st in get_stats(branch) {
        if st.has_field("packets-sent") && st.has_field("nack-count") {
            println!(
                "  outbound: sent {} nack {} pli {}",
                field_u64(&st, "packets-sent"),
                field_u64(&st, "nack-count"),
                field_u64(&st, "pli-count"),
            );
        }
        if st.has_field("round-trip-time") {
            println!(
                "  remote-inbound: lost {} rtt ms {:.2} jitter ms {:.2}",
                field_u64(&st, "packets-lost"),
                st.get::<f64>("round-trip-time").unwrap_or(f64::NAN) * 1000.0,
                st.get::<f64>("jitter").unwrap_or(f64::NAN) * 1000.0,
            );
        }
    }
}

/// Host process: connects to a listening viewer and streams to it.
fn run_host(a: &Args, peer: &str) {
    let stats = Arc::new(HostStats::default());
    let host = host_pipeline(a, stats.clone());
    let failed = Arc::new(AtomicBool::new(false));
    watch_bus(&host, "host".into(), failed.clone());
    host.set_state(gst::State::Playing).unwrap();

    let stream = std::net::TcpStream::connect(peer).expect("cannot reach the viewer");
    let (link, mut reader) = net::Link::new(stream);
    let link = Arc::new(link);
    let branch = add_host_branch(a, &host, 0, Peer::Remote(link.clone()));

    let (webrtc, host_weak, link2) = (branch.clone(), host.downgrade(), link.clone());
    std::thread::spawn(move || {
        while let Some((kind, payload)) = net::recv(&mut reader) {
            match kind.as_str() {
                "answer" => webrtc.emit_by_name::<()>(
                    "set-remote-description",
                    &[&sdp(WebRTCSDPType::Answer, &payload), &None::<gst::Promise>],
                ),
                "ice" => add_remote_ice(&webrtc, &payload),
                "ping" => {
                    let now = host_weak.upgrade().and_then(|h| host_running(&h)).unwrap_or(0);
                    link2.send("pong", &format!("{payload} {now}"));
                }
                _ => {}
            }
        }
    });

    std::thread::sleep(WARMUP);
    let begin = snapshot(&stats);
    std::thread::sleep(Duration::from_secs(a.seconds));
    print_host(&stats, begin);
    print_outbound(&branch);
    link.send("bye", "");
    std::thread::sleep(Duration::from_millis(500));
    host.set_state(gst::State::Null).ok();
    println!("result: {}", if failed.load(Ordering::SeqCst) { "ERRORS" } else { "no pipeline errors" });
}

/// Viewer process: listens for the host, answers, and estimates the host clock by pings.
fn run_viewer(a: &Args, listen: &str) {
    let listener = std::net::TcpListener::bind(listen).expect("cannot listen");
    println!("viewer listening on {listen}");
    let (stream, from) = listener.accept().unwrap();
    println!("host connected from {from}");
    let (link, mut reader) = net::Link::new(stream);
    let link = Arc::new(link);

    // Offset such that host_running = local + offset, from the ping with the lowest RTT.
    let base = Instant::now();
    let offset = Arc::new(AtomicU64::new(0));
    let best_rtt = Arc::new(AtomicU64::new(u64::MAX));
    let clock: HostClock = {
        let (offset, best_rtt) = (offset.clone(), best_rtt.clone());
        Arc::new(move || {
            if best_rtt.load(Ordering::Relaxed) == u64::MAX {
                return None;
            }
            let local = base.elapsed().as_nanos() as i64;
            Some((local + offset.load(Ordering::Relaxed) as i64) as u64)
        })
    };

    let failed = Arc::new(AtomicBool::new(false));
    let v = build_viewer(a, 0, clock);
    watch_bus(&v.pipeline, "viewer".into(), failed.clone());
    v.pipeline.set_state(gst::State::Playing).unwrap();

    {
        let link = link.clone();
        v.webrtc.connect("on-ice-candidate", false, move |values| {
            let mline = values[1].get::<u32>().unwrap();
            let candidate = values[2].get::<String>().unwrap();
            link.send("ice", &format!("{mline} {candidate}"));
            None
        });
    }
    {
        let link = link.clone();
        std::thread::spawn(move || loop {
            link.send("ping", &base.elapsed().as_nanos().to_string());
            std::thread::sleep(Duration::from_millis(100));
        });
    }

    let done = Arc::new(AtomicBool::new(false));
    let (webrtc, link2, done2) = (v.webrtc.clone(), link.clone(), done.clone());
    let rtts: Arc<Mutex<Vec<u64>>> = Arc::default();
    let rtts2 = rtts.clone();
    std::thread::spawn(move || {
        while let Some((kind, payload)) = net::recv(&mut reader) {
            match kind.as_str() {
                "offer" => {
                    webrtc.emit_by_name::<()>(
                        "set-remote-description",
                        &[&sdp(WebRTCSDPType::Offer, &payload), &None::<gst::Promise>],
                    );
                    let (w, l) = (webrtc.clone(), link2.clone());
                    let promise = gst::Promise::with_change_func(move |reply| {
                        let reply = reply.expect("answer reply").expect("answer structure");
                        let ans = reply.get::<WebRTCSessionDescription>("answer").expect("answer");
                        w.emit_by_name::<()>("set-local-description", &[&ans, &None::<gst::Promise>]);
                        l.send("answer", &ans.sdp().as_text().unwrap());
                    });
                    webrtc.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
                }
                "ice" => add_remote_ice(&webrtc, &payload),
                "pong" => {
                    let now = base.elapsed().as_nanos() as u64;
                    if let Some((sent, host_ns)) = payload.split_once(' ') {
                        let (sent, host_ns): (u64, u64) = (sent.parse().unwrap(), host_ns.parse().unwrap());
                        let rtt = now - sent;
                        rtts2.lock().unwrap().push(rtt);
                        if rtt < best_rtt.load(Ordering::Relaxed) {
                            best_rtt.store(rtt, Ordering::Relaxed);
                            let est = host_ns as i64 + rtt as i64 / 2 - now as i64;
                            offset.store(est as u64, Ordering::Relaxed);
                        }
                    }
                }
                "bye" => break,
                _ => {}
            }
        }
        done2.store(true, Ordering::SeqCst);
    });

    let deadline = Instant::now() + WARMUP + Duration::from_secs(a.seconds + 30);
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut r = rtts.lock().unwrap().clone();
    r.sort_unstable();
    println!(
        "clock: pings {} | rtt ms p50 {:.2} min {:.2} | offset uncertainty up to +-{:.2} ms",
        r.len(),
        percentile(&r, 0.5),
        percentile(&r, 0.0),
        percentile(&r, 0.0) / 2.0,
    );
    print_viewer(0, &v);
    v.pipeline.set_state(gst::State::Null).ok();
    println!("result: {}", if failed.load(Ordering::SeqCst) { "ERRORS" } else { "no pipeline errors" });
}

fn print_viewer(i: usize, v: &Viewer) {
    let s = v.stats.lock().unwrap();
    let (mut iv, mut lat, mut pre) = (s.intervals_ns.clone(), s.latency_ns.clone(), s.before_decoder_ns.clone());
    drop(s);
    pre.sort_unstable();
    println!(
        "  before decoder: latency ms p50 {:.2} p95 {:.2} p99 {:.2}",
        percentile(&pre, 0.5),
        percentile(&pre, 0.95),
        percentile(&pre, 0.99),
    );
    iv.sort_unstable();
    lat.sort_unstable();
    println!(
        "viewer {i}: decoded {} | interval ms p50 {:.2} p99 {:.2} max {:.2} | latency ms p50 {:.2} p95 {:.2} p99 {:.2}",
        iv.len(),
        percentile(&iv, 0.5),
        percentile(&iv, 0.99),
        percentile(&iv, 1.0),
        percentile(&lat, 0.5),
        percentile(&lat, 0.95),
        percentile(&lat, 0.99),
    );
    for st in get_stats(&v.webrtc) {
        if st.has_field("packets-received") && st.has_field("packets-lost") {
            println!(
                "  inbound: received {} lost {} repaired {} nack {} pli {} fir {}",
                field_u64(&st, "packets-received"),
                field_u64(&st, "packets-lost"),
                field_u64(&st, "packets-repaired"),
                field_u64(&st, "nack-count"),
                field_u64(&st, "pli-count"),
                field_u64(&st, "fir-count"),
            );
        }
    }
}

fn main() {
    let a = parse_args();
    // Windows timers default to ~15.6 ms; jitterbuffer timers then fire late.
    #[cfg(windows)]
    if std::env::var_os("PROBE_DEFAULT_TIMER").is_none() {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        println!("timer: 1 ms");
    }
    gst::init().unwrap();
    println!("args: {a:?}");
    match a.role.as_str() {
        "host" => return run_host(&a, &a.peer.clone().expect("--peer IP:PORT")),
        "viewer" => return run_viewer(&a, &a.listen.clone().expect("--listen IP:PORT")),
        _ => {}
    }

    let host_stats = Arc::new(HostStats::default());
    let host = host_pipeline(&a, host_stats.clone());
    let failed = Arc::new(AtomicBool::new(false));
    watch_bus(&host, "host".into(), failed.clone());
    host.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let proxy = (a.loss_pct > 0.0).then(|| LossProxy::start(a.loss_pct));
    let mut viewers = vec![];
    for i in 0..a.viewers {
        let hw = host.downgrade();
        let clock: HostClock = Arc::new(move || hw.upgrade().and_then(|h| host_running(&h)));
        let v = build_viewer(&a, i, clock);
        watch_bus(&v.pipeline, format!("viewer-{i}"), failed.clone());
        v.pipeline.set_state(gst::State::Playing).unwrap();
        v.pipeline.use_clock(host.clock().as_ref());
        let relay = if i == 0 { proxy.clone() } else { None };
        let branch = add_host_branch(&a, &host, i, Peer::Local(v.webrtc.clone(), relay));
        viewers.push((v, branch));
    }
    for i in 0..a.stuck {
        add_host_branch(&a, &host, a.viewers + i, Peer::Stuck);
    }

    let begin = (
        host_stats.frames.load(Ordering::Relaxed),
        host_stats.bytes.load(Ordering::Relaxed),
        host_stats.key_frames.load(Ordering::Relaxed),
        Instant::now(),
    );
    std::thread::sleep(Duration::from_secs(a.seconds));
    let elapsed = begin.3.elapsed().as_secs_f64();
    let frames = host_stats.frames.load(Ordering::Relaxed) - begin.0;
    let bytes = host_stats.bytes.load(Ordering::Relaxed) - begin.1;
    println!(
        "host: {:.1} fps, {:.2} Mbit/s, key frames {}, key requests seen {}",
        frames as f64 / elapsed,
        bytes as f64 * 8.0 / elapsed / 1e6,
        host_stats.key_frames.load(Ordering::Relaxed) - begin.2,
        host_stats.key_requests.load(Ordering::Relaxed),
    );

    if let Some(p) = &proxy {
        let (d, f) = (p.dropped.load(Ordering::Relaxed), p.forwarded.load(Ordering::Relaxed));
        println!("proxy (viewer 0): dropped {d}, forwarded {f}, loss {:.2}%", d as f64 * 100.0 / (d + f).max(1) as f64);
    }

    for (i, (v, branch)) in viewers.iter().enumerate() {
        let s = v.stats.lock().unwrap();
        let (mut iv, mut lat) = (s.intervals_ns.clone(), s.latency_ns.clone());
        iv.sort_unstable();
        lat.sort_unstable();
        println!(
            "viewer {i}: decoded {} | interval ms p50 {:.2} p99 {:.2} max {:.2} | latency ms p50 {:.2} p95 {:.2} p99 {:.2}",
            iv.len(),
            percentile(&iv, 0.5),
            percentile(&iv, 0.99),
            percentile(&iv, 1.0),
            percentile(&lat, 0.5),
            percentile(&lat, 0.95),
            percentile(&lat, 0.99),
        );
        drop(s);
        let mut pr = v.presents.lock().unwrap().clone();
        if !pr.is_empty() {
            pr.sort_unstable();
            let slow = pr.iter().filter(|&&x| x > 25_000_000).count();
            println!(
                "  present: {} | interval ms p50 {:.2} p99 {:.2} max {:.2} | over 25 ms: {}",
                pr.len(),
                percentile(&pr, 0.5),
                percentile(&pr, 0.99),
                percentile(&pr, 1.0),
                slow,
            );
        }
        let mut pl = v.present_latency.lock().unwrap().clone();
        if !pl.is_empty() {
            pl.sort_unstable();
            println!(
                "  latency to Present ms: p50 {:.2} p95 {:.2} p99 {:.2}",
                percentile(&pl, 0.5),
                percentile(&pl, 0.95),
                percentile(&pl, 0.99),
            );
        }
        for st in get_stats(&v.webrtc) {
            if st.has_field("packets-received") && st.has_field("packets-lost") {
                println!(
                    "  inbound: received {} lost {} repaired {} nack {} pli {} fir {}",
                    field_u64(&st, "packets-received"),
                    field_u64(&st, "packets-lost"),
                    field_u64(&st, "packets-repaired"),
                    field_u64(&st, "nack-count"),
                    field_u64(&st, "pli-count"),
                    field_u64(&st, "fir-count"),
                );
            }
        }
        for st in get_stats(branch) {
            if st.has_field("packets-sent") && st.has_field("nack-count") {
                println!(
                    "  outbound: sent {} nack {} pli {}",
                    field_u64(&st, "packets-sent"),
                    field_u64(&st, "nack-count"),
                    field_u64(&st, "pli-count"),
                );
            }
        }
    }

    let keys = host_stats.key_frames.load(Ordering::Relaxed).max(1);
    let total_frames = host_stats.frames.load(Ordering::Relaxed).max(keys + 1);
    let key_bytes = host_stats.key_bytes.load(Ordering::Relaxed);
    let delta_bytes = host_stats.bytes.load(Ordering::Relaxed).saturating_sub(key_bytes);
    println!(
        "frame size KB: key avg {:.0} max {:.0} | delta avg {:.0}",
        key_bytes as f64 / keys as f64 / 1000.0,
        host_stats.max_key_bytes.load(Ordering::Relaxed) as f64 / 1000.0,
        delta_bytes as f64 / (total_frames - keys) as f64 / 1000.0,
    );

    for (v, _) in &viewers {
        v.pipeline.set_state(gst::State::Null).ok();
    }
    host.set_state(gst::State::Null).ok();
    println!("result: {}", if failed.load(Ordering::SeqCst) { "ERRORS" } else { "no pipeline errors" });
}
