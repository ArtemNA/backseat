//! The join form: make a room, join one by its invite code, or a direct connection (beta)
//! where two people swap two texts (or short codes for them) through any messenger.
//! Same rules as the webview UI's form (app.js, renderJoinForm and renderDirectSteps).

use crate::i18n::{t, tf, tu};
use crate::{color, settings, set_setting, Room, Shell};
use backseat_core as core;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Create,
    Code,
    Direct,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Role {
    Invite,
    Answer,
}

/// A short code for one of our texts.
#[derive(Clone, Debug)]
pub enum Code {
    Valid { code: String, until: Instant },
    /// The service did not answer: only the full text.
    Failed,
    Expired,
}

/// Where a direct connection stands, from our side.
#[derive(Default)]
pub struct Step {
    pub invite: Option<String>,
    pub invite_code: Option<Code>,
    pub answer: Option<String>,
    pub answer_code: Option<Code>,
    pub accepted: bool,
    /// A button's work is running (a few seconds).
    pub making: bool,
}

pub struct Join {
    pub open: bool,
    pub mode: Mode,
    pub role: Role,
    pub server: Entity<InputState>,
    pub name: Entity<InputState>,
    pub title: Entity<InputState>,
    pub code: Entity<InputState>,
    /// What we got from the friend: their invite or their answer (or a short code).
    pub incoming: Entity<TextareaState>,
    /// Our text to send, read only.
    pub outgoing: Entity<TextareaState>,
    pub error: Option<String>,
    pub busy: bool,
    pub step: Step,
    /// The direct connection is ours to close the form for once it connects.
    pub direct_open: bool,
    /// Which copy button was pressed last ("code" or "text"), to say "Copied".
    pub copied: Option<&'static str>,
    /// An invite code to put in the code field at the next draw (it needs the window).
    pub pending_code: Option<String>,
    /// The page's scroll, so a tall form (the direct texts) scrolls inside the window.
    pub scroll: ScrollHandle,
    /// The language, also on the form (as the webview had it), and its listener.
    pub lang: Entity<SelectState<Vec<SharedString>>>,
    _lang_sub: Subscription,
}

fn lang_row() -> usize {
    let now = crate::i18n::current();
    crate::i18n::LANGS.iter().position(|(id, _)| *id == now).unwrap_or(0)
}

// Rooms made here are named by a random invite code (Crockford base32, 12 symbols, about
// 60 bits): nobody finds them by guessing. Older rooms are plain names anyone can type.
const CODE_ABC: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn new_code() -> String {
    let mut b = [0u8; 12];
    unsafe {
        use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
        let _ = BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
    }
    b.iter().map(|x| CODE_ABC[(x & 31) as usize] as char).collect()
}

pub fn is_code(room: &str) -> bool {
    room.len() == 12 && room.bytes().all(|c| CODE_ABC.contains(&c))
}

/// The https page for a room's invite, on the room's own server (ws -> http, wss -> https).
pub fn invite_link(r: &Room) -> String {
    let rest = r.server.strip_prefix("ws").unwrap_or(&r.server);
    let host = rest.split("://").nth(1).unwrap_or_default().split('/').next().unwrap_or_default();
    let proto = rest.split("://").next().unwrap_or_default();
    format!("http{proto}://{host}/j/{}", fmt_code(&r.room))
}

pub fn fmt_code(c: &str) -> String {
    c.as_bytes().chunks(4).map(|x| String::from_utf8_lossy(x).into_owned()).collect::<Vec<_>>().join("-")
}

/// A code or a link carrying one, as typed or pasted; None if it is neither.
pub fn parse_code(text: &str) -> Option<String> {
    let mut v = text.trim().to_string();
    for key in ["/j/", "join/"] {
        if let Some(i) = v.find(key) {
            v = v[i + key.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
            break;
        }
    }
    let v: String = v
        .to_uppercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            'U' => 'V',
            c => c,
        })
        .collect();
    is_code(&v).then_some(v)
}

/// A pasted direct text is a short code when, without spaces and hyphens, it is 8 letters
/// and digits.
fn looks_like_short(v: &str) -> bool {
    let v: String = v.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    v.len() == 8 && v.chars().all(|c| c.is_ascii_alphanumeric())
}

pub fn show_short(c: &str) -> String {
    format!("{}-{}", &c[..4.min(c.len())], &c[4.min(c.len())..])
}

fn clock(left: Duration) -> String {
    let s = left.as_secs_f64().ceil() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn short_codes_on() -> bool {
    settings().get("shortCode").and_then(|v| v.as_bool()) != Some(false)
}

/// A short code for a text, or what went wrong; None when short codes are off. Blocks.
fn short_code_for(text: &str) -> Option<Code> {
    if !short_codes_on() {
        return None;
    }
    Some(match core::shortcode_put(text) {
        Ok(r) => Code::Valid { code: r.code, until: Instant::now() + Duration::from_secs(r.expires_in) },
        Err(_) => Code::Failed,
    })
}

/// What the person pasted: a full text as is, or the text behind a short code. Blocks.
fn pasted_text(value: &str) -> Result<String, String> {
    let v = value.trim();
    if !looks_like_short(v) {
        return Ok(v.to_string());
    }
    if !short_codes_on() {
        return Err(t("direct.codes_off").to_string());
    }
    core::shortcode_get(v).map_err(|e| if e == "not_found" { t("direct.code_not_found").to_string() } else { tf("direct.code_failed", &[("e", &e)]).to_string() })
}

impl Join {
    pub fn new(window: &mut Window, cx: &mut Context<Shell>) -> Join {
        let state = core::state_load();
        let saved = state.get("join");
        let s = |k: &str| saved.and_then(|j| j.get(k)).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let default_server = settings().get("server").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(String::from).unwrap_or_else(|| {
            let v = s("server");
            if v.is_empty() { "ws://127.0.0.1:7800/ws".into() } else { v }
        });
        let server = core::built_in().map(String::from).unwrap_or(default_server);
        let name = s("name");
        let names: Vec<SharedString> = crate::i18n::LANGS.iter().map(|(_, n)| SharedString::from(*n)).collect();
        let lang = cx.new(|cx| SelectState::new(names, Some(IndexPath::new(lang_row())), window, cx));
        let _lang_sub = cx.subscribe_in(&lang, window, |_, p, e: &SelectEvent<Vec<SharedString>>, _, cx| {
            let SelectEvent::Confirm(_) = e;
            let row = p.read(cx).selected_index(cx).map_or(0, |i| i.row);
            if let Some((id, _)) = crate::i18n::LANGS.get(row) {
                crate::i18n::set_language(id);
            }
            cx.notify();
        });
        Join {
            open: false,
            mode: Mode::Create,
            role: Role::Invite,
            server: cx.new(|cx| InputState::new(window, cx).default_value(server)),
            name: cx.new(|cx| InputState::new(window, cx).placeholder(t("connect.name")).default_value(name)),
            title: cx.new(|cx| InputState::new(window, cx).placeholder(t("join.room_title"))),
            code: cx.new(|cx| InputState::new(window, cx).placeholder("K7Q2-9XMB-4TQ1")),
            incoming: cx.new(|cx| TextareaState::new(window, cx).rows(4)),
            outgoing: cx.new(|cx| TextareaState::new(window, cx).rows(4)),
            error: None,
            busy: false,
            step: Step::default(),
            direct_open: false,
            copied: None,
            pending_code: None,
            scroll: ScrollHandle::new(),
            lang,
            _lang_sub,
        }
    }
}

impl Shell {
    pub fn join_shown(&self) -> bool {
        self.join.open || (!self.snap.connected && self.rooms.is_empty())
    }

    pub fn open_join(&mut self, cx: &mut Context<Self>) {
        self.join.open = true;
        self.join.error = None;
        cx.notify();
    }

    fn field(&self, state: &Entity<InputState>, cx: &App) -> String {
        state.read(cx).value().trim().to_string()
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.join.mode = mode;
        self.join.error = None;
        cx.notify();
    }

    pub fn set_role(&mut self, role: Role, window: &mut Window, cx: &mut Context<Self>) {
        self.join.role = role;
        self.reset_direct(window, cx);
    }

    /// The direct form starts over (a new role, or the connection ended).
    pub fn reset_direct(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.join.step = Step::default();
        self.join.copied = None;
        self.join.incoming.update(cx, |s, cx| s.set_value("", window, cx));
        self.join.outgoing.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// The form's main button: make a room or join one by its invite.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        self.join.error = None;
        let server = self.field(&self.join.server.clone(), cx);
        let name = self.field(&self.join.name.clone(), cx);
        let room = match self.join.mode {
            Mode::Direct => return,
            Mode::Create => {
                let title = self.field(&self.join.title.clone(), cx);
                if title.is_empty() {
                    self.join.error = Some(t("join.need_title").to_string());
                    cx.notify();
                    return;
                }
                Room { server, room: new_code(), name, title: Some(title) }
            }
            Mode::Code => match parse_code(&self.field(&self.join.code.clone(), cx)) {
                Some(code) => Room { server, room: code, name, title: None },
                None => {
                    self.join.error = Some(t("join.bad_code").to_string());
                    cx.notify();
                    return;
                }
            },
        };
        if room.name.is_empty() {
            self.join.error = Some(t("direct.need_name").to_string());
            cx.notify();
            return;
        }
        self.join_room(room, cx);
    }

    /// Joins from the form; on success the form closes and the room is saved.
    fn join_room(&mut self, r: Room, cx: &mut Context<Self>) {
        self.join.busy = true;
        let core = self.core.clone();
        let room = r.clone();
        let task = cx.background_executor().spawn(async move { core::connect(&core, &room.server, &room.room, &room.name, room.title.as_deref()) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.join.busy = false;
                match result {
                    Ok(()) => {
                        s.current = Some(r.clone());
                        s.remember(&r);
                        let mut state = core::state_load();
                        state.insert("join".into(), r.to_json());
                        let _ = core::state_save(&serde_json::Value::Object(state).to_string());
                        s.join.open = false;
                        s.error = None;
                    }
                    Err(e) => s.join.error = Some(e),
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// "Make the invite": our invite text, then a short code for it.
    pub fn make_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.field(&self.join.name.clone(), cx);
        if name.is_empty() {
            self.join.error = Some(t("direct.need_name").to_string());
            cx.notify();
            return;
        }
        self.join.error = None;
        self.join.step.making = true;
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move {
            let text = core::direct_invite(&core, &name)?;
            let code = short_code_for(&text);
            Ok::<_, String>((text, code))
        });
        cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |s, window, cx| {
                s.join.step.making = false;
                match result {
                    Ok((text, code)) => {
                        s.join.outgoing.update(cx, |o, cx| o.set_value(text.clone(), window, cx));
                        s.join.step.invite = Some(text);
                        s.join.step.invite_code = code;
                        s.join.open = true;
                    }
                    Err(e) => s.join.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// "Connect": the friend's answer (or its short code).
    pub fn accept_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pasted = self.join.incoming.read(cx).value().to_string();
        self.join.error = None;
        self.join.step.making = true;
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move { core::direct_accept(&core, &pasted_text(&pasted)?) });
        cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |s, _, cx| {
                s.join.step.making = false;
                match result {
                    Ok(()) => {
                        s.join.step.accepted = true;
                        s.join.direct_open = true;
                    }
                    Err(e) => s.join.error = Some(e),
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// "Make the answer": takes the friend's invite (or its short code), gives our answer.
    pub fn make_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.field(&self.join.name.clone(), cx);
        if name.is_empty() {
            self.join.error = Some(t("direct.need_name").to_string());
            cx.notify();
            return;
        }
        let pasted = self.join.incoming.read(cx).value().to_string();
        self.join.error = None;
        self.join.step.making = true;
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move {
            let text = core::direct_answer(&core, &pasted_text(&pasted)?, &name)?;
            let code = short_code_for(&text);
            Ok::<_, String>((text, code))
        });
        cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |s, window, cx| {
                s.join.step.making = false;
                match result {
                    Ok((text, code)) => {
                        s.join.direct_open = true;
                        s.join.outgoing.update(cx, |o, cx| o.set_value(text.clone(), window, cx));
                        s.join.step.answer = Some(text);
                        s.join.step.answer_code = code;
                        s.join.open = true;
                    }
                    Err(e) => s.join.error = Some(e),
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Codes past their 5 minutes become "expired".
    pub fn expire_codes(&mut self) {
        for c in [&mut self.join.step.invite_code, &mut self.join.step.answer_code] {
            if let Some(Code::Valid { until, .. }) = c {
                if *until <= Instant::now() {
                    *c = Some(Code::Expired);
                }
            }
        }
    }

    /// The code shown with our text, as the test driver reads it: "K7Q2-9XMB valid 4:59".
    pub fn shown_code(&self) -> Option<String> {
        let code = if self.join.step.answer.is_some() { &self.join.step.answer_code } else { &self.join.step.invite_code };
        match code {
            Some(Code::Valid { code, until }) => Some(format!("{} valid {}", show_short(code), clock(until.saturating_duration_since(Instant::now())))),
            _ => None,
        }
    }

    pub fn render_join(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The language may have been changed in the settings meanwhile.
        let row = lang_row();
        if self.join.lang.read(cx).selected_index(cx).map(|i| i.row) != Some(row) {
            self.join.lang.update(cx, |s, cx| s.set_selected_index(Some(IndexPath::new(row)), window, cx));
        }
        let j = &self.join;
        let mode = j.mode;
        let back = self.snap.connected || !self.rooms.is_empty();
        let label = |text: SharedString| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::MUTED)).child(text);
        let server_row = core::built_in().is_none() && mode != Mode::Direct;
        // Like the webview: the darkest page, the form a card on it, fields darker than the card.
        div().id("join-page").size_full().bg(rgb(color::RAIL)).overflow_y_scroll().track_scroll(&self.join.scroll).child(div().min_h_full().flex().flex_col().items_center().child(
            div()
                .my_auto()
                .py_8()
                .flex_none()
                .child(
            div()
                .w(px(440.))
                .p(px(28.))
                .rounded(px(12.))
                .bg(rgb(color::MAIN))
                .flex()
                .flex_col()
                .gap(px(14.))
                .child(div().text_size(px(20.)).font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(t("connect.title")))
                .when(server_row, |d| d.child(label(tu("connect.server"))).child(sunk(Input::new(&j.server))))
                .child(label(tu("connect.name")))
                .child(sunk(Input::new(&j.name)))
                .child(
                    seg_bar()
                        .child(seg("m-create", t("join.create_mode"), mode == Mode::Create).on_click(cx.listener(|s, _, _, cx| s.set_mode(Mode::Create, cx))))
                        .child(seg("m-code", t("join.code_mode"), mode == Mode::Code).on_click(cx.listener(|s, _, _, cx| s.set_mode(Mode::Code, cx))))
                        .child(seg("m-direct", t("join.direct"), mode == Mode::Direct).child(crate::beta()).on_click(cx.listener(|s, _, _, cx| s.set_mode(Mode::Direct, cx)))),
                )
                .when(mode == Mode::Create, |d| d.child(label(tu("join.room_title"))).child(sunk(Input::new(&j.title))))
                .when(mode == Mode::Code, |d| d.child(label(tu("join.code"))).child(sunk(Input::new(&j.code))))
                .when(mode == Mode::Direct, |d| d.child(self.render_direct(cx)))
                .child(label(tu("connect.language")))
                .child(sunk(Select::new(&j.lang)))
                .when_some(j.error.clone(), |d, e| d.child(div().p_2().rounded_md().bg(rgb(0x3b2326)).text_color(rgb(0xffb4b4)).child(e)))
                .when(mode != Mode::Direct, |d| {
                    d.child(
                        Button::new("join-submit")
                            .primary()
                            .disabled(self.join.busy)
                            .label(t(if self.join.busy { "direct.connecting" } else if mode == Mode::Create { "join.create" } else { "join.enter" }))
                            .on_click(cx.listener(|s, _, _, cx| s.submit(cx))),
                    )
                })
                .when(back, |d| {
                    d.child(Button::new("join-back").label(t("join.back")).on_click(cx.listener(|s, _, _, cx| {
                        s.join.open = false;
                        cx.notify();
                    })))
                })
                .child(Button::new("join-settings").ghost().label(t("set.open")).on_click(cx.listener(|s, _, w, cx| s.open_settings(crate::dialogs::SetTab::General, w, cx)))),
                ),
        ))
        .vertical_scrollbar(&self.join.scroll)
    }

    fn render_direct(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let j = &self.join;
        let st = &j.step;
        let making = st.making;
        let bold = |text: SharedString| div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(text);
        let small = |text: SharedString| div().text_xs().text_color(rgb(color::MUTED)).child(text);
        let short = Checkbox::new("direct-short").label(t("direct.short")).checked(short_codes_on()).on_click(cx.listener(|_, v: &bool, _, cx| {
            set_setting("shortCode", serde_json::Value::Bool(*v));
            cx.notify();
        }));
        // Our text: the short code first when there is one, the full text below it.
        let outgoing = |title: SharedString, code: &Option<Code>, cx: &mut Context<Self>| {
            let mut col = div().flex().flex_col().gap_2().child(bold(title));
            match code {
                Some(Code::Valid { code, until }) => {
                    let shown = show_short(code);
                    let copy = shown.clone();
                    col = col
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().px_3().py_1().rounded_md().bg(rgb(color::RAIL)).border_1().border_color(rgb(color::LINE)).text_size(px(22.)).font_family("Consolas").font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::BRIGHT)).child(shown))
                                .child(Button::new("copy-code").label(t(if j.copied == Some("code") { "direct.copied" } else { "direct.copy_code" })).on_click(cx.listener(move |s, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    s.join.copied = Some("code");
                                    cx.notify();
                                })))
                                .child(small(tf("direct.code_left", &[("time", &clock(until.saturating_duration_since(Instant::now())))]))),
                        )
                        .child(small(t("direct.or_text")));
                }
                Some(Code::Failed) => col = col.child(small(t("direct.no_code"))),
                Some(Code::Expired) => col = col.child(small(t("direct.code_expired"))),
                None => {}
            }
            let text = j.outgoing.read(cx).value().to_string();
            col.child(sunk(Textarea::new(&j.outgoing).h(px(90.)).readonly(true))).child(div().flex().child(Button::new("copy-text").label(t(if j.copied == Some("text") { "direct.copied" } else { "direct.copy" })).on_click(cx.listener(move |s, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                s.join.copied = Some("text");
                cx.notify();
            }))))
        };
        let role = j.role;
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(small(t("direct.about")))
            .child(
                seg_bar()
                    .child(seg("r-invite", t("direct.role_invite"), role == Role::Invite).on_click(cx.listener(|s, _, w, cx| s.set_role(Role::Invite, w, cx))))
                    .child(seg("r-answer", t("direct.role_answer"), role == Role::Answer).on_click(cx.listener(|s, _, w, cx| s.set_role(Role::Answer, w, cx)))),
            );
        let busy_label = |idle: &str| t(if making { "direct.making" } else { idle });
        match (role, st.invite.is_some(), st.answer.is_some()) {
            (Role::Invite, false, _) => {
                col = col.child(short).child(Button::new("make-invite").primary().disabled(making).label(busy_label("direct.make_invite")).on_click(cx.listener(|s, _, w, cx| s.make_invite(w, cx))));
            }
            (Role::Invite, true, _) => {
                col = col
                    .child(outgoing(t("direct.send_invite"), &st.invite_code, cx))
                    .child(bold(t("direct.paste_answer")))
                    .child(sunk(Textarea::new(&j.incoming).h(px(90.))))
                    .child(
                        Button::new("connect")
                            .primary()
                            .disabled(making || st.accepted)
                            .label(if st.accepted { t("direct.connecting") } else { busy_label("direct.connect") })
                            .on_click(cx.listener(|s, _, w, cx| s.accept_answer(w, cx))),
                    );
            }
            (Role::Answer, _, false) => {
                col = col
                    .child(short)
                    .child(bold(t("direct.paste_invite")))
                    .child(sunk(Textarea::new(&j.incoming).h(px(90.))))
                    .child(Button::new("make-answer").primary().disabled(making).label(busy_label("direct.make_answer")).on_click(cx.listener(|s, _, w, cx| s.make_answer(w, cx))));
            }
            (Role::Answer, _, true) => {
                col = col.child(outgoing(t("direct.send_answer"), &st.answer_code, cx)).child(small(t("direct.wait")));
            }
        }
        col
    }

    /// What the direct form says, in short, for tests ("direct form after the end: ...").
    pub fn direct_form_text(&self) -> String {
        let st = &self.join.step;
        match (self.join.role, st.invite.is_some(), st.answer.is_some()) {
            (Role::Invite, false, _) => "Make a short code (keeps the text on our server for 5 minutes) Make the invite".into(),
            (Role::Invite, true, _) => "1. Send this to your friend: ...".into(),
            (Role::Answer, _, false) => "Make a short code (keeps the text on our server for 5 minutes) 1. Paste the code or the invite you got".into(),
            (Role::Answer, _, true) => "2. Send this back: ...".into(),
        }
    }
}

/// A field on the darkest colour: gpui-component draws a dark-theme field see-through, so
/// what is under it makes it darker than the card, as in the webview.
fn sunk(field: impl IntoElement) -> Div {
    div().rounded_md().bg(rgb(color::RAIL)).child(field)
}

/// A segmented switch like the webview's: options on a sunk strip, the chosen one lighter.
/// The whole width of the form, the options sharing it; a long name (in a translation)
/// takes a second line instead of pushing out of the card.
fn seg_bar() -> Div {
    div().w_full().flex().gap_1().p_1().rounded_lg().bg(rgb(color::RAIL))
}

fn seg(id: &'static str, label: SharedString, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1p5()
        .min_h(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .text_center()
        .rounded_md()
        .cursor_pointer()
        .when(on, |d| d.bg(rgb(0x404249)).text_color(rgb(color::BRIGHT)).font_weight(FontWeight::SEMIBOLD))
        .when(!on, |d| d.text_color(rgb(color::MUTED)).hover(|s| s.text_color(rgb(color::TEXT))))
        .child(label)
}

/// Tests only: STREAMER_APP_TEST_DIRECT="invite|answer NAME DIR [code|nocode] [leave]" drives
/// the direct form like a person; the texts go through files in DIR instead of a messenger.
/// Reports in the same words as the webview's driver, so the same gate reads both.
pub fn test_direct(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(spec) = std::env::var("STREAMER_APP_TEST_DIRECT") else { return };
    let parts: Vec<String> = spec.split(' ').map(String::from).collect();
    if parts.len() < 3 {
        return;
    }
    let (role, name, dir) = (parts[0].clone(), parts[1].clone(), std::path::PathBuf::from(&parts[2]));
    let rest: Vec<String> = parts[3..].to_vec();
    let use_code = rest.iter().any(|r| r == "code");
    set_setting("shortCode", serde_json::Value::Bool(!rest.iter().any(|r| r == "nocode")));
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let executor = cx.background_executor().clone();
        let wait = |ms: u64| executor.timer(Duration::from_millis(ms));
        wait(1500).await;
        // Polls `f` on the UI thread every 250 ms until it gives something, up to `ms`.
        macro_rules! until {
            ($ms:expr, $f:expr) => {{
                let end = Instant::now() + Duration::from_millis($ms);
                let mut got = None;
                while Instant::now() < end {
                    if let Ok(Some(v)) = this.update(cx, $f) {
                        got = Some(v);
                        break;
                    }
                    executor.timer(Duration::from_millis(250)).await;
                }
                got
            }};
        }
        let file = |n: &str| dir.join(n);
        let _ = this.update_in(cx, |s, window, cx| {
            s.join.open = true;
            s.join.mode = Mode::Direct;
            s.join.role = if role == "invite" { Role::Invite } else { Role::Answer };
            s.reset_direct(window, cx);
            let n = name.clone();
            s.join.name.update(cx, |i, cx| i.set_value(n, window, cx));
        });
        // What we send: the short code when the form shows one (and the run uses codes).
        let outgoing = |s: &mut Shell, _: &mut Context<Shell>| -> Option<(String, Option<String>)> {
            let st = &s.join.step;
            let text = st.answer.clone().or(st.invite.clone())?;
            Some((text, s.shown_code()))
        };
        let report = |m: String| eprintln!("test: {m}");
        if role == "invite" {
            let _ = this.update_in(cx, |s, window, cx| s.make_invite(window, cx));
            let Some((text, code)) = until!(20000, outgoing) else {
                let e = this.update(cx, |s, _| s.join.error.clone()).ok().flatten();
                report(format!("direct invite failed: {}", e.unwrap_or_default()));
                return;
            };
            let send = if use_code {
                report(format!("direct code {}", code.clone().unwrap_or_else(|| "none".into())));
                code.map(|c| c.split(' ').next().unwrap_or_default().to_string()).unwrap_or(text.clone())
            } else {
                text.clone()
            };
            let _ = std::fs::write(file("invite.txt"), &send);
            report(format!("direct invite {} chars", send.len()));
            let Some(answer) = until!(60000, |_, _| std::fs::read_to_string(file("answer.txt")).ok()) else { return };
            let _ = this.update_in(cx, |s, window, cx| {
                s.join.incoming.update(cx, |i, cx| i.set_value(answer.clone(), window, cx));
                s.accept_answer(window, cx);
            });
        } else {
            let Some(invite) = until!(60000, |_, _| std::fs::read_to_string(file("invite.txt")).ok()) else { return };
            let _ = this.update_in(cx, |s, window, cx| {
                s.join.incoming.update(cx, |i, cx| i.set_value(invite.clone(), window, cx));
                s.make_answer(window, cx);
            });
            let Some((text, code)) = until!(30000, outgoing) else {
                let e = this.update(cx, |s, _| s.join.error.clone()).ok().flatten();
                report(format!("direct answer failed: {}", e.unwrap_or_default()));
                return;
            };
            let send = if use_code {
                report(format!("direct code {}", code.clone().unwrap_or_else(|| "none".into())));
                code.map(|c| c.split(' ').next().unwrap_or_default().to_string()).unwrap_or(text.clone())
            } else {
                text.clone()
            };
            let _ = std::fs::write(file("answer.txt"), &send);
            report(format!("direct answer {} chars", send.len()));
        }
        let _ = until!(30000, |s: &mut Shell, _| (s.snap.direct == Some("connected")).then_some(()));
        wait(2000).await;
        let _ = this.update(cx, |s, _| {
            let peers: Vec<String> = s.snap.peers.iter().map(|p| p.name.clone()).collect();
            report(format!("direct connected; join form shown {}; peers {}", s.join_shown(), peers.join(",")));
        });
        if rest.iter().any(|r| r == "leave") {
            wait(12000).await;
            let _ = this.update(cx, |s, cx| s.leave(cx));
            report("direct left".into());
        }
        let _ = until!(90000, |s: &mut Shell, _| (s.snap.direct != Some("connected")).then_some(()));
        wait(1500).await;
        let _ = this.update(cx, |s, _| report(format!("direct form after the end: {}", s.direct_form_text())));
    })
    .detach();
}

/// Tests only: STREAMER_APP_TEST_FORM="create|Room title|Name|server" or
/// "code|K7Q2-...|Name|server" fills the join form and submits it, then reports the room it
/// is in, in the webview driver's words.
pub fn test_form(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(spec) = std::env::var("STREAMER_APP_TEST_FORM") else { return };
    let p: Vec<String> = spec.split('|').map(String::from).collect();
    if p.len() < 3 || !["create", "code", "filled"].contains(&p[0].as_str()) {
        return;
    }
    if p[0] == "filled" {
        test_filled(window, cx, p[1].parse().unwrap_or(720), p[2].clone());
        return;
    }
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let executor = cx.background_executor().clone();
        executor.timer(Duration::from_millis(1500)).await;
        let _ = this.update_in(cx, |s, window, cx| {
            s.join.open = true;
            s.join.mode = if p[0] == "create" { Mode::Create } else { Mode::Code };
            s.join.name.update(cx, |i, cx| i.set_value(p[2].clone(), window, cx));
            if let Some(server) = p.get(3).filter(|v| !v.is_empty()) {
                s.join.server.update(cx, |i, cx| i.set_value(server.clone(), window, cx));
            }
            let field = if p[0] == "create" { s.join.title.clone() } else { s.join.code.clone() };
            field.update(cx, |i, cx| i.set_value(p[1].clone(), window, cx));
            s.submit(cx);
        });
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if this.update(cx, |s, _| s.snap.connected && !s.join.busy).unwrap_or(false) {
                executor.timer(Duration::from_millis(1500)).await;
                let _ = this.update(cx, |s, _| {
                    let header = s.snap.title.clone().or_else(|| s.current.as_ref().map(Room::label)).unwrap_or_default();
                    let peers: Vec<String> = s.snap.peers.iter().map(|x| x.name.clone()).collect();
                    eprintln!("test: room {} title {} header {} peers {} join {}", s.snap.room, s.snap.title.clone().unwrap_or("null".into()), header, peers.join(","), s.join_shown());
                });
                return;
            }
            // The form refused it, or the join failed: say so now.
            if let Ok(Some(e)) = this.update(cx, |s, _| s.join.error.clone().filter(|_| !s.join.busy)) {
                eprintln!("test: form failed: {e}");
                return;
            }
            executor.timer(Duration::from_millis(300)).await;
        }
        eprintln!("test: form failed: nothing within 20 s");
    })
    .detach();
}

/// Tests only (STREAMER_APP_TEST_FORM="filled|LEN|Name"): the direct form at its tallest, the
/// inviting side after "Make the invite" with a short code, made-up texts of LEN characters
/// (no addresses) in both boxes. Reports how far the page scrolls and the window's height.
fn test_filled(window: &mut Window, cx: &mut Context<Shell>, len: usize, name: String) {
    let text: String = "BACKSEAT1.".chars().chain((0..len.saturating_sub(10)).map(|i| (b'A' + (i * 7 % 26) as u8) as char)).collect();
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let executor = cx.background_executor().clone();
        executor.timer(Duration::from_millis(1500)).await;
        let _ = this.update_in(cx, |s, window, cx| {
            s.join.open = true;
            s.join.mode = Mode::Direct;
            s.join.role = Role::Invite;
            s.join.name.update(cx, |i, cx| i.set_value(name.clone(), window, cx));
            s.join.outgoing.update(cx, |o, cx| o.set_value(text.clone(), window, cx));
            s.join.incoming.update(cx, |o, cx| o.set_value(text.clone(), window, cx));
            s.join.step.invite = Some(text.clone());
            s.join.step.invite_code = Some(Code::Valid { code: "K7Q29XMB".into(), until: Instant::now() + Duration::from_secs(300) });
            s.join.error = Some("Could not get the text behind the code (timeout). Ask for the full text.".into());
            cx.notify();
        });
        // Where the page is scrolled to, each time it changes, for 10 s (a test turns the wheel).
        let mut last = String::new();
        for _ in 0..40 {
            executor.timer(Duration::from_millis(250)).await;
            let line = this
                .update_in(cx, |s, window, _| {
                    let max = s.join.scroll.max_offset();
                    let b = s.join.scroll.bounds();
                    format!("join page scroll max {:.0} at {:.0} view {:.0} window {:.0}", f32::from(max.y), -f32::from(s.join.scroll.offset().y), f32::from(b.size.height), f32::from(window.viewport_size().height))
                })
                .unwrap_or_default();
            if line != last {
                eprintln!("test: {line}");
                last = line;
            }
        }
    })
    .detach();
}
