//! Backseat with a native UI: GPUI and gpui-component instead of the webview. Everything
//! apart from the window is `backseat-core`. Stage 1: the window, rooms in the rail (from
//! the state file), people in the open room, a 1 s refresh, the tray.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dialogs;
mod i18n;
mod join;
mod room;
mod tray;

use backseat_core::{self as core, App as Core, Snapshot};
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::{Sizable as _, Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use i18n::{t, tf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The main window's HWND, for showing and hiding it from the tray and other threads.
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub(crate) mod color {
    pub const RAIL: u32 = 0x1e1f22;
    pub const SIDE: u32 = 0x2b2d31;
    pub const MAIN: u32 = 0x313338;
    pub const LINE: u32 = 0x1f2023;
    pub const TEXT: u32 = 0xdbdee1;
    pub const MUTED: u32 = 0xb5bac1;
    pub const BRIGHT: u32 = 0xf2f3f5;
    pub const ACCENT: u32 = 0x5865f2;
    pub const GREEN: u32 = 0x23a55a;
    pub const LIVE: u32 = 0xed4245;
    pub const BAR: u32 = 0x232428;
}

/// A saved room, as the Tauri UI keeps it in the state file.
#[derive(Clone, Debug, PartialEq)]
struct Room {
    server: String,
    room: String,
    name: String,
    title: Option<String>,
}

impl Room {
    fn from_json(v: &serde_json::Value) -> Option<Room> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        Some(Room { server: s("server")?, room: s("room")?, name: s("name")?, title: s("title").filter(|t| !t.is_empty()) })
    }
    fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::json!({ "server": self.server, "room": self.room, "name": self.name });
        if let Some(t) = &self.title {
            m["title"] = t.clone().into();
        }
        m
    }
    fn same(&self, other: &Room) -> bool {
        self.room == other.room && self.server == other.server
    }
    fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.room.clone())
    }
    /// Two letters for the rail, like the webview UI: first letters of two words, else the
    /// first two characters.
    fn initials(&self) -> String {
        let label = self.label();
        let words: Vec<&str> = label.split(|c: char| c.is_whitespace() || "-_.".contains(c)).filter(|w| !w.is_empty()).collect();
        let two: String = if words.len() > 1 { words[0].chars().take(1).chain(words[1].chars().take(1)).collect() } else { label.chars().take(2).collect() };
        two.to_uppercase()
    }
}

struct Shell {
    core: Arc<Core>,
    snap: Snapshot,
    rooms: Vec<Room>,
    current: Option<Room>,
    /// A join running in the background.
    joining: bool,
    error: Option<String>,
    /// The last line written for tests, so only changes are written.
    reported: String,
    join: join::Join,
    /// The direct connection's state at the last refresh, to see it end.
    prev_direct: Option<&'static str>,
    /// The direct form starts over at the next refresh (it needs the window).
    reset_pending: bool,
    room: room::Room,
    /// When the message in `error` came, so a notice in a room goes away after a while.
    error_since: Option<std::time::Instant>,
    dialogs: dialogs::Dialogs,
    subs: Vec<Subscription>,
    /// Refreshes so far, for things done every other second.
    ticks: u64,
}

/// One value of the state file.
pub(crate) fn state_value(key: &str) -> Option<serde_json::Value> {
    core::state_load().remove(key)
}

/// The saved settings (the state file's "settings" object).
pub(crate) fn settings() -> serde_json::Map<String, serde_json::Value> {
    match core::state_load().remove("settings") {
        Some(serde_json::Value::Object(m)) => m,
        _ => Default::default(),
    }
}

pub(crate) fn set_setting(key: &str, value: serde_json::Value) {
    let mut state = core::state_load();
    let mut s = settings();
    s.insert(key.into(), value);
    state.insert("settings".into(), serde_json::Value::Object(s));
    if let Err(e) = core::state_save(&serde_json::Value::Object(state).to_string()) {
        eprintln!("state: {e}");
    }
}

impl Shell {
    fn new(core: Arc<Core>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = core::state_load();
        let rooms: Vec<Room> = state.get("rooms").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(Room::from_json).collect()).unwrap_or_default();
        let last = state.get("lastRoom").and_then(Room::from_json);
        let snap = core::snapshot(&core);
        let join = join::Join::new(window, cx);
        let mut shell = Shell { core, snap, rooms, current: None, joining: false, error: None, reported: String::new(), join, prev_direct: None, reset_pending: false, room: room::Room::default(), error_since: None, dialogs: Default::default(), subs: vec![], ticks: 0 };
        // With a saved room, start in the last one opened. Not when a test joins by itself.
        if ["STREAMER_APP_AUTOJOIN", "STREAMER_APP_TEST_DIRECT", "STREAMER_APP_TEST_FORM"].iter().all(|k| std::env::var_os(k).is_none()) {
            let start = last.as_ref().and_then(|l| shell.rooms.iter().find(|r| r.same(l))).or(shell.rooms.last()).cloned();
            if let Some(r) = start {
                shell.open_room(r, cx);
            }
        }
        cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this.update_in(cx, |s, window, cx| {
                s.tick(cx);
                s.ticks += 1;
                // Pictures in the go-live dialog every 2 s.
                if s.ticks % 2 == 0 {
                    s.refresh_thumbs(cx);
                }
                s.sync_tiles(window, cx);
                if s.reset_pending {
                    s.reset_pending = false;
                    s.reset_direct(window, cx);
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        test_actions(cx);
        join::test_direct(window, cx);
        join::test_form(window, cx);
        room::test_click_watch(window, cx);
        room::test_quality(window, cx);
        dialogs::test_dialogs(window, cx);
        shell
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.snap = core::snapshot(&self.core);
        if let Some(n) = self.snap.notice.clone() {
            let (kind, rest) = n.split_once(':').unwrap_or((n.as_str(), ""));
            self.error = Some(match kind {
                "direct_ended" => t("notice.direct_ended").to_string(),
                "lost" => t("notice.lost").to_string(),
                "watch_ended" => tf("notice.watch_ended", &[("reason", &rest)]).to_string(),
                _ => tf("notice.error", &[("msg", &rest)]).to_string(),
            });
            self.error_since = Some(std::time::Instant::now());
        }
        if self.error.is_some() && self.snap.connected && self.error_since.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
            self.error = None;
            self.error_since = None;
        }
        if let Some(link) = self.snap.link.clone() {
            self.handle_link(&link, cx);
        }
        self.expire_codes();
        // A direct connection that ended: its form starts over. One that connected: the
        // form closes.
        if self.prev_direct == Some("connected") && self.snap.direct != Some("connected") {
            self.reset_pending = true;
        }
        if self.snap.direct == Some("connected") && self.join.direct_open {
            self.join.direct_open = false;
            self.join.open = false;
        }
        self.prev_direct = self.snap.direct;
        if self.snap.direct.is_some() {
            self.current = None;
            self.report();
            cx.notify();
            return;
        }
        // Joined some other way (tests): remember what we are in.
        if self.snap.connected && !self.joining && !self.join.busy && self.current.as_ref().is_none_or(|c| c.room != self.snap.room) {
            let me = self.snap.peers.iter().find(|p| p.me).map(|p| p.name.clone()).unwrap_or_default();
            let server = std::env::var("STREAMER_APP_AUTOJOIN").ok().and_then(|j| j.split(';').next().map(String::from)).unwrap_or_default();
            let r = Room { server, room: self.snap.room.clone(), name: me, title: self.snap.title.clone() };
            self.remember(&r);
            self.current = Some(r);
        }
        self.report();
        self.report_room();
        cx.notify();
    }

    /// Joins a room in the background (joining blocks; video windows wait for this thread).
    fn open_room(&mut self, r: Room, cx: &mut Context<Self>) {
        if self.joining || (self.snap.connected && self.current.as_ref().is_some_and(|c| c.same(&r))) {
            return;
        }
        self.current = Some(r.clone());
        self.joining = true;
        self.error = None;
        self.remember(&r);
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move { core::connect(&core, &r.server, &r.room, &r.name, r.title.as_deref()) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.joining = false;
                if let Err(e) = result {
                    s.error = Some(e);
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// An invite link from a click (backseat://join/CODE): go there, or ask for a name.
    fn handle_link(&mut self, link: &str, cx: &mut Context<Self>) {
        let Some(code) = join::parse_code(link) else { return };
        if self.current.as_ref().is_some_and(|c| c.room == code) {
            return;
        }
        if let Some(known) = self.rooms.iter().find(|r| r.room == code).cloned() {
            self.open_room(known, cx);
            return;
        }
        let server = core::built_in()
            .map(String::from)
            .or_else(|| settings().get("server").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(String::from))
            .or_else(|| self.current.as_ref().map(|c| c.server.clone()))
            .unwrap_or_else(|| self.join.server.read(cx).value().to_string());
        let name = self.current.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| self.join.name.read(cx).value().trim().to_string());
        if !name.is_empty() {
            self.open_room(Room { server, room: code, name, title: None }, cx);
            return;
        }
        // First time: ask for a name; the code goes into the form.
        self.join.mode = join::Mode::Code;
        self.join.pending_code = Some(join::fmt_code(&code));
        self.open_join(cx);
    }

    fn leave(&mut self, cx: &mut Context<Self>) {
        core::leave(&self.core);
        self.current = None;
        self.tick(cx);
    }

    /// Puts the room in the saved list (or updates it) and makes it the last one opened.
    fn remember(&mut self, r: &Room) {
        match self.rooms.iter_mut().find(|x| x.same(r)) {
            Some(x) => *x = r.clone(),
            None => self.rooms.push(r.clone()),
        }
        let mut state = core::state_load();
        state.insert("rooms".into(), serde_json::Value::Array(self.rooms.iter().map(Room::to_json).collect()));
        state.insert("lastRoom".into(), r.to_json());
        if let Err(e) = core::state_save(&serde_json::Value::Object(state).to_string()) {
            eprintln!("state: {e}");
        }
    }

    /// Tests only (STREAMER_APP_TEST_GPUI): what the window shows, when it changes.
    fn report(&mut self) {
        if std::env::var_os("STREAMER_APP_TEST_GPUI").is_none() {
            return;
        }
        let rail: Vec<String> = self.rooms.iter().map(|r| format!("{}={}", r.initials(), r.room)).collect();
        let peers: Vec<String> = self.snap.peers.iter().map(|p| format!("{}{}{}", p.name, if p.streaming { "(live)" } else { "" }, if p.me { "(me)" } else { "" })).collect();
        let line = format!(
            "rail [{}]; current {}; connected {}; room {}; peers {}; visible {}; join {}; error {}",
            rail.join(", "),
            self.current.as_ref().map(|c| c.room.as_str()).unwrap_or("-"),
            self.snap.connected,
            self.snap.room,
            peers.join(","),
            window_visible(),
            self.join_shown(),
            self.error.as_deref().unwrap_or("-")
        );
        if line != self.reported {
            eprintln!("test: {line}");
            self.reported = line;
        }
    }

    fn rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current.clone();
        div().w(px(72.)).h_full().flex_none().bg(rgb(color::RAIL)).flex().flex_col().items_center().gap_2().pt_3().children(self.rooms.iter().enumerate().map(|(i, r)| {
            let on = current.as_ref().is_some_and(|c| c.same(r));
            let room = r.clone();
            div()
                .id(("room", i))
                .size_12()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(if on { 16. } else { 24. }))
                .bg(rgb(if on { color::ACCENT } else { color::MAIN }))
                .text_color(rgb(color::BRIGHT))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .hover(|s| s.bg(rgb(color::ACCENT)).rounded(px(16.)))
                .child(r.initials())
                .on_click(cx.listener(move |this, _, _, cx| this.open_room(room.clone(), cx)))
        }))
        .child(
            div()
                .id("add-room")
                .size_12()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(24.))
                .bg(rgb(color::MAIN))
                .text_color(rgb(color::GREEN))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(color::GREEN)).text_color(rgb(color::BRIGHT)).rounded(px(16.)))
                // An icon, not a "+" character: a font's plus sits off centre.
                .child(gpui_kit::component::Icon::default().path("icons/plus.svg").size_6())
                .on_click(cx.listener(|this, _, _, cx| this.open_join(cx))),
        )
    }

    fn side(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let direct = self.snap.direct.is_some();
        let title = if direct { t("direct.title").to_string() } else { self.snap.title.clone().or_else(|| self.current.as_ref().map(Room::label)).unwrap_or_else(|| "Backseat".into()) };
        let invite = self.current.clone().filter(|_| self.snap.connected && self.snap.direct.is_none());
        let me = self.snap.peers.iter().find(|p| p.me).map(|p| p.name.clone()).or_else(|| self.current.as_ref().map(|c| c.name.clone())).unwrap_or_default();
        let live = self.snap.live.is_some();
        div()
            .w(px(240.))
            .h_full()
            .flex_none()
            .bg(rgb(color::SIDE))
            .flex()
            .flex_col()
            .child(
                div()
                    .h_12()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(color::LINE))
                    .child(div().flex_1().min_w_0().flex().items_center().overflow_hidden().whitespace_nowrap().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::BRIGHT)).child(title).when(direct, |d| d.child(beta())))
                    .when_some(invite, |d, r| {
                        d.child(icon_button("invite", "icons/user-plus.svg", t("room.invite"), color::BRIGHT).on_click(cx.listener(move |this, _, _, cx| {
                            let code = if join::is_code(&r.room) { join::fmt_code(&r.room) } else { r.room.clone() };
                            let text = if join::is_code(&r.room) { tf("invite.text", &[("title", &r.label()), ("code", &code), ("link", &join::invite_link(&r))]).to_string() } else { code.clone() };
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            this.error = Some(tf(if join::is_code(&r.room) { "room.copied" } else { "room.copied_plain" }, &[("code", &code)]).to_string());
                            this.error_since = Some(std::time::Instant::now());
                            cx.notify();
                        })))
                    })
                    .when(self.snap.connected, |d| {
                        d.child(icon_button("leave", "icons/log-out.svg", t("room.leave"), color::LIVE).on_click(cx.listener(|this, _, _, cx| this.leave(cx))))
                    }),
            )
            .child({
                let person = |p: &backseat_core::PeerView| {
                    row()
                        .child(avatar(&p.name))
                        .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_color(rgb(if p.me { color::BRIGHT } else { color::TEXT })).child(if p.me { format!("{} ({})", p.name, t("room.you")) } else { p.name.clone() }))
                        .when(p.streaming, |d| d.child(div().px_1p5().rounded_sm().bg(rgb(color::LIVE)).text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child("LIVE")))
                };
                let section = |t: String| div().px_4().pt_4().pb_1().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::MUTED)).child(t);
                let live: Vec<_> = self.snap.peers.iter().filter(|p| p.streaming).collect();
                let rest: Vec<_> = self.snap.peers.iter().filter(|p| !p.streaming).collect();
                div()
                    .id("people")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(self.snap.connected, |d| d.child(section(tf("room.live", &[("n", &live.len())]).to_string())).children(live.iter().map(|p| person(p))).child(section(tf("room.here", &[("n", &rest.len())]).to_string())).children(rest.iter().map(|p| person(p))))
            })
            .child(
                row()
                    .h(px(56.))
                    .bg(rgb(color::BAR))
                    .child(avatar(&me))
                    .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_color(rgb(color::BRIGHT)).font_weight(FontWeight::SEMIBOLD).child(me)).child(div().text_xs().text_color(rgb(if live { 0xff9aa0 } else { color::MUTED })).child(match &self.snap.live { Some(l) => room::live_for(l.since_secs), None => t("room.not_live") })))
                    .child(icon_button("settings-btn", "icons/settings.svg", t("set.open"), color::BRIGHT).on_click(cx.listener(|s, _, w, cx| s.open_settings(dialogs::SetTab::General, w, cx))))
                    .when(self.snap.connected && !live, |d| {
                        d.child(
                            gpui_kit::component::button::Button::new("go-live")
                                .primary()
                                .small()
                                .icon(gpui_kit::component::Icon::default().path("icons/monitor.svg"))
                                .label(t("room.go_live"))
                                .on_click(cx.listener(|s, _, w, cx| s.open_golive(false, w, cx))),
                        )
                    }),
            )
    }

    fn main(&self) -> impl IntoElement {
        let status = if self.joining {
            t("direct.connecting").to_string()
        } else if let Some(e) = &self.error {
            tf("join.failed", &[("e", e)]).to_string()
        } else if self.rooms.is_empty() {
            t("lobby.pick").to_string()
        } else {
            t("lobby.pick").to_string()
        };
        let header = self.snap.title.clone().or_else(|| self.current.as_ref().map(Room::label)).unwrap_or_default();
        div()
            .flex_1()
            .h_full()
            .bg(rgb(color::MAIN))
            .flex()
            .flex_col()
            .child(div().h_12().px_4().flex().items_center().gap_3().border_b_1().border_color(rgb(color::LINE)).child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(header)))
            .child(div().flex_1().flex().items_center().justify_center().text_color(rgb(color::MUTED)).child(status))
    }
}

/// The "BETA" badge, like the webview's: small, bold, on orange.
pub(crate) fn beta() -> Div {
    div().ml_1p5().px(px(5.)).py(px(1.)).rounded(px(4.)).bg(rgb(0xb5652a)).text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(rgb(0xffffff)).child("BETA")
}

/// A small icon button in a header: grey, `hover` colour under the pointer, a tooltip.
pub(crate) fn icon_button(id: &'static str, icon: &'static str, tip: SharedString, hover: u32) -> Stateful<Div> {
    use gpui_kit::component::{Icon, Sizable as _};
    div()
        .id(id)
        .size_7()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(color::MUTED))
        .hover(move |s| s.text_color(rgb(hover)).bg(rgb(0x3a3c42)))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
        .child(Icon::default().path(icon).small())
}

fn row() -> Div {
    div().flex().items_center().gap_2p5().px_4().py_1p5()
}

fn avatar(name: &str) -> Div {
    const COLORS: [u32; 7] = [0xc2622d, 0x3a8a5c, 0x5d5fa8, 0x7a4e9a, 0x2f7f95, 0x9a4e6b, 0x7a6a2e];
    let h = name.chars().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    div().size_8().flex_none().rounded_full().bg(rgb(COLORS[(h % COLORS.len() as u32) as usize])).flex().items_center().justify_center().text_color(rgb(color::BRIGHT)).font_weight(FontWeight::BOLD).child(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default())
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(code) = self.join.pending_code.take() {
            self.join.code.update(cx, |i, cx| i.set_value(code, window, cx));
        }
        self.sync_videos_hidden();
        self.sync_tiles(window, cx);
        let dialog = self.render_dialog(cx);
        if self.join_shown() {
            return div().size_full().relative().font_family("Segoe UI").text_size(px(14.)).text_color(rgb(color::TEXT)).child(self.render_join(window, cx)).children(dialog).into_any_element();
        }
        div()
            .relative()
            .flex()
            .size_full()
            .font_family("Segoe UI")
            .text_size(px(14.))
            .text_color(rgb(color::TEXT))
            .bg(rgb(color::MAIN))
            .child(self.rail(cx))
            .child(self.side(cx))
            .child(if self.snap.connected && !self.joining { self.render_room(cx) } else { self.main().into_any_element() })
            .children(dialog)
            .into_any_element()
    }
}

// Icons (lucide): gpui-kit's default set for its components, plus the few this app uses that
// the default set leaves out. Only these are in the exe, not the whole catalog.
gpui_kit::assets::icon_assets!(ExtraIcons, [UserPlus, LogOut, Volume2, VolumeX, SquareArrowOutUpRight, PictureInPicture, Maximize, Plus, Settings, Monitor]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn hwnd() -> windows::Win32::Foundation::HWND {
    windows::Win32::Foundation::HWND(MAIN_HWND.load(Ordering::SeqCst) as *mut _)
}

fn window_visible() -> bool {
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd()).as_bool() }
}

/// Brings the main window back (the tray, a later start).
pub fn show_main_window() {
    use windows::Win32::UI::WindowsAndMessaging::*;
    if MAIN_HWND.load(Ordering::SeqCst) == 0 {
        return;
    }
    unsafe {
        let h = hwnd();
        let _ = ShowWindow(h, if IsIconic(h).as_bool() { SW_RESTORE } else { SW_SHOW });
        let _ = SetForegroundWindow(h);
    }
}

fn hide_main_window() {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let _ = ShowWindow(hwnd(), SW_HIDE);
    }
}

/// Tests only: STREAMER_APP_TEST_GPUI=close asks the window to close at +8 s (as its X
/// button does) and shows it again at +12 s (as the tray does).
fn test_actions(cx: &mut Context<Shell>) {
    if std::env::var("STREAMER_APP_TEST_GPUI").as_deref() != Ok("close") {
        return;
    }
    cx.spawn(async move |_, cx: &mut AsyncApp| {
        cx.background_executor().timer(Duration::from_secs(8)).await;
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::*;
            let _ = PostMessageW(Some(hwnd()), WM_CLOSE, Default::default(), Default::default());
        }
        eprintln!("test: close asked");
        cx.background_executor().timer(Duration::from_secs(4)).await;
        show_main_window();
        eprintln!("test: show asked");
    })
    .detach();
}

fn main() {
    let Some(core) = core::start(|_, _| show_main_window()) else { return };
    i18n::init();
    if settings().get("tray").and_then(|v| v.as_bool()) == Some(false) {
        core.close_to_tray.store(false, Ordering::SeqCst);
    }
    gpui_kit::application().with_assets(AppAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        // The webview's colours for fields and buttons: fields darker than the card with a
        // visible border, blue primary buttons, grey ones.
        {
            let t = Theme::global_mut(cx);
            t.background = rgb(color::RAIL).into();
            t.input = rgb(0x3f4147).into();
            t.border = rgb(0x3f4147).into();
            t.foreground = rgb(color::BRIGHT).into();
            t.muted_foreground = rgb(color::MUTED).into();
            t.primary = rgb(0x3b6fd8).into();
            t.primary_hover = rgb(0x4a7ee6).into();
            t.primary_foreground = rgb(0xffffff).into();
            t.secondary = rgb(0x4e5058).into();
            t.secondary_hover = rgb(0x5c5e66).into();
            t.secondary_foreground = rgb(0xffffff).into();
            t.ring = rgb(0x3b6fd8).into();
            // Scrollbars stay visible, like the webview's, so a tall page shows it scrolls.
            t.scrollbar_mode = gpui_kit::component::scroll::ScrollbarMode::Always;
            // Buttons read their colours from the theme's tokens.
            let h = |c: u32| -> Hsla { rgb(c).into() };
            t.tokens.button_primary = h(0x3b6fd8).into();
            t.tokens.button_primary_hover = h(0x4a7ee6).into();
            t.tokens.button_primary_active = h(0x335fba).into();
            t.tokens.button_primary_foreground = h(0xffffff).into();
            t.tokens.button = h(0x4e5058).into();
            t.tokens.button_hover = h(0x5c5e66).into();
            t.tokens.button_active = h(0x45474e).into();
            t.tokens.button_foreground = h(0xffffff).into();
            // ...and their text colour from the plain colours.
            t.button_primary_foreground = h(0xffffff);
            t.button_foreground = h(0xffffff);
            // Checkboxes and other accents read the primary token: blue, as the buttons.
            t.tokens.primary = h(0x3b6fd8).into();
            t.tokens.primary_hover = h(0x4a7ee6).into();
            t.tokens.primary_active = h(0x335fba).into();
            t.tokens.button_danger = h(0xa1282f).into();
            t.tokens.button_danger_hover = h(0xb8323a).into();
            t.tokens.button_danger_active = h(0x8c2229).into();
            t.tokens.button_danger_foreground = h(0xffffff).into();
            t.button_danger_foreground = h(0xffffff);
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1280.), px(800.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Backseat".into()), ..Default::default() }),
            ..Default::default()
        };
        let c = core.clone();
        let opened = gpui_kit::open_window(options, cx, move |window, cx| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Win32(h)) = window.window_handle().map(|h| h.as_raw()) {
                MAIN_HWND.store(h.hwnd.get(), Ordering::SeqCst);
                // Video windows go inside the main window (the UI gives each a place).
                media::present::set_default_parent(h.hwnd.get());
            }
            // Closing hides the window in the tray (the default); the tray's Quit ends the app.
            let close = c.clone();
            window.on_window_should_close(cx, move |_, cx| {
                if close.close_to_tray.load(Ordering::SeqCst) {
                    hide_main_window();
                    eprintln!("window: hidden in the tray");
                    return false;
                }
                core::leave_for_good(&close, Duration::from_millis(500));
                cx.quit();
                true
            });
            let c = c.clone();
            cx.new(|cx| Shell::new(c, window, cx))
        });
        if let Err(e) = opened {
            eprintln!("window: {e}");
            cx.quit();
            return;
        }
        match tray::start(core.clone()) {
            Ok(()) => eprintln!("tray: ready"),
            Err(e) => eprintln!("tray: {e}"),
        }
        cx.activate(true);
    });
}
