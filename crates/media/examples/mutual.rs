//! Gate for role tags: two sessions in one room both stream (test bars) and each watches the
//! other, headless. Both directions must deliver video. Run against a local server:
//! `mutual ws://127.0.0.1:7800/ws [seconds]`. STREAMER_DEBUG_NO_TAGS is not a thing: the
//! spoiled control is to run the old, untagged binaries, which cannot tell the two apart.

use media::config::{Codec, HostConfig, IceConfig, Source, VideoMode, ViewerConfig};
use media::session::Session;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn main() {
    let url = std::env::args().nth(1).unwrap_or("ws://127.0.0.1:7800/ws".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    media::init();
    let host_cfg = |kbit| HostConfig {
        mode: VideoMode::parse("720p30").unwrap(),
        kbit,
        codec: Codec::H264,
        keyframe_secs: 2,
        audio: true,
        source: Source::Bars,
        ice: IceConfig::default(),
    };
    let (a, _ea) = Session::join(&url, "mutual", "a").expect("join a");
    let (b, _eb) = Session::join(&url, "mutual", "b").expect("join b");
    a.start_streaming(host_cfg(3000)).expect("a streams");
    b.start_streaming(host_cfg(2000)).expect("b streams");
    std::thread::sleep(Duration::from_millis(500));
    let headless = ViewerConfig { headless: true, ..Default::default() };
    let wa = a.watch(b.id, headless.clone(), None).expect("a watches b");
    let wb = b.watch(a.id, headless, None).expect("b watches a");
    for t in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        if t % 5 == 0 {
            let ra = wa.viewer.inbound().map(|i| i.received).unwrap_or(0);
            let rb = wb.viewer.inbound().map(|i| i.received).unwrap_or(0);
            let ha = a.host().map(|h| h.viewer_count()).unwrap_or(0);
            let hb = b.host().map(|h| h.viewer_count()).unwrap_or(0);
            println!(
                "t={t:>3}s | a watching b: {ra} packets | b watching a: {rb} packets | viewers of a {ha}, of b {hb} | rtt-clock a {:?} b {:?}",
                wa.clock.best_rtt_ns().map(|r| r / 1000),
                wb.clock.best_rtt_ns().map(|r| r / 1000),
            );
        }
    }
    let _ = wa.viewer.stats.frames.load(Ordering::Relaxed);
    a.leave();
    b.leave();
    println!("done");
}
