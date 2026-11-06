//! Desktop app, Windows first. Probe stage: one Tauri window whose button starts a viewer in
//! this same process; the video goes to the viewer's own native window and swapchain, never
//! through the webview (decision D4).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use media::config::ViewerConfig;
use media::session::{Event, Session};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Last status line, read by the UI once a second.
static STATUS: Mutex<String> = Mutex::new(String::new());
static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

fn status(s: String) {
    eprintln!("app: {s}");
    *STATUS.lock().unwrap() = s;
}

#[tauri::command]
fn stats() -> String {
    STATUS.lock().unwrap().clone()
}

/// Joins the room and watches one streamer; runs on its own thread.
#[tauri::command]
fn watch(server: String, room: String, streamer: String) -> Result<String, String> {
    let reply = format!("connecting to {streamer}…");
    std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || {
            if let Err(e) = run_viewer(&server, &room, &streamer) {
                status(format!("error: {e}"));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(reply)
}

fn run_viewer(server: &str, room: &str, streamer_name: &str) -> Result<(), String> {
    let (s, events) = Session::join(server, room, "app")?;
    *SESSION.lock().unwrap() = Some(s.clone());
    let deadline = Instant::now() + Duration::from_secs(30);
    let streamer = loop {
        if let Some(p) = s.peers().iter().find(|p| p.streaming && p.name == streamer_name) {
            break p.id;
        }
        if Instant::now() > deadline {
            return Err(format!("no streamer named {streamer_name}"));
        }
        let _ = events.recv_timeout(Duration::from_millis(500));
    };
    let w = s.watch(streamer, ViewerConfig::default(), None)?;
    let st = &w.viewer.stats;
    let (mut last_frames, mut last_t) = (0u64, Instant::now());
    loop {
        std::thread::sleep(Duration::from_secs(1));
        while let Ok(e) = events.try_recv() {
            if let Event::WatchEnded { reason, .. } = e {
                status(format!("stopped: {reason}"));
                return Ok(());
            }
        }
        let frames = st.frames.load(Ordering::Relaxed);
        let dt = last_t.elapsed().as_secs_f64();
        let iv = st.frame_interval.take();
        let lat = st.video_latency.take();
        let shown = w.viewer.render_report().map(|r| r.presented).unwrap_or(0);
        status(format!(
            "{:.1} fps | gap p99 {:.1} max {:.1} ms | video {:.1} ms | shown total {shown}",
            (frames - last_frames) as f64 / dt,
            iv.ms(0.99),
            iv.ms(1.0),
            lat.ms(0.5),
        ));
        last_frames = frames;
        last_t = Instant::now();
    }
}

fn main() {
    media::init();
    // Test only: STREAMER_APP_AUTOWATCH="ws://...;room;streamer" starts watching at launch.
    if let Ok(a) = std::env::var("STREAMER_APP_AUTOWATCH") {
        let p: Vec<String> = a.split(';').map(str::to_string).collect();
        if p.len() == 3 {
            let _ = watch(p[0].clone(), p[1].clone(), p[2].clone());
        }
    }
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![watch, stats])
        .run(tauri::generate_context!())
        .expect("tauri app");
}
