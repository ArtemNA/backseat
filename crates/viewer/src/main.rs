//! Viewer: joins a room on the signaling server and watches one streamer, or listens on
//! IP:PORT for a host that connects directly.

use gst::prelude::*;
use media::clock::HostClock;
use media::config::ViewerConfig;
use media::server_client::ServerClient;
use media::signal::{self, Link, Outbox};
use media::viewer::Viewer;
use protocol::{ClientMsg, PeerInfo, ServerMsg};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: viewer (--server ws://HOST:PORT/ws [--room NAME] [--name NAME] [--watch STREAMER] | \
--listen 0.0.0.0:7700) [--latency 40] [--wait-keyframe] [--no-audio] [--audio-nosync] [--decoder NAME] \
[--seconds 0] [--report-secs 5]";

fn main() {
    let mut listen = "0.0.0.0:7700".to_string();
    let (mut server, mut room, mut name, mut watch) = (None::<String>, "default".to_string(), "viewer".to_string(), None::<String>);
    let mut subscribe = None;
    let mut never_answer = false;
    let mut cfg =
        ViewerConfig { latency_ms: 40, audio: true, wait_keyframe: false, decoder: None, audio_sync: true, headless: false };
    let (mut seconds, mut report_secs) = (0u64, 5u64);
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--wait-keyframe" => cfg.wait_keyframe = true,
            "--no-audio" => cfg.audio = false,
            "--audio-nosync" => cfg.audio_sync = false,
            // Test only: subscribes but never answers the offer, like a stuck peer.
            "--debug-never-answer" => never_answer = true,
            // Test only: no window and no decoding, so the process does not touch the GPU.
            "--headless" => cfg.headless = true,
            key => {
                let Some(val) = argv.get(i + 1) else {
                    eprintln!("missing value for {key}\n{USAGE}");
                    std::process::exit(2);
                };
                let ok = match key {
                    "--listen" => {
                        listen = val.clone();
                        true
                    }
                    "--server" => {
                        server = Some(val.clone());
                        true
                    }
                    "--room" => {
                        room = val.clone();
                        true
                    }
                    "--name" => {
                        name = val.clone();
                        true
                    }
                    "--watch" => {
                        watch = Some(val.clone());
                        true
                    }
                    "--latency" => val.parse().map(|v| cfg.latency_ms = v).is_ok(),
                    "--decoder" => {
                        cfg.decoder = Some(val.clone());
                        true
                    }
                    "--seconds" => val.parse().map(|v| seconds = v).is_ok(),
                    "--report-secs" => val.parse().map(|v| report_secs = v).is_ok(),
                    _ => false,
                };
                if !ok {
                    eprintln!("bad argument {key} {val}\n{USAGE}");
                    std::process::exit(2);
                }
                i += 1;
            }
        }
        i += 1;
    }

    media::init();
    // Messages from the host, as (kind, payload), whichever way we reach it.
    let (in_tx, in_rx) = mpsc::channel::<(String, String)>();
    let link: Arc<dyn Outbox> = match &server {
        None => {
            let listener = std::net::TcpListener::bind(&listen).unwrap_or_else(|e| {
                eprintln!("viewer: cannot listen on {listen}: {e}");
                std::process::exit(1);
            });
            println!("viewer: waiting for the host on {listen}");
            let (stream, from) = listener.accept().expect("accept");
            println!("viewer: host connected from {}", from.ip());
            let (link, mut reader) = Link::new(stream);
            std::thread::spawn(move || {
                while let Some(m) = signal::recv(&mut reader) {
                    if in_tx.send(m).is_err() {
                        break;
                    }
                }
                let _ = in_tx.send(("bye".into(), String::new()));
            });
            Arc::new(link)
        }
        Some(url) => {
            let (client, rx) = ServerClient::join(url, &room, &name).unwrap_or_else(|e| {
                eprintln!("viewer: {e}");
                std::process::exit(1);
            });
            println!("viewer: joined room {room} as {name} (id {})", client.id);
            let pick = |p: &PeerInfo| p.streaming && watch.as_ref().is_none_or(|w| *w == p.name);
            let mut streamer = client.peers.iter().find(|p| pick(p)).map(|p| p.id);
            while streamer.is_none() {
                println!("viewer: waiting for a streamer{}", watch.as_ref().map(|w| format!(" named {w}")).unwrap_or_default());
                match rx.recv() {
                    Ok(ServerMsg::PeerJoined { peer } | ServerMsg::PeerUpdated { peer }) if pick(&peer) => streamer = Some(peer.id),
                    Ok(_) => {}
                    Err(_) => {
                        eprintln!("viewer: lost the signaling server");
                        std::process::exit(1);
                    }
                }
            }
            let streamer = streamer.unwrap();
            println!("viewer: watching peer {streamer}");
            let out = client.to_peer(streamer);
            let c = client.clone();
            std::thread::spawn(move || {
                while let Ok(msg) = rx.recv() {
                    let forward = match msg {
                        ServerMsg::Signal { from, kind, payload } if from == streamer => (kind, payload),
                        ServerMsg::PeerLeft { id } if id == streamer => ("bye".into(), String::new()),
                        ServerMsg::PeerUpdated { peer } if peer.id == streamer && !peer.streaming => ("bye".into(), String::new()),
                        ServerMsg::Error { message } => {
                            eprintln!("viewer: server error: {message}");
                            continue;
                        }
                        _ => continue,
                    };
                    if in_tx.send(forward).is_err() {
                        break;
                    }
                }
                let _ = in_tx.send(("bye".into(), String::new()));
            });
            // The subscription goes out once the pipeline exists, so the offer finds it.
            subscribe = Some((c, streamer));
            out
        }
    };
    let clock = HostClock::default();

    let viewer = match Viewer::new(cfg, clock.clone(), link.clone()) {
        Ok(v) => Arc::new(v),
        Err(e) => {
            eprintln!("viewer: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = viewer.start() {
        eprintln!("viewer: {e}");
        std::process::exit(1);
    }
    if let Some((client, streamer)) = subscribe.take() {
        client.send(ClientMsg::Subscribe { streamer });
    }

    {
        let (link, clock) = (link.clone(), clock.clone());
        std::thread::spawn(move || loop {
            link.send("ping", &clock.local_ns().to_string());
            std::thread::sleep(Duration::from_millis(200));
        });
    }
    let gone = Arc::new(AtomicBool::new(false));
    {
        let (viewer, link, clock, gone) = (viewer.clone(), link.clone(), clock.clone(), gone.clone());
        std::thread::spawn(move || {
            while let Ok((kind, payload)) = in_rx.recv() {
                if kind == "bye" {
                    break;
                }
                if never_answer && kind == "offer" {
                    continue;
                }
                viewer.on_message(&link, &clock, &kind, &payload);
            }
            gone.store(true, Ordering::SeqCst);
        });
    }

    let started = Instant::now();
    let st = &viewer.stats;
    let mut last_frames = 0;
    let mut last_presented = 0;
    let mut last_t = Instant::now();
    loop {
        let tick = Instant::now() + Duration::from_secs(report_secs);
        while Instant::now() < tick && !gone.load(Ordering::SeqCst) && !viewer.window_closed() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let frames = st.frames.load(Ordering::Relaxed);
        let dt = last_t.elapsed().as_secs_f64();
        let (iv, lat, pre, alat) = (
            st.frame_interval.take(),
            st.video_latency.take(),
            st.video_before_decoder.take(),
            st.audio_latency.take(),
        );
        println!(
            "viewer t={:>4}s | {:.1} fps | gap p99 {:.1} max {:.1} ms (>50 ms: {}) | video {:.1}/{:.1} ms (net {:.1}) | audio {:.1} ms | clock +-{:.1} ms",
            started.elapsed().as_secs(),
            (frames - last_frames) as f64 / dt,
            iv.ms(0.99),
            iv.ms(1.0),
            iv.count_over(50_000_000),
            lat.ms(0.5),
            lat.ms(0.99),
            pre.ms(0.5),
            alat.ms(0.5),
            clock.best_rtt_ns().map(|r| r as f64 / 2e6).unwrap_or(f64::NAN),
        );
        if let Some(r) = viewer.render_report() {
            println!(
                "viewer present | shown {:.1} fps | skipped total {} | Present max {:.1} ms | decoder lock held max {:.1} ms",
                (r.presented - last_presented) as f64 / dt,
                r.skipped,
                r.max_present_ms,
                r.max_lock_ms,
            );
            last_presented = r.presented;
        }
        last_frames = frames;
        last_t = Instant::now();
        if gone.load(Ordering::SeqCst) || viewer.window_closed() || (seconds > 0 && started.elapsed().as_secs() >= seconds) {
            break;
        }
    }
    link.send("bye", "");
    let _ = viewer.pipeline.set_state(gst::State::Null);
    println!("viewer: stopped");
}
