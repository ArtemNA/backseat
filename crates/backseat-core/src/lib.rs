//! What the app does apart from drawing its window: rooms, the direct connection (beta),
//! streaming and watching, short codes, the state file and updates. The UI (Tauri or GPUI)
//! calls these and shows `snapshot` once a second; a sampler thread keeps the numbers.
//! Video goes to each viewer's own native window, never through the UI (decision D4).

pub mod sampler;
pub mod update;

use media::config::{Codec, HostConfig, IceConfig, Quality, Source, VideoMode, ViewerConfig};
use media::session::{Event, Session};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct App {
    pub session: Mutex<Option<Arc<Session>>>,
    /// Last error worth showing, cleared when read.
    pub notice: Mutex<Option<String>>,
    /// An invite link (backseat://join/CODE) to open, from the command line or from a
    /// later start; cleared when read.
    pub link: Mutex<Option<String>>,
    pub sampler: sampler::Sampler,
    /// Closing the main window hides it in the tray (the default) instead of quitting.
    pub close_to_tray: std::sync::atomic::AtomicBool,
    /// The release found by the last check, for `install_update`.
    pub release: Mutex<Option<update::Release>>,
    /// Key of the open room in the saved places (`resume_key`).
    joined: Mutex<Option<String>>,
    /// Rooms switched away from while watching or streaming there: still connected, their
    /// video hidden, until opened again. By `resume_key`. Only one room watches or streams
    /// at a time (`quiet_background`).
    background: Mutex<Vec<(String, Arc<Session>)>>,
    /// The direct connection (beta) and our name in it, while there is one.
    direct: Mutex<Option<(Arc<media::direct::Control>, String)>>,
}

/// Runs a closure on the UI's main thread; set by the UI at start (tests use it).
pub static ON_MAIN: std::sync::OnceLock<Box<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>> = std::sync::OnceLock::new();

/// Whether this is a test run (STREAMER_APP_* variables set).
pub fn testing() -> bool {
    std::env::vars().any(|(k, _)| k.starts_with("STREAMER_APP_"))
}

/// Places this app holds on servers, saved so a new start after a crash, a forced close or
/// an update takes its own place back instead of joining as a second copy of the same
/// person (the server keeps a place 20 s for a broken connection). Key: server, room, name.
fn resume_key(server: &str, room: &str, name: &str) -> String {
    format!("{server}|{room}|{name}")
}

fn resume_file() -> Option<std::path::PathBuf> {
    // Test runs start several apps on purpose; they would take each other's places. The
    // restart gate turns it on with STREAMER_APP_TEST_RESUME.
    let testing = std::env::vars().any(|(k, _)| k.starts_with("STREAMER_APP_") && k != "STREAMER_APP_TEST_RESUME");
    if testing && std::env::var_os("STREAMER_APP_TEST_RESUME").is_none() {
        return None;
    }
    let name = std::env::var("STREAMER_APP_TEST_RESUME").ok().filter(|v| v.ends_with(".json")).unwrap_or_else(|| "places.json".into());
    Some(std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Backseat").join(name))
}

fn saved_places() -> std::collections::HashMap<String, protocol::Resume> {
    resume_file().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Saves (Some) or forgets (None) the place for `key`.
fn save_place(key: &str, place: Option<protocol::Resume>) {
    let Some(path) = resume_file() else { return };
    let mut all = saved_places();
    match place {
        Some(r) => all.insert(key.to_string(), r),
        None => all.remove(key),
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, serde_json::to_string(&all).unwrap_or_default());
}

/// Leaves the open room for good: the server is told before we go on, and the saved
/// place is forgotten. `wait`: how long to wait for the server to be told (exiting).
pub fn leave_for_good(app: &App, wait: std::time::Duration) {
    let s = app.session.lock().unwrap().take();
    if let Some(s) = s {
        s.leave_and_wait(wait);
    }
    if let Some(k) = app.joined.lock().unwrap().take() {
        save_place(&k, None);
    }
    // A direct connection ends with the session; the other side sees it close.
    let direct = app.direct.lock().unwrap().take();
    if let Some((c, _)) = direct {
        // A moment for our Leave to reach them before the channel closes.
        if c.is_open() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        c.close();
    }
}

impl App {
    /// A message for the UI, also written to the log.
    pub fn notify(&self, msg: String) {
        eprintln!("notice: {msg}");
        *self.notice.lock().unwrap() = Some(msg);
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct PeerView {
    pub id: u64,
    pub name: String,
    pub streaming: bool,
    pub me: bool,
    pub watching: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Snapshot {
    pub connected: bool,
    pub room: String,
    /// The room's display name from the server.
    pub title: Option<String>,
    pub me: u64,
    pub peers: Vec<PeerView>,
    pub watches: Vec<sampler::WatchStats>,
    pub live: Option<sampler::LiveStats>,
    pub notice: Option<String>,
    pub link: Option<String>,
    /// A direct session (no server, beta): "connected" or "waiting".
    pub direct: Option<&'static str>,
    /// The connection to the server broke and is being made again (our place is kept).
    pub reconnecting: bool,
    /// Room codes of rooms kept connected in the background (watching or streaming there).
    pub background: Vec<String>,
}

/// The watched streams only (no notices or links taken), for a quick refresh after a click.
pub fn watches(app: &App) -> Vec<sampler::WatchStats> {
    app.sampler.watches()
}

pub fn snapshot(app: &App) -> Snapshot {
    let s = app.session.lock().unwrap().clone();
    let notice = app.notice.lock().unwrap().take();
    let link = app.link.lock().unwrap().take();
    let Some(s) = s else {
        return Snapshot { connected: false, room: String::new(), title: None, me: 0, peers: vec![], watches: vec![], live: None, notice, link, direct: None, reconnecting: false, background: background_rooms(app) };
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
        reconnecting: s.reconnecting(),
        background: background_rooms(app),
        direct: s.direct.then(|| if app.direct.lock().unwrap().as_ref().is_some_and(|(c, _)| c.is_open()) { "connected" } else { "waiting" }),
    }
}

/// Makes a session of a direct connection the open one.
fn install_direct(app: &Arc<App>, control: Arc<media::direct::Control>, name: &str, other: &str, inviter: bool) -> Result<(), String> {
    let (s, events) = Session::direct(control, name, other, inviter)?;
    *app.session.lock().unwrap() = Some(s.clone());
    let mine = Arc::downgrade(&s);
    app.sampler.attach(s);
    let a = app.clone();
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            match e {
                Event::WatchEnded { reason, .. } if reason != "left the room" => a.notify(format!("watch_ended:{reason}")),
                Event::Error(m) => a.notify(format!("error:{m}")),
                // Left on purpose (another room opened): no news, and not ours to close.
                Event::Lost if !is_open_session(&a, &mine) => break,
                Event::Lost => {
                    // The other person left or the connection broke: stop cleanly.
                    a.notify("direct_ended".into());
                    leave_for_good(&a, std::time::Duration::from_millis(100));
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

/// Direct connection (beta), the inviting side: our invite text. Blocks up to ~8 s.
pub fn direct_invite(app: &Arc<App>, name: &str) -> Result<String, String> {
    leave_for_good(app, std::time::Duration::from_millis(300));
    let control = media::direct::Control::new(&IceConfig::default())?;
    *app.direct.lock().unwrap() = Some((control.clone(), name.to_string()));
    control.invite(name, std::time::Duration::from_secs(8))
}

/// Direct connection, the inviting side: the friend's answer. Connects in a moment.
pub fn direct_accept(app: &Arc<App>, text: &str) -> Result<(), String> {
    let (control, name) = app.direct.lock().unwrap().clone().ok_or("make an invite first")?;
    let other = control.accept(text)?;
    install_direct(app, control, &name, &other, true)
}

/// Direct connection, the invited side: takes the invite, returns our answer text.
/// Blocks up to ~8 s.
pub fn direct_answer(app: &Arc<App>, text: &str, name: &str) -> Result<String, String> {
    leave_for_good(app, std::time::Duration::from_millis(300));
    let control = media::direct::Control::new(&IceConfig::default())?;
    *app.direct.lock().unwrap() = Some((control.clone(), name.to_string()));
    let (answer, other) = control.answer(text, name, std::time::Duration::from_secs(8))?;
    install_direct(app, control, name, &other, false)?;
    Ok(answer)
}

const BUILT_IN_SERVER: Option<&str> = option_env!("STREAMER_SERVER_URL");

/// The signaling server fixed at build time (STREAMER_SERVER_URL when compiling), if any.
/// Such a build has no server field and ignores any other address.
pub fn built_in() -> Option<&'static str> {
    BUILT_IN_SERVER.filter(|s| !s.is_empty())
}

/// Test runs (STREAMER_APP_* variables) never look for updates, unless STREAMER_UPDATE_URL
/// points them at a test release.
pub fn updates_enabled() -> bool {
    let on = std::env::var_os("STREAMER_UPDATE_URL").is_some() || !testing();
    eprintln!("update: checks {}", if on { "on" } else { "off" });
    on
}

/// Short codes for direct connection texts: a service keeps a text 5 minutes under an
/// 8-character code (local-drop's /api/backseat/code). Optional: on any failure the app
/// keeps showing the full text, and a pasted full text never needs the service.
#[derive(Serialize, Clone, Debug)]
pub struct ShortCode {
    pub code: String,
    pub expires_in: u64,
}

/// The short-code service; `STREAMER_SHORTCODE_URL` points tests elsewhere.
fn shortcode_service() -> String {
    std::env::var("STREAMER_SHORTCODE_URL").unwrap_or_else(|_| "https://local-drop.onrender.com".into()).trim_end_matches('/').to_string()
}

fn shortcode_agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(secs))).http_status_as_error(false).build().into()
}

/// Puts a direct connection text on the service; gives up after 4 s. Blocks.
pub fn shortcode_put(text: &str) -> Result<ShortCode, String> {
    let url = format!("{}/api/backseat/code", shortcode_service());
    let r = (|| {
        let mut r = shortcode_agent(4).post(&url).header("Content-Type", "application/json").send(serde_json::json!({ "text": text }).to_string()).map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let body: serde_json::Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if status != 201 {
            return Err(body["error"].as_str().unwrap_or("error").to_string());
        }
        Ok(ShortCode { code: body["code"].as_str().unwrap_or_default().to_string(), expires_in: body["expires_in"].as_u64().unwrap_or(300) })
    })();
    r.inspect_err(|e| eprintln!("shortcode: put failed: {e}"))
}

/// The text behind a short code; gives up after 8 s. "not_found" for an unknown code. Blocks.
pub fn shortcode_get(code: &str) -> Result<String, String> {
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let url = format!("{}/api/backseat/code/{code}", shortcode_service());
    let r = (|| {
        let mut r = shortcode_agent(8).get(&url).call().map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let body: serde_json::Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        match status {
            200 => Ok(body["text"].as_str().unwrap_or_default().to_string()),
            404 => Err("not_found".into()),
            _ => Err(body["error"].as_str().unwrap_or("error").to_string()),
        }
    })();
    r.inspect_err(|e| eprintln!("shortcode: get failed: {e}"))
}

/// The UI's saved rooms and settings (%LOCALAPPDATA%\Backseat\state.json). The Tauri UI
/// mirrors its localStorage here; the GPUI UI keeps them only here.
pub fn state_file() -> Option<std::path::PathBuf> {
    // Test runs keep their data apart from the real one, next to their own WebView2 profile
    // (the webview UI's saved data in a test run): runs that share a profile share the file.
    // The state gate (STREAMER_APP_TEST_STATE) uses the real place under its own LOCALAPPDATA.
    let testing = std::env::vars().any(|(k, _)| k.starts_with("STREAMER_APP_") && k != "STREAMER_APP_TEST_STATE");
    if testing && std::env::var_os("STREAMER_APP_TEST_STATE").is_none() {
        let dir = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("backseat-test-webview"));
        return Some(dir.join("state.json"));
    }
    Some(std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Backseat").join("state.json"))
}

/// Writes the state file whole: a temporary file first, then a rename over the old one.
/// `json` must be a JSON object; it is written as given, so keys keep their order.
pub fn state_save(json: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("the state is not a JSON object".into());
    }
    let Some(path) = state_file() else { return Ok(()) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        eprintln!("state: save failed: {e}");
        e.to_string()
    })
}

/// The state file's object, or an empty one when there is none or it cannot be read.
/// STREAMER_APP_STATE_FILE reads another file (tests of the GPUI UI).
pub fn state_load() -> serde_json::Map<String, serde_json::Value> {
    let path = std::env::var_os("STREAMER_APP_STATE_FILE").map(std::path::PathBuf::from).or_else(state_file);
    let text = path.and_then(|p| std::fs::read_to_string(p).ok());
    match text.map(|t| serde_json::from_str::<serde_json::Value>(&t)) {
        Some(Ok(serde_json::Value::Object(m))) => m,
        Some(Err(e)) => {
            eprintln!("state: cannot read the file: {e}");
            Default::default()
        }
        _ => Default::default(),
    }
}

/// Opens the log folder in Explorer, for sending logs with a bug report.
pub fn open_logs() -> Result<(), String> {
    let dir = media::logfile::dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if std::env::var_os("STREAMER_APP_TEST_OPEN_LOGS").is_some() {
        // Tests: say what would open instead of opening a window.
        eprintln!("test: open logs {}", dir.display());
        return Ok(());
    }
    std::process::Command::new("explorer").arg(&dir).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The latest release and whether it is newer than this app. Blocks.
pub fn check_update(app: &App) -> Result<update::Release, String> {
    let r = update::check();
    match &r {
        Ok(r) => eprintln!("update: this {} latest {} newer {}", r.current, r.latest, r.newer),
        Err(e) => eprintln!("update: check failed: {e}"),
    }
    let r = r?;
    *app.release.lock().unwrap() = Some(r.clone());
    Ok(r)
}

/// Downloads the release found by `check_update`, checks it and starts the installer.
/// Returns true when the app must quit now so the installer can replace its files. Blocks.
pub fn install_update(app: &App) -> Result<bool, String> {
    let r = app.release.lock().unwrap().clone().filter(|r| r.newer).ok_or("no newer release")?;
    let path = update::download(&r).inspect_err(|e| eprintln!("update: {e}"))?;
    eprintln!("update: downloaded and checked {}", path.display());
    update::run_installer(&path)?;
    if std::env::var_os("STREAMER_UPDATE_DRY").is_none() {
        leave_everything(app, std::time::Duration::from_millis(500));
        return Ok(true);
    }
    Ok(false)
}

/// The tray menu's words in the UI language.
pub fn tray_text() -> (&'static str, &'static str) {
    match sampler::UI_LANG.lock().unwrap().as_str() {
        "uk" => ("Відкрити Backseat", "Вийти"),
        "ru" => ("Открыть Backseat", "Выйти"),
        _ => ("Open Backseat", "Quit"),
    }
}

/// Codecs this machine can stream (hardware encoders only), e.g. ["h265", "h264", "av1"].
pub fn codecs() -> Vec<String> {
    media::platform::encodable_codecs().into_iter().map(|c| c.short().to_string()).collect()
}

/// Joins a room (leaving any other). A build with a fixed server goes there. Blocks: run it
/// off the UI thread (creating a video window inside the main window waits for that thread).
pub fn connect(app: &Arc<App>, server: &str, room: &str, name: &str, title: Option<&str>) -> Result<(), String> {
    let server = built_in().unwrap_or(server);
    do_connect(app, server, room, name, title).inspect_err(|e| eprintln!("connect failed: {e}"))
}

/// `title` names the room if it is new (a room made here, with its invite code as `room`).
fn do_connect(app: &Arc<App>, server: &str, room: &str, name: &str, title: Option<&str>) -> Result<(), String> {
    // One join at a time. Connect runs off the main thread, so an invite link arriving twice
    // (the page opens the app, and the user also presses its button) joined twice and left
    // both connections alive: the same person twice in the room.
    static JOINING: Mutex<()> = Mutex::new(());
    let _one = JOINING.lock().unwrap_or_else(|e| e.into_inner());
    let key = resume_key(server, room, name);
    if app.joined.lock().unwrap().as_deref() == Some(key.as_str()) && app.session.lock().unwrap().is_some() {
        return Ok(());
    }
    step_away(app);
    // A room kept in the background opens again as it is: no new join.
    let kept = {
        let mut bg = app.background.lock().unwrap();
        bg.iter().position(|(k, _)| *k == key).map(|i| bg.remove(i).1)
    };
    if let Some(s) = kept {
        eprintln!("rooms: back to {room} (kept in the background)");
        set_videos_hidden(&s, false);
        *app.joined.lock().unwrap() = Some(key);
        *app.session.lock().unwrap() = Some(s.clone());
        app.sampler.attach(s);
        return Ok(());
    }
    let (s, events) = Session::join_resuming(server, room, name, title, saved_places().remove(&key))?;
    save_place(&key, s.resume());
    *app.joined.lock().unwrap() = Some(key);
    // Whatever got in meanwhile leaves.
    if let Some(other) = app.session.lock().unwrap().replace(s.clone()) {
        other.leave();
    }
    app.sampler.attach(s.clone());
    let a = app.clone();
    let mine = Arc::downgrade(&s);
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            match e {
                // Leaving the room on purpose ends its watches; that is no news.
                Event::WatchEnded { reason, .. } if reason != "left the room" => a.notify(format!("watch_ended:{reason}")),
                Event::Error(m) => a.notify(format!("error:{m}")),
                // A room left on purpose (another one opened) ends too. That is no news, and
                // the open session is the new room's: taking it here left the new room and
                // showed "Lost the connection". STREAMER_DEBUG_LOST_ANY_SESSION: the old way.
                Event::Lost if !is_open_session(&a, &mine) && std::env::var_os("STREAMER_DEBUG_LOST_ANY_SESSION").is_none() => {
                    // A room kept in the background lost its connection: it ends.
                    let gone = {
                        let mut bg = a.background.lock().unwrap();
                        let i = bg.iter().position(|(_, s)| mine.upgrade().is_some_and(|m| Arc::ptr_eq(s, &m)));
                        i.map(|i| bg.remove(i))
                    };
                    if let Some((k, s)) = gone {
                        eprintln!("rooms: a room in the background lost its connection");
                        s.leave();
                        save_place(&k, None);
                    }
                    break;
                }
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
                    if let Some(k) = a.joined.lock().unwrap().take() {
                        save_place(&k, None);
                    }
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

/// Room codes of the rooms kept in the background.
fn background_rooms(app: &App) -> Vec<String> {
    app.background.lock().unwrap().iter().map(|(_, s)| s.room.clone()).collect()
}

/// Hides (or shows again) a room's video inside the main window. A video popped out into a
/// window of its own is not touched by this (`present::place` ignores `hidden` for it).
fn set_videos_hidden(s: &Session, hidden: bool) {
    for w in s.watching() {
        if let Some(win) = w.viewer.window {
            win.set_hidden(hidden);
        }
    }
}

/// The open room steps aside for another: watching or streaming there, it stays connected
/// in the background (its video hidden); otherwise its connection closes with no Leave, so
/// the server keeps the place 20 s and coming back takes it again. A direct connection ends.
/// STREAMER_DEBUG_LEAVE_ON_SWITCH: every room is left, as before.
fn step_away(app: &App) {
    if app.direct.lock().unwrap().is_some() || std::env::var_os("STREAMER_DEBUG_LEAVE_ON_SWITCH").is_some() {
        leave_for_good(app, std::time::Duration::from_millis(300));
        return;
    }
    let s = app.session.lock().unwrap().take();
    let key = app.joined.lock().unwrap().take();
    let (Some(s), Some(key)) = (s, key) else { return };
    if s.busy() {
        eprintln!("rooms: {} stays connected in the background", s.room);
        set_videos_hidden(&s, true);
        app.background.lock().unwrap().push((key, s));
    } else {
        eprintln!("rooms: {} closed, place kept", s.room);
        s.close_keeping_place();
    }
}

/// Watching or streaming in the open room ends it in the background rooms (one room at a
/// time), which then close keeping their place.
fn quiet_background(app: &App) {
    let rooms: Vec<(String, Arc<Session>)> = std::mem::take(&mut *app.background.lock().unwrap());
    for (_, s) in rooms {
        eprintln!("rooms: {} stops in the background (now active here)", s.room);
        s.leave_activity();
        s.close_keeping_place();
    }
}

/// Leaves a room kept in the background (taken off the list, renamed).
pub fn leave_background(app: &App, room: &str) {
    let gone: Vec<(String, Arc<Session>)> = {
        let mut bg = app.background.lock().unwrap();
        let (gone, keep) = std::mem::take(&mut *bg).into_iter().partition(|(_, s)| s.room == room);
        *bg = keep;
        gone
    };
    for (k, s) in gone {
        s.leave_and_wait(std::time::Duration::from_millis(300));
        save_place(&k, None);
    }
}

/// The app quits: every room is left, the open one and those in the background.
pub fn leave_everything(app: &App, wait: std::time::Duration) {
    let rooms: Vec<(String, Arc<Session>)> = std::mem::take(&mut *app.background.lock().unwrap());
    for (k, s) in rooms {
        s.leave_and_wait(wait);
        save_place(&k, None);
    }
    leave_for_good(app, wait);
}

/// Whether `mine` is the open session (not one left for another room).
fn is_open_session(app: &App, mine: &std::sync::Weak<Session>) -> bool {
    let Some(mine) = mine.upgrade() else { return false };
    app.session.lock().unwrap().as_ref().is_some_and(|s| Arc::ptr_eq(s, &mine))
}

pub fn leave(app: &App) {
    leave_for_good(app, std::time::Duration::from_millis(300));
}

#[derive(Serialize, Clone, Debug)]
pub struct Sources {
    pub monitors: Vec<media::platform::MonitorInfo>,
    pub windows: Vec<media::platform::WindowInfo>,
}

pub fn dev_sources() -> bool {
    std::env::var_os("STREAMER_APP_DEV_SOURCES").is_some()
}

pub fn sources() -> Sources {
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

pub fn video_window(app: &App, streamer: u64) -> Option<media::present::Window> {
    let s = app.session.lock().unwrap().clone()?;
    s.watching().into_iter().find(|w| w.streamer == streamer)?.viewer.window
}

/// What to do with a stream's video window: "show", "fullscreen" (on), "windowed",
/// "toggle_fullscreen", "topmost" (on), "not_topmost", "overlay", "no_overlay",
/// "pop_out" or "pop_in".
pub fn video_window_do(app: &App, streamer: u64, action: &str) -> Result<(), String> {
    let w = video_window(app, streamer).ok_or("no video window for this stream")?;
    match action {
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
        "badge" => w.set_badge(true),
        "no_badge" => w.set_badge(false),
        _ => return Err(format!("unknown window action {action}")),
    }
    Ok(())
}

/// Where a stream's video goes in the main window, in physical pixels of its client area;
/// None: not on screen.
pub fn video_place(app: &App, streamer: u64, rect: Option<(i32, i32, i32, i32)>) {
    if std::env::var_os("STREAMER_DEBUG_PLACE").is_some() {
        eprintln!("video_place {streamer} {rect:?}");
    }
    let Some(w) = video_window(app, streamer) else { return };
    w.place(rect);
}

/// Sound of one watched stream off or on.
pub fn set_muted(app: &App, streamer: u64, muted: bool) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    let w = s.watching().into_iter().find(|w| w.streamer == streamer).ok_or("not watching this stream")?;
    w.viewer.set_muted(muted);
    Ok(())
}

/// Hides all video inside the main window while a dialog is open over it.
pub fn videos_hidden(app: &App, hidden: bool) {
    let Some(s) = app.session.lock().unwrap().clone() else { return };
    for w in s.watching() {
        if let Some(win) = w.viewer.window {
            win.set_hidden(hidden);
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Thumb {
    pub id: u64,
    /// A `data:` URL, None when there is nothing to show (a minimized window).
    pub image: Option<String>,
    pub icon: Option<String>,
}

/// Pictures for the source picker. Slow: run it off the UI thread.
pub fn thumbnails(kind: &str, ids: Vec<u64>) -> Vec<Thumb> {
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
            match kind {
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

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct GoLive {
    /// "monitor" or "window".
    pub kind: String,
    /// HMONITOR or HWND.
    pub id: u64,
    /// "system", "app" or "none".
    pub audio: String,
    pub audio_pid: Option<u32>,
    /// "1440p60" etc.
    pub mode: String,
    pub kbit: u32,
    /// "h264" or "h265"; H.265 when missing.
    #[serde(default)]
    pub codec: Option<String>,
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

/// Starts streaming in the open room. Blocks.
pub fn go_live(app: &Arc<App>, req: GoLive) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    quiet_background(app);
    let cfg = live_config(&req)?;
    eprintln!("live: {}x{}@{} {} kbit/s {} audio {}", cfg.mode.width, cfg.mode.height, cfg.mode.fps, cfg.kbit, cfg.codec.short(), if !cfg.audio { "none" } else if cfg.audio_pid.is_some() { "app" } else { "system" });
    s.start_streaming(cfg).inspect_err(|e| eprintln!("go_live failed: {e}"))?;
    Ok(())
}

/// Changes the running stream (source, quality, which program's sound); viewers stay. Blocks.
pub fn update_live(app: &App, req: GoLive) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    let h = s.host().ok_or("not live")?;
    let cfg = live_config(&req)?;
    h.reconfigure(cfg).inspect_err(|e| eprintln!("update_live failed: {e}"))
}

pub fn end_live(app: &App) {
    let s = app.session.lock().unwrap().clone();
    if let Some(s) = s {
        s.stop_streaming();
    }
}

/// Watches a streamer in the open room. Blocks: run it off the UI thread (creating the
/// video window inside the main window waits for that thread).
pub fn watch(app: &App, streamer: u64, quality: Option<String>) -> Result<(), String> {
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    quiet_background(app);
    s.watch(streamer, ViewerConfig::default(), quality).inspect_err(|e| eprintln!("watch failed: {e}"))?;
    Ok(())
}

pub fn unwatch(app: &App, streamer: u64) {
    let s = app.session.lock().unwrap().clone();
    if let Some(s) = s {
        s.unwatch(streamer);
    }
}

pub fn set_quality(app: &App, streamer: u64, quality: &str) -> Result<(), String> {
    // Checked here so a typo shows in the UI instead of being dropped by the host.
    let any = Quality::new(VideoMode::parse("1440p60")?, 50000);
    Quality::parse_request(quality, any)?;
    let s = app.session.lock().unwrap().clone().ok_or("not in a room")?;
    s.set_watch_quality(streamer, quality);
    Ok(())
}

/// Tests only, so a run needs no clicks:
/// STREAMER_APP_AUTOJOIN="ws://...;room;name" joins at launch;
/// STREAMER_APP_AUTOLIVE="1080p60;8000" then streams the test pattern (needs dev sources);
/// a third field "monitor" streams the primary monitor, a fourth sets the codec
/// ("1080p60;8000;bars;h264");
/// STREAMER_APP_AUTOWATCH="streamer name;720p30 2000" then watches that streamer.
pub fn automate(app: &Arc<App>) {
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
            if let Err(e) = go_live(&app, req) {
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
    // "popout": out in a window of its own at +2 s, and stays there.
    if mode == "popout" {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            w.set_popped(true);
        });
        return;
    }
    if mode == "fsloop" {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let spam = {
                let (stop, w) = (stop.clone(), w);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Some(on_main) = ON_MAIN.get() {
                            on_main(Box::new(move || w.place_again()));
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
pub fn link_arg() -> Option<String> {
    std::env::args().skip(1).find(|a| a.to_ascii_lowercase().starts_with("backseat://"))
}

/// Start-up shared by both UIs, before the window: one app per user (a second start hands
/// over its link and leaves), the log file, GStreamer, the sampler and test automation.
/// None: another app runs, this one must leave. `on_message`: what a later start sent
/// ("show" or a backseat:// link), called on the listener's thread.
pub fn start(on_message: impl Fn(&Arc<App>, String) + Send + Sync + 'static) -> Option<Arc<App>> {
    // Test runs start several apps on purpose (STREAMER_APP_* variables), so not them.
    let testing = testing();
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
                    return None;
                }
                // The other app may have just quit; then this one runs.
                Some(media::instance::claim()?)
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
                *a.link.lock().unwrap() = Some(msg.clone());
            }
            on_message(&a, msg);
        });
    }
    // Try the encoders once in the background, so the go-live dialog has the answer ready.
    std::thread::spawn(|| {
        let _ = media::platform::encodable_codecs();
    });
    app.sampler.start();
    automate(&app);
    Some(app)
}
