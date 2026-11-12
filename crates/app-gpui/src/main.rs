//! Backseat with a native UI: GPUI and gpui-component instead of the webview. Everything
//! apart from the window is `backseat-core`. Stage 1: the window, rooms in the rail (from
//! the state file), people in the open room, a 1 s refresh, the tray.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dialogs;
mod i18n;
mod join;
mod room;
mod testing;
mod voice_ui;
mod tray;

use backseat_core::{self as core, App as Core, Snapshot};
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::{Sizable as _, Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

actions!(backseat, [VideoBadge, VideoStats, VideoFullscreen, VideoWindowed]);
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
    /// A room's menu (right click in the rail) and where it opened.
    room_menu: Option<(Room, Point<Pixels>)>,
    /// A voice member's volume (right click on them).
    voice_menu: Option<voice_ui::VoiceMenu>,
    /// The voice members the saved volumes were last given to.
    volumes_for: Vec<u64>,
    /// A newer release found by the update check.
    update: Option<core::update::Release>,
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
        let mut shell = Shell { core, snap, rooms, current: None, joining: false, error: None, reported: String::new(), join, prev_direct: None, reset_pending: false, room: room::Room::default(), error_since: None, dialogs: Default::default(), subs: vec![], ticks: 0, room_menu: None, update: None, voice_menu: None, volumes_for: vec![] };
        voice_ui::apply_settings(&shell.core);
        // With a saved room, start in the last one opened. Not when a test joins by itself.
        if ["STREAMER_APP_AUTOJOIN", "STREAMER_APP_TEST_DIRECT", "STREAMER_APP_TEST_FORM"].iter().all(|k| std::env::var_os(k).is_none() || (*k == "STREAMER_APP_TEST_FORM" && std::env::var(k).is_ok_and(|v| v.starts_with("show")))) {
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
        // "Who is speaking" needs more than the 1 s refresh: the voice part alone, 6 times a
        // second, redrawing only on a change.
        cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| loop {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let alive = this.update_in(cx, |s, _, cx| {
                if !s.snap.connected {
                    return;
                }
                let v = core::voice_view(&s.core);
                if v != s.snap.voice {
                    s.snap.voice = v;
                    cx.notify();
                }
                s.apply_volumes();
                voice_ui::test_volume(s);
                if s.dialogs.settings.as_ref().is_some_and(|d| d.tab == dialogs::SetTab::Voice) && s.snap.voice.joined {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        test_actions(cx);
        testing::test_voice(window, cx);
        join::test_direct(window, cx);
        join::test_form(window, cx);
        room::test_click_watch(window, cx);
        room::test_quality(window, cx);
        dialogs::test_dialogs(window, cx);
        testing::test_form_more(window, cx);
        testing::test_rooms(window, cx);
        testing::test_ui_stress(window, cx);
        testing::on_main(window, cx);
        shell.start_updates(cx);
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

    /// The rail's menu for a room: open it, copy its invite, change my name in it, remove it.
    fn render_room_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (r, at) = self.room_menu.clone()?;
        let is_current = self.current.as_ref().is_some_and(|c| c.same(&r)) && self.snap.connected;
        let item = |id: &'static str, label: SharedString, danger: bool| {
            div()
                .id(id)
                .px_3()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if danger { 0xff8a8f } else { color::TEXT }))
                .hover(move |s| s.bg(rgb(if danger { 0xa1282f } else { 0x3b6fd8 })).text_color(rgb(color::BRIGHT)))
                .child(label)
        };
        let (r1, r2, r3, r4) = (r.clone(), r.clone(), r.clone(), r.clone());
        Some(
            div()
                .id("room-menu-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.room_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.room_menu = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("room-menu")
                        .absolute()
                        .left(at.x)
                        .top(at.y)
                        .w(px(220.))
                        .p_1p5()
                        .rounded_lg()
                        .bg(rgb(0x111214))
                        .border_1()
                        .border_color(rgb(color::LINE))
                        .flex()
                        .flex_col()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .when(!is_current, |d| {
                            d.child(item("rm-open", t("rooms.open"), false).on_click(cx.listener(move |this, _, _, cx| {
                                this.room_menu = None;
                                this.open_room(r1.clone(), cx);
                            })))
                        })
                        .child(item("rm-invite", t("rooms.copy_invite"), false).on_click(cx.listener(move |this, _, _, cx| {
                            this.room_menu = None;
                            this.copy_invite(&r2, cx);
                        })))
                        .child(item("rm-rename", t("rooms.rename"), false).on_click(cx.listener(move |this, _, w, cx| {
                            this.room_menu = None;
                            this.open_rename(r3.clone(), w, cx);
                        })))
                        .child(item("rm-remove", t("rooms.remove"), true).on_click(cx.listener(move |this, _, _, cx| {
                            this.room_menu = None;
                            this.forget_room(&r4);
                            cx.notify();
                        }))),
                )
                .into_any_element(),
        )
    }

    /// Copies a room's invite (a code room: text with the link and code; an older room: its name).
    fn copy_invite(&mut self, r: &Room, cx: &mut Context<Self>) {
        let code = if join::is_code(&r.room) { join::fmt_code(&r.room) } else { r.room.clone() };
        let text = if join::is_code(&r.room) { tf("invite.text", &[("title", &r.label()), ("code", &code), ("link", &join::invite_link(r))]).to_string() } else { code.clone() };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.error = Some(tf(if join::is_code(&r.room) { "room.copied" } else { "room.copied_plain" }, &[("code", &code)]).to_string());
        self.error_since = Some(std::time::Instant::now());
        cx.notify();
    }

    /// Takes a room off the saved list (the open one stays open).
    fn forget_room(&mut self, r: &Room) {
        self.rooms.retain(|x| !x.same(r));
        core::leave_background(&self.core, &r.room);
        let mut state = core::state_load();
        state.insert("rooms".into(), serde_json::Value::Array(self.rooms.iter().map(Room::to_json).collect()));
        let _ = core::state_save(&serde_json::Value::Object(state).to_string());
    }

    /// The new name for a room; in the open room it takes effect by joining again.
    pub fn rename_room(&mut self, r: &Room, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() || name == r.name {
            return;
        }
        let next = Room { name: name.to_string(), ..r.clone() };
        // Kept in the background under the old name: that place goes.
        core::leave_background(&self.core, &r.room);
        for x in self.rooms.iter_mut().filter(|x| x.same(r)) {
            *x = next.clone();
        }
        let mut state = core::state_load();
        state.insert("rooms".into(), serde_json::Value::Array(self.rooms.iter().map(Room::to_json).collect()));
        let _ = core::state_save(&serde_json::Value::Object(state).to_string());
        if self.current.as_ref().is_some_and(|c| c.same(r)) && self.snap.connected {
            self.current = None;
            core::leave(&self.core);
            self.snap = core::snapshot(&self.core);
            self.open_room(next, cx);
        }
        cx.notify();
    }

    /// Looks for a newer release 1.5 s after start (and installs it unless that is off: nothing
    /// is watched or streamed yet), then every 6 hours only to show it.
    fn start_updates(&mut self, cx: &mut Context<Self>) {
        if !core::updates_enabled() {
            return;
        }
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let ex = cx.background_executor().clone();
            ex.timer(Duration::from_millis(1500)).await;
            let mut first = true;
            loop {
                let Ok(core) = this.update(cx, |s, _| s.core.clone()) else { return };
                let r = ex.spawn(async move { core::check_update(&core) }).await;
                let install = first && settings().get("autoUpdate").and_then(|v| v.as_bool()) != Some(false);
                let newer = r.as_ref().is_ok_and(|r| r.newer);
                let _ = this.update(cx, |s, cx| {
                    s.update = r.ok().filter(|r| r.newer);
                    cx.notify();
                });
                if newer && install {
                    let Ok(core) = this.update(cx, |s, _| s.core.clone()) else { return };
                    let quit = ex.spawn(async move { core::install_update(&core) }).await;
                    match quit {
                        Ok(true) => {
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                        Ok(false) => {}
                        Err(e) => eprintln!("update: {e}"),
                    }
                }
                first = false;
                ex.timer(Duration::from_secs(6 * 3600)).await;
            }
        })
        .detach();
    }

    pub(crate) fn leave(&mut self, cx: &mut Context<Self>) {
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
        // Many rooms scroll; their buttons keep their size (flex_none) instead of squeezing.
        div().id("rail").w(px(72.)).h_full().flex_none().bg(rgb(color::RAIL)).overflow_y_scroll().flex().flex_col().items_center().gap_2().py_3().children(self.rooms.iter().enumerate().map(|(i, r)| {
            let on = current.as_ref().is_some_and(|c| c.same(r));
            // Still connected in the background (watching or streaming there): a green dot.
            let kept = !on && self.snap.background.iter().any(|b| *b == r.room);
            let room = r.clone();
            div()
                .id(("room", i))
                .relative()
                .when(kept, |d| d.child(div().absolute().right(px(-2.)).bottom(px(-2.)).size(px(16.)).rounded_full().bg(rgb(color::RAIL)).flex().items_center().justify_center().child(div().size(px(10.)).rounded_full().bg(rgb(color::GREEN)))))
                .size_12()
                .flex_none()
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
                .on_mouse_down(MouseButton::Right, {
                    let room = r.clone();
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        this.room_menu = Some((room.clone(), e.position));
                        cx.notify();
                    })
                })
        }))
        .child(
            div()
                .id("add-room")
                .size_12()
                .flex_none()
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
        .when_some(self.update.clone(), |d, u| {
            d.child(
                div()
                    .id("update")
                    .size_12()
                .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(24.))
                    .bg(rgb(color::MAIN))
                    .text_color(rgb(color::GREEN))
                    .text_size(px(22.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(color::GREEN)).text_color(rgb(color::BRIGHT)).rounded(px(16.)))
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tf("set.available", &[("v", &u.latest)])).build(window, cx))
                    .child("↓")
                    .on_click(cx.listener(|this, _, w, cx| this.open_settings(dialogs::SetTab::Updates, w, cx))),
            )
        })
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
                let v = &self.snap.voice;
                let in_voice: Vec<_> = self.snap.peers.iter().filter(|p| v.members.contains(&p.id)).collect();
                let voice_person = |p: &backseat_core::PeerView| {
                    use gpui_kit::component::{Icon, Sizable as _};
                    let (id, name, me) = (p.id, p.name.clone(), p.me);
                    let muted = if p.me { v.mic_muted || v.deafened } else { v.muted.contains(&p.id) };
                    let deaf = if p.me { v.deafened } else { v.deaf.contains(&p.id) };
                    row()
                        .py_1()
                        // Right click: this member's volume (not for ourselves).
                        .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            if !me {
                                this.open_voice_menu(id, name.clone(), e.position, window, cx);
                            }
                        }))
                        .child(voice_avatar(&p.name, v.speaking.contains(&p.id)))
                        .child(div().flex_1().min_w_0().truncate().text_color(rgb(if v.speaking.contains(&p.id) { color::BRIGHT } else { color::TEXT })).child(p.name.clone()))
                        .when(muted, |d| d.child(div().text_color(rgb(color::LIVE)).child(Icon::default().path("icons/mic-off.svg").small())))
                        .when(deaf, |d| d.child(div().text_color(rgb(color::LIVE)).child(Icon::default().path("icons/headphone-off.svg").small())))
                };
                div()
                    .id("people")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(self.snap.connected && self.snap.direct.is_none(), |d| {
                        use gpui_kit::component::{Icon, Sizable as _};
                        d.child(section(tf("voice.section", &[("n", &in_voice.len())]).to_string()))
                            .children(in_voice.iter().map(|p| voice_person(p)))
                            .when(!v.joined, |d| {
                                d.child(
                                    row()
                                        .id("join-voice")
                                        .mx_2()
                                        .px_2()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(rgb(color::MUTED))
                                        .hover(|s| s.bg(rgb(0x35373c)).text_color(rgb(color::BRIGHT)))
                                        .child(Icon::default().path("icons/headphones.svg").small())
                                        .child(t("voice.join"))
                                        .on_click(cx.listener(|s, _, _, cx| s.join_voice(cx))),
                                )
                            })
                    })
                    .when(self.snap.connected, |d| d.child(section(tf("room.live", &[("n", &live.len())]).to_string())).children(live.iter().map(|p| person(p))).child(section(tf("room.here", &[("n", &rest.len())]).to_string())).children(rest.iter().map(|p| person(p))))
            })
            .when(self.snap.connected && (self.snap.voice.joined || self.snap.voice.elsewhere.is_some()), |d| d.child(self.voice_bar(cx)))
            .child(
                row()
                    .h(px(56.))
                    .bg(rgb(color::BAR))
                    .child(avatar(&me))
                    // One line each; a long name ends with "…" instead of wrapping.
                    .child(div().flex_1().min_w_0().flex().flex_col().child(div().truncate().text_color(rgb(color::BRIGHT)).font_weight(FontWeight::SEMIBOLD).child(me)).child(div().truncate().text_xs().text_color(rgb(if live { 0xff9aa0 } else { color::MUTED })).child(match &self.snap.live { Some(l) => room::live_for(l.since_secs), None => t("room.not_live") })))
                    .child(icon_button("settings-btn", "icons/settings.svg", t("set.open"), color::BRIGHT).on_click(cx.listener(|s, _, w, cx| s.open_settings(dialogs::SetTab::General, w, cx))))
                    .when(self.snap.connected && !live, |d| {
                        d.child(
                            gpui_kit::component::button::Button::new("go-live").cursor_pointer()
                                .primary()
                                .small()
                                .icon(gpui_kit::component::Icon::default().path("icons/monitor.svg"))
                                // An icon (with its name as a tooltip): the bar is narrow, the name needs the room.
                                .tooltip(t("room.go_live"))
                                .on_click(cx.listener(|s, _, w, cx| s.open_golive(false, w, cx))),
                        )
                    }),
            )
    }

    /// "Voice connected" with the microphone, sound and hang-up buttons, above the name.
    fn voice_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let v = &self.snap.voice;
        let (mic_off, deaf) = (v.mic_muted || v.deafened, v.deafened);
        // Tighter than the other rows: three buttons and the words share 240 px.
        div()
            .flex()
            .items_center()
            .gap_0p5()
            .pl_4()
            .pr_2()
            .h(px(44.))
            .bg(rgb(color::BAR))
            .border_b_1()
            .border_color(rgb(color::LINE))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::GREEN)).child(t("voice.connected")))
                    // In voice in a room kept in the background: which one, under it (as Discord).
                    .when_some(v.elsewhere.clone(), |d, code| {
                        let label = self.rooms.iter().find(|r| r.room == code).map(|r| r.label()).unwrap_or(code);
                        d.child(div().truncate().text_xs().text_color(rgb(color::MUTED)).child(label))
                    }),
            )
            .child(
                icon_button("voice-mic", if mic_off { "icons/mic-off.svg" } else { "icons/mic.svg" }, t(if mic_off { "voice.unmute" } else { "voice.mute" }), color::BRIGHT)
                    .when(mic_off, |d| d.text_color(rgb(color::LIVE)))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        core::set_mic_muted(&s.core, !mic_off);
                        s.snap.voice = core::voice_view(&s.core);
                        cx.notify();
                    })),
            )
            .child(
                icon_button("voice-deaf", if deaf { "icons/headphone-off.svg" } else { "icons/headphones.svg" }, t(if deaf { "voice.undeafen" } else { "voice.deafen" }), color::BRIGHT)
                    .when(deaf, |d| d.text_color(rgb(color::LIVE)))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        core::set_deafened(&s.core, !deaf);
                        s.snap.voice = core::voice_view(&s.core);
                        cx.notify();
                    })),
            )
            .child(icon_button("voice-leave", "icons/phone-off.svg", t("voice.leave"), color::LIVE).on_click(cx.listener(|s, _, _, cx| {
                core::leave_voice(&s.core);
                s.snap.voice = core::voice_view(&s.core);
                cx.notify();
            })))
    }

    /// Joins the room's voice. Opening the microphone may take a moment: off the UI thread.
    pub(crate) fn join_voice(&mut self, cx: &mut Context<Self>) {
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move { core::join_voice(&core) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Err(e) = result {
                    s.error = Some(tf("voice.failed", &[("e", &e)]).to_string());
                    s.error_since = Some(std::time::Instant::now());
                }
                s.snap.voice = core::voice_view(&s.core);
                cx.notify();
            });
        })
        .detach();
    }

    /// F3, Shift+F3, F11 and Esc for the video. F3 the badge, Shift+F3 the detailed stats:
    /// each one state for all videos (on if no video shows it). F11 and Esc: the most
    /// recently opened video.
    fn video_keys(&mut self, key: &str, shift: bool, cx: &mut Context<Self>) {
        let Some(w) = self.snap.watches.last().cloned() else { return };
        let any_stats = self.snap.watches.iter().any(|w| w.overlay);
        let any_badge = self.snap.watches.iter().any(|w| w.badge);
        let action = match key {
            "f3" if shift => if any_stats { "no_overlay" } else { "overlay" },
            "f3" => if any_badge { "no_badge" } else { "badge" },
            "f11" => "toggle_fullscreen",
            "escape" if w.fullscreen => "windowed",
            _ => return,
        };
        if let Err(e) = core::video_window_do(&self.core, w.streamer, action) {
            eprintln!("keys: {action}: {e}");
        }
        eprintln!("keys: {key} -> {action}");
        self.snap.watches = core::watches(&self.core);
        cx.notify();
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

/// An avatar with a green ring while its person speaks (the ring's room is always kept, so
/// nothing moves when it shows).
fn voice_avatar(name: &str, speaking: bool) -> Div {
    div().p(px(2.)).flex_none().rounded_full().border_2().border_color(rgb(if speaking { color::GREEN } else { color::SIDE })).child(avatar(name).size_6().text_xs())
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
        self.sync_videos_hidden(cx);
        self.sync_tiles(window, cx);
        let dialog = self.render_dialog(cx);
        let menu = self.render_room_menu(cx);
        let voice_menu = self.render_voice_menu(cx);
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
            .children(menu)
            .children(voice_menu)
            .children(dialog)
            .into_any_element()
    }
}

// Icons (lucide): gpui-kit's default set for its components, plus the few this app uses that
// the default set leaves out. Only these are in the exe, not the whole catalog.
gpui_kit::assets::icon_assets!(ExtraIcons, [UserPlus, LogOut, Volume2, VolumeX, SquareArrowOutUpRight, PictureInPicture, Maximize, Plus, Settings, Monitor, Mic, MicOff, Headphones, HeadphoneOff, PhoneOff]);

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
    testing::apply_test_prefs();
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
                core::leave_everything(&close, Duration::from_millis(500));
                cx.quit();
                true
            });
            let c = c.clone();
            let shell = cx.new(|cx| Shell::new(c, window, cx));
            // The video's keys anywhere in the app, not only after a click on the video itself
            // (which has its own window and focus). App-wide actions: they run with nothing
            // focused too, where an element's key listener never hears the key.
            cx.bind_keys([
                KeyBinding::new("f3", VideoBadge, None),
                KeyBinding::new("shift-f3", VideoStats, None),
                KeyBinding::new("f11", VideoFullscreen, None),
                KeyBinding::new("escape", VideoWindowed, None),
            ]);
            for (key, shift) in [("f3", false), ("shift-f3", true), ("f11", false), ("escape", false)] {
                let w = shell.downgrade();
                let run = move |cx: &mut App| {
                    let _ = w.update(cx, |s, cx| s.video_keys(key.trim_start_matches("shift-"), shift, cx));
                };
                match key {
                    "f3" => cx.on_action(move |_: &VideoBadge, cx| run(cx)),
                    "shift-f3" => cx.on_action(move |_: &VideoStats, cx| run(cx)),
                    "f11" => cx.on_action(move |_: &VideoFullscreen, cx| run(cx)),
                    _ => cx.on_action(move |_: &VideoWindowed, cx| run(cx)),
                };
            }
            shell
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
