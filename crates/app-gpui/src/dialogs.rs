//! Dialogs over the window: going live (pick a screen or a program window, sound, quality;
//! also changing the running stream) and the settings. Same rules as the webview UI
//! (app.js: renderGoLive, openGoLive, openSettings, fillPrefs). While one is open the video
//! windows are hidden: they are native windows and would cover it.

use crate::room::PRESETS;
use crate::i18n::{num, t, tf, tu, LANGS};
use crate::{color, set_setting, settings, state_value, Shell};
use backseat_core::{self as core, GoLive, Sources};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Open {
    GoLive,
    Settings,
    Rename,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Audio {
    System,
    App,
    None,
}

impl Audio {
    fn key(self) -> &'static str {
        match self {
            Audio::System => "system",
            Audio::App => "app",
            Audio::None => "none",
        }
    }
}

/// A source in the picker.
#[derive(Clone, Debug)]
struct Src {
    id: u64,
    title: String,
    sub: String,
    minimized: bool,
    pid: Option<u32>,
    height: i32,
}

pub struct GoLiveState {
    pub screen: bool,
    pub sources: Option<Sources>,
    /// (screen tab, id) -> picture and program icon.
    thumbs: HashMap<(bool, u64), (Option<Arc<Image>>, Option<Arc<Image>>)>,
    pub pick: Option<u64>,
    pub res: &'static str,
    pub fps: &'static str,
    pub kbit: u32,
    pub codec: String,
    pub codecs: Option<Vec<String>>,
    res_picked: bool,
    codec_picked: bool,
    pub audio: Audio,
    audio_picked: bool,
    /// Changing the running stream instead of starting one.
    pub editing: bool,
    /// The running stream has no sound (it stays so while live).
    live_audio_none: bool,
    pub error: Option<String>,
    pub busy: bool,
    limit: Entity<InputState>,
    scroll: ScrollHandle,
}

/// One setting chosen from a list: where it is saved and the value of each row.
struct Pref {
    section: Option<&'static str>,
    key: &'static str,
    values: Vec<&'static str>,
    picker: Entity<SelectState<Vec<SharedString>>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SetTab {
    General,
    Stream,
    Watch,
    Voice,
    Updates,
}

pub struct SettingsState {
    pub tab: SetTab,
    server: Entity<InputState>,
    prefs: Vec<Pref>,
    pub status: String,
    pub update: Option<core::update::Release>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
    /// Voice activity threshold slider.
    pub threshold: Entity<gpui_kit::component::slider::SliderState>,
}

#[derive(Default)]
pub struct Dialogs {
    pub open: Option<Open>,
    pub golive: Option<GoLiveState>,
    pub settings: Option<SettingsState>,
    /// The room whose name in it is being changed, and the field.
    pub rename: Option<(crate::Room, Entity<InputState>)>,
}

fn decode_data_url(url: &str) -> Option<Arc<Image>> {
    use base64::Engine as _;
    let (head, data) = url.split_once(',')?;
    let format = if head.contains("image/bmp") { ImageFormat::Bmp } else if head.contains("image/png") { ImageFormat::Png } else { ImageFormat::Jpeg };
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn mbit(kbit: u32) -> String {
    num(kbit as f64 / 1000.0)
}

pub fn upload_limit() -> f64 {
    let v = state_value("uploadLimit");
    v.as_ref().and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0).max(0.0)
}

fn set_upload_limit(v: f64) {
    let mut state = core::state_load();
    if v > 0.0 {
        state.insert("uploadLimit".into(), serde_json::json!(v));
    } else {
        state.remove("uploadLimit");
    }
    let _ = core::state_save(&serde_json::Value::Object(state).to_string());
}

fn pref(section: &str, key: &str) -> String {
    settings().get(section).and_then(|s| s.get(key)).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn set_pref(section: Option<&str>, key: &str, value: &str) {
    // The language is the state file's own "lang", as the webview keeps it.
    if section.is_none() && key == "lang" {
        crate::i18n::set_language(value);
        return;
    }
    match section {
        None => set_setting(key, serde_json::Value::String(value.into())),
        Some(sec) => {
            let mut s = settings().get(sec).cloned().unwrap_or_else(|| serde_json::json!({}));
            if !s.is_object() {
                s = serde_json::json!({});
            }
            s[key] = serde_json::Value::String(value.into());
            set_setting(sec, s);
        }
    }
}

/// Microphones or speakers for a settings list: the system's default first (value ""),
/// then each device by id.
fn devices(input: bool) -> Vec<(&'static str, String)> {
    let mut out = vec![("", t("voice.default").to_string())];
    // The ids live as long as the app: the lists are made when the settings open.
    out.extend(core::audio_devices(input).into_iter().map(|d| (Box::leak(d.id.into_boxed_str()) as &'static str, d.name)));
    out
}

/// A segmented switch: options on a sunk strip, the chosen one lighter.
fn seg_bar() -> Div {
    div().flex().flex_wrap().self_start().gap_1().p_1().rounded_lg().bg(rgb(color::RAIL))
}

fn seg(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool, disabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .h(px(30.))
        .flex()
        .items_center()
        .rounded_md()
        .when(!disabled, |d| d.cursor_pointer())
        .when(on, |d| d.bg(rgb(0x404249)).text_color(rgb(color::BRIGHT)).font_weight(FontWeight::SEMIBOLD))
        .when(!on && !disabled, |d| d.text_color(rgb(color::MUTED)).hover(|s| s.text_color(rgb(color::TEXT))))
        .when(!on && disabled, |d| d.text_color(rgb(0x5c5f66)))
        .child(label.into())
}

fn legend(text: SharedString) -> Div {
    div().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::MUTED)).child(text)
}

fn small(t: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(color::MUTED)).child(t.into())
}

fn banner(t: impl Into<SharedString>, warn: bool) -> Div {
    div().p_3().rounded_md().bg(rgb(if warn { 0x3a3220 } else { 0x3b2326 })).text_color(rgb(if warn { 0xf0d48a } else { 0xffb4b4 })).child(t.into())
}

impl Shell {
    pub fn dialog_open(&self) -> bool {
        self.dialogs.open.is_some()
    }

    pub fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialogs.open = None;
        cx.notify();
    }

    /// The go-live dialog; `editing`: change the running stream instead of starting one.
    pub fn open_golive(&mut self, editing: bool, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.dialogs.golive.is_none();
        let mut g = self.dialogs.golive.take().unwrap_or_else(|| GoLiveState {
            screen: true,
            sources: None,
            thumbs: HashMap::new(),
            pick: None,
            res: "1080",
            fps: "60",
            kbit: 20000,
            codec: "h265".into(),
            codecs: None,
            res_picked: false,
            codec_picked: false,
            audio: Audio::System,
            audio_picked: false,
            editing: false,
            live_audio_none: false,
            error: None,
            busy: false,
            limit: cx.new(|cx| InputState::new(window, cx).placeholder(t("unit.mbit"))),
            scroll: ScrollHandle::new(),
        });
        if first {
            // The upload limit is saved as it is typed.
            let sub = cx.subscribe_in(&g.limit.clone(), window, |_, s, e: &InputEvent, _, cx| {
                if let InputEvent::Change = e {
                    set_upload_limit(s.read(cx).value().trim().replace(',', ".").parse().unwrap_or(0.0));
                    cx.notify();
                }
            });
            self.subs.push(sub);
        }
        g.editing = editing;
        g.error = None;
        g.busy = false;
        g.thumbs.clear();
        // STREAMER_DEBUG_IGNORE_PREFS: the spoiled variant for the go-live gate.
        if !editing && std::env::var_os("STREAMER_DEBUG_IGNORE_PREFS").is_none() {
            // The settings' defaults, each time; what this computer cannot do falls back.
            let (res, fps, kbit, codec) = (pref("stream", "res"), pref("stream", "fps"), pref("stream", "kbit"), pref("stream", "codec"));
            g.res_picked = !res.is_empty();
            if let Some(r) = ["720", "1080", "1440"].into_iter().find(|x| *x == res) {
                g.res = r;
            }
            if let Some(f) = ["30", "60"].into_iter().find(|x| *x == fps) {
                g.fps = f;
            }
            if let Ok(k) = kbit.parse::<u32>() {
                if PRESETS.contains(&k) {
                    g.kbit = k;
                }
            }
            g.codec_picked = !codec.is_empty();
            if !codec.is_empty() {
                g.codec = codec;
            }
            g.audio_picked = false;
        }
        let limit = upload_limit();
        g.limit.update(cx, |i, cx| i.set_value(if limit > 0.0 { format!("{limit}") } else { String::new() }, window, cx));
        g.sources = Some(core::sources());
        self.dialogs.golive = Some(g);
        self.dialogs.open = Some(Open::GoLive);
        self.golive_fixups();
        if self.dialogs.golive.as_ref().is_some_and(|g| g.codecs.is_none()) {
            let task = cx.background_executor().spawn(async move { core::codecs() });
            cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
                let codecs = task.await;
                let _ = this.update(cx, |s, cx| {
                    if let Some(g) = s.dialogs.golive.as_mut() {
                        g.codecs = Some(codecs);
                    }
                    s.golive_fixups();
                    cx.notify();
                });
            })
            .detach();
        }
        self.refresh_thumbs(cx);
        cx.notify();
    }

    fn golive_list(&self) -> Vec<Src> {
        let Some(g) = &self.dialogs.golive else { return vec![] };
        let Some(s) = &g.sources else { return vec![] };
        if g.screen {
            s.monitors.iter().map(|m| Src { id: m.handle, title: format!("{}{}", m.name.trim_start_matches(r"\\.\"), if m.primary { format!(" · {}", t("golive.primary")) } else { String::new() }), sub: format!("{}×{}", m.width, m.height), minimized: false, pid: None, height: m.height as i32 }).collect()
        } else {
            s.windows.iter().map(|w| Src { id: w.hwnd, title: w.title.clone(), sub: w.exe.clone(), minimized: w.minimized, pid: Some(w.pid), height: w.height as i32 }).collect()
        }
    }

    /// What follows the picks unless chosen by hand: the source, the resolution by the
    /// source's height, the sound by the kind of source, the codec by what this computer has.
    fn golive_fixups(&mut self) {
        let list = self.golive_list();
        let Some(g) = self.dialogs.golive.as_mut() else { return };
        if !list.iter().any(|x| Some(x.id) == g.pick) {
            g.pick = if g.screen { list.first().map(|x| x.id) } else { None };
        }
        if let Some(p) = list.iter().find(|x| Some(x.id) == g.pick) {
            if !g.res_picked && p.height > 0 {
                g.res = if p.height >= 1440 { "1440" } else if p.height >= 1080 { "1080" } else { "720" };
            }
        }
        if !g.audio_picked && !g.editing {
            let p = pref("stream", "audio");
            g.audio = if p == "none" { Audio::None } else if p == "system" { Audio::System } else if g.screen { Audio::System } else { Audio::App };
        }
        if g.screen && g.audio == Audio::App {
            g.audio = Audio::System;
        }
        let codecs = self.golive_codecs();
        let Some(g) = self.dialogs.golive.as_mut() else { return };
        // Until this machine's encoders are known (they are found in the background), nothing
        // is replaced: an AV1 from the settings was swapped for H.265 that way and stayed so.
        if g.codecs.is_none() {
            return;
        }
        if !g.codec_picked && !codecs.is_empty() && g.codec != codecs[0] {
            if codecs[0] == "av1" && g.kbit == 20000 {
                g.kbit = 15000;
            }
            g.codec = codecs[0].clone();
        }
        if !codecs.contains(&g.codec) && !codecs.is_empty() {
            g.codec = codecs[0].clone();
        }
    }

    /// Only codecs this machine encodes in hardware, AV1 first; while live only the running one.
    fn golive_codecs(&self) -> Vec<String> {
        let Some(g) = &self.dialogs.golive else { return vec![] };
        // Not known yet: all three for now.
        let have = g.codecs.clone().unwrap_or_else(|| vec!["av1".into(), "h265".into(), "h264".into()]);
        ["av1", "h265", "h264"].iter().filter(|c| have.iter().any(|h| h == *c) && (!g.editing || **c == g.codec)).map(|c| c.to_string()).collect()
    }

    /// Pictures for the visible tab, every 2 s while the dialog is open.
    pub fn refresh_thumbs(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &self.dialogs.golive else { return };
        if self.dialogs.open != Some(Open::GoLive) {
            return;
        }
        let screen = g.screen;
        let ids: Vec<u64> = self.golive_list().iter().map(|x| x.id).collect();
        let task = cx.background_executor().spawn(async move {
            core::thumbnails(if screen { "monitor" } else { "window" }, ids).into_iter().map(|t| (t.id, t.image.as_deref().and_then(decode_data_url), t.icon.as_deref().and_then(decode_data_url))).collect::<Vec<_>>()
        });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let shots = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    for (id, image, icon) in shots {
                        if image.is_some() || icon.is_some() {
                            g.thumbs.insert((screen, id), (image, icon));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Start streaming" (or "Apply" while live).
    pub fn golive_start(&mut self, cx: &mut Context<Self>) {
        let list = self.golive_list();
        let Some(g) = self.dialogs.golive.as_mut() else { return };
        let Some(pick) = list.iter().find(|x| Some(x.id) == g.pick) else {
            g.error = Some(t("golive.pick_window").to_string());
            cx.notify();
            return;
        };
        let req = GoLive { kind: if g.screen { "monitor".into() } else { "window".into() }, id: pick.id, audio: g.audio.key().into(), audio_pid: pick.pid, mode: format!("{}p{}", g.res, g.fps), kbit: g.kbit, codec: Some(g.codec.clone()) };
        let editing = g.editing;
        let audio_none = g.audio == Audio::None;
        g.busy = true;
        g.error = None;
        let core = self.core.clone();
        // STREAMER_DEBUG_NO_CHANGE: Apply sends nothing (the spoiled variant for the gate).
        let no_change = std::env::var_os("STREAMER_DEBUG_NO_CHANGE").is_some();
        let task = cx.background_executor().spawn(async move {
            if editing && no_change {
                Ok(())
            } else if editing {
                core::update_live(&core, req)
            } else {
                core::go_live(&core, req)
            }
        });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    g.busy = false;
                    match result {
                        Ok(()) => {
                            if !editing {
                                g.live_audio_none = audio_none;
                            }
                            s.dialogs.open = None;
                        }
                        Err(e) => g.error = Some(e),
                    }
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn render_golive(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.golive_fixups();
        let list = self.golive_list();
        let codecs = self.golive_codecs();
        let peers = self.snap.peers.len();
        let room = self.snap.room.clone();
        let Some(g) = self.dialogs.golive.as_ref() else { return div().into_any_element() };
        let n = peers.saturating_sub(1).max(1);
        let need = g.kbit as f64 * n as f64 / 1000.0;
        let limit = upload_limit();
        let over = limit > 0.0 && need > limit;
        let screen = g.screen;
        let thumbs = &g.thumbs;
        let source = |x: &Src, cx: &mut Context<Self>| {
            let id = x.id;
            let on = g.pick == Some(id);
            let (image, icon) = thumbs.get(&(screen, id)).cloned().unwrap_or((None, None));
            div()
                .id(SharedString::from(format!("src-{id}")))
                .w(px(196.))
                .p_2()
                .flex()
                .flex_col()
                .gap_1p5()
                .rounded_lg()
                .cursor_pointer()
                .bg(rgb(if on { 0x2d4f8f } else { color::SIDE }))
                .border_1()
                .border_color(rgb(if on { 0x3b6fd8 } else { color::SIDE }))
                .hover(|s| s.border_color(rgb(0x3b6fd8)))
                .child(
                    div().w_full().h(px(101.)).rounded_md().bg(rgb(0x000000)).overflow_hidden().flex().items_center().justify_center().map(|d| match image {
                        Some(i) => d.child(img(i).w_full().h_full().object_fit(ObjectFit::Contain)),
                        None => d.child(small(if x.minimized { t("golive.minimized") } else { SharedString::default() })),
                    }),
                )
                .child(div().flex().items_center().gap_1p5().overflow_hidden().whitespace_nowrap().text_color(rgb(color::BRIGHT)).when_some(icon, |d, i| d.child(img(i).size_4().flex_none())).child(div().overflow_hidden().child(x.title.clone())))
                .child(small(x.sub.clone()))
                .on_click(cx.listener(move |s, _, _, cx| {
                    if let Some(g) = s.dialogs.golive.as_mut() {
                        g.pick = Some(id);
                    }
                    cx.notify();
                }))
        };
        let sources: Vec<AnyElement> = list.iter().map(|x| source(x, cx).into_any_element()).collect();
        let set = |f: fn(&mut GoLiveState)| move |s: &mut Shell, _: &ClickEvent, _: &mut Window, cx: &mut Context<Shell>| {
            if let Some(g) = s.dialogs.golive.as_mut() {
                f(g);
            }
            cx.notify();
        };
        let audio_row = {
            let opts: Vec<(Audio, SharedString)> = [(Audio::System, t("golive.audio_system")), (Audio::App, t("golive.audio_app")), (Audio::None, t("golive.audio_none"))].into_iter().filter(|(a, _)| *a != Audio::App || !screen).collect();
            seg_bar().children(opts.into_iter().map(|(a, label)| {
                // While live, sound stays on or off as it started; which sound may change.
                let locked = g.editing && (g.live_audio_none != (a == Audio::None));
                seg(SharedString::from(format!("audio-{}", a.key())), label, g.audio == a, locked).when(!locked, |d| {
                    d.on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(g) = s.dialogs.golive.as_mut() {
                            g.audio = a;
                            g.audio_picked = true;
                        }
                        cx.notify();
                    }))
                })
            }))
        };
        let res_row = seg_bar().children(["720", "1080", "1440"].into_iter().map(|r| {
            seg(SharedString::from(format!("res-{r}")), format!("{r}p"), g.res == r, false).on_click(cx.listener(move |s, _, _, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    g.res = r;
                    g.res_picked = true;
                }
                cx.notify();
            }))
        }));
        let fps_row = seg_bar().children(["30", "60"].into_iter().map(|f| {
            seg(SharedString::from(format!("fps-{f}")), f, g.fps == f, false).on_click(cx.listener(move |s, _, _, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    g.fps = f;
                }
                cx.notify();
            }))
        }));
        let kbit_row = seg_bar().children(PRESETS.into_iter().map(|k| {
            seg(SharedString::from(format!("kbit-{k}")), mbit(k), g.kbit == k, false).on_click(cx.listener(move |s, _, _, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    g.kbit = k;
                }
                cx.notify();
            }))
        }));
        let codec_row = seg_bar().children(codecs.iter().cloned().map(|c| {
            let label = match c.as_str() {
                "av1" => "AV1",
                "h265" => "H.265",
                _ => "H.264",
            };
            seg(SharedString::from(format!("codec-{c}")), label, g.codec == c, false).on_click(cx.listener(move |s, _, _, cx| {
                if let Some(g) = s.dialogs.golive.as_mut() {
                    // AV1 gives about the same picture with fewer bits: 15 where the others use 20.
                    if c == "av1" && g.codec != "av1" && g.kbit == 20000 {
                        g.kbit = 15000;
                    }
                    if c != "av1" && g.codec == "av1" && g.kbit == 15000 {
                        g.kbit = 20000;
                    }
                    g.codec = c.clone();
                    g.codec_picked = true;
                }
                cx.notify();
            }))
        }));
        let note = t(match g.codec.as_str() {
            "h264" => "golive.codec_h264",
            "av1" => "golive.codec_av1",
            _ => "golive.codec_h265",
        });
        let control = |label: SharedString, row: Div| div().flex().flex_col().gap_1().child(small(label)).child(row);
        let width = (need / if limit > 0.0 { limit } else { need.max(1.0) }).min(1.0) as f32;
        let up = if limit > 0.0 { tf("up.of", &[("used", &num(need)), ("limit", &num(limit))]) } else { tf("golive.upload_value", &[("mbit", &mbit(g.kbit * n as u32))]) };
        let card = div()
            .w(px(860.))
            .max_h(relative(0.92))
            .bg(rgb(color::MAIN))
            .rounded(px(12.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(div().px_6().pt_5().pb_3().flex().flex_col().gap_1().child(div().text_size(px(18.)).font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(t(if g.editing { "live.change_title" } else { "golive.title" }))).child(small(tf("golive.sub", &[("room", &room), ("n", &peers.saturating_sub(1))]))))
            .child(
                div().id("golive-scroll").flex_1().min_h_0().overflow_y_scroll().track_scroll(&g.scroll).child(
                    div()
                        .px_6()
                        .pb_4()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(seg_bar().child(seg("tab-screen", t("golive.screen"), screen, false).on_click(cx.listener(set(|g| g.screen = true)))).child(seg("tab-window", t("golive.window"), !screen, false).on_click(cx.listener(set(|g| g.screen = false)))))
                        .child(div().flex().flex_wrap().gap_2p5().children(sources))
                        .child(div().flex().flex_col().gap_2().child(legend(tu("golive.audio"))).child(audio_row))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(legend(tu("golive.quality")))
                                .child(div().flex().flex_wrap().gap_4().child(control(t("golive.resolution"), res_row)).child(control(t("golive.fps"), fps_row)).child(control(t("golive.codec"), codec_row)))
                                .child(control(t("golive.bitrate"), kbit_row))
                                .child(small(note)),
                        )
                        .child(
                            div()
                                .p_3()
                                .rounded_lg()
                                .bg(rgb(color::SIDE))
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(div().flex().justify_between().child(small(tf("golive.upload", &[("n", &n)]))).child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(up)))
                                .child(div().h_2().rounded(px(4.)).bg(rgb(color::RAIL)).child(div().h_2().rounded(px(4.)).w(relative(width)).bg(rgb(if over { 0xf0b232 } else { 0x2f9e6e }))))
                                .child(small(tf("golive.upload_note", &[("kbit", &mbit(g.kbit))])))
                                .child(div().flex().items_center().gap_3().child(small(t("up.limit"))).child(div().w(px(120.)).rounded_md().bg(rgb(color::RAIL)).child(Input::new(&g.limit).small())))
                                .child(small(t("up.limit_hint")))
                                .when(over, |d| d.child(banner(tf("up.over", &[("need", &num(need)), ("limit", &num(limit))]), true))),
                        )
                        .when_some(g.error.clone(), |d, e| d.child(banner(e, false))),
                ).vertical_scrollbar(&g.scroll),
            )
            .child(
                div()
                    .px_6()
                    .py_4()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .bg(rgb(color::SIDE))
                    .child(Button::new("gl-cancel").cursor_pointer().ghost().label(t("golive.cancel")).on_click(cx.listener(|s, _, _, cx| s.close_dialog(cx))))
                    .child(Button::new("gl-start").cursor_pointer().primary().disabled(g.busy).label(t(if g.busy { "direct.making" } else if g.editing { "live.apply" } else { "golive.start" })).on_click(cx.listener(|s, _, _, cx| s.golive_start(cx)))),
            );
        card.into_any_element()
    }

    pub fn open_rename(&mut self, r: crate::Room, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(r.name.clone()));
        name.focus_handle(cx).focus(window, cx);
        self.dialogs.rename = Some((r, name));
        self.dialogs.open = Some(Open::Rename);
        cx.notify();
    }

    fn render_rename(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some((r, name)) = self.dialogs.rename.clone() else { return div().into_any_element() };
        div()
            .w(px(380.))
            .p_6()
            .bg(rgb(color::MAIN))
            .rounded(px(12.))
            .flex()
            .flex_col()
            .gap_3()
            .child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(tf("rename.title", &[("room", &r.label())])))
            .child(div().rounded_md().bg(rgb(color::RAIL)).child(Input::new(&name)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rn-cancel").cursor_pointer().label(t("common.cancel")).on_click(cx.listener(|s, _, _, cx| s.close_dialog(cx))))
                    .child(Button::new("rn-save").cursor_pointer().primary().label(t("common.save")).on_click(cx.listener(move |s, _, _, cx| {
                        let v = name.read(cx).value().to_string();
                        s.dialogs.open = None;
                        s.rename_room(&r, &v, cx);
                    }))),
            )
            .into_any_element()
    }

    pub fn open_settings(&mut self, tab: SetTab, window: &mut Window, cx: &mut Context<Self>) {
        let started = std::time::Instant::now();
        let server_now = settings().get("server").and_then(|v| v.as_str()).map(String::from).filter(|v| !v.is_empty()).unwrap_or_else(|| self.join.server.read(cx).value().to_string());
        let server = cx.new(|cx| InputState::new(window, cx).placeholder("wss://example.com/ws").default_value(core::built_in().map(String::from).unwrap_or(server_now)));
        let mut subs = vec![cx.subscribe_in(&server, window, |_, s, e: &InputEvent, _, cx| {
            if let InputEvent::Change = e {
                if core::built_in().is_none() {
                    set_setting("server", serde_json::Value::String(s.read(cx).value().trim().to_string()));
                }
            }
        })];
        let kbits: Vec<(&'static str, String)> = PRESETS.iter().map(|k| (Box::leak(k.to_string().into_boxed_str()) as &'static str, format!("{} {}", mbit(*k), t("unit.mbit")))).collect();
        type Rows = Vec<(&'static str, String)>;
        let rows = |items: &[(&'static str, SharedString)]| -> Rows { items.iter().map(|(v, l)| (*v, l.to_string())).collect() };
        let plain = |v: &'static str| SharedString::from(v);
        let with_kbits = |first: &str| -> Rows { std::iter::once(("", first.to_string())).chain(kbits.iter().cloned()).collect() };
        let defs: Vec<(Option<&'static str>, &'static str, Rows)> = vec![
            (Some("stream"), "res", rows(&[("", t("set.auto_res")), ("720", plain("720p")), ("1080", plain("1080p")), ("1440", plain("1440p"))])),
            (Some("stream"), "fps", rows(&[("", t("set.auto")), ("30", plain("30")), ("60", plain("60"))])),
            (Some("stream"), "kbit", with_kbits(&t("set.auto"))),
            (Some("stream"), "codec", rows(&[("", t("set.auto")), ("av1", plain("AV1")), ("h265", plain("H.265")), ("h264", plain("H.264"))])),
            (Some("stream"), "audio", rows(&[("", t("set.audio_auto")), ("system", t("golive.audio_system")), ("none", t("golive.audio_none"))])),
            (Some("watch"), "res", rows(&[("", t("set.as_streamer")), ("1440", plain("1440p")), ("1080", plain("1080p")), ("720", plain("720p"))])),
            (Some("watch"), "fps", rows(&[("", t("set.as_streamer")), ("60", plain("60")), ("30", plain("30"))])),
            (Some("watch"), "kbit", with_kbits(&t("set.as_streamer"))),
            (None, "lang", rows(&LANGS.map(|(id, name)| (id, plain(name))))),
            (Some("voice"), "mic", devices(true)),
            (Some("voice"), "out", devices(false)),
            (Some("voice"), "mode", rows(&[("", t("voice.mode_va")), ("ptt", t("voice.mode_ptt"))])),
            (Some("voice"), "key", rows(&crate::voice_ui::PTT_KEYS.map(|(k, n)| (k, plain(n))))),
        ];
        let mut prefs = vec![];
        for (section, key, items) in defs {
            let now = if section.is_none() && key == "lang" { crate::i18n::current() } else { pref(section.unwrap_or_default(), key) };
            let row = items.iter().position(|(v, _)| *v == now).unwrap_or(0);
            let labels: Vec<SharedString> = items.iter().map(|(_, l)| SharedString::from(l.clone())).collect();
            let values: Vec<&'static str> = items.iter().map(|(v, _)| *v).collect();
            let picker = cx.new(|cx| SelectState::new(labels, Some(IndexPath::new(row)), window, cx));
            let vals = values.clone();
            subs.push(cx.subscribe_in(&picker, window, move |this: &mut Shell, p, e: &SelectEvent<Vec<SharedString>>, _, cx| {
                let SelectEvent::Confirm(_) = e;
                let row = p.read(cx).selected_index(cx).map_or(0, |i| i.row);
                set_pref(section, key, vals.get(row).copied().unwrap_or(""));
                if section == Some("voice") {
                    crate::voice_ui::apply_settings(&this.core);
                }
                cx.notify();
            }));
            prefs.push(Pref { section, key, values, picker });
        }
        let update = self.update.clone().or_else(|| self.dialogs.settings.as_ref().and_then(|s| s.update.clone()));
        let (threshold, sub) = self.threshold_slider(window, cx);
        subs.push(sub);
        self.dialogs.settings = Some(SettingsState { tab, server, prefs, status: String::new(), update, scroll: ScrollHandle::new(), _subs: subs, threshold });
        self.dialogs.open = Some(Open::Settings);
        eprintln!("settings: opened in {} ms", started.elapsed().as_millis());
        cx.notify();
    }

    /// Picks a value in one of the settings' lists the way a click does (tests).
    pub fn pick_pref(&mut self, section: Option<&str>, key: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(st) = self.dialogs.settings.as_ref() else { return false };
        let Some(p) = st.prefs.iter().find(|p| p.section == section && p.key == key) else { return false };
        let Some(row) = p.values.iter().position(|v| *v == value) else { return false };
        p.picker.update(cx, |s, cx| s.set_selected_index(Some(IndexPath::new(row)), window, cx));
        set_pref(section, key, value);
        if section == Some("voice") {
            crate::voice_ui::apply_settings(&self.core);
        }
        true
    }

    pub fn set_tray(&mut self, on: bool) {
        set_setting("tray", serde_json::Value::Bool(on));
        let on = on && std::env::var_os("STREAMER_DEBUG_NO_TRAY").is_none();
        self.core.close_to_tray.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    fn check_update(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.dialogs.settings.as_mut() {
            s.status = t("set.checking").to_string();
        }
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move { core::check_update(&core) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let r = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(st) = s.dialogs.settings.as_mut() {
                    match r {
                        Ok(r) => {
                            st.status = if r.newer { tf("set.available", &[("v", &r.latest)]).to_string() } else { t("set.uptodate").to_string() };
                            st.update = r.newer.then_some(r);
                        }
                        Err(e) => st.status = tf("set.failed", &[("e", &e)]).to_string(),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.dialogs.settings.as_mut() {
            s.status = t("set.installing").to_string();
        }
        let core = self.core.clone();
        let task = cx.background_executor().spawn(async move { core::install_update(&core) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let r = task.await;
            let _ = this.update(cx, |s, cx| {
                match r {
                    Ok(true) => cx.quit(),
                    Ok(false) => {}
                    Err(e) => {
                        if let Some(st) = s.dialogs.settings.as_mut() {
                            st.status = tf("set.failed", &[("e", &e)]).to_string();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(st) = self.dialogs.settings.as_ref() else { return div().into_any_element() };
        let tab = st.tab;
        let tray = settings().get("tray").and_then(|v| v.as_bool()) != Some(false);
        let auto = settings().get("autoUpdate").and_then(|v| v.as_bool()) != Some(false);
        let field = |label: SharedString, el: AnyElement| div().flex().flex_col().gap_1p5().child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::MUTED)).child(label)).child(el);
        let pick = |section: Option<&'static str>, key: &'static str| -> AnyElement {
            match st.prefs.iter().find(|p| p.section == section && p.key == key) {
                Some(p) => div().rounded_md().bg(rgb(color::RAIL)).cursor_pointer().child(Select::new(&p.picker)).into_any_element(),
                None => div().into_any_element(),
            }
        };
        let tab_button = |which: SetTab, id: &'static str, label: SharedString, cx: &mut Context<Self>| {
            let on = tab == which;
            div()
                .id(id)
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(0x404249)).text_color(rgb(color::BRIGHT)).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |d| d.text_color(rgb(color::MUTED)).hover(|s| s.bg(rgb(0x35373c)).text_color(rgb(color::TEXT))))
                .child(label)
                .on_click(cx.listener(move |s, _, _, cx| {
                    if let Some(st) = s.dialogs.settings.as_mut() {
                        st.tab = which;
                    }
                    cx.notify();
                }))
        };
        let page: AnyElement = match tab {
            SetTab::General => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(field(tu("set.server"), div().rounded_md().bg(rgb(color::RAIL)).child(Input::new(&st.server).disabled(core::built_in().is_some())).into_any_element()))
                .child(small(match core::built_in() {
                    Some(u) => tf("set.server_fixed", &[("url", &u)]),
                    None => t("set.server_hint"),
                }))
                .child(field(tu("connect.language"), pick(None, "lang")))
                .child(Checkbox::new("set-tray").cursor_pointer().label(t("set.tray")).checked(tray).on_click(cx.listener(|s, v: &bool, _, cx| {
                    s.set_tray(*v);
                    cx.notify();
                })))
                .child(div().flex().flex_col().items_start().gap_1p5().child(Button::new("set-logs").cursor_pointer().label(t("set.open_logs")).on_click(cx.listener(|s, _, _, cx| {
                    if let Err(e) = core::open_logs() {
                        s.error = Some(e);
                    }
                    cx.notify();
                }))).child(small(t("set.logs_hint"))))
                .into_any_element(),
            SetTab::Stream => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(small(t("set.stream_hint")))
                .child(field(tu("golive.resolution"), pick(Some("stream"), "res")))
                .child(field(tu("golive.fps"), pick(Some("stream"), "fps")))
                .child(field(tu("golive.bitrate"), pick(Some("stream"), "kbit")))
                .child(field(tu("golive.codec"), pick(Some("stream"), "codec")))
                .child(field(tu("golive.audio"), pick(Some("stream"), "audio")))
                .into_any_element(),
            SetTab::Watch => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(small(t("set.watch_hint")))
                .child(field(tu("set.max_res"), pick(Some("watch"), "res")))
                .child(field(tu("set.max_fps"), pick(Some("watch"), "fps")))
                .child(field(tu("set.max_kbit"), pick(Some("watch"), "kbit")))
                .into_any_element(),
            SetTab::Voice => {
                let (sounds, ptt_sounds) = (settings().get("voiceSounds").and_then(|v| v.as_bool()) != Some(false), settings().get("pttSounds").and_then(|v| v.as_bool()) != Some(false));
                let ptt = pref("voice", "mode") == "ptt";
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(small(t("voice.settings_hint")))
                    .child(field(tu("voice.mic"), pick(Some("voice"), "mic")))
                    .child(field(tu("voice.speakers"), pick(Some("voice"), "out")))
                    .child(field(tu("voice.mode"), pick(Some("voice"), "mode")))
                    .when(ptt, |d| d.child(field(tu("voice.key"), pick(Some("voice"), "key"))).child(small(t("voice.key_hint"))))
                    .when(!ptt, |d| d.child(crate::voice_ui::threshold_view(&st.threshold, core::mic_input_db(&self.core))))
                    .child(Checkbox::new("set-voice-sounds").cursor_pointer().label(t("voice.sounds")).checked(sounds).on_click(cx.listener(|s, v: &bool, _, cx| {
                        set_setting("voiceSounds", serde_json::Value::Bool(*v));
                        crate::voice_ui::apply_settings(&s.core);
                        cx.notify();
                    })))
                    .child(Checkbox::new("set-stream-echo").cursor_pointer().label(t("voice.stream_echo")).checked(settings().get("voiceStreamEcho").and_then(|v| v.as_bool()) == Some(true)).on_click(cx.listener(|s, v: &bool, _, cx| {
                        set_setting("voiceStreamEcho", serde_json::Value::Bool(*v));
                        crate::voice_ui::apply_settings(&s.core);
                        cx.notify();
                    })))
                    .child(small(t("voice.stream_echo_hint")))
                    .child(Checkbox::new("set-ptt-sounds").cursor_pointer().label(t("voice.ptt_sounds")).checked(ptt_sounds).on_click(cx.listener(|s, v: &bool, _, cx| {
                        set_setting("pttSounds", serde_json::Value::Bool(*v));
                        crate::voice_ui::apply_settings(&s.core);
                        cx.notify();
                    })))
                    .into_any_element()
            }
            SetTab::Updates => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(tf("set.version", &[("v", &core::app_version())])))
                .child(Checkbox::new("set-auto").cursor_pointer().label(t("set.auto_update")).checked(auto).on_click(cx.listener(|_, v: &bool, _, cx| {
                    set_setting("autoUpdate", serde_json::Value::Bool(*v));
                    cx.notify();
                })))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new("set-check").cursor_pointer().label(t("set.check")).on_click(cx.listener(|s, _, _, cx| s.check_update(cx))))
                        .when(st.update.is_some(), |d| d.child(Button::new("set-install").cursor_pointer().primary().label(t("set.install")).on_click(cx.listener(|s, _, _, cx| s.install_update(cx))))),
                )
                .child(small(st.status.clone()))
                .into_any_element(),
        };
        div()
            .w(px(680.))
            .h(px(560.))
            .max_h(relative(0.92))
            .bg(rgb(color::MAIN))
            .rounded(px(12.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(div().px_6().pt_5().pb_3().text_size(px(18.)).font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(t("set.title")))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_4()
                    .px_6()
                    .pb_4()
                    .child(div().w(px(150.)).flex_none().flex().flex_col().gap_1().child(tab_button(SetTab::General, "tab-general", t("set.general"), cx)).child(tab_button(SetTab::Stream, "tab-stream", t("set.stream"), cx)).child(tab_button(SetTab::Watch, "tab-watch", t("set.watch"), cx)).child(tab_button(SetTab::Voice, "tab-voice", t("set.voice"), cx)).child(tab_button(SetTab::Updates, "tab-updates", t("set.updates"), cx)))
                    .child(div().id("set-page").flex_1().min_w_0().overflow_y_scroll().track_scroll(&st.scroll).child(div().pb_2().pr_3().child(page)).vertical_scrollbar(&st.scroll)),
            )
            .child(div().px_6().py_4().flex().justify_end().bg(rgb(color::SIDE)).child(Button::new("set-close").cursor_pointer().primary().label(t("set.close")).on_click(cx.listener(|s, _, _, cx| s.close_dialog(cx)))))
            .into_any_element()
    }

    /// The open dialog over a dimmed window, or nothing.
    pub fn render_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let card = match self.dialogs.open? {
            Open::GoLive => self.render_golive(cx),
            Open::Rename => self.render_rename(cx),
            Open::Settings => self.render_settings(cx),
        };
        Some(
            div()
                .id("dialog-backdrop")
                .absolute()
                .inset_0()
                .bg(rgba(0x000000b0))
                .flex()
                .items_center()
                .justify_center()
                .occlude()
                .child(card)
                .into_any_element(),
        )
    }
}

/// Tests only.
/// STREAMER_APP_TEST_PREFS_UI="stream.res=1080,watch.kbit=4000,tray=false,...": opens the
/// settings at +2 s and picks each value in its list the way a click does, then closes them.
/// STREAMER_APP_OPEN_GOLIVE="1|start": opens the go-live dialog at +4 s, reports what it shows
/// once the pictures came, and with "start" presses Start streaming.
/// STREAMER_APP_TEST_CHANGE="720p30 4000": 12 s after going live, opens the dialog to change
/// the stream, picks that mode and bitrate and presses Apply.
pub fn test_dialogs(window: &mut Window, cx: &mut Context<Shell>) {
    let prefs = std::env::var("STREAMER_APP_TEST_PREFS_UI").ok();
    let golive = std::env::var("STREAMER_APP_OPEN_GOLIVE").ok();
    let change = std::env::var("STREAMER_APP_TEST_CHANGE").ok();
    // STREAMER_APP_TEST_SETTINGS=general|stream|watch|updates: the settings open at that tab.
    let show = std::env::var("STREAMER_APP_TEST_SETTINGS").ok();
    if prefs.is_none() && golive.is_none() && change.is_none() && show.is_none() {
        return;
    }
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let ex = cx.background_executor().clone();
        ex.timer(Duration::from_secs(2)).await;
        if let Some(p) = prefs {
            let _ = this.update_in(cx, |s, window, cx| {
                s.open_settings(SetTab::Stream, window, cx);
                for item in p.split(',') {
                    let Some((k, v)) = item.split_once('=') else { continue };
                    if k == "tray" {
                        s.set_tray(v != "false");
                        continue;
                    }
                    let (sec, key) = k.split_once('.').map(|(a, b)| (Some(a), b)).unwrap_or((None, k));
                    if !s.pick_pref(sec, key, v, window, cx) {
                        eprintln!("test: settings: no {k}={v} in the lists");
                    }
                }
                s.close_dialog(cx);
            });
            eprintln!("test: settings saved {}", serde_json::Value::Object(settings()));
            let (show, quit) = core::tray_text();
            eprintln!("test: language now {}; tray {show} / {quit}", crate::i18n::current());
        }
        if let Some(tab) = show {
            let tab = match tab.as_str() {
                "stream" => SetTab::Stream,
                "watch" => SetTab::Watch,
                "voice" => SetTab::Voice,
                "updates" => SetTab::Updates,
                _ => SetTab::General,
            };
            let _ = this.update_in(cx, |s, window, cx| s.open_settings(tab, window, cx));
        }
        if let Some(g) = golive {
            // 3 s after start, as the webview's driver (gates take pictures by that clock);
            // later only if the app is not in a room yet.
            ex.timer(Duration::from_secs(1)).await;
            for _ in 0..40 {
                if this.update(cx, |s, _| s.snap.connected).unwrap_or(false) {
                    break;
                }
                ex.timer(Duration::from_millis(250)).await;
            }
            let _ = this.update_in(cx, |s, window, cx| s.open_golive(false, window, cx));
            ex.timer(Duration::from_millis(2500)).await;
            let _ = this.update(cx, |s, _| {
                if let Some(g) = &s.dialogs.golive {
                    let src = g.sources.as_ref().map(|x| format!("{}/{}", x.monitors.len(), x.windows.len())).unwrap_or_default();
                    eprintln!("test: golive sources {src} pictures {} pick {:?} mode {}p{} kbit {} codec {} audio {} codecs {:?}", g.thumbs.values().filter(|t| t.0.is_some()).count(), g.pick, g.res, g.fps, g.kbit, g.codec, g.audio.key(), g.codecs);
                }
            });
            if g == "start" {
                let _ = this.update(cx, |s, cx| s.golive_start(cx));
            }
        }
        if let Some(c) = change {
            let (mode, kbit) = c.split_once(' ').unwrap_or((c.as_str(), ""));
            let (res, fps) = mode.split_once('p').unwrap_or(("", ""));
            for _ in 0..80 {
                if this.update(cx, |s, _| s.snap.live.is_some()).unwrap_or(false) {
                    break;
                }
                ex.timer(Duration::from_millis(250)).await;
            }
            ex.timer(Duration::from_secs(12)).await;
            let (res, fps, kbit) = (res.to_string(), fps.to_string(), kbit.parse::<u32>().unwrap_or(0));
            let _ = this.update_in(cx, |s, window, cx| {
                s.open_golive(true, window, cx);
                if let Some(g) = s.dialogs.golive.as_mut() {
                    if let Some(r) = ["720", "1080", "1440"].into_iter().find(|x| *x == res) {
                        g.res = r;
                        g.res_picked = true;
                    }
                    if let Some(f) = ["30", "60"].into_iter().find(|x| *x == fps) {
                        g.fps = f;
                    }
                    if PRESETS.contains(&kbit) {
                        g.kbit = kbit;
                    }
                }
                s.golive_start(cx);
            });
            eprintln!("test: change applied {c}");
        }
    })
    .detach();
}
