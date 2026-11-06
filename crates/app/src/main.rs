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

impl App {
    /// A message for the UI, also written to the log.
    fn notify(&self, msg: String) {
        eprintln!("notice: {msg}");
        *self.notice.lock().unwrap() = Some(msg);
    }
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

/// The signaling server fixed at build time (STREAMER_SERVER_URL when compiling), if any.
/// Such a build has no server field and ignores any other address.
const BUILT_IN_SERVER: Option<&str> = option_env!("STREAMER_SERVER_URL");

fn built_in() -> Option<&'static str> {
    BUILT_IN_SERVER.filter(|s| !s.is_empty())
}

/// The webview tells the language it shows, for text drawn natively (video window).
#[tauri::command]
fn set_ui_language(lang: String) {
    *sampler::UI_LANG.lock().unwrap() = lang;
}

#[tauri::command]
fn built_in_server() -> Option<String> {
    built_in().map(String::from)
}

#[tauri::command]
fn connect(app: tauri::State<'_, Arc<App>>, server: String, room: String, name: String) -> Result<(), String> {
    let server = built_in().map(String::from).unwrap_or(server);
    do_connect(app.inner(), &server, &room, &name).inspect_err(|e| eprintln!("connect failed: {e}"))
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
                Event::WatchEnded { reason, .. } => a.notify(format!("watch_ended:{reason}")),
                Event::Error(m) => a.notify(format!("error:{m}")),
                Event::Lost => {
                    a.notify("lost".into());
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

fn video_window(app: &App, streamer: u64) -> Option<media::present::Window> {
    let s = app.session.lock().unwrap().clone()?;
    s.watching().into_iter().find(|w| w.streamer == streamer)?.viewer.window
}

/// What to do with a stream's video window: "show", "fullscreen" (on), "windowed",
/// "toggle_fullscreen", "topmost" (on) or "not_topmost".
#[tauri::command]
fn video_window_do(app: tauri::State<'_, Arc<App>>, streamer: u64, action: String) -> Result<(), String> {
    let w = video_window(&app, streamer).ok_or("no video window for this stream")?;
    match action.as_str() {
        "show" => w.show(),
        "fullscreen" => w.set_fullscreen(true),
        "windowed" => w.set_fullscreen(false),
        "toggle_fullscreen" => w.toggle_fullscreen(),
        "topmost" => w.set_topmost(true),
        "not_topmost" => w.set_topmost(false),
        "overlay" => w.set_overlay(true),
        "no_overlay" => w.set_overlay(false),
        _ => return Err(format!("unknown window action {action}")),
    }
    Ok(())
}

#[derive(Serialize)]
struct Thumb {
    id: u64,
    /// A `data:` URL, None when there is nothing to show (a minimized window).
    image: Option<String>,
    icon: Option<String>,
}

/// Pictures for the source picker. Async, so the work runs off the UI thread.
#[tauri::command]
async fn thumbnails(kind: String, ids: Vec<u64>) -> Vec<Thumb> {
    let t0 = std::time::Instant::now();
    // Diagnosis only: STREAMER_APP_THUMBS=off|monitors|windows limits which pictures are taken.
    let only = std::env::var("STREAMER_APP_THUMBS").unwrap_or_default();
    let skip = match only.as_str() {
        "off" => true,
        "monitors" => kind == "window",
        "windows" => kind != "window",
        _ => false,
    };
    if skip {
        return ids.into_iter().map(|id| Thumb { id, image: None, icon: None }).collect();
    }
    let out: Vec<Thumb> = ids
        .into_iter()
        .map(|id| {
            if id == 0 && dev_sources() {
                return Thumb { id, image: Some(media::thumbs::test_pattern(256, 144)), icon: None };
            }
            match kind.as_str() {
                "window" => Thumb { id, image: media::thumbs::window(id, 256, 144), icon: media::thumbs::icon(id, 20) },
                _ => Thumb { id, image: media::thumbs::monitor(id, 256, 144), icon: None },
            }
        })
        .collect();
    if std::env::var_os("STREAMER_APP_DEBUG_THUMBS").is_some() {
        let bytes: usize = out.iter().map(|t| t.image.as_ref().map_or(0, |s| s.len()) + t.icon.as_ref().map_or(0, |s| s.len())).sum();
        eprintln!("thumbs: {} {kind} in {} ms, {} KB", out.len(), t0.elapsed().as_millis(), bytes / 1024);
    }
    out
}

#[tauri::command]
fn test_open_golive() -> bool {
    std::env::var_os("STREAMER_APP_OPEN_GOLIVE").is_some()
}

/// Tests only: STREAMER_APP_UI_STRESS keeps the UI busy (dialog with pictures, language).
/// "screen" or "window": which tab the busy UI opens; None when not busy.
#[tauri::command]
fn test_ui_stress() -> Option<String> {
    let v = std::env::var("STREAMER_APP_UI_STRESS").ok()?;
    Some(if v == "windows" { "window".into() } else { "screen".into() })
}

/// Tests only: STREAMER_APP_UPLOAD_LIMIT sets the upload limit (Mbit/s) for screenshots.
#[tauri::command]
fn test_upload_limit() -> Option<f64> {
    std::env::var("STREAMER_APP_UPLOAD_LIMIT").ok()?.parse().ok()
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
    /// "h264" or "h265"; H.265 when missing.
    #[serde(default)]
    codec: Option<String>,
}

#[tauri::command]
fn go_live(app: tauri::State<'_, Arc<App>>, req: GoLive) -> Result<(), String> {
    do_go_live(app.inner(), req).inspect_err(|e| eprintln!("go_live failed: {e}"))
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
        codec: req.codec.as_deref().map(Codec::parse).transpose()?.unwrap_or(Codec::H265),
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
    s.watch(streamer, ViewerConfig::default(), quality).inspect_err(|e| eprintln!("watch failed: {e}"))?;
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
/// a third field "monitor" streams the primary monitor, a fourth sets the codec
/// ("1080p60;8000;bars;h264");
/// STREAMER_APP_AUTOWATCH="streamer name;720p30 2000" then watches that streamer.
fn automate(app: &Arc<App>) {
    let Ok(join) = std::env::var("STREAMER_APP_AUTOJOIN") else { return };
    let app = app.clone();
    std::thread::spawn(move || {
        let p: Vec<&str> = join.split(';').collect();
        if p.len() != 3 {
            return;
        }
        // A build with a fixed server goes there, like the join screen does.
        if let Err(e) = do_connect(&app, built_in().unwrap_or(p[0]), p[1], p[2]) {
            app.notify(format!("error:{e}"));
            return;
        }
        if let Ok(live) = std::env::var("STREAMER_APP_AUTOLIVE") {
            let p: Vec<&str> = live.split(';').collect();
            let (mode, kbit) = (p.first().copied().unwrap_or("1080p60"), p.get(1).copied().unwrap_or("8000"));
            // A third field "monitor" streams the primary real monitor instead of the test pattern.
            let id = if p.get(2) == Some(&"monitor") {
                media::platform::monitors().into_iter().find(|m| m.primary).map(|m| m.handle).unwrap_or(0)
            } else {
                0
            };
            let req = GoLive { kind: "monitor".into(), id, audio: "system".into(), audio_pid: None, mode: mode.into(), kbit: kbit.parse().unwrap_or(8000), codec: p.get(3).map(|c| c.to_string()) };
            if let Err(e) = do_go_live(&app, req) {
                app.notify(format!("error:{e}"));
            }
        }
        if let Ok(w) = std::env::var("STREAMER_APP_AUTOWATCH") {
            // "name;quality", several separated by '|'.
            let mut todo: Vec<(String, Option<String>)> = w
                .split('|')
                .map(|one| one.split_once(';').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((one.to_string(), None)))
                .collect();
            for _ in 0..60 {
                let s = app.session.lock().unwrap().clone();
                if let Some(s) = s {
                    todo.retain(|(name, quality)| {
                        let Some(p) = s.peers().into_iter().find(|p| p.streaming && &p.name == name && p.id != s.id) else { return true };
                        match s.watch(p.id, ViewerConfig::default(), quality.clone()) {
                            Ok(w) => test_window_actions(w.viewer.window),
                            Err(e) => app.notify(format!("error:{e}")),
                        }
                        false
                    });
                }
                if todo.is_empty() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    });
}

/// Tests only: STREAMER_APP_AUTOWINDOW=1 makes the video window fullscreen and topmost
/// after 3 s, and windowed again after 10 s.
fn test_window_actions(w: Option<media::present::Window>) {
    let (Some(w), Some(_)) = (w, std::env::var_os("STREAMER_APP_AUTOWINDOW")) else { return };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        w.set_fullscreen(true);
        w.set_topmost(true);
        std::thread::sleep(std::time::Duration::from_secs(7));
        w.set_fullscreen(false);
    });
}

fn main() {
    if let Some(p) = media::logfile::start() {
        eprintln!("log file: {}", p.display());
    }
    media::init();
    media::logfile::gst_warnings();
    let app = Arc::new(App::default());
    app.sampler.start();
    automate(&app);
    tauri::Builder::default()
        .manage(app)
        .invoke_handler(tauri::generate_handler![set_ui_language, built_in_server, test_ui_stress, test_upload_limit, video_window_do, thumbnails, test_open_golive, forced_language, snapshot, connect, leave, sources, go_live, end_live, watch, unwatch, set_quality])
        .run(tauri::generate_context!())
        .expect("tauri app");
}
