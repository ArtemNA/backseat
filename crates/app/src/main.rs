//! Desktop app with the webview UI (Tauri). Everything apart from the window lives in
//! `backseat-core`; these are thin commands over it. The UI polls `snapshot` once a second.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use backseat_core::{self as core, sampler, update, App, GoLive, ShortCode, Snapshot, Sources, Thumb};
use std::sync::Arc;

type St<'a> = tauri::State<'a, Arc<App>>;

static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// Blocking work off the async runtime's threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn snapshot(app: St<'_>) -> Snapshot {
    core::snapshot(&app)
}

#[tauri::command]
async fn direct_invite(app: St<'_>, name: String) -> Result<String, String> {
    let a = app.inner().clone();
    blocking(move || core::direct_invite(&a, &name)).await?
}

#[tauri::command]
async fn direct_accept(app: St<'_>, text: String) -> Result<(), String> {
    core::direct_accept(app.inner(), &text)
}

#[tauri::command]
async fn direct_answer(app: St<'_>, text: String, name: String) -> Result<String, String> {
    let a = app.inner().clone();
    blocking(move || core::direct_answer(&a, &text, &name)).await?
}

/// The webview tells the language it shows, for text drawn natively (video window, tray).
#[tauri::command]
fn set_ui_language(lang: String) {
    *sampler::UI_LANG.lock().unwrap() = lang;
    tray_labels();
}

/// Closing the window hides it in the tray (true) or quits (false).
#[tauri::command]
fn set_close_to_tray(app: St<'_>, on: bool) {
    let on = on && std::env::var_os("STREAMER_DEBUG_NO_TRAY").is_none();
    app.close_to_tray.store(on, std::sync::atomic::Ordering::SeqCst);
}

#[tauri::command]
fn updates_enabled() -> bool {
    core::updates_enabled()
}

/// Tests only: STREAMER_APP_TEST_DIRECT="stream|watch NAME DIR" drives the direct panel;
/// the texts go through files in DIR instead of a messenger.
#[tauri::command]
fn test_direct() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_DIRECT").ok()
}

#[tauri::command]
fn test_put(dir: String, name: String, text: String) {
    if std::env::var_os("STREAMER_APP_TEST_DIRECT").is_some() {
        let _ = std::fs::write(std::path::Path::new(&dir).join(name), text);
    }
}

#[tauri::command]
fn test_get(dir: String, name: String) -> Option<String> {
    std::env::var_os("STREAMER_APP_TEST_DIRECT")?;
    std::fs::read_to_string(std::path::Path::new(&dir).join(name)).ok()
}

#[tauri::command]
async fn shortcode_put(text: String) -> Result<ShortCode, String> {
    blocking(move || core::shortcode_put(&text)).await?
}

#[tauri::command]
async fn shortcode_get(code: String) -> Result<String, String> {
    blocking(move || core::shortcode_get(&code)).await?
}

#[tauri::command]
fn state_save(json: String) -> Result<(), String> {
    core::state_save(&json)
}

/// Tests only: STREAMER_APP_TEST_STATE=fill|show (see app.js).
#[tauri::command]
fn test_state() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_STATE").ok()
}

#[tauri::command]
fn open_logs() -> Result<(), String> {
    core::open_logs()
}

/// Tests only: STREAMER_APP_TEST_PREFS, JSON merged into the saved settings at start.
#[tauri::command]
fn test_prefs() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_PREFS").ok()
}

/// Tests only: STREAMER_APP_TEST_SETTINGS=general|updates opens the settings at start.
#[tauri::command]
fn test_open_settings() -> Option<String> {
    std::env::var("STREAMER_APP_TEST_SETTINGS").ok()
}

#[tauri::command]
fn app_version() -> &'static str {
    core::app_version()
}

#[tauri::command]
async fn check_update(app: St<'_>) -> Result<update::Release, String> {
    let a = app.inner().clone();
    blocking(move || core::check_update(&a)).await?
}

/// Downloads the release found by `check_update`, checks it, starts the installer and quits.
#[tauri::command]
async fn install_update(app: St<'_>) -> Result<(), String> {
    let a = app.inner().clone();
    if blocking(move || core::install_update(&a)).await?? {
        if let Some(h) = APP_HANDLE.get() {
            h.exit(0);
        }
    }
    Ok(())
}

static TRAY_ITEMS: std::sync::OnceLock<(tauri::menu::MenuItem<tauri::Wry>, tauri::menu::MenuItem<tauri::Wry>)> = std::sync::OnceLock::new();

fn tray_labels() {
    if let Some((show, quit)) = TRAY_ITEMS.get() {
        let (s, q) = core::tray_text();
        let _ = show.set_text(s);
        let _ = quit.set_text(q);
    }
}

/// The tray icon: a click opens the window; the menu opens it or quits.
fn make_tray(a: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let (s, q) = core::tray_text();
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
            core::leave_for_good(&h.state::<Arc<App>>(), std::time::Duration::from_millis(500));
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

#[tauri::command]
async fn codecs() -> Vec<String> {
    core::codecs()
}

#[tauri::command]
fn built_in_server() -> Option<String> {
    core::built_in().map(String::from)
}

/// Async: runs off the main thread. On it, creating or closing a video window inside the
/// main window waited for the main thread itself, and the app froze on Watch.
#[tauri::command]
async fn connect(app: St<'_>, server: String, room: String, name: String, title: Option<String>) -> Result<(), String> {
    core::connect(app.inner(), &server, &room, &name, title.as_deref())
}

#[tauri::command]
async fn leave(app: St<'_>) -> Result<(), String> {
    core::leave(&app);
    Ok(())
}

#[tauri::command]
fn sources() -> Sources {
    core::sources()
}

#[tauri::command]
fn video_window_do(app: St<'_>, streamer: u64, action: String) -> Result<(), String> {
    core::video_window_do(&app, streamer, &action)
}

/// Where a stream's video goes in the main window, in CSS pixels of the webview (the
/// webview fills the client area); None: not on screen.
#[tauri::command]
fn video_place(app: St<'_>, window: tauri::WebviewWindow, streamer: u64, rect: Option<(f64, f64, f64, f64)>) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let px = |v: f64| (v * scale).round() as i32;
    core::video_place(&app, streamer, rect.map(|(x, y, wd, ht)| (px(x), px(y), px(wd), px(ht))));
}

#[tauri::command]
fn set_muted(app: St<'_>, streamer: u64, muted: bool) -> Result<(), String> {
    core::set_muted(&app, streamer, muted)
}

/// Hides all video inside the main window while a dialog is open: the webview cannot
/// draw over it.
#[tauri::command]
fn videos_hidden(app: St<'_>, hidden: bool) {
    core::videos_hidden(&app, hidden)
}

/// Pictures for the source picker. Async, so the work runs off the UI thread.
#[tauri::command]
async fn thumbnails(kind: String, ids: Vec<u64>) -> Vec<Thumb> {
    core::thumbnails(&kind, ids)
}

#[tauri::command]
fn test_open_golive() -> Option<String> {
    // "start" also presses Go live once the dialog is open.
    std::env::var("STREAMER_APP_OPEN_GOLIVE").ok()
}

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
    if std::env::var_os("STREAMER_APP_TEST_FORM").is_some() || std::env::var_os("STREAMER_APP_OPEN_GOLIVE").is_some() || std::env::var_os("STREAMER_APP_TEST_DIRECT").is_some() || std::env::var_os("STREAMER_APP_TEST_STATE").is_some() {
        eprintln!("test: {msg}");
    }
}

/// Whether a test joins a room by itself (then the page does not open the last room).
#[tauri::command]
fn test_autojoin() -> bool {
    std::env::var_os("STREAMER_APP_AUTOJOIN").is_some() || std::env::var_os("STREAMER_APP_TEST_FORM").is_some()
}

/// Tests only: STREAMER_APP_TEST_ROOMS="room B" makes the page go to room B after a
/// while, then back to the first room through its button in the rail, then open the
/// menu of room B.
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

#[tauri::command]
async fn go_live(app: St<'_>, req: GoLive) -> Result<(), String> {
    core::go_live(app.inner(), req)
}

/// Changes the running stream (source, quality, which program's sound); viewers stay.
#[tauri::command]
async fn update_live(app: St<'_>, req: GoLive) -> Result<(), String> {
    let a = app.inner().clone();
    blocking(move || core::update_live(&a, req)).await?
}

#[tauri::command]
async fn end_live(app: St<'_>) -> Result<(), String> {
    core::end_live(&app);
    Ok(())
}

/// Async: runs off the main thread. On it, creating or closing a video window inside the
/// main window waited for the main thread itself, and the app froze on Watch.
#[tauri::command]
async fn watch(app: St<'_>, streamer: u64, quality: Option<String>) -> Result<(), String> {
    if std::env::var_os("STREAMER_DEBUG_WATCH_ON_MAIN").is_some() {
        // Spoiled variant for the gate: do the work on the main thread, as before.
        let (tx, rx) = std::sync::mpsc::channel();
        let a = app.inner().clone();
        let main = APP_HANDLE.get().cloned().ok_or("no app handle")?;
        main.run_on_main_thread(move || {
            let _ = tx.send(core::watch(&a, streamer, quality));
        })
        .map_err(|e| e.to_string())?;
        return rx.recv().map_err(|e| e.to_string())?;
    }
    core::watch(&app, streamer, quality)
}

#[tauri::command]
async fn unwatch(app: St<'_>, streamer: u64) -> Result<(), String> {
    core::unwatch(&app, streamer);
    Ok(())
}

#[tauri::command]
fn set_quality(app: St<'_>, streamer: u64, quality: String) -> Result<(), String> {
    core::set_quality(&app, streamer, &quality)
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
    if core::testing() && std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        // Test runs keep their saved rooms and settings out of the real app's profile.
        // Runs before any other thread starts.
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", std::env::temp_dir().join("backseat-test-webview"));
    }
    let _ = core::ON_MAIN.set(Box::new(|f| {
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.run_on_main_thread(f);
        }
    }));
    let Some(app) = core::start(|_, _| {
        // The window may not exist yet in the first moments; tauri queues nothing, so
        // try a few times.
        for _ in 0..10 {
            if APP_HANDLE.get().is_some() {
                show_main_window();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }) else {
        return;
    };
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
        .invoke_handler(tauri::generate_handler![state_save, test_state, shortcode_put, shortcode_get, open_logs, test_direct, test_put, test_get, direct_invite, direct_accept, direct_answer, test_prefs, test_open_settings, update_live, updates_enabled, set_close_to_tray, app_version, check_update, install_update, codecs, set_muted, test_form, test_report, test_click_watch, test_autojoin, test_rooms, video_place, videos_hidden, set_ui_language, built_in_server, test_ui_stress, test_upload_limit, video_window_do, thumbnails, test_open_golive, forced_language, snapshot, connect, leave, sources, go_live, end_live, watch, unwatch, set_quality])
        .build(tauri::generate_context!())
        .expect("tauri app")
        .run(|h, e| {
            // Any way out (the tray's Quit, closing with the tray off) tells the server first.
            if let tauri::RunEvent::Exit = e {
                use tauri::Manager;
                core::leave_for_good(&h.state::<Arc<App>>(), std::time::Duration::from_millis(500));
            }
        });
}
