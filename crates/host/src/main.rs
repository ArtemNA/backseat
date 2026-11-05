//! Host: streams the screen (or a test source) and system audio. Joins a room on the
//! signaling server and serves every viewer that subscribes, or connects directly to one
//! viewer that listens on IP:PORT.

use gst::prelude::*;
use media::config::{Codec, HostConfig, Source, VideoMode};
use media::host::Host;
use media::server_client::ServerClient;
use media::signal::{self, Link, Outbox};
use protocol::{ClientMsg, PeerId, ServerMsg};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: host (--server ws://HOST:PORT/ws [--room NAME] [--name NAME] | --peer IP:PORT) \
[--mode 1080p60] [--kbit 20000] [--codec h264|h265] [--keyframe-secs 2] [--source capture|bars|noise] \
[--monitor -1] [--no-audio] [--seconds 0] [--report-secs 5]";

enum Target {
    Direct(String),
    Server { url: String, room: String, name: String },
}

struct Args {
    target: Target,
    cfg: HostConfig,
    seconds: u64,
    report_secs: u64,
}

fn parse() -> Result<Args, String> {
    let mut peer = None;
    let (mut server, mut room, mut name) = (None, "default".to_string(), "host".to_string());
    let mut cfg = HostConfig {
        mode: VideoMode::parse("1080p60")?,
        kbit: 20000,
        codec: Codec::H265,
        keyframe_secs: 2,
        audio: true,
        source: Source::Capture { monitor: -1 },
    };
    let (mut seconds, mut report_secs, mut monitor) = (0, 5, -1);
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let key = argv[i].as_str();
        if key == "--no-audio" {
            cfg.audio = false;
            i += 1;
            continue;
        }
        let val = argv.get(i + 1).ok_or(format!("missing value for {key}"))?;
        let num = |v: &str| v.parse::<i64>().map_err(|_| format!("bad number for {key}: {v}"));
        match key {
            "--peer" => peer = Some(val.clone()),
            "--server" => server = Some(val.clone()),
            "--room" => room = val.clone(),
            "--name" => name = val.clone(),
            "--mode" => cfg.mode = VideoMode::parse(val)?,
            "--kbit" => cfg.kbit = num(val)? as u32,
            "--codec" => cfg.codec = Codec::parse(val)?,
            "--keyframe-secs" => cfg.keyframe_secs = num(val)? as u32,
            "--source" => {
                cfg.source = match val.as_str() {
                    "capture" => Source::Capture { monitor },
                    "bars" => Source::Bars,
                    "noise" => Source::Noise,
                    _ => return Err(format!("unknown source {val}")),
                }
            }
            "--monitor" => monitor = num(val)? as i32,
            "--seconds" => seconds = num(val)? as u64,
            "--report-secs" => report_secs = num(val)? as u64,
            _ => return Err(format!("unknown argument {key}")),
        }
        i += 2;
    }
    if let Source::Capture { monitor: m } = &mut cfg.source {
        *m = monitor;
    }
    let target = match (server, peer) {
        (Some(url), None) => Target::Server { url, room, name },
        (None, Some(p)) => Target::Direct(p),
        _ => return Err("give exactly one of --server or --peer".into()),
    };
    Ok(Args { target, cfg, seconds, report_secs })
}

fn connect(peer: &str) -> Result<std::net::TcpStream, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match std::net::TcpStream::connect(peer) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() > deadline => return Err(format!("cannot reach viewer {peer}: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    media::init();
    let host = match Host::new(args.cfg.clone()) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("host: {e}");
            std::process::exit(1);
        }
    };
    let m = args.cfg.mode;
    println!(
        "host: {}x{}@{} {} {} kbit/s, encoder {}, audio {}",
        m.width,
        m.height,
        m.fps,
        args.cfg.codec.short(),
        args.cfg.kbit,
        host.encoder,
        args.cfg.audio
    );
    if let Err(e) = host.start() {
        eprintln!("host: {e}");
        std::process::exit(1);
    }

    let gone = Arc::new(AtomicBool::new(false));
    let mut direct: Option<Arc<Link>> = None;
    match &args.target {
        Target::Direct(peer) => {
            let stream = connect(peer).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            });
            let (link, mut reader) = Link::new(stream);
            let link = Arc::new(link);
            let webrtc = host.add_viewer(0, link.clone());
            println!("host: viewer connected, negotiating");
            let (host, l, gone) = (host.clone(), link.clone(), gone.clone());
            std::thread::spawn(move || {
                while let Some((kind, payload)) = signal::recv(&mut reader) {
                    if kind == "bye" {
                        break;
                    }
                    host.on_message(&webrtc, l.as_ref(), &kind, &payload);
                }
                gone.store(true, Ordering::SeqCst);
            });
            direct = Some(link);
        }
        Target::Server { url, room, name } => {
            let (client, rx) = ServerClient::join(url, room, name).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            });
            client.send(ClientMsg::SetStreaming { streaming: true });
            println!("host: joined room {room} as {name} (id {}), streaming", client.id);
            let (host, gone) = (host.clone(), gone.clone());
            std::thread::spawn(move || {
                let mut viewers: HashMap<PeerId, (gst::Element, Arc<dyn Outbox>)> = HashMap::new();
                while let Ok(msg) = rx.recv() {
                    match msg {
                        ServerMsg::SubscribeRequest { viewer } => {
                            let out = client.to_peer(viewer);
                            let webrtc = host.add_viewer(viewer as usize, out.clone());
                            viewers.insert(viewer, (webrtc, out));
                            println!("host: viewer {viewer} subscribed, negotiating ({} viewers)", viewers.len());
                        }
                        ServerMsg::Signal { from, kind, payload } => {
                            if let Some((webrtc, out)) = viewers.get(&from) {
                                host.on_message(webrtc, out.as_ref(), &kind, &payload);
                            }
                        }
                        ServerMsg::UnsubscribeRequest { viewer } => {
                            // Removing the branch at runtime is stage 3.
                            println!("host: viewer {viewer} unsubscribed (branch kept until stage 3)");
                        }
                        ServerMsg::Error { message } => eprintln!("host: server error: {message}"),
                        _ => {}
                    }
                }
                eprintln!("host: lost the signaling server");
                gone.store(true, Ordering::SeqCst);
            });
        }
    }

    let started = Instant::now();
    let s = &host.stats;
    let load = || {
        (
            s.frames.load(Ordering::Relaxed),
            s.bytes.load(Ordering::Relaxed),
            s.key_frames.load(Ordering::Relaxed),
            s.key_bytes.load(Ordering::Relaxed),
            Instant::now(),
        )
    };
    let mut last = load();
    while !gone.load(Ordering::SeqCst) && (args.seconds == 0 || started.elapsed().as_secs() < args.seconds) {
        std::thread::sleep(Duration::from_secs(args.report_secs));
        let now = load();
        let dt = now.4.duration_since(last.4).as_secs_f64();
        let (frames, bytes, keys, key_bytes) = (now.0 - last.0, now.1 - last.1, now.2 - last.2, now.3 - last.3);
        let delta_avg = (bytes - key_bytes) as f64 / (frames.saturating_sub(keys)).max(1) as f64 / 1000.0;
        let key_avg = key_bytes as f64 / keys.max(1) as f64 / 1000.0;
        println!(
            "host t={:>4}s | {:.1} fps | {:.2} Mbit/s | key frame {:.0} KB vs delta {:.0} KB (x{:.1}) | max frame {} KB | key requests dropped {}",
            started.elapsed().as_secs(),
            frames as f64 / dt,
            bytes as f64 * 8.0 / dt / 1e6,
            key_avg,
            delta_avg,
            key_avg / delta_avg.max(0.001),
            s.max_frame_bytes.swap(0, Ordering::Relaxed) / 1000,
            s.key_requests_dropped.load(Ordering::Relaxed),
        );
        last = now;
    }
    if let Some(link) = direct {
        link.send("bye", "");
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = host.pipeline.set_state(gst::State::Null);
    println!("host: stopped");
}
