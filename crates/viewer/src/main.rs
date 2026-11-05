//! Stage 1 viewer: listens on IP:PORT, the host connects, then plays its stream.

use gst::prelude::*;
use media::clock::HostClock;
use media::config::ViewerConfig;
use media::signal::{self, Link};
use media::viewer::Viewer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: viewer [--listen 0.0.0.0:7700] [--latency 40] [--wait-keyframe] [--no-audio] [--audio-nosync] \
[--decoder NAME] [--seconds 0] [--report-secs 5]";

fn main() {
    let mut listen = "0.0.0.0:7700".to_string();
    let mut cfg = ViewerConfig { latency_ms: 40, audio: true, wait_keyframe: false, decoder: None, audio_sync: true };
    let (mut seconds, mut report_secs) = (0u64, 5u64);
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--wait-keyframe" => cfg.wait_keyframe = true,
            "--no-audio" => cfg.audio = false,
            "--audio-nosync" => cfg.audio_sync = false,
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
    let listener = std::net::TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("viewer: cannot listen on {listen}: {e}");
        std::process::exit(1);
    });
    println!("viewer: waiting for the host on {listen}");
    let (stream, from) = listener.accept().expect("accept");
    println!("viewer: host connected from {}", from.ip());
    let (link, mut reader) = Link::new(stream);
    let link = Arc::new(link);
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
            while let Some((kind, payload)) = signal::recv(&mut reader) {
                if kind == "bye" {
                    break;
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
