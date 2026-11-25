//! The local control API (API.md): what each call does. The HTTP side is
//! `backseat_core::api`. Calls that change what the window shows (open a room, leave it,
//! watch, join voice) go through the UI thread, the way a click does, so the window and the
//! saved rooms stay right; the rest call the core directly.
//!
//! Settings: "api" in the saved settings: {"enabled", "lan", "port", "token"}. Off by default.
//! Tests: STREAMER_APP_TEST_API="PORT;TOKEN" turns it on for this computer only.

use crate::{set_setting, settings, Room, Shell};
use backseat_core::api::{self, Reply, Request};
use backseat_core::{self as core, App as Core, GoLive};
use gpui_kit::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// A body field read with `Request::opt_*`; a wrong type answers 400.
macro_rules! field {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Reply::err(400, e),
        }
    };
}

/// Told how a call the UI ran went.
pub type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

enum Cmd {
    Open(String),
    Leave,
    Watch(u64, Option<String>),
    Unwatch(u64),
    JoinVoice,
    /// Anything else that must run on the UI thread (it writes the settings, as the UI does).
    Run(Box<dyn FnOnce(&mut Shell) -> Result<(), String> + Send>),
}

/// Calls for the UI thread, each with whether its caller gave up waiting (then it is dropped:
/// a retry of a timed-out call must not run it twice).
static QUEUE: Mutex<Vec<(Cmd, Done, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

/// What the UI knows and the core does not, copied from the UI thread for the API's threads.
#[derive(Clone, Default)]
struct Mirror {
    rooms: Vec<Room>,
    current: Option<Room>,
    joining: bool,
}

static MIRROR: Mutex<Option<Mirror>> = Mutex::new(None);
/// Why the server could not start (a port in use), for the settings.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static CORE: Mutex<Option<Arc<Core>>> = Mutex::new(None);

fn mirror() -> Mirror {
    MIRROR.lock().unwrap().clone().unwrap_or_default()
}

/// The saved "api" settings, with a token made on first use.
#[derive(Clone, Debug)]
pub struct Settings {
    pub enabled: bool,
    pub lan: bool,
    pub port: u16,
    pub token: String,
}

pub fn load() -> Settings {
    let a = settings().get("api").cloned().unwrap_or_default();
    Settings {
        enabled: a.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        lan: a.get("lan").and_then(|v| v.as_bool()).unwrap_or(true),
        port: a.get("port").and_then(|v| v.as_u64()).filter(|p| (1024..=65535).contains(p)).unwrap_or(api::DEFAULT_PORT as u64) as u16,
        token: a.get("token").and_then(|v| v.as_str()).filter(|t| t.len() >= 16).map(String::from).unwrap_or_default(),
    }
}

pub fn save(s: &Settings) {
    set_setting("api", json!({ "enabled": s.enabled, "lan": s.lan, "port": s.port, "token": s.token }));
}

/// 128 random bits as 32 hex digits.
/// Empty if the system's random source fails: then the API does not start (`apply`), rather
/// than run with a known token.
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    let ok = unsafe {
        use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
        BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG).is_ok()
    };
    if !ok {
        eprintln!("api: no random numbers for a token");
        return String::new();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn test_config() -> Option<api::Config> {
    let v = std::env::var("STREAMER_APP_TEST_API").ok()?;
    let (port, token) = v.split_once(';')?;
    Some(api::Config { port: port.parse().ok()?, lan: false, token: token.to_string() })
}

/// Starts, restarts or stops the server as the settings say now.
pub fn apply() {
    let Some(core) = CORE.lock().unwrap().clone() else { return };
    let cfg = test_config().or_else(|| {
        let mut s = load();
        if !s.enabled {
            return None;
        }
        if s.token.is_empty() {
            s.token = new_token();
            save(&s);
        }
        Some(api::Config { port: s.port, lan: s.lan, token: s.token })
    });
    let handler: Arc<api::Handler> = Arc::new(move |r: &Request| handle(&core, r));
    let r = api::run(cfg, handler);
    if let Err(e) = &r {
        eprintln!("api: {e}");
    }
    *LAST_ERROR.lock().unwrap() = r.err();
}

pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().clone()
}

/// The address to give a phone: this computer's local address, or 127.0.0.1.
pub fn shown_address() -> Option<String> {
    let addr = api::address()?;
    let ip = if addr.ip().is_unspecified() { api::lan_address().map(|i| i.to_string()).unwrap_or_else(|| "127.0.0.1".into()) } else { addr.ip().to_string() };
    Some(format!("http://{ip}:{}", addr.port()))
}

/// At start: the server as the settings say, and the UI's side of the calls (every 50 ms).
pub fn start(core: &Arc<Core>, window: &mut Window, cx: &mut Context<Shell>) {
    *CORE.lock().unwrap() = Some(core.clone());
    apply();
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| loop {
        cx.background_executor().timer(Duration::from_millis(50)).await;
        if this.update(cx, |s, cx| pump(s, cx)).is_err() {
            break;
        }
    })
    .detach();
}

fn pump(s: &mut Shell, cx: &mut Context<Shell>) {
    // Nothing to do while the API is off (the usual case).
    if api::address().is_none() && QUEUE.lock().unwrap().is_empty() {
        return;
    }
    *MIRROR.lock().unwrap() = Some(Mirror { rooms: s.rooms.clone(), current: s.current.clone(), joining: s.joining });
    let todo: Vec<(Cmd, Done, Arc<AtomicBool>)> = std::mem::take(&mut *QUEUE.lock().unwrap());
    for (cmd, done, gave_up) in todo {
        if gave_up.load(Ordering::SeqCst) {
            continue;
        }
        match cmd {
            Cmd::Open(code) => match s.rooms.iter().find(|r| r.room == code).cloned() {
                Some(r) => s.open_room_then(r, cx, Some(done)),
                None => done(Err("not a saved room".into())),
            },
            // A join still running would open the room again after this leave.
            Cmd::Leave if s.joining => done(Err("busy: a room is being joined".into())),
            Cmd::Leave => {
                s.leave(cx);
                done(Ok(()));
            }
            Cmd::Watch(id, q) => s.watch_stream_then(id, q, cx, Some(done)),
            Cmd::Unwatch(id) => {
                s.stop_watch(id, cx);
                done(Ok(()));
            }
            Cmd::JoinVoice => s.join_voice_then(cx, Some(done)),
            Cmd::Run(f) => {
                done(f(s));
                cx.notify();
            }
        }
    }
}

/// Runs a call on the UI thread and waits for it.
fn ui(cmd: Cmd, wait: Duration) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let gave_up = Arc::new(AtomicBool::new(false));
    let mut q = QUEUE.lock().unwrap();
    q.retain(|(_, _, g)| !g.load(Ordering::SeqCst));
    if q.len() >= 64 {
        return Err("the app is busy; try again".into());
    }
    q.push((
        cmd,
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
        gave_up.clone(),
    ));
    drop(q);
    let r = rx.recv_timeout(wait);
    if r.is_err() {
        // Not started yet: it never will. Started: it finishes, but nobody waits.
        gave_up.store(true, Ordering::SeqCst);
    }
    r.map_err(|_| "the app did not finish in time (it may still do it, or it was busy); check /status".to_string())?
}

/// A core error as a reply: the call could not be done in this state.
fn conflict(e: String) -> Reply {
    Reply::err(409, e)
}

fn handle(core: &Arc<Core>, r: &Request) -> Reply {
    // `[true]` or `5` is not "no body": every field would read as missing (a toggle, or the
    // primary monitor streamed instead of the window asked for).
    if !(r.body.is_null() || r.body.is_object()) {
        return Reply::err(400, "the body must be a JSON object");
    }
    let get = r.method == "GET";
    let post = r.method == "POST";
    let p: Vec<&str> = r.path.iter().map(String::as_str).collect();
    match (get, post, p.as_slice()) {
        (true, _, [] | ["info"]) => Reply::ok(json!({ "app": "backseat", "version": core::app_version(), "api": api::VERSION })),
        (true, _, ["status"]) => Reply::ok(status(core)),
        (true, _, ["rooms"]) => Reply::ok(json!({ "rooms": rooms(core) })),
        (true, _, ["sources"]) => Reply::ok(sources()),
        (_, true, ["rooms", code, "open"]) => open(core, code),
        (_, true, ["room", "open"]) => match field!(r.opt_str("room")) {
            Some(code) => open(core, &code),
            None => Reply::err(400, "\"room\" is missing"),
        },
        (_, true, ["room", "leave"]) => match ui(Cmd::Leave, Duration::from_secs(5)) {
            Ok(()) => Reply::ok(status(core)),
            Err(e) => conflict(e),
        },
        (_, true, ["voice", "join"]) => {
            if !core::status(core).connected {
                return conflict("not in a room".into());
            }
            match ui(Cmd::JoinVoice, Duration::from_secs(15)) {
                Ok(()) => Reply::ok(status(core)),
                Err(e) => conflict(e),
            }
        }
        (_, true, ["voice", "leave"]) => {
            core::leave_voice(core);
            Reply::ok(status(core))
        }
        (_, true, ["voice", "mute"]) => {
            // As the app's button: deafened counts as muted, and turning the microphone on
            // turns the sound on too.
            let v = core::voice_view(core);
            // STREAMER_DEBUG_API_MUTE_RAW: the raw switch only (the spoiled variant).
            let shown = if std::env::var_os("STREAMER_DEBUG_API_MUTE_RAW").is_some() { v.mic_muted } else { v.mic_muted || v.deafened };
            let on = field!(r.opt_bool("muted")).unwrap_or(!shown);
            if !spoiled() {
                core::set_mic_muted(core, on);
            }
            Reply::ok(status(core))
        }
        (_, true, ["voice", "deafen"]) => {
            let on = field!(r.opt_bool("deafened")).unwrap_or(!core::voice_view(core).deafened);
            if !spoiled() {
                core::set_deafened(core, on);
            }
            Reply::ok(status(core))
        }
        (_, true, ["voice", "members", id, "volume"]) => {
            let Ok(id) = id.parse::<u64>() else { return Reply::err(400, "the member id is a number") };
            let Some(v) = field!(r.opt_f64("volume")).map(f64::round).filter(|v| (0.0..=200.0).contains(v)) else {
                return Reply::err(400, "\"volume\" is a percent, 0 to 200 (100: as they speak)");
            };
            let snap = core::status(core);
            // Volumes act on the voice we are in; in another room's, these ids are not theirs.
            if !snap.voice.joined {
                return conflict(if snap.voice.elsewhere.is_some() { "your voice is in another room; open it first".into() } else { "not in voice".into() });
            }
            let Some(p) = snap.peers.iter().find(|p| p.id == id && !p.me).cloned() else { return Reply::err(404, "no such member (not you) in the open room") };
            let c = core.clone();
            let run = Cmd::Run(Box::new(move |s| {
                if spoiled() {
                    return Ok(());
                }
                core::set_peer_volume(&c, id, v / 100.0);
                // Kept by name, as the voice menu keeps it: it comes back next time.
                crate::voice_ui::save_volume(&p.name, v as f32);
                // Their volume menu, if open, shows the old value: closed (it opens again right).
                if s.voice_menu.as_ref().is_some_and(|m| m.id == id) {
                    s.voice_menu = None;
                }
                Ok(())
            }));
            match ui(run, Duration::from_secs(5)) {
                Ok(()) => Reply::ok(status(core)),
                Err(e) => conflict(e),
            }
        }
        (_, true, ["live", "start"]) => live(core, r, false),
        (_, true, ["live", "update"]) => live(core, r, true),
        (_, true, ["live", "stop"]) => {
            // STREAMER_DEBUG_API_STOP_OPEN_ONLY: only the open room's (the spoiled variant).
            if std::env::var_os("STREAMER_DEBUG_API_STOP_OPEN_ONLY").is_some() {
                core::end_live(core);
            } else {
                core::end_live_anywhere(core);
            }
            Reply::ok(status(core))
        }
        (_, true, ["watch", id, rest @ ..]) => {
            let Ok(id) = id.parse::<u64>() else { return Reply::err(400, "the streamer id is a number (\"id\" of a member)") };
            watch(core, id, rest, r)
        }
        (true, _, _) | (_, true, _) => Reply::err(404, "no such call; see API.md"),
        _ => Reply::err(405, "use GET or POST"),
    }
}

fn open(core: &Arc<Core>, code: &str) -> Reply {
    if !mirror().rooms.iter().any(|r| r.room == code) {
        return Reply::err(404, "not a saved room (GET /rooms lists them; open a new room once in the app)");
    }
    match ui(Cmd::Open(code.to_string()), Duration::from_secs(30)) {
        Ok(()) => Reply::ok(status(core)),
        Err(e) => conflict(e),
    }
}

/// Tests only: STREAMER_DEBUG_API_NO_EFFECT answers the changing calls without doing them
/// (mute, deafen, volumes, quality, changing the stream), the spoiled run of the API gate.
fn spoiled() -> bool {
    std::env::var_os("STREAMER_DEBUG_API_NO_EFFECT").is_some()
}

fn check_quality(q: &str) -> Result<(), String> {
    use media::config::{Quality, VideoMode};
    let any = Quality::new(VideoMode::parse("1440p60")?, 50000);
    Quality::parse_request(q, any).map(|_| ())
}

fn watch(core: &Arc<Core>, id: u64, rest: &[&str], r: &Request) -> Reply {
    let snap = core::status(core);
    // From the session, not the sampler (up to a second behind).
    let session = core.session.lock().unwrap().clone();
    let watching = session.and_then(|s| s.watching().into_iter().find(|w| w.streamer == id));
    let streaming = snap.peers.iter().any(|p| p.id == id && p.streaming && !p.me);
    let ok = |core: &Arc<Core>| Reply::ok(status(core));
    match rest {
        [] => {
            if !streaming {
                return Reply::err(404, "nobody with this id streams in the open room");
            }
            let q = field!(r.opt_str("quality"));
            if let Some(q) = &q {
                if let Err(e) = check_quality(q) {
                    return Reply::err(400, e);
                }
            }
            // Already watched: only the quality, if one is asked for.
            if watching.is_some() {
                if let Some(q) = q.as_ref().filter(|_| !spoiled()) {
                    if let Err(e) = core::set_quality(core, id, q) {
                        return Reply::err(400, e);
                    }
                }
                return ok(core);
            }
            match ui(Cmd::Watch(id, q), Duration::from_secs(20)) {
                Ok(()) => ok(core),
                Err(e) => conflict(e),
            }
        }
        ["stop"] => {
            if watching.is_none() {
                return Reply::err(404, "not watching this stream");
            }
            match ui(Cmd::Unwatch(id), Duration::from_secs(5)) {
                Ok(()) => ok(core),
                Err(e) => conflict(e),
            }
        }
        ["mute"] => {
            let Some(w) = watching else { return Reply::err(404, "not watching this stream") };
            let on = field!(r.opt_bool("muted")).unwrap_or(!w.viewer.muted());
            if spoiled() {
                return ok(core);
            }
            match core::set_muted(core, id, on) {
                Ok(()) => ok(core),
                Err(e) => conflict(e),
            }
        }
        ["volume"] => {
            let Some(v) = field!(r.opt_f64("volume")).map(f64::round).filter(|v| (0.0..=100.0).contains(v)) else {
                return Reply::err(400, "\"volume\" is a percent, 0 to 100");
            };
            if watching.is_none() {
                return Reply::err(404, "not watching this stream");
            }
            if spoiled() {
                return ok(core);
            }
            match core::set_stream_volume(core, id, v as f32 / 100.0) {
                Ok(()) => ok(core),
                Err(e) => conflict(e),
            }
        }
        ["quality"] => {
            let Some(q) = field!(r.opt_str("quality")) else { return Reply::err(400, "\"quality\" is missing, e.g. \"720p30 4000\"") };
            if watching.is_none() {
                return Reply::err(404, "not watching this stream");
            }
            if spoiled() {
                return ok(core);
            }
            match core::set_quality(core, id, &q) {
                Ok(()) => ok(core),
                Err(e) => Reply::err(400, e),
            }
        }
        ["window"] => {
            let Some(a) = field!(r.opt_str("action")) else { return Reply::err(400, "\"action\" is missing") };
            const ACTIONS: [&str; 11] = ["show", "fullscreen", "windowed", "toggle_fullscreen", "topmost", "not_topmost", "pop_out", "pop_in", "overlay", "no_overlay", "badge"];
            if !ACTIONS.contains(&a.as_str()) && a != "no_badge" {
                return Reply::err(400, format!("unknown action; one of {}, no_badge", ACTIONS.join(", ")));
            }
            match core::video_window_do(core, id, &a) {
                Ok(()) => ok(core),
                Err(e) => conflict(e),
            }
        }
        _ => Reply::err(404, "no such call; see API.md"),
    }
}

/// Starts a stream (`update` false: missing fields as the go-live dialog fills them) or
/// changes the running one (`update` true: missing fields stay as they are; viewers stay).
fn live(core: &Arc<Core>, r: &Request, update: bool) -> Reply {
    let snap = core::status(core);
    if !snap.connected {
        return conflict("not in a room".into());
    }
    let base = core::live_now(core);
    // Live in a room kept in the background: starting here would end it unseen, and changing
    // it needs that room open.
    if let (None, Some((room, _))) = (&base, core::live_anywhere(core)) {
        return conflict(if update { format!("your stream runs in room {room}; open it to change the stream") } else { format!("already live in room {room}; stop first") });
    }
    match (&base, update) {
        (Some(_), false) => return conflict("already live; use /live/update to change it, or stop first".into()),
        (None, true) => return conflict("not live".into()),
        _ => {}
    }
    let src = core::sources();
    if r.body.get("source").is_some_and(|v| !(v.is_null() || v.is_object())) {
        return Reply::err(400, "\"source\" must be an object: {\"kind\": \"window\", \"id\": ...}");
    }
    let given_kind = field!(r.opt_str("/source/kind")).map(|k| k.to_ascii_lowercase());
    let given_id = field!(r.opt_u64("/source/id"));
    // The source: given, else the running one, else the primary monitor.
    let kind = given_kind.clone().or_else(|| base.as_ref().map(|b| b.kind.clone())).unwrap_or_else(|| "monitor".into());
    // No id: the running one if the kind is the running one (an update), else the default.
    let id = given_id.or_else(|| base.as_ref().filter(|b| given_kind.as_ref().is_none_or(|k| *k == b.kind)).map(|b| b.id));
    let keep = base.as_ref().filter(|b| given_id.is_none() && given_kind.as_ref().is_none_or(|k| *k == b.kind));
    let (id, height, pid) = match kind.as_str() {
        // Not looked up again: a window gone from the list (hidden) must not fail a new bitrate.
        _ if keep.is_some() => {
            let b = keep.unwrap();
            // A stream of a window with the system's sound has no program yet: found now, for
            // a change to "app".
            let pid = b.audio_pid.or_else(|| (b.kind == "window").then(|| src.windows.iter().find(|w| w.hwnd == b.id).map(|w| w.pid)).flatten());
            (b.id, 0, pid)
        }
        "monitor" => {
            let m = match id {
                Some(id) => src.monitors.iter().find(|m| m.handle == id),
                None => src.monitors.iter().find(|m| m.primary).or(src.monitors.first()),
            };
            let Some(m) = m else { return Reply::err(404, "no such monitor (GET /sources)") };
            (m.handle, m.height as i64, None)
        }
        "window" => {
            let Some(id) = id else { return Reply::err(400, "a window needs \"source\": {\"kind\": \"window\", \"id\": ...}") };
            let Some(w) = src.windows.iter().find(|w| w.hwnd == id) else { return Reply::err(404, "no such window (GET /sources)") };
            (w.hwnd, w.height as i64, Some(w.pid))
        }
        _ => return Reply::err(400, "\"source.kind\" is \"monitor\" or \"window\""),
    };
    let stream = settings().get("stream").cloned().unwrap_or_default();
    let pref = |k: &str| stream.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    // "H265" means h265, as everywhere else.
    let given_codec = field!(r.opt_str("codec")).map(|c| c.to_ascii_lowercase());
    let mut codec_auto = false;
    let codec = match (&base, given_codec) {
        (Some(b), Some(c)) if Some(&c) != b.codec.as_ref() => return conflict("the codec cannot change while live; stop and start again".into()),
        (Some(b), _) => b.codec.clone().unwrap_or_default(),
        (None, given) => {
            let codecs = core::codecs();
            let order: Vec<String> = ["av1", "h265", "h264"].iter().filter(|c| codecs.iter().any(|h| h == *c)).map(|c| c.to_string()).collect();
            match given.clone().or_else(|| Some(pref("codec")).filter(|c| !c.is_empty())) {
                Some(c) if order.contains(&c) => c,
                Some(c) if given.is_some() => return Reply::err(400, format!("this computer cannot encode {c}; it can: {}", order.join(", "))),
                _ => match order.first() {
                    Some(c) => {
                        // As the dialog: only when no codec is saved (a saved one it cannot use
                        // is replaced, but counts as picked).
                        codec_auto = pref("codec").is_empty();
                        c.clone()
                    }
                    None => return conflict("no hardware video encoder found".into()),
                },
            }
        }
    };
    let mode = match (field!(r.opt_str("mode")).map(|m| m.to_ascii_lowercase()), &base) {
        (Some(m), _) => m,
        (None, Some(b)) => b.mode.clone(),
        (None, None) => {
            let res = Some(pref("res")).filter(|x| ["720", "1080", "1440"].contains(&x.as_str())).unwrap_or_else(|| if height >= 1440 { "1440" } else if height >= 1080 { "1080" } else { "720" }.into());
            let fps = Some(pref("fps")).filter(|x| ["30", "60"].contains(&x.as_str())).unwrap_or_else(|| "60".into());
            format!("{res}p{fps}")
        }
    };
    if !MODES.contains(&mode.as_str()) {
        return Reply::err(400, format!("\"mode\" is one of {}", MODES.join(", ")));
    }
    let kbit = match (field!(r.opt_u64("kbit")), &base) {
        (Some(k), _) => u32::try_from(k).unwrap_or(0),
        (None, Some(b)) => b.kbit,
        // As the go-live dialog: the setting, else 20000; an AV1 it picked by itself gets 15000.
        (None, None) => {
            let k = pref("kbit").parse().ok().filter(|k| crate::room::PRESETS.contains(k)).unwrap_or(20000);
            if codec_auto && codec == "av1" && k == 20000 {
                15000
            } else {
                k
            }
        }
    };
    if !crate::room::PRESETS.contains(&kbit) {
        return Reply::err(400, format!("\"kbit\" is one of {:?}", crate::room::PRESETS));
    }
    let audio = match (field!(r.opt_str("audio")).map(|a| a.to_ascii_lowercase()), &base) {
        (Some(a), _) => a,
        // A running stream keeps its sound: a program's own sound follows a new window; a
        // monitor has no program of its own, so it takes the whole system's.
        (None, Some(b)) if b.audio == "app" && kind != "window" => "system".into(),
        (None, Some(b)) => b.audio.clone(),
        (None, None) => match pref("audio").as_str() {
            "none" => "none".into(),
            "system" => "system".into(),
            _ if kind == "window" => "app".into(),
            _ => "system".into(),
        },
    };
    if !["system", "app", "none"].contains(&audio.as_str()) || (audio == "app" && kind != "window") {
        return Reply::err(400, "\"audio\" is \"system\" or \"none\" (and \"app\" for a window)");
    }
    if audio == "app" && pid.is_none() {
        return Reply::err(404, "the streamed window is gone from the list; give \"source\" again");
    }
    if let Some(b) = &base {
        if (b.audio == "none") != (audio == "none") {
            return conflict("sound on or off cannot change while live; stop and start again".into());
        }
    }
    let req = GoLive { kind, id, audio, audio_pid: pid, audio_exclude: crate::dialogs::audio_exclude(), mode, kbit, codec: Some(codec) };
    let done = if update && spoiled() {
        Ok(())
    } else if update {
        core::update_live(core, req)
    } else {
        core::go_live(core, req)
    };
    match done {
        Ok(()) => Reply::ok(status(core)),
        Err(e) => conflict(e),
    }
}

const MODES: [&str; 6] = ["720p30", "720p60", "1080p30", "1080p60", "1440p30", "1440p60"];

fn sources() -> Value {
    let s = core::sources();
    let have = core::codecs();
    let codecs: Vec<&str> = ["av1", "h265", "h264"].into_iter().filter(|c| have.iter().any(|h| h == c)).collect();
    json!({
        "monitors": s.monitors.iter().map(|m| json!({ "id": m.handle, "name": m.name.trim_start_matches(r"\\.\"), "width": m.width, "height": m.height, "primary": m.primary })).collect::<Vec<_>>(),
        "windows": s.windows.iter().map(|w| json!({ "id": w.hwnd, "title": w.title, "exe": w.exe, "pid": w.pid, "minimized": w.minimized, "width": w.width, "height": w.height })).collect::<Vec<_>>(),
        "codecs": codecs,
        "modes": MODES,
        "kbit_presets": crate::room::PRESETS,
    })
}

fn rooms(core: &Arc<Core>) -> Vec<Value> {
    let m = mirror();
    let snap = core::status(core);
    m.rooms
        .iter()
        .map(|r| {
            let open = snap.connected && snap.room == r.room && m.current.as_ref().is_some_and(|c| c.same(r));
            json!({
                "code": r.room,
                "title": r.title,
                "label": r.label(),
                "server": r.server,
                "my_name": r.name,
                "open": open,
                "background": snap.background.contains(&r.room),
                "temporary": protocol::temp::looks_temp(&r.room),
            })
        })
        .collect()
}

fn status(core: &Arc<Core>) -> Value {
    let s = core::status(core);
    let m = mirror();
    let v = &s.voice;
    let name_of = |id: u64| s.peers.iter().find(|p| p.id == id).map(|p| p.name.clone());
    // Whether we watch or stream comes from the session itself: the sampler's numbers are up
    // to a second old, and a reply right after Watch or Stop must already show it.
    let session = core.session.lock().unwrap().clone();
    let watched: Vec<u64> = session.as_ref().map(|x| x.watching().iter().map(|w| w.streamer).collect()).unwrap_or_default();
    let live_now = session.as_ref().is_some_and(|x| x.host().is_some());
    // Our stream may run in a room kept in the background (switched away from while live).
    let live_away = if live_now { None } else { core::live_anywhere(core) };
    let room = s.connected.then(|| {
        let cur = m.current.as_ref().filter(|c| c.room == s.room);
        json!({
            "code": s.room,
            "title": s.title.clone().or_else(|| cur.and_then(|c| c.title.clone())),
            "server": cur.map(|c| c.server.clone()),
            "direct": s.direct,
            "reconnecting": s.reconnecting,
            "me": s.me,
            "members": s.peers.iter().map(|p| {
                let in_voice = v.members.contains(&p.id);
                json!({
                    "id": p.id,
                    "name": p.name,
                    "me": p.me,
                    "streaming": if p.me { live_now } else { p.streaming },
                    "watching": watched.contains(&p.id),
                    "in_voice": in_voice,
                    "speaking": v.speaking.contains(&p.id),
                    "mic_muted": in_voice && if p.me { v.mic_muted || v.deafened } else { v.muted.contains(&p.id) },
                    "deafened": in_voice && if p.me { v.deafened } else { v.deaf.contains(&p.id) },
                    "volume": (!p.me && in_voice && v.joined).then(|| (core::peer_volume(core, p.id) * 100.0).round()),
                })
            }).collect::<Vec<_>>(),
        })
    });
    let voice_room = if v.joined { Some(s.room.clone()) } else { v.elsewhere.clone() };
    let watching: Vec<Value> = watched
        .iter()
        .map(|id| {
            let w = s.watches.iter().find(|w| w.streamer == *id);
            let viewer = session.as_ref().and_then(|x| x.watching().into_iter().find(|x| x.streamer == *id));
            json!({
                "streamer": id,
                "name": name_of(*id),
                "quality": w.and_then(|w| w.quality.clone()),
                "codec": w.map(|w| w.codec.clone()).unwrap_or_default(),
                "fps": w.map_or(0.0, |w| w.received_fps),
                "latency_ms": w.map_or(0.0, |w| w.latency_ms),
                "mbit": w.map_or(0.0, |w| w.mbit),
                "loss_pct": w.map_or(0.0, |w| w.loss_pct),
                "muted": viewer.as_ref().is_some_and(|x| x.viewer.muted()),
                "volume": viewer.as_ref().map_or(100.0, |x| (x.viewer.volume() * 100.0).round() as f64),
                "fullscreen": w.is_some_and(|w| w.fullscreen),
                "popped_out": w.is_some_and(|w| w.popped),
                "window_closed": w.is_some_and(|w| w.window_closed),
            })
        })
        .collect();
    let settings_now = core::live_anywhere(core);
    let live = if let Some((room, _)) = &live_away {
        // No numbers for a room in the background (the sampler follows the open one).
        Some(json!({ "room": room, "since_secs": 0, "quality": "", "encoder": "", "upload_mbit": 0.0, "viewers": [] }))
    } else {
        live_now.then(|| match &s.live {
        Some(l) => json!({
            "since_secs": l.since_secs,
            "quality": l.ceiling,
            "encoder": l.encoder,
            "upload_mbit": l.upload_mbit,
            "viewers": l.viewers.iter().map(|x| json!({ "id": x.id, "name": x.name, "quality": x.quality, "rtt_ms": x.rtt_ms, "loss_pct": x.loss_pct, "problems": x.problems })).collect::<Vec<_>>(),
        }),
        // Just started: no numbers yet.
        None => json!({ "since_secs": 0, "quality": "", "encoder": "", "upload_mbit": 0.0, "viewers": [] }),
    })
    .map(|mut l| {
        l["room"] = json!(s.room);
        l
    })
    };
    json!({
        "version": core::app_version(),
        "joining": m.joining,
        "reconnecting": s.reconnecting,
        "room": room,
        "background_rooms": s.background,
        "voice": {
            "joined": voice_room.is_some(),
            "room": voice_room,
            // As heard: while deafened the microphone is off too (as `members[].mic_muted`).
            "mic_muted": v.mic_muted || v.deafened,
            "deafened": v.deafened,
            "push_to_talk": v.ptt,
            "talking": v.talking,
        },
        "live": live.map(|mut l| {
            if let Some((_, g)) = &settings_now {
                l["settings"] = json!({ "source": { "kind": g.kind, "id": g.id }, "mode": g.mode, "kbit": g.kbit, "codec": g.codec, "audio": g.audio });
            }
            l
        }),
        "watching": watching,
    })
}
