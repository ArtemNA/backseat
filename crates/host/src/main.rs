//! Host: streams the screen (or a test source) and system audio. Joins a room on the
//! signaling server and serves every viewer that subscribes, or connects directly to one
//! viewer that listens on IP:PORT.

use media::config::{Codec, HostConfig, IceConfig, Source, VideoMode};
use media::host::Host;
use media::session::{Event, Session};
use media::signal::{self, Link};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: host (--server ws://HOST:PORT/ws [--room NAME] [--name NAME] | --peer IP:PORT) \
[--mode 1080p60] [--kbit 20000] [--codec h264|h265] [--keyframe-secs 2] [--source capture|bars|noise] \
[--monitor -1] [--no-audio] [--stun stun://HOST:PORT|none] [--turn turn://USER:PASS@HOST:PORT] [--seconds 0] [--report-secs 5]";

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
        audio_pid: None, audio_exclude: Vec::new(),
        source: Source::Capture { monitor: -1 },
        ice: IceConfig::default(),
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
            "--stun" => cfg.ice.stun = (val != "none").then(|| val.clone()),
            "--turn" => cfg.ice.turn = Some(val.clone()),
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
            // Capture one window instead of a monitor.
            "--window" => cfg.source = Source::Window { hwnd: num(val)? as u64 },
            // Capture only this program's audio.
            "--audio-pid" => cfg.audio_pid = Some(num(val)? as u32),
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
    if let Some(msg) = media::platform::set_gpu_priority_from_env() {
        println!("host: {msg}");
    }
    let gone = Arc::new(AtomicBool::new(false));
    let mut direct: Option<Arc<Link>> = None;
    let mut session = None;
    let host = match &args.target {
        Target::Direct(peer) => {
            let host = Arc::new(Host::new(args.cfg.clone()).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            }));
            let stream = connect(peer).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            });
            let (link, mut reader) = Link::new(stream);
            let link = Arc::new(link);
            if let Err(e) = host.add_viewer(0, link.clone(), None) {
                eprintln!("host: {e}");
                std::process::exit(1);
            }
            println!("host: viewer connected, negotiating");
            let (h, l, gone) = (host.clone(), link.clone(), gone.clone());
            std::thread::spawn(move || {
                while let Some((kind, payload)) = signal::recv(&mut reader) {
                    if kind == "bye" {
                        break;
                    }
                    h.on_message(0, l.as_ref(), &kind, &payload);
                }
                gone.store(true, Ordering::SeqCst);
            });
            direct = Some(link);
            host
        }
        Target::Server { url, room, name } => {
            let (s, events) = Session::join(url, room, name).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            });
            let host = s.start_streaming(args.cfg.clone()).unwrap_or_else(|e| {
                eprintln!("host: {e}");
                std::process::exit(1);
            });
            println!("host: joined room {room} as {name} (id {}), streaming", s.id);
            let (h, gone) = (host.clone(), gone.clone());
            std::thread::spawn(move || {
                while let Ok(e) = events.recv() {
                    match e {
                        Event::ViewerJoined { viewer, quality } => {
                            let at = quality.map(|q| format!(" at {q}")).unwrap_or_default();
                            println!("host: viewer {viewer} subscribed{at} ({} viewers)", h.viewer_count());
                        }
                        Event::ViewerLeft { viewer, took_ms } => {
                            println!("host: viewer {viewer} removed in {took_ms} ms ({} viewers)", h.viewer_count());
                        }
                        Event::Error(m) => eprintln!("host: server error: {m}"),
                        Event::Lost => break,
                        _ => {}
                    }
                }
                eprintln!("host: lost the signaling server");
                gone.store(true, Ordering::SeqCst);
            });
            session = Some(s);
            host
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

    // Test only: STREAMER_DEBUG_RECONFIGURE="SECS MODE KBIT SOURCE" changes the running stream
    // after SECS seconds (SOURCE: bars, noise or capture), as the app's Change button does.
    if let Ok(v) = std::env::var("STREAMER_DEBUG_RECONFIGURE") {
        let p: Vec<String> = v.split_whitespace().map(String::from).collect();
        let (h, base) = (host.clone(), args.cfg.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(p[0].parse().unwrap_or(10)));
            let mut cfg = base;
            cfg.mode = media::config::VideoMode::parse(&p[1]).expect("mode");
            cfg.kbit = p[2].parse().expect("kbit");
            cfg.source = match p.get(3).map(String::as_str) {
                Some("noise") => media::config::Source::Noise,
                Some("capture") => media::config::Source::Capture { monitor: 0 },
                _ => media::config::Source::Bars,
            };
            match h.reconfigure(cfg) {
                Ok(()) => println!("host: reconfigured to {} {} {}", p[1], p[2], p.get(3).map(String::as_str).unwrap_or("bars")),
                Err(e) => println!("host: reconfigure failed: {e}"),
            }
        });
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
    let mut marker = media::marker::Marker::default();
    while !gone.load(Ordering::SeqCst) && (args.seconds == 0 || started.elapsed().as_secs() < args.seconds) {
        std::thread::sleep(Duration::from_secs(args.report_secs));
        let now = load();
        let dt = now.4.duration_since(last.4).as_secs_f64();
        let (frames, bytes, keys, key_bytes) = (now.0 - last.0, now.1 - last.1, now.2 - last.2, now.3 - last.3);
        let delta_avg = (bytes - key_bytes) as f64 / (frames.saturating_sub(keys)).max(1) as f64 / 1000.0;
        let key_avg = key_bytes as f64 / keys.max(1) as f64 / 1000.0;
        let iv = s.frame_interval.take();
        println!(
            "host t={:>4}s | viewers {} | {:.1} fps | encoded gap p99 {:.1} max {:.1} ms | {:.2} Mbit/s | {} key frames, {:.0} KB vs delta {:.0} KB (x{:.1}) | max frame {} KB | key requests seen {}",
            started.elapsed().as_secs(),
            host.viewer_count(),
            frames as f64 / dt,
            iv.ms(0.99),
            iv.ms(1.0),
            bytes as f64 * 8.0 / dt / 1e6,
            keys,
            key_avg,
            delta_avg,
            key_avg / delta_avg.max(0.001),
            s.max_frame_bytes.swap(0, Ordering::Relaxed) / 1000,
            s.key_requests_dropped.load(Ordering::Relaxed),
        );
        host.housekeeping();
        // Layers below the host's own quality, started because some viewer asked for less.
        for l in host.layers().into_iter().filter(|l| l.quality != host.ceiling()) {
            let s = &l.stats;
            let (frames, bytes, keys) = (
                s.frames.swap(0, Ordering::Relaxed),
                s.bytes.swap(0, Ordering::Relaxed),
                s.key_frames.swap(0, Ordering::Relaxed),
            );
            println!(
                "host   layer {}{} | viewers {} | {:.1} fps | {:.2} Mbit/s | {} key frames",
                l.quality,
                if l.parked { " (parked)" } else { "" },
                l.viewers,
                frames as f64 / dt,
                bytes as f64 * 8.0 / dt / 1e6,
                keys,
            );
        }
        for q in host.viewer_quality() {
            let problems = marker.assess(&q);
            let mark = if problems.is_empty() {
                String::new()
            } else {
                format!("  <- PROBLEMS: {}", problems.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "))
            };
            println!(
                "host   viewer {} | lost {:.1}% | rtt {:.1} ms | nack {} | pli {} | rtx requests {} resent {} | catch-ups {} skipped frames {}{mark}",
                q.id,
                q.fraction_lost * 100.0,
                q.rtt_ms,
                q.nack,
                q.pli,
                q.rtx_requests,
                q.rtx_packets,
                q.catchups,
                q.skipped_frames,
            );
        }
        last = now;
    }
    if let Some(link) = direct {
        link.send("bye", "");
    }
    std::thread::sleep(Duration::from_millis(300));
    match session {
        Some(s) => s.leave(),
        None => host.stop(),
    }
    println!("host: stopped");
}
