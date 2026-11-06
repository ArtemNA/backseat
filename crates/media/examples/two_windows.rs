//! Gate for per-window state: one session watches two streamers, each in its own video
//! window. Stopping the first watch must close only its window; the second keeps playing.
//! Run against a local server with two hosts named "a" and "b" in room "tw" (test bars).

use media::config::ViewerConfig;
use media::session::Session;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn main() {
    let url = std::env::args().nth(1).unwrap_or("ws://127.0.0.1:7800/ws".into());
    media::init();
    let (s, _events) = Session::join(&url, "tw", "watcher").expect("join");
    let find = |name: &str| {
        let t = Instant::now();
        loop {
            if let Some(p) = s.peers().into_iter().find(|p| p.streaming && p.name == name) {
                return p.id;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "no streamer {name}");
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let (a, b) = (find("a"), find("b"));
    let wa = s.watch(a, ViewerConfig { audio: false, ..Default::default() }, None).expect("watch a");
    let wb = s.watch(b, ViewerConfig { audio: false, ..Default::default() }, None).expect("watch b");
    let fps = |w: &media::session::Watch, secs: u64| {
        let f0 = w.viewer.stats.frames.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(secs));
        (w.viewer.stats.frames.load(Ordering::Relaxed) - f0) as f64 / secs as f64
    };
    std::thread::sleep(Duration::from_secs(4));
    println!("both open      | a {:.1} fps | b {:.1} fps", fps(&wa, 3), fps(&wb, 3));
    let (win_a, win_b) = (wa.viewer.window.expect("window a"), wb.viewer.window.expect("window b"));
    s.unwatch(a);
    std::thread::sleep(Duration::from_secs(1));
    println!(
        "a stopped      | window a alive {} | window b alive {} | b closed flag {} | b {:.1} fps",
        win_a.alive(),
        win_b.alive(),
        wb.viewer.window_closed(),
        fps(&wb, 4)
    );
    s.leave();
    std::thread::sleep(Duration::from_secs(1));
    println!("left           | window b alive {}", win_b.alive());
}
