//! The open room's main area: streams being watched (each a native video window placed over
//! its tile), others live with a Watch button, and our own stream's panel. Same rules as the
//! webview UI (app.js: renderContent, tile, chip, syncVideos, renderLive).

use crate::i18n::{num, t, tf, tu};
use crate::{color, settings, Shell};
use backseat_core::{self as core, sampler::{LiveStats, WatchStats}, PeerView};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Icon, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// What a viewer may ask for: the modes and bitrate presets the host can make.
pub const VIEW_MODES: [&str; 5] = ["1440p60", "1080p60", "1080p30", "720p60", "720p30"];
pub const PRESETS: [u32; 8] = [2000, 4000, 8000, 12000, 15000, 20000, 35000, 50000];

type Picker = Entity<SelectState<Vec<SharedString>>>;

/// A watched stream's quality pickers. Until the viewer picks something they follow what is
/// being watched.
pub struct Tile {
    mode: Picker,
    kbit: Picker,
    /// The rows we set last; a different selection means the viewer picked it.
    shown: (usize, usize),
    touched: bool,
    /// A pick is applied once the pickers stay still for a moment (`APPLY_AFTER`): changing
    /// both the size and the bitrate then restarts the streamer's encoder once, not twice.
    picked: u64,
    _subs: [Subscription; 2],
}

/// How long the pickers must stay still before a pick is asked for.
const APPLY_AFTER: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct Room {
    pub tiles: HashMap<u64, Tile>,
    /// Where each video window was put last (physical px), so it moves only on a change.
    /// Shared with the boxes' paint step, which places the windows.
    pub placed: Rc<RefCell<HashMap<u64, Option<(i32, i32, i32, i32)>>>>,
    pub below: ScrollHandle,
    pub videos_hidden: bool,
    /// Tests: what was last written about the layout and our stream.
    pub reported: String,
}

fn mbit(kbit: u32) -> String {
    num(kbit as f64 / 1000.0)
}

fn ms(v: f64) -> String {
    if v >= 1.0 {
        format!("{} {}", v.round(), t("unit.ms"))
    } else if v > 0.0 {
        format!("<1 {}", t("unit.ms"))
    } else {
        "—".into()
    }
}

/// "720p30 2000" -> "720p30 · 2 Mbit/s"
fn request_label(q: &Option<String>) -> String {
    match q.as_deref().map(|q| q.split_once(' ').unwrap_or((q, ""))) {
        None => t("card.source_quality").to_string(),
        Some((m, "")) => m.into(),
        Some((m, k)) => format!("{m} · {} {}", mbit(k.parse().unwrap_or(0)), t("unit.mbit")),
    }
}

/// "1280x720@30 4000 kbit/s" -> "720p30 · 4"
pub fn short_quality(q: &str) -> String {
    let parse = || -> Option<String> {
        let (wh, rest) = q.split_once('@')?;
        let (_, h) = wh.split_once('x')?;
        let mut it = rest.split_whitespace();
        let fps = it.next()?;
        let kbit: u32 = it.next()?.parse().ok()?;
        Some(format!("{h}p{fps} · {}", mbit(kbit)))
    };
    parse().unwrap_or_else(|| if q.is_empty() { "—".into() } else { q.into() })
}

/// What to ask for when starting to watch: the settings' "most to receive", or nothing.
pub fn watch_request() -> Option<String> {
    let w = settings().get("watch").cloned().unwrap_or_default();
    let s = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let (res, fps, kbit) = (s("res"), s("fps"), s("kbit"));
    if res.is_empty() && fps.is_empty() && kbit.is_empty() {
        return None;
    }
    let res = if res.is_empty() { "1440".into() } else { res };
    let fps = if fps.is_empty() { "60".into() } else { fps };
    Some(if kbit.is_empty() { format!("{res}p{fps}") } else { format!("{res}p{fps} {kbit}") })
}

fn clock(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

pub fn live_for(secs: u64) -> SharedString {
    tf("room.live_for", &[("t", &clock(secs))])
}

fn small_text(t: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(color::MUTED)).child(t.into())
}

fn card() -> Div {
    div().bg(rgb(color::SIDE)).rounded(px(10.))
}

impl Shell {
    /// Watch a streamer (a Watch button). Joining the video blocks: off the UI thread.
    pub fn watch_stream(&mut self, id: u64, cx: &mut Context<Self>) {
        let core = self.core.clone();
        let quality = watch_request();
        let task = cx.background_executor().spawn(async move { core::watch(&core, id, quality) });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Err(e) = result {
                    s.error = Some(tf("notice.error", &[("msg", &e)]).to_string());
                }
                s.tick(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// A picker changed: asks for it when no other change follows within `APPLY_AFTER`.
    fn picker_changed(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(t) = self.room.tiles.get_mut(&id) else { return };
        t.touched = true;
        t.picked += 1;
        let picked = t.picked;
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            cx.background_executor().timer(APPLY_AFTER).await;
            let _ = this.update(cx, |s, cx| {
                if s.room.tiles.get(&id).is_some_and(|t| t.picked == picked) {
                    s.apply_quality(id, cx);
                }
            });
        })
        .detach();
    }

    /// Asks the streamer for what the pickers say (one of the host's presets). "As streamed"
    /// with no bitrate asks for the most: the streamer's own quality.
    pub fn apply_quality(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(t) = self.room.tiles.get_mut(&id) {
            t.touched = true;
        }
        eprintln!("quality: asking {id} for {:?}", self.picked_request(id, cx));
        if let Some(q) = self.picked_request(id, cx).or_else(|| Some("1440p60".to_string())) {
            if let Err(e) = core::set_quality(&self.core, id, &q) {
                self.error = Some(e);
                self.error_since = Some(std::time::Instant::now());
            }
        }
        cx.notify();
    }

    pub fn stop_watch(&mut self, id: u64, cx: &mut Context<Self>) {
        core::unwatch(&self.core, id);
        self.tick(cx);
    }

    fn picked_request(&self, id: u64, cx: &App) -> Option<String> {
        let t = self.room.tiles.get(&id)?;
        let m = t.mode.read(cx).selected_index(cx).map(|i| i.row).unwrap_or(0);
        let k = t.kbit.read(cx).selected_index(cx).map(|i| i.row).unwrap_or(0);
        if m == 0 && k == 0 {
            return None;
        }
        let mode = if m == 0 { "1440p60" } else { VIEW_MODES[m - 1] };
        Some(if k == 0 { mode.to_string() } else { format!("{mode} {}", PRESETS[k - 1]) })
    }

    /// Makes the pickers for newly watched streams, and makes untouched ones show what is
    /// being watched.
    pub fn sync_tiles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let watches = self.snap.watches.clone();
        self.room.tiles.retain(|id, _| watches.iter().any(|w| w.streamer == *id));
        self.room.placed.borrow_mut().retain(|id, _| watches.iter().any(|w| w.streamer == *id));
        for w in &watches {
            let (m, k) = match w.quality.as_deref().map(|q| q.split_once(' ').unwrap_or((q, ""))) {
                None => (0, 0),
                Some((m, k)) => (VIEW_MODES.iter().position(|x| *x == m).map_or(0, |i| i + 1), PRESETS.iter().position(|x| x.to_string() == k).map_or(0, |i| i + 1)),
            };
            if !self.room.tiles.contains_key(&w.streamer) {
                let modes: Vec<SharedString> = std::iter::once(t("card.source_quality")).chain(VIEW_MODES.iter().map(|m| SharedString::from(*m))).collect();
                let kbits: Vec<SharedString> = std::iter::once("—".into()).chain(PRESETS.iter().map(|k| SharedString::from(format!("{} {}", mbit(*k), t("unit.mbit"))))).collect();
                let mode = cx.new(|cx| SelectState::new(modes, Some(IndexPath::new(m)), window, cx));
                let kbit = cx.new(|cx| SelectState::new(kbits, Some(IndexPath::new(k)), window, cx));
                let id = w.streamer;
                let on_pick = move |s: &mut Shell, _: Picker, e: &SelectEvent<Vec<SharedString>>, cx: &mut Context<Shell>| {
                    if let SelectEvent::Confirm(Some(_)) = e {
                        s.picker_changed(id, cx);
                    }
                };
                let _subs = [cx.subscribe(&mode, on_pick), cx.subscribe(&kbit, on_pick)];
                self.room.tiles.insert(w.streamer, Tile { mode, kbit, shown: (m, k), touched: false, picked: 0, _subs });
                continue;
            }
            let t = self.room.tiles.get_mut(&w.streamer).unwrap();
            let now = (t.mode.read(cx).selected_index(cx).map_or(0, |i| i.row), t.kbit.read(cx).selected_index(cx).map_or(0, |i| i.row));
            // A picker the viewer changed counts as touched from then on.
            if now != t.shown {
                t.touched = true;
            }
            if t.touched || t.shown == (m, k) {
                continue;
            }
            t.shown = (m, k);
            let (mode, kbit) = (t.mode.clone(), t.kbit.clone());
            mode.update(cx, |s, cx| s.set_selected_index(Some(IndexPath::new(m)), window, cx));
            kbit.update(cx, |s, cx| s.set_selected_index(Some(IndexPath::new(k)), window, cx));
        }
    }

    /// Hides the video windows while the join form covers the room, or a list is open: the
    /// video windows sit above the main window, so a list opening over the video (the quality
    /// pickers under it) was hidden behind it. STREAMER_DEBUG_LISTS_UNDER_VIDEO: the old way.
    pub fn sync_videos_hidden(&mut self, cx: &App) {
        let list_open = gpui_kit::component::GlobalState::is_in_deferred_context(cx) && std::env::var_os("STREAMER_DEBUG_LISTS_UNDER_VIDEO").is_none();
        let hide = self.join_shown() || self.dialog_open() || list_open;
        if hide != self.room.videos_hidden {
            self.room.videos_hidden = hide;
            core::videos_hidden(&self.core, hide);
        }
    }

    pub fn render_room(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let me = self.snap.me;
        let watched: Vec<PeerView> = self.snap.peers.iter().filter(|p| p.streaming && p.id != me && p.watching).cloned().collect();
        let others: Vec<PeerView> = self.snap.peers.iter().filter(|p| p.streaming && p.id != me && !p.watching).cloned().collect();
        let live = self.snap.live.clone();
        let header = self.room_header(&live, cx);
        let stage = if watched.is_empty() { None } else { Some(self.stage(&watched, cx)) };
        let hint = self.snap.watches.iter().find(|w| w.received_fps > 20.0 && w.shown_fps < w.received_fps * 0.8).map(|w| {
            let name = self.snap.peers.iter().find(|p| p.id == w.streamer).map(|p| p.name.clone()).unwrap_or("?".into());
            tf("hint.background", &[("name", &name), ("shown", &w.shown_fps.round()), ("rx", &w.received_fps.round())]).to_string()
        });
        // Reconnecting shows where a hint would (amber): the room is kept meanwhile.
        let hint = if self.snap.reconnecting { Some(t("notice.reconnecting").to_string()) } else { hint };
        let notice = self.error.clone();
        let has_stage = stage.is_some();
        // The content goes in its own box: the scroll area holds only it and the scrollbar
        // (in the scroll area itself the scrollbar took a gap and made 24 px to scroll).
        let content = div()
            .px_4()
            .pt_2()
            .pb_4()
            .flex()
            .flex_col()
            .gap_4()
            // With a video on: one line of fixed height under it for a notice, a hint or the
            // keys, so a notice coming or going no longer resizes the video (it jumped).
            // Long text ends with "…"; all of it shows on hover.
            .when(has_stage, |d| {
                let (text, bg, fg): (SharedString, Option<u32>, u32) = match (&notice, &hint) {
                    (Some(n), _) => (n.clone().into(), Some(0x3b2326), 0xffb4b4),
                    (None, Some(h)) => (h.clone().into(), Some(0x3a3220), 0xf0d48a),
                    (None, None) => (t("tile.keys"), None, color::MUTED),
                };
                let tip = text.clone();
                d.child(
                    div()
                        .id("below-line")
                        .h(px(30.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px_3()
                        .rounded_md()
                        .when_some(bg, |d, c| d.bg(rgb(c)))
                        .text_color(rgb(fg))
                        .text_xs()
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .child(div().min_w_0().truncate().child(text)),
                )
            })
            .when(!has_stage, |d| {
                d.when_some(notice.clone(), |d, n| d.child(div().p_3().rounded_md().bg(rgb(0x3b2326)).text_color(rgb(0xffb4b4)).child(n)))
                    .when_some(hint.clone(), |d, h| d.child(div().p_3().rounded_md().bg(rgb(0x3a3220)).text_color(rgb(0xf0d48a)).child(h)))
            })
            .when(!others.is_empty(), |d| {
                d.child(div().flex().flex_col().gap_2().child(div().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::MUTED)).child(tu("room.also_live"))).child(div().flex().flex_wrap().gap_2p5().children(others.iter().map(|p| {
                    let id = p.id;
                    card().flex().items_center().gap_2p5().pl_2().pr_2p5().py_2().child(crate::avatar(&p.name)).child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(p.name.clone())).child(Button::new(SharedString::from(format!("watch-{id}"))).cursor_pointer().primary().small().label(t("card.watch")).on_click(cx.listener(move |s, _, _, cx| s.watch_stream(id, cx))))
                }))))
            })
            .when_some(live.clone(), |d, l| d.child(self.live_panel(&l)))
            .when(watched.is_empty() && others.is_empty() && live.is_none(), |d| d.child(div().py_10().flex().justify_center().text_color(rgb(color::MUTED)).child(t("room.none"))))
            // The spoiled variant for the watch gate's scroll check.
            .when(std::env::var_os("STREAMER_DEBUG_TALL_BELOW").is_some(), |d| d.child(div().flex_none().h(px(600.))));
        let below = div()
            .id("below")
            .overflow_y_scroll()
            .track_scroll(&self.room.below)
            .when(has_stage, |d| d.flex_none().max_h(relative(0.45)))
            .when(!has_stage, |d| d.flex_1().min_h_0())
            .child(content)
            .vertical_scrollbar(&self.room.below);
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(rgb(color::MAIN))
            .flex()
            .flex_col()
            .child(header)
            .when_some(stage, |d, s| d.child(s))
            .child(below)
            .into_any_element()
    }

    fn room_header(&self, live: &Option<LiveStats>, cx: &mut Context<Self>) -> impl IntoElement {
        let row = div().h_12().flex_none().px_4().flex().items_center().gap_3().border_b_1().border_color(rgb(color::LINE));
        match live {
            Some(l) => row
                .child(div().px_1p5().rounded_sm().bg(rgb(color::LIVE)).text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child("LIVE"))
                .child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(t("live.title")))
                .child(div().flex_1().text_xs().text_color(rgb(color::MUTED)).child(format!("{} {} · {}", short_quality(&l.ceiling), t("unit.mbit"), l.encoder)))
                .child(Button::new("change-live").cursor_pointer().small().label(t("live.change")).on_click(cx.listener(|s, _, w, cx| s.open_golive(true, w, cx))))
                .child(Button::new("end-live").cursor_pointer().danger().small().label(t("live.end")).on_click(cx.listener(|s, _, _, cx| {
                    core::end_live(&s.core);
                    s.tick(cx);
                }))),
            None => row.child(div().font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(t("room.streams"))),
        }
    }

    /// Watched streams side by side: one, two in a row, or a 2×2 grid.
    fn stage(&self, watched: &[PeerView], cx: &mut Context<Self>) -> impl IntoElement {
        let tiles: Vec<AnyElement> = watched.iter().map(|p| self.tile(p, cx).into_any_element()).collect();
        let mut rows: Vec<Vec<AnyElement>> = vec![];
        let per_row = if tiles.len() == 1 { 1 } else { 2 };
        for t in tiles {
            if rows.last().is_none_or(|r| r.len() == per_row) {
                rows.push(vec![]);
            }
            rows.last_mut().unwrap().push(t);
        }
        div().flex_1().min_h_0().px_4().pt_4().pb_2().flex().flex_col().gap_3().children(rows.into_iter().map(|r| div().flex_1().min_h_0().flex().gap_3().children(r)))
    }

    fn tile(&self, p: &PeerView, cx: &mut Context<Self>) -> impl IntoElement {
        let id = p.id;
        let w: WatchStats = self.snap.watches.iter().find(|w| w.streamer == id).cloned().unwrap_or_default();
        let core = self.core.clone();
        let embedded = w.embedded;
        let stat = format!("{:.0} fps · {}{}", w.shown_fps, ms(w.latency_ms), if w.quality.is_some() { format!(" · {}", request_label(&w.quality)) } else { String::new() });
        let throttled = w.received_fps > 20.0 && w.shown_fps < w.received_fps * 0.8;
        let placed = self.room.placed.clone();
        // The video window goes over this box, in physical pixels of the window's client area.
        let video = canvas(
            move |b, window, cx| {
                let s = window.scale_factor();
                let rect = (embedded && f32::from(b.size.width) > 4.0 && f32::from(b.size.height) > 4.0).then(|| {
                    ((f32::from(b.origin.x) * s).round() as i32, (f32::from(b.origin.y) * s).round() as i32, (f32::from(b.size.width) * s).round() as i32, (f32::from(b.size.height) * s).round() as i32)
                });
                if std::env::var_os("STREAMER_DEBUG_NO_PLACE").is_some() {
                    return;
                }
                let mut placed = placed.borrow_mut();
                if placed.get(&id) != Some(&rect) {
                    core::video_place(&core, id, rect);
                    placed.insert(id, rect);
                }
                let _ = cx;
            },
            |_, _, _, _| {},
        )
        .size_full();
        let tile = self.room.tiles.get(&id);
        let act = |action: &'static str| {
            cx.listener(move |s: &mut Shell, _: &ClickEvent, _: &mut Window, cx: &mut Context<Shell>| {
                let _ = core::video_window_do(&s.core, id, action);
                // The buttons' look follows at once (the stats sample once a second).
                s.snap.watches = core::watches(&s.core);
                cx.notify();
            })
        };
        card()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .bg(rgb(0x000000))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(color::MUTED))
                    .child(video)
                    .when(!embedded, |d| d.child(tf("tile.popped", &[("name", &p.name)]))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1p5()
                    .px_2p5()
                    .py_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::BRIGHT)).child(p.name.clone()))
                    .child(div().flex_1().text_xs().whitespace_nowrap().text_color(rgb(if throttled { 0xf0b232 } else { color::MUTED })).child(stat))
                    .when_some(tile, |d, t| d.child(div().w(px(150.)).cursor_pointer().child(Select::new(&t.mode).small())).child(div().w(px(110.)).cursor_pointer().child(Select::new(&t.kbit).small())))
                    .child(Button::new(SharedString::from(format!("mute-{id}"))).cursor_pointer().small().icon(Icon::default().path(if w.muted { "icons/volume-x.svg" } else { "icons/volume-2.svg" })).tooltip(t(if w.muted { "tile.unmute" } else { "tile.mute" })).on_click(cx.listener(move |s, _, _, cx| {
                        let _ = core::set_muted(&s.core, id, !w.muted);
                        s.tick(cx);
                    })))
                    .child(Button::new(SharedString::from(format!("stats-{id}"))).cursor_pointer().small().label(t("tile.stats")).when(w.overlay, |b| b.primary()).on_click(act(if w.overlay { "no_overlay" } else { "overlay" })))
                    .child(Button::new(SharedString::from(format!("pop-{id}"))).cursor_pointer().small().icon(Icon::default().path(if w.embedded { "icons/square-arrow-out-up-right.svg" } else { "icons/picture-in-picture.svg" })).tooltip(t(if w.embedded { "tile.popout" } else { "tile.popin" })).on_click(act(if w.embedded { "pop_out" } else { "pop_in" })))
                    .child(Button::new(SharedString::from(format!("fs-{id}"))).cursor_pointer().small().icon(Icon::default().path("icons/maximize.svg")).tooltip(t("tile.fullscreen")).on_click(act("toggle_fullscreen")))
                    .child(Button::new(SharedString::from(format!("stop-{id}"))).cursor_pointer().danger().small().label(t("card.stop")).on_click(cx.listener(move |s, _, _, cx| s.stop_watch(id, cx)))),
            )
    }

    fn live_panel(&self, l: &LiveStats) -> impl IntoElement {
        let active: Vec<_> = l.layers.iter().filter(|x| !x.parked).collect();
        let fps = active.iter().map(|x| x.fps).fold(0.0, f64::max).round();
        let limit = settings_upload_limit();
        let cap = if limit > 0.0 { limit } else { (l.ceiling_mbit * l.viewers.len().max(1) as f64).max(1.0) };
        let wont_fit = limit > 0.0 && l.upload_mbit + l.ceiling_mbit > limit;
        let tile = |label: SharedString, num: String| card().flex_1().p(px(14.)).flex().flex_col().gap_1p5().child(small_text(label)).child(div().text_size(px(22.)).font_weight(FontWeight::BOLD).text_color(rgb(color::BRIGHT)).child(num));
        let row = |cells: [AnyElement; 4], head: bool| {
            let grid = div().flex().items_center().gap_3().px_4().py(px(if head { 10. } else { 12. })).border_b_1().border_color(rgb(0x34363c));
            let mut i = 0;
            grid.children(cells.into_iter().map(move |c| {
                let w = [2.0, 1.6, 1.0, 2.4][i];
                i += 1;
                div().flex_grow(w).flex_shrink(w).flex_basis(px(0.)).min_w_0().child(c)
            }))
        };
        let up = if limit > 0.0 { tf("up.of", &[("used", &num(l.upload_mbit)), ("limit", &num(limit))]).to_string() } else { format!("{} {}", num(l.upload_mbit), t("unit.mbit")) };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(tile(t("live.viewers"), l.viewers.len().to_string()))
                    .child(tile(t("live.encoding"), format!("{fps} fps")).child(small_text(tf("live.layers", &[("n", &active.len()), ("list", &if active.is_empty() { "—".to_string() } else { active.iter().map(|x| short_quality(&x.quality)).collect::<Vec<_>>().join(", ") })]))))
                    .child(tile(t("live.upload"), up).child(div().h_2().rounded(px(4.)).bg(rgb(color::RAIL)).child(div().h_2().rounded(px(4.)).w(relative((l.upload_mbit / cap).min(1.0) as f32)).bg(rgb(if wont_fit { 0xf0b232 } else { 0x2f9e6e }))))),
            )
            .when(wont_fit, |d| d.child(div().p_3().rounded_md().bg(rgb(0x3a3220)).text_color(rgb(0xf0d48a)).child(tf("up.next_wont_fit", &[("used", &num(l.upload_mbit)), ("add", &num(l.ceiling_mbit)), ("limit", &num(limit))]))))
            .child(if l.viewers.is_empty() {
                div().py_6().flex().justify_center().text_color(rgb(color::MUTED)).child(t("live.no_viewers")).into_any_element()
            } else {
                card()
                    .overflow_hidden()
                    .child(row([small_text(t("live.th_viewer")).into_any_element(), small_text(t("live.th_quality")).into_any_element(), small_text(t("live.th_rtt")).into_any_element(), small_text(t("live.th_state")).into_any_element()], true))
                    .children(l.viewers.iter().map(|v| {
                        let q = v.problems.iter().find(|p| p.starts_with("queue:"));
                        let loss = v.problems.iter().find(|p| p.starts_with("loss:"));
                        let dot = |c: u32| div().size_2().rounded_full().bg(rgb(c));
                        let state = if let Some(q) = q {
                            let secs = q[6..].parse::<f64>().unwrap_or(0.0) / 1000.0;
                            div().flex().flex_col().child(div().flex().items_center().gap_1p5().text_color(rgb(0xf0b232)).child(dot(0xf0b232)).child(tf("live.queue", &[("s", &num(secs))]))).child(small_text(t("live.queue_note")))
                        } else if let Some(l) = loss {
                            div().flex().items_center().gap_1p5().text_color(rgb(0xf0b232)).child(dot(0xf0b232)).child(tf("live.loss", &[("pct", &&l[5..])]))
                        } else {
                            div().flex().items_center().gap_1p5().text_color(rgb(0x3fbf7f)).child(dot(0x3fbf7f)).child(t("live.ok"))
                        };
                        row([div().flex().items_center().gap_2p5().child(crate::avatar(&v.name)).child(v.name.clone()).into_any_element(), div().child(short_quality(&v.quality)).into_any_element(), div().child(ms(v.rtt_ms)).into_any_element(), state.into_any_element()], false)
                    }))
                    .into_any_element()
            })
            .child(small_text(t("live.isolation")))
    }

    /// Tests only (STREAMER_APP_TEST_GPUI): the room's layout and our stream, when they change.
    pub fn report_room(&mut self) {
        if std::env::var_os("STREAMER_APP_TEST_GPUI").is_none() {
            return;
        }
        let tiles = self.snap.peers.iter().filter(|p| p.watching).count();
        let max = f32::from(self.room.below.max_offset().y);
        let view = f32::from(self.room.below.bounds().size.height);
        let placed: Vec<String> = self.room.placed.borrow().iter().map(|(id, r)| format!("{id}:{}", r.map(|(x, y, w, h)| format!("{x},{y},{w}x{h}")).unwrap_or("none".into()))).collect();
        let live = self.snap.live.as_ref().map(|l| format!("viewers {} [{}] layers {} [{}] ceiling {}", l.viewers.len(), l.viewers.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(","), l.layers.iter().filter(|x| !x.parked).count(), l.layers.iter().filter(|x| !x.parked).map(|x| x.quality.as_str()).collect::<Vec<_>>().join(","), l.ceiling)).unwrap_or("-".into());
        let asked: Vec<String> = self.snap.watches.iter().map(|w| format!("{}:{}", w.streamer, w.quality.clone().unwrap_or("source".into()))).collect();
        let line = format!("tiles {tiles}; below scroll max {max:.0} view {view:.0}; placed [{}]; asked [{}]; live {live}", placed.join(" "), asked.join(" "));
        if line != self.room.reported {
            eprintln!("test: {line}");
            self.room.reported = line;
        }
    }
}

fn settings_upload_limit() -> f64 {
    let v = crate::state_value("uploadLimit");
    v.as_ref().and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0).max(0.0)
}

/// Tests only: STREAMER_APP_CLICK_WATCH="name" presses that streamer's Watch button once it
/// shows, the way a user does.
pub fn test_click_watch(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(name) = std::env::var("STREAMER_APP_CLICK_WATCH") else { return };
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let executor = cx.background_executor().clone();
        for _ in 0..120 {
            executor.timer(Duration::from_millis(250)).await;
            let id = this.update(cx, |s, _| s.snap.peers.iter().find(|p| p.streaming && !p.me && !p.watching && p.name == name).map(|p| p.id)).ok().flatten();
            if let Some(id) = id {
                let _ = this.update(cx, |s, cx| s.watch_stream(id, cx));
                eprintln!("test: watch pressed for {name}");
                return;
            }
        }
        eprintln!("test: no stream of {name} to watch");
    })
    .detach();
}

/// Tests only: STREAMER_APP_TEST_QUALITY="720p30 2000" picks that mode and bitrate in the
/// first watched stream's pickers 24 s after it shows and presses Apply; "...|stop" then
/// presses Stop 10 s later.
pub fn test_quality(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(spec) = std::env::var("STREAMER_APP_TEST_QUALITY") else { return };
    let (want, stop) = match spec.split_once('|') {
        Some((q, s)) => (q.to_string(), s == "stop"),
        None => (spec.clone(), false),
    };
    let (mode, kbit) = want.split_once(' ').map(|(m, k)| (m.to_string(), k.to_string())).unwrap_or((want.clone(), String::new()));
    let m = VIEW_MODES.iter().position(|x| *x == mode).map_or(0, |i| i + 1);
    let k = PRESETS.iter().position(|x| x.to_string() == kbit).map_or(0, |i| i + 1);
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let executor = cx.background_executor().clone();
        let mut id = None;
        for _ in 0..120 {
            executor.timer(Duration::from_millis(250)).await;
            id = this.update(cx, |s, _| s.room.tiles.keys().next().copied()).ok().flatten();
            if id.is_some() {
                break;
            }
        }
        let Some(id) = id else { return };
        executor.timer(Duration::from_secs(24)).await;
        let _ = this.update_in(cx, |s, window, cx| {
            if let Some(t) = s.room.tiles.get(&id) {
                let (a, b) = (t.mode.clone(), t.kbit.clone());
                a.update(cx, |p, cx| p.set_selected_index(Some(IndexPath::new(m)), window, cx));
                b.update(cx, |p, cx| p.set_selected_index(Some(IndexPath::new(k)), window, cx));
            }
            s.apply_quality(id, cx);
        });
        eprintln!("test: quality applied {want}");
        if stop {
            executor.timer(Duration::from_secs(10)).await;
            let _ = this.update(cx, |s, cx| s.stop_watch(id, cx));
            eprintln!("test: stop pressed");
        }
    })
    .detach();
}
