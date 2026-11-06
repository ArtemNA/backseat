//! Viewer: joins a room on the signaling server and watches one streamer, or listens on
//! IP:PORT for a host that connects directly.

use gst::prelude::*;
use media::clock::HostClock;
use media::config::ViewerConfig;
use media::session::{Event, Session};
use media::signal::{self, Link, Outbox};
use media::viewer::Viewer;
use protocol::PeerInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: viewer (--server ws://HOST:PORT/ws [--room NAME] [--name NAME] [--watch STREAMER] | \
--listen 0.0.0.0:7700) [--stun stun://HOST:PORT|none] [--turn turn://USER:PASS@HOST:PORT] [--latency 40] [--wait-keyframe] [--no-audio] [--audio-nosync] [--quality '720p30 4000'] [--decoder NAME] \
[--seconds 0] [--report-secs 5]";

fn main() {
    let mut listen = "0.0.0.0:7700".to_string();
    let (mut server, mut room, mut name, mut watch) = (None::<String>, "default".to_string(), "viewer".to_string(), None::<String>);
    // Quality to ask the host for, as "720p30 4000"; the host never gives more than its own.
    let mut quality = None::<String>;
    let mut cfg = ViewerConfig::default();
    let (mut seconds, mut report_secs) = (0u64, 5u64);
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--wait-keyframe" => cfg.wait_keyframe = true,
            "--no-audio" => cfg.audio = false,
            "--audio-nosync" => cfg.audio_sync = false,
            // Test only: subscribes but never answers the offer, like a stuck peer.
            "--debug-never-answer" => cfg.debug_never_answer = true,
            // Test only: ignore the host's ICE candidates, so no path can be found.
            "--debug-no-ice" => cfg.debug_no_ice = true,
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
                    "--stun" => {
                        cfg.ice.stun = (val != "none").then(|| val.clone());
                        true
                    }
                    "--turn" => {
                        cfg.ice.turn = Some(val.clone());
                        true
                    }
                    "--latency" => val.parse().map(|v| cfg.latency_ms = v).is_ok(),
                    // Test only: drop this percentage of the host's RTP packets.
                    "--debug-loss" => val.parse().map(|v| cfg.impair.loss_pct = v).is_ok(),
                    // Test only: squeeze the host's traffic through a link of this many kbit/s.
                    "--debug-cap-kbit" => val.parse().map(|v| cfg.impair.cap_kbit = v).is_ok(),
                    // Test only: add this one-way delay to the host's traffic, in ms.
                    "--debug-delay" => val.parse().map(|v| cfg.impair.delay_ms = v).is_ok(),
                    "--quality" => {
                        quality = Some(val.clone());
                        true
                    }
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
    let gone = Arc::new(AtomicBool::new(false));
    // Asks the host for another quality, whichever way we reach it.
    let ask: Arc<dyn Fn(&str) + Send + Sync>;
    let mut session = None;
    let (viewer, clock, link): (Arc<Viewer>, HostClock, Option<Arc<dyn Outbox>>) = match &server {
        None => {
            let listener = std::net::TcpListener::bind(&listen).unwrap_or_else(|e| {
                eprintln!("viewer: cannot listen on {listen}: {e}");
                std::process::exit(1);
            });
            println!("viewer: waiting for the host on {listen}");
            let (stream, from) = listener.accept().expect("accept");
            println!("viewer: host connected from {}", from.ip());
            let (link, mut reader) = Link::new(stream);
            let link: Arc<dyn Outbox> = Arc::new(link);
            let clock = HostClock::default();
            let viewer = Arc::new(Viewer::new(cfg, clock.clone(), link.clone()).unwrap_or_else(|e| {
                eprintln!("viewer: {e}");
                std::process::exit(1);
            }));
            if let Err(e) = viewer.start() {
                eprintln!("viewer: {e}");
                std::process::exit(1);
            }
            let (v, l, c, g) = (viewer.clone(), link.clone(), clock.clone(), gone.clone());
            std::thread::spawn(move || {
                while let Some((kind, payload)) = signal::recv(&mut reader) {
                    if kind == "bye" {
                        break;
                    }
                    v.on_message(&l, &c, &kind, &payload);
                }
                g.store(true, Ordering::SeqCst);
            });
            {
                let (l, c) = (link.clone(), clock.clone());
                std::thread::spawn(move || loop {
                    l.send("ping", &c.local_ns().to_string());
                    std::thread::sleep(Duration::from_millis(200));
                });
            }
            let l = link.clone();
            ask = Arc::new(move |q| l.send(media::host::QUALITY_MSG, q));
            // Direct mode has no subscription: ask once the link is up.
            if let Some(q) = &quality {
                ask(q);
            }
            (viewer, clock, Some(link))
        }
        Some(url) => {
            let (s, events) = Session::join(url, &room, &name).unwrap_or_else(|e| {
                eprintln!("viewer: {e}");
                std::process::exit(1);
            });
            println!("viewer: joined room {room} as {name} (id {})", s.id);
            let pick = |p: &PeerInfo| p.id != s.id && p.streaming && watch.as_ref().is_none_or(|w| *w == p.name);
            let mut streamer = s.peers().iter().find(|p| pick(p)).map(|p| p.id);
            while streamer.is_none() {
                println!("viewer: waiting for a streamer{}", watch.as_ref().map(|w| format!(" named {w}")).unwrap_or_default());
                match events.recv() {
                    Ok(Event::Peers(peers)) => streamer = peers.iter().find(|p| pick(p)).map(|p| p.id),
                    Ok(Event::Lost) | Err(_) => {
                        eprintln!("viewer: lost the signaling server");
                        std::process::exit(1);
                    }
                    Ok(_) => {}
                }
            }
            let streamer = streamer.unwrap();
            println!("viewer: watching peer {streamer}");
            // The quality rides on the subscription, so the stream starts at it.
            // STREAMER_DEBUG_QUALITY_LATE asks only after subscribing: the spoiled variant.
            let late = std::env::var_os("STREAMER_DEBUG_QUALITY_LATE").is_some();
            let w = s.watch(streamer, cfg, if late { None } else { quality.clone() }).unwrap_or_else(|e| {
                eprintln!("viewer: {e}");
                std::process::exit(1);
            });
            let s2 = s.clone();
            ask = Arc::new(move |q| s2.set_watch_quality(streamer, q));
            if late {
                if let Some(q) = &quality {
                    ask(q);
                }
            }
            let g = gone.clone();
            std::thread::spawn(move || {
                while let Ok(e) = events.recv() {
                    match e {
                        Event::WatchEnded { streamer: id, reason } if id == streamer => {
                            println!("viewer: stream ended: {reason}");
                            break;
                        }
                        Event::Error(m) => eprintln!("viewer: server error: {m}"),
                        Event::Lost => {
                            eprintln!("viewer: lost the signaling server");
                            break;
                        }
                        _ => {}
                    }
                }
                g.store(true, Ordering::SeqCst);
            });
            let (viewer, clock) = (w.viewer.clone(), w.clock.clone());
            session = Some((s, streamer));
            (viewer, clock, None)
        }
    };
    // Test only: STREAMER_DEBUG_QUALITY_CYCLE="SECS;Q1;Q2;..." asks for each quality in turn.
    if let Ok(cycle) = std::env::var("STREAMER_DEBUG_QUALITY_CYCLE") {
        let mut parts = cycle.split(';');
        let secs: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(5);
        let list: Vec<String> = parts.map(str::to_string).collect();
        let ask = ask.clone();
        std::thread::spawn(move || {
            for q in list.iter().cycle() {
                std::thread::sleep(Duration::from_secs(secs));
                println!("viewer: asking for {q}");
                ask(q);
            }
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
        if let Some(i) = viewer.inbound() {
            let (dropped, forwarded, overflow, max_queue_ms) = viewer
                .relay
                .as_ref()
                .map(|r| {
                    (
                        r.dropped.load(Ordering::Relaxed),
                        r.forwarded.load(Ordering::Relaxed),
                        r.overflow.load(Ordering::Relaxed),
                        r.max_queue_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
                    )
                })
                .unwrap_or((0, 0, 0, 0.0));
            println!(
                "viewer rtp | received {} lost {} repaired {} nack {} pli {} | test relay dropped {} of {} overflow {} queue max {:.0} ms",
                i.received,
                i.lost,
                i.repaired,
                i.nack,
                i.pli,
                dropped,
                dropped + forwarded,
                overflow,
                max_queue_ms,
            );
        }
        {
            let t = viewer.take_video_transit();
            let j = viewer.jitter_report();
            println!(
                "viewer jitter | arrival spread p50 {:.1} p99 {:.1} max {:.1} ms over {} packets | jitterbuffer late {} lost {} (since start) | lag max {:.0} ms, catch-up requests {}",
                t.spread_ms(0.5),
                t.spread_ms(0.99),
                t.spread_ms(1.0),
                t.len(),
                j.late,
                j.lost,
                viewer.lag.max_lag_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
                viewer.lag.requests.load(Ordering::Relaxed),
            );
        }
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
        if viewer.ice_failed.load(Ordering::SeqCst) {
            break;
        }
        if gone.load(Ordering::SeqCst) || viewer.window_closed() || (seconds > 0 && started.elapsed().as_secs() >= seconds) {
            break;
        }
    }
    match (&session, &link) {
        (Some((s, streamer)), _) => s.unwatch(*streamer),
        (None, Some(l)) => {
            l.send("bye", "");
            let _ = viewer.pipeline.set_state(gst::State::Null);
        }
        _ => {}
    }
    println!("viewer: stopped");
}
