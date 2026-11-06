//! Desktop app, Windows first. The webview shows rooms, people and controls; video goes to
//! each viewer's own native window and swapchain, never through the webview (decision D4).
//! The UI polls `snapshot` once a second; a sampler thread keeps the numbers it shows.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sampler;

use media::config::{Codec, HostConfig, IceConfig, Quality, Source, VideoMode, ViewerConfig};
use media::session::{Event, Session};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct App {
    session: Mutex<Option<Arc<Session>>>,
    /// Last error worth showing, cleared when read.
    notice: Mutex<Option<String>>,
    sampler: sampler::Sampler,
}

#[derive(Serialize)]
struct PeerView {
    id: u64,
    name: String,
    streaming: bool,
    me: bool,
    watching: bool,
}

#[derive(Serialize)]
struct Snapshot {
    connected: bool,
    room: String,
    me: u64,
    peers: Vec<PeerView>,
    watches: Vec<sampler::WatchStats>,
    live: Option<sampler::LiveStats>,
    notice: Option<String>,
}

#[tauri::command]
fn snapshot(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    let s = app.session.lock().unwrap().clone();
    let notice = app.notice.lock().unwrap().take();
    let Some(s) = s else {
        return Snapshot { connected: false, room: String::new(), me: 0, peers: vec![], watches: vec![], live: None, notice };
    };
    let watching: Vec<u64> = s.watching().iter().map(|w| w.streamer).collect();
    let peers = s
        .peers()
        .into_iter()
        .map(|p| PeerView { me: p.id == s.id, watching: watching.contains(&p.id), id: p.id, name: p.name, streaming: p.streaming })
        .collect();
    Snapshot {
        connected: true,
        room: s.room.clone(),
        me: s.id,
        peers,
        watches: app.sampler.watches(),
        live: app.sampler.live(),
        notice,
    }
}

#[tauri::command]
fn connect(app: tauri::State<'_, Arc<App>>, server: String, room: String, name: String) -> Result<(), String> {
    do_connect(app.inner(), &server, &room, &name)
}

fn do_connect(app: &Arc<App>, server: &str, room: &str, name: &str) -> Result<(), String> {
    if let Some(old) = app.session.lock().unwrap().take() {
        old.leave();
    }
    let (s, events) = Session::join(server, room, name)?;
    *app.session.lock().unwrap() = Some(s.clone());
    app.sampler.attach(s.clone());
    let a = app.clone();
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            match e {
                Event::WatchEnded { reason, .. } => *a.notice.lock().unwrap() = Some(format!("watch_ended:{reason}")),
                Event::Error(m) => *a.notice.lock().unwrap() = Some(format!("error:{m}")),
                Event::Lost => {
                    *a.notice.lock().unwrap() = Some("lost".into());
                    *a.session.lock().unwrap() = None;
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn leave(app: tauri::State<'_, Arc<App>>) {
    if let Some(s) = app.session.lock().unwrap().take() {
        s.leave();
    }
}

#[derive(Serialize)]
struct Sources {
    monitors: Vec<media::platform::MonitorInfo>,
    windows: Vec<media::platform::WindowInfo>,
}

#[tauri::command]
fn sources() -> Sources {
    let mut monitors = media::platform::monitors();
    // Tests only: a moving test pattern instead of a real screen, so test runs never
    // capture the user's desktop. Its id is 0.
    if dev_sources() {
        monitors = vec![media::platform::MonitorInfo { handle: 0, name: "Test pattern".into(), width: 1920, height: 1080, primary: true }];
    }
    // Test runs show test entries only, never the user's own window titles.
    let windows = if dev_sources() {
        vec![media::platform::WindowInfo { hwnd: 0, title: "Test game window".into(), pid: 0, exe: "game.exe".into(), minimized: false }]
    } else {
        media::platform::windows()
    };
    Sources { monitors, windows }
}

#[tauri::command]
fn test_open_golive() -> bool {
    std::env::var_os("STREAMER_APP_OPEN_GOLIVE").is_some()
}

#[tauri::command]
fn forced_language() -> Option<String> {
    std::env::var("STREAMER_APP_LANG").ok()
}

fn dev_sources() -> bool {
    std::env::var_os("STREAMER_APP_DEV_SOURCES").is_some()
}

#[derive(Deserialize)]
struct GoLive {
    /// "monitor" or "window".
    kind: String,
    /// HMONITOR or HWND.
    id: u64,
    /// "system", "app" or "none".
    audio: String,
    audio_pid: Option<u32>,
    /// "1440p60" etc.
    mode: String,
    kbit: u32,
}

#[tauri::command]
fn go_live(app: tauri::State<'_, Arc<App>>, req: GoLive) -> Result<(), String> {
    do_go_live(app.inner(), req)
}

fn do_go_live(app: &Arc<App>, req: GoLive) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    let source = match req.kind.as_str() {
        "window" => Source::Window { hwnd: req.id },
        _ if req.id == 0 && dev_sources() => Source::Bars,
        _ => Source::Monitor { handle: req.id },
    };
    let cfg = HostConfig {
        mode: VideoMode::parse(&req.mode)?,
        kbit: req.kbit,
        codec: Codec::H265,
        keyframe_secs: 2,
        audio: req.audio != "none",
        audio_pid: if req.audio == "app" { req.audio_pid } else { None },
        source,
        ice: IceConfig::default(),
    };
    s.start_streaming(cfg)?;
    Ok(())
}

#[tauri::command]
fn end_live(app: tauri::State<'_, Arc<App>>) {
    if let Some(s) = app.session.lock().unwrap().clone() {
        s.stop_streaming();
    }
}

#[tauri::command]
fn watch(app: tauri::State<'_, Arc<App>>, streamer: u64, quality: Option<String>) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    s.watch(streamer, ViewerConfig::default(), quality)?;
    Ok(())
}

#[tauri::command]
fn unwatch(app: tauri::State<'_, Arc<App>>, streamer: u64) {
    if let Some(s) = app.session.lock().unwrap().clone() {
        s.unwatch(streamer);
    }
}

#[tauri::command]
fn set_quality(app: tauri::State<'_, Arc<App>>, streamer: u64, quality: String) -> Result<(), String> {
    // Checked here so a typo shows in the UI instead of being dropped by the host.
    let any = Quality::new(VideoMode::parse("1440p60")?, 50000);
    Quality::parse_request(&quality, any)?;
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    s.set_watch_quality(streamer, &quality);
    Ok(())
}

/// Tests only, so a run needs no clicks:
/// STREAMER_APP_AUTOJOIN="ws://...;room;name" joins at launch;
/// STREAMER_APP_AUTOLIVE="1080p60;8000" then streams the test pattern (needs dev sources);
/// STREAMER_APP_AUTOWATCH="streamer name;720p30 2000" then watches that streamer.
fn automate(app: &Arc<App>) {
    let Ok(join) = std::env::var("STREAMER_APP_AUTOJOIN") else { return };
    let app = app.clone();
    std::thread::spawn(move || {
        let p: Vec<&str> = join.split(';').collect();
        if p.len() != 3 {
            return;
        }
        if let Err(e) = do_connect(&app, p[0], p[1], p[2]) {
            *app.notice.lock().unwrap() = Some(format!("error:{e}"));
            return;
        }
        if let Ok(live) = std::env::var("STREAMER_APP_AUTOLIVE") {
            let (mode, kbit) = live.split_once(';').unwrap_or(("1080p60", "8000"));
            let req = GoLive { kind: "monitor".into(), id: 0, audio: "system".into(), audio_pid: None, mode: mode.into(), kbit: kbit.parse().unwrap_or(8000) };
            if let Err(e) = do_go_live(&app, req) {
                *app.notice.lock().unwrap() = Some(format!("error:{e}"));
            }
        }
        if let Ok(w) = std::env::var("STREAMER_APP_AUTOWATCH") {
            let (name, quality) = w.split_once(';').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((w.clone(), None));
            for _ in 0..60 {
                let s = app.session.lock().unwrap().clone();
                if let Some(s) = s {
                    if let Some(p) = s.peers().into_iter().find(|p| p.streaming && p.name == name && p.id != s.id) {
                        if let Err(e) = s.watch(p.id, ViewerConfig::default(), quality.clone()) {
                            *app.notice.lock().unwrap() = Some(format!("error:{e}"));
                        }
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    });
}

fn main() {
    media::init();
    let app = Arc::new(App::default());
    app.sampler.start();
    automate(&app);
    tauri::Builder::default()
        .manage(app)
        .invoke_handler(tauri::generate_handler![test_open_golive, forced_language, snapshot, connect, leave, sources, go_live, end_live, watch, unwatch, set_quality])
        .run(tauri::generate_context!())
        .expect("tauri app");
}
