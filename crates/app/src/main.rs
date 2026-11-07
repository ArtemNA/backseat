//! Desktop app, Windows first. The webview shows rooms, people and controls; video goes to
//! each viewer's own native window and swapchain, never through the webview (decision D4).
//! The UI polls `snapshot` once a second; a sampler thread keeps the numbers it shows.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sampler;
mod update;

use media::config::{Codec, HostConfig, IceConfig, Quality, Source, VideoMode, ViewerConfig};
use media::session::{Event, Session};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct App {
    session: Mutex<Option<Arc<Session>>>,
    /// Last error worth showing, cleared when read.
    notice: Mutex<Option<String>>,
    /// An invite link (backseat://join/CODE) to open, from the command line or from a
    /// later start; cleared when read.
    link: Mutex<Option<String>>,
    sampler: sampler::Sampler,
    /// Closing the main window hides it in the tray (the default) instead of quitting.
    close_to_tray: std::sync::atomic::AtomicBool,
    /// The release found by the last check, for `install_update`.
    release: Mutex<Option<update::Release>>,
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
    /// The room's display name from the server.
    title: Option<String>,
    me: u64,
    peers: Vec<PeerView>,
    watches: Vec<sampler::WatchStats>,
    live: Option<sampler::LiveStats>,
    notice: Option<String>,
    link: Option<String>,
}

#[tauri::command]
fn snapshot(app: tauri::State<'_, Arc<App>>) -> Snapshot {
    let s = app.session.lock().unwrap().clone();
    let notice = app.notice.lock().unwrap().take();
    let link = app.link.lock().unwrap().take();
    let Some(s) = s else {
        return Snapshot { connected: false, room: String::new(), title: None, me: 0, peers: vec![], watches: vec![], live: None, notice, link };
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
        title: s.title.clone(),
        me: s.id,
        peers,
        watches: app.sampler.watches(),
        live: app.sampler.live(),
        notice,
        link,
    }
}

/// The signaling server fixed at build time (STREAMER_SERVER_URL when compiling), if any.
/// Such a build has no server field and ignores any other address.
/// For the spoiled variant of the click-watch gate only.
static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

const BUILT_IN_SERVER: Option<&str> = option_env!("STREAMER_SERVER_URL");

fn built_in() -> Option<&'static str> {
    BUILT_IN_SERVER.filter(|s| !s.is_empty())
}

/// The webview tells the language it shows, for text drawn natively (video window, tray).
#[tauri::command]
fn set_ui_language(lang: String) {
    *sampler::UI_LANG.lock().unwrap() = lang;
    tray_labels();
}

/// Closing the window hides it in the tray (true) or quits (false).
#[tauri::command]
fn set_close_to_tray(app: tauri::State<'_, Arc<App>>, on: bool) {
    let on = on && std::env::var_os("STREAMER_DEBUG_NO_TRAY").is_none();
    app.close_to_tray.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Test runs (STREAMER_APP_* variables) never look for updates, unless STREAMER_UPDATE_URL
/// points them at a test release.
#[tauri::command]
fn updates_enabled() -> bool {
    let on = std::env::var_os("STREAMER_UPDATE_URL").is_some() || !std::env::vars().any(|(k, _)| k.starts_with("STREAMER_APP_"));
    eprintln!("update: checks {}", if on { "on" } else { "off" });
    on
}

/// Tests only: STREAMER_APP_TEST_SETTINGS=general|updates opens the settings at start.
#[tauri::command]
fn test_open_settings() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_SETTINGS").ok()
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The latest release and whether it is newer than this app.
#[tauri::command]
async fn check_update(app: tauri::State<'_, Arc<App>>) -> Result<update::Release, String> {
    let r = tauri::async_runtime::spawn_blocking(update::check).await.map_err(|e| e.to_string())?;
    match &r {
        Ok(r) => eprintln!("update: this {} latest {} newer {}", r.current, r.latest, r.newer),
        Err(e) => eprintln!("update: check failed: {e}"),
    }
    let r = r?;
    *app.release.lock().unwrap() = Some(r.clone());
    Ok(r)
}

/// Downloads the release found by `check_update`, checks it, starts the installer and quits.
#[tauri::command]
async fn install_update(app: tauri::State<'_, Arc<App>>) -> Result<(), String> {
    let r = app.release.lock().unwrap().clone().filter(|r| r.newer).ok_or("no newer release")?;
    let path = tauri::async_runtime::spawn_blocking(move || update::download(&r)).await.map_err(|e| e.to_string())?.inspect_err(|e| eprintln!("update: {e}"))?;
    eprintln!("update: downloaded and checked {}", path.display());
    update::run_installer(&path)?;
    if std::env::var_os("STREAMER_UPDATE_DRY").is_none() {
        if let Some(s) = app.session.lock().unwrap().take() {
            s.leave();
        }
        if let Some(h) = APP_HANDLE.get() {
            h.exit(0);
        }
    }
    Ok(())
}

/// The tray menu's words in the UI language.
fn tray_text() -> (&'static str, &'static str) {
    match sampler::UI_LANG.lock().unwrap().as_str() {
        "uk" => ("Відкрити Backseat", "Вийти"),
        "ru" => ("Открыть Backseat", "Выйти"),
        _ => ("Open Backseat", "Quit"),
    }
}

static TRAY_ITEMS: std::sync::OnceLock<(tauri::menu::MenuItem<tauri::Wry>, tauri::menu::MenuItem<tauri::Wry>)> = std::sync::OnceLock::new();

fn tray_labels() {
    if let Some((show, quit)) = TRAY_ITEMS.get() {
        let (s, q) = tray_text();
        let _ = show.set_text(s);
        let _ = quit.set_text(q);
    }
}

/// The tray icon: a click opens the window; the menu opens it or quits.
fn make_tray(a: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let (s, q) = tray_text();
    let show = MenuItem::with_id(a, "show", s, true, None::<&str>)?;
    let quit = MenuItem::with_id(a, "quit", q, true, None::<&str>)?;
    let menu = Menu::with_items(a, &[&show, &quit])?;
    let _ = TRAY_ITEMS.set((show, quit));
    let mut tray = TrayIconBuilder::with_id("main").tooltip("Backseat").menu(&menu).show_menu_on_left_click(false);
    if let Some(icon) = a.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.on_menu_event(|h, e| match e.id.as_ref() {
        "show" => show_main_window(),
        "quit" => {
            use tauri::Manager;
            if let Some(s) = h.state::<Arc<App>>().session.lock().unwrap().take() {
                s.leave();
            }
            h.exit(0);
        }
        _ => {}
    })
    .on_tray_icon_event(|_, e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show_main_window();
        }
    })
    .build(a)?;
    Ok(())
}

/// Codecs this machine can stream (hardware encoders only), e.g. ["h265", "h264", "av1"].
#[tauri::command]
async fn codecs() -> Vec<String> {
    media::platform::encodable_codecs().into_iter().map(|c| c.short().to_string()).collect()
}

#[tauri::command]
fn built_in_server() -> Option<String> {
    built_in().map(String::from)
}

#[tauri::command]
/// Async: runs off the main thread. On it, creating or closing a video window inside the
/// main window waited for the main thread itself, and the app froze on Watch.
async fn connect(app: tauri::State<'_, Arc<App>>, server: String, room: String, name: String, title: Option<String>) -> Result<(), String> {
    let server = built_in().map(String::from).unwrap_or(server);
    do_connect(app.inner(), &server, &room, &name, title.as_deref()).inspect_err(|e| eprintln!("connect failed: {e}"))
}

/// `title` names the room if it is new (a room made here, with its invite code as `room`).
fn do_connect(app: &Arc<App>, server: &str, room: &str, name: &str, title: Option<&str>) -> Result<(), String> {
    // One join at a time. Connect runs off the main thread, so an invite link arriving twice
    // (the page opens the app, and the user also presses its button) joined twice and left
    // both connections alive: the same person twice in the room.
    static JOINING: Mutex<()> = Mutex::new(());
    let _one = JOINING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = app.session.lock().unwrap().take() {
        old.leave();
    }
    let (s, events) = Session::join_titled(server, room, name, title)?;
    // Whatever got in meanwhile leaves.
    if let Some(other) = app.session.lock().unwrap().replace(s.clone()) {
        other.leave();
    }
    app.sampler.attach(s.clone());
    let a = app.clone();
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            match e {
                // Leaving the room on purpose ends its watches; that is no news.
                Event::WatchEnded { reason, .. } if reason != "left the room" => a.notify(format!("watch_ended:{reason}")),
                Event::Error(m) => a.notify(format!("error:{m}")),
                Event::Lost => {
                    a.notify("lost".into());
                    // Stop the stream and the watches first: dropping them while their
                    // pipelines played crashed the app (0xC0000005) when Render's TLS
                    // connection broke during a stream. STREAMER_DEBUG_DROP_ON_LOST keeps
                    // the old drop, the spoiled variant for the gate.
                    let s = a.session.lock().unwrap().take();
                    if let Some(s) = s {
                        if std::env::var_os("STREAMER_DEBUG_DROP_ON_LOST").is_none() {
                            s.leave();
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

#[tauri::command]
async fn leave(app: tauri::State<'_, Arc<App>>) -> Result<(), String> {
    let s = app.session.lock().unwrap().take();
    if let Some(s) = s {
        s.leave();
    }
    Ok(())
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
        vec![media::platform::WindowInfo { hwnd: 0, title: "Test game window".into(), pid: 0, exe: "game.exe".into(), minimized: false, width: 2560, height: 1440 }]
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
        "pop_out" => w.set_popped(true),
        "pop_in" => w.set_popped(false),
        "no_overlay" => w.set_overlay(false),
        _ => return Err(format!("unknown window action {action}")),
    }
    Ok(())
}

/// Where a stream's video goes in the main window, in CSS pixels of the webview (the
/// webview fills the client area); None: not on screen.
#[tauri::command]
fn video_place(app: tauri::State<'_, Arc<App>>, window: tauri::WebviewWindow, streamer: u64, rect: Option<(f64, f64, f64, f64)>) {
    if std::env::var_os("STREAMER_DEBUG_PLACE").is_some() {
        eprintln!("video_place {streamer} {rect:?}");
    }
    let Some(w) = video_window(&app, streamer) else { return };
    let scale = window.scale_factor().unwrap_or(1.0);
    let px = |v: f64| (v * scale).round() as i32;
    w.place(rect.map(|(x, y, wd, ht)| (px(x), px(y), px(wd), px(ht))));
}

/// Hides all video inside the main window while a dialog is open: the webview cannot
/// draw over it.
/// Sound of one watched stream off or on.
#[tauri::command]
fn set_muted(app: tauri::State<'_, Arc<App>>, streamer: u64, muted: bool) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    let w = s.watching().into_iter().find(|w| w.streamer == streamer).ok_or("not watching this stream")?;
    w.viewer.set_muted(muted);
    Ok(())
}

#[tauri::command]
fn videos_hidden(app: tauri::State<'_, Arc<App>>, hidden: bool) {
    let Some(s) = app.session.lock().unwrap().clone() else { return };
    for w in s.watching() {
        if let Some(win) = w.viewer.window {
            win.set_hidden(hidden);
        }
    }
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
fn test_open_golive() -> Option<String> {
    // "start" also presses Go live once the dialog is open.
    std::env::var("STREAMER_APP_OPEN_GOLIVE").ok()
}

/// Tests only: STREAMER_APP_TEST_ROOMS="room B" makes the page go to room B after a
/// while, then back to the first room through its button in the rail, then open the
/// menu of room B.
/// Tests only: STREAMER_APP_CLICK_WATCH="name" makes the page press that streamer's
/// Watch button, the way a user does (the command then runs like a click's would).
#[tauri::command]
fn test_click_watch() -> Option<String> {
    std::env::var("STREAMER_APP_CLICK_WATCH").ok()
}

/// Tests only: STREAMER_APP_TEST_FORM="create|Room title|Name" or "code|K7Q2-...|Name" fills
/// the join form and submits it; the page then reports the room it is in (test_report).
#[tauri::command]
fn test_form() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_FORM").ok()
}

#[tauri::command]
fn test_report(msg: String) {
    if std::env::var_os("STREAMER_APP_TEST_FORM").is_some() {
        eprintln!("test: {msg}");
    }
}

/// Whether a test joins a room by itself (then the page does not open the last room).
#[tauri::command]
fn test_autojoin() -> bool {
    std::env::var_os("STREAMER_APP_AUTOJOIN").is_some() || std::env::var_os("STREAMER_APP_TEST_FORM").is_some()
}

#[tauri::command]
fn test_rooms() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_ROOMS").ok()
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
async fn go_live(app: tauri::State<'_, Arc<App>>, req: GoLive) -> Result<(), String> {
    do_go_live(app.inner(), req).inspect_err(|e| eprintln!("go_live failed: {e}"))
}

fn live_config(req: &GoLive) -> Result<HostConfig, String> {
    let source = match req.kind.as_str() {
        "window" => Source::Window { hwnd: req.id },
        _ if req.id == 0 && dev_sources() => Source::Bars,
        _ => Source::Monitor { handle: req.id },
    };
    Ok(HostConfig {
        mode: VideoMode::parse(&req.mode)?,
        kbit: req.kbit,
        codec: req.codec.as_deref().map(Codec::parse).transpose()?.unwrap_or(Codec::H265),
        keyframe_secs: 2,
        audio: req.audio != "none",
        audio_pid: if req.audio == "app" { req.audio_pid } else { None },
        source,
        ice: IceConfig::default(),
    })
}

/// Changes the running stream (source, quality, which program's sound); viewers stay.
#[tauri::command]
async fn update_live(app: tauri::State<'_, Arc<App>>, req: GoLive) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    let h = s.host().ok_or("not live")?;
    let cfg = live_config(&req)?;
    tauri::async_runtime::spawn_blocking(move || h.reconfigure(cfg)).await.map_err(|e| e.to_string())?.inspect_err(|e| eprintln!("update_live failed: {e}"))
}

fn do_go_live(app: &Arc<App>, req: GoLive) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    s.start_streaming(live_config(&req)?)?;
    Ok(())
}

#[tauri::command]
async fn end_live(app: tauri::State<'_, Arc<App>>) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone();
    if let Some(s) = s {
        s.stop_streaming();
    }
    Ok(())
}

#[tauri::command]
/// Async: runs off the main thread. On it, creating or closing a video window inside the
/// main window waited for the main thread itself, and the app froze on Watch.
async fn watch(app: tauri::State<'_, Arc<App>>, streamer: u64, quality: Option<String>) -> Result<(), String> {
    if std::env::var_os("STREAMER_DEBUG_WATCH_ON_MAIN").is_some() {
        // Spoiled variant for the gate: do the work on the main thread, as before.
        let (tx, rx) = std::sync::mpsc::channel();
        let a = app.inner().clone();
        let q = quality.clone();
        let main = APP_HANDLE.get().cloned().ok_or("no app handle")?;
        main.run_on_main_thread(move || {
            let r = a.session.lock().unwrap().clone().ok_or("not in a room".to_string()).and_then(|s| s.watch(streamer, ViewerConfig::default(), q).map(|_| ()));
            let _ = tx.send(r);
        }).map_err(|e| e.to_string())?;
        return rx.recv().map_err(|e| e.to_string())?;
    }
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    s.watch(streamer, ViewerConfig::default(), quality).inspect_err(|e| eprintln!("watch failed: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn unwatch(app: tauri::State<'_, Arc<App>>, streamer: u64) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone();
    if let Some(s) = s {
        s.unwatch(streamer);
    }
    Ok(())
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
        // Video windows go inside the main window, which may not exist yet this early.
        for _ in 0..100 {
            if media::present::default_parent() != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // A build with a fixed server goes there, like the join screen does.
        if let Err(e) = do_connect(&app, built_in().unwrap_or(p[0]), p[1], p[2], None) {
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
                            Ok(w) => {
                                test_window_actions(w.viewer.window);
                                // Tests only: STREAMER_APP_TEST_MUTE mutes at +8 s and unmutes at +14 s.
                                if std::env::var_os("STREAMER_APP_TEST_MUTE").is_some() {
                                    let w = w.clone();
                                    std::thread::spawn(move || {
                                        std::thread::sleep(std::time::Duration::from_secs(8));
                                        w.viewer.set_muted(true);
                                        eprintln!("test: muted");
                                        std::thread::sleep(std::time::Duration::from_secs(6));
                                        w.viewer.set_muted(false);
                                        eprintln!("test: unmuted");
                                    });
                                }
                            }
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
    let (Some(w), Some(mode)) = (w, std::env::var("STREAMER_APP_AUTOWINDOW").ok()) else { return };
    // "embed": pop out at +6 s, back in at +12 s, fullscreen at +18 s, back at +24 s.
    // "fsloop": from +5 s, fullscreen on and off 30 times at uneven intervals while the main
    // thread keeps placing the video (as the UI does while it lays out), then stays embedded.
    if mode == "fsloop" {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let spam = {
                let (stop, w) = (stop.clone(), w);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Some(h) = APP_HANDLE.get() {
                            let _ = h.run_on_main_thread(move || {
                                let st = w.state();
                                let _ = st;
                                w.place_again();
                            });
                        }
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    }
                })
            };
            for i in 0..30u64 {
                w.toggle_fullscreen();
                std::thread::sleep(std::time::Duration::from_millis(150 + (i * 137) % 700));
            }
            w.set_fullscreen(false);
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = spam.join();
            eprintln!("test: fsloop done");
        });
        return;
    }
    if mode == "embed" {
        std::thread::spawn(move || {
            let step = |secs| std::thread::sleep(std::time::Duration::from_secs(secs));
            step(6);
            w.set_popped(true);
            step(6);
            w.set_popped(false);
            step(6);
            w.set_fullscreen(true);
            step(6);
            w.set_fullscreen(false);
        });
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        w.set_fullscreen(true);
        w.set_topmost(true);
        std::thread::sleep(std::time::Duration::from_secs(7));
        w.set_fullscreen(false);
    });
}

/// The invite link this start was opened with (backseat://join/CODE), if any.
fn link_arg() -> Option<String> {
    std::env::args().skip(1).find(|a| a.to_ascii_lowercase().starts_with("backseat://"))
}

/// Brings the main window to the front (a link arrived from a later start).
fn show_main_window() {
    use tauri::Manager;
    if let Some(w) = APP_HANDLE.get().and_then(|h| h.get_webview_window("main")) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn main() {
    // One app per user: a second start (an invite link clicked) hands over and leaves.
    // Test runs start several apps on purpose (STREAMER_APP_* variables), so not them.
    let testing = std::env::vars().any(|(k, _)| k.starts_with("STREAMER_APP_"));
    if testing && std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        // Test runs keep their saved rooms and settings out of the real app's profile.
        // Runs before any other thread starts.
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", std::env::temp_dir().join("backseat-test-webview"));
    }
    // The pipe is claimed before anything else, so of two starts at once exactly one runs.
    let claim = if testing {
        None
    } else {
        match media::instance::claim() {
            Some(c) => Some(c),
            None => {
                // Another start holds the pipe: hand it this one's link and leave. Even if it
                // does not answer in time, leave: a second app would join the room twice.
                if media::instance::send_to_running(&link_arg().unwrap_or_else(|| "show".into())) {
                    return;
                }
                // The other app may have just quit; then this one runs.
                match media::instance::claim() {
                    Some(c) => Some(c),
                    None => return,
                }
            }
        }
    };
    if let Some(p) = media::logfile::start() {
        eprintln!("log file: {}", p.display());
    }
    media::init();
    media::logfile::gst_warnings();
    let app = Arc::new(App::default());
    // STREAMER_DEBUG_NO_TRAY: closing quits (the spoiled variant for the tray gate).
    app.close_to_tray.store(std::env::var_os("STREAMER_DEBUG_NO_TRAY").is_none(), std::sync::atomic::Ordering::SeqCst);
    *app.link.lock().unwrap() = link_arg();
    if let Some(claim) = claim {
        let a = app.clone();
        media::instance::listen(claim, move |msg| {
            let msg = msg.trim().to_string();
            eprintln!("instance: a later start sent {}", if msg.starts_with("backseat://") { "a link" } else { "show" });
            if msg.starts_with("backseat://") {
                *a.link.lock().unwrap() = Some(msg);
            }
            // The window may not exist yet in the first moments; tauri queues nothing, so
            // try a few times.
            for _ in 0..10 {
                if APP_HANDLE.get().is_some() {
                    show_main_window();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });
    }
    // Try the encoders once in the background, so the go-live dialog has the answer ready.
    std::thread::spawn(|| {
        let _ = media::platform::encodable_codecs();
    });
    app.sampler.start();
    automate(&app);
    tauri::Builder::default()
        .setup(|a| {
            let _ = APP_HANDLE.set(a.handle().clone());
            // Video windows go inside the main window (the UI gives each a place).
            use tauri::Manager;
            if let Some(w) = a.get_webview_window("main") {
                if let Ok(h) = w.hwnd() {
                    media::present::set_default_parent(h.0 as isize);
                }
            }
            if let Err(e) = make_tray(a) {
                eprintln!("tray: {e}");
            }
            Ok(())
        })
        .on_window_event(|w, e| {
            // Closing the main window hides it in the tray; the tray's Quit ends the app.
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                use tauri::Manager;
                if w.label() == "main" && w.state::<Arc<App>>().close_to_tray.load(std::sync::atomic::Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = w.hide();
                }
            }
        })
        .manage(app)
        .invoke_handler(tauri::generate_handler![test_open_settings, update_live, updates_enabled, set_close_to_tray, app_version, check_update, install_update, codecs, set_muted, test_form, test_report, test_click_watch, test_autojoin, test_rooms, video_place, videos_hidden, set_ui_language, built_in_server, test_ui_stress, test_upload_limit, video_window_do, thumbnails, test_open_golive, forced_language, snapshot, connect, leave, sources, go_live, end_live, watch, unwatch, set_quality])
        .run(tauri::generate_context!())
        .expect("tauri app");
}
