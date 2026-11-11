//! Test drivers that press the UI's own controls, the way the webview's test drivers did
//! (app.js: test_form, test_rooms, test_ui_stress, test_prefs), so the same gates run against
//! this app. Each is off unless its STREAMER_APP_* variable is set.

use crate::join::{Mode, Role};
use crate::{dialogs, i18n, set_setting, settings, Shell};
use backseat_core as core;
use gpui_kit::*;
use std::sync::mpsc;
use std::time::Duration;

/// STREAMER_APP_TEST_PREFS: JSON merged into the saved settings at start, before anything
/// reads them.
pub fn apply_test_prefs() {
    let Ok(p) = std::env::var("STREAMER_APP_TEST_PREFS") else { return };
    let Ok(serde_json::Value::Object(add)) = serde_json::from_str::<serde_json::Value>(&p) else {
        eprintln!("test: STREAMER_APP_TEST_PREFS is not a JSON object");
        return;
    };
    let _ = settings();
    for (k, v) in add {
        set_setting(&k, v);
    }
}

/// The core runs a closure on the UI thread through this (a video test places the window
/// from there while it toggles fullscreen). Polled every 2 ms, and only in that test.
pub fn on_main(window: &mut Window, cx: &mut Context<Shell>) {
    if std::env::var_os("STREAMER_APP_AUTOWINDOW").is_none() {
        return;
    }
    let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
    let tx = std::sync::Mutex::new(tx);
    let _ = core::ON_MAIN.set(Box::new(move |f| {
        let _ = tx.lock().unwrap().send(f);
    }));
    cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| loop {
        cx.background_executor().timer(Duration::from_millis(2)).await;
        while let Ok(f) = rx.try_recv() {
            f();
        }
    })
    .detach();
}

/// What the window shows, in the webview driver's words (leave gate).
fn layout(s: &Shell) -> String {
    let header = s.snap.title.clone().or_else(|| s.current.as_ref().map(|c| c.label())).unwrap_or_else(|| i18n::t("lobby.title").to_string());
    format!("shell {} join {} rail {} header {} connected {} room {}", !s.join_shown(), s.join_shown(), s.rooms.len(), header, s.snap.connected, s.snap.room)
}

/// STREAMER_APP_TEST_FORM="show" opens the join form; "direct|<pasted>|<name>" opens the
/// direct form on the invited side with that text pasted; "leave|<room code>|" leaves the
/// room it is in, then goes to that room through the rail, reporting each step.
/// (create|... and code|... are in join.rs.)
pub fn test_form_more(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(spec) = std::env::var("STREAMER_APP_TEST_FORM") else { return };
    let p: Vec<String> = spec.split('|').map(String::from).collect();
    let mode = p.first().cloned().unwrap_or_default();
    if !["show", "direct", "leave"].contains(&mode.as_str()) {
        return;
    }
    let value = p.get(1).cloned().unwrap_or_default();
    let name = p.get(2).cloned().unwrap_or_default();
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let ex = cx.background_executor().clone();
        ex.timer(Duration::from_millis(1500)).await;
        match mode.as_str() {
            "show" => {
                let _ = this.update(cx, |s, cx| s.open_join(cx));
            }
            "direct" => {
                let _ = this.update_in(cx, |s, window, cx| {
                    s.join.open = true;
                    s.join.mode = Mode::Direct;
                    s.join.role = Role::Answer;
                    s.reset_direct(window, cx);
                    s.join.incoming.update(cx, |i, cx| i.set_value(value.clone(), window, cx));
                    s.join.name.update(cx, |i, cx| i.set_value(name.clone(), window, cx));
                });
            }
            _ => {
                ex.timer(Duration::from_millis(500)).await;
                let _ = this.update(cx, |s, _| eprintln!("test: before: {}", layout(s)));
                let _ = this.update(cx, |s, cx| s.leave(cx));
                ex.timer(Duration::from_millis(2500)).await;
                let _ = this.update(cx, |s, _| eprintln!("test: after leave: {}", layout(s)));
                let _ = this.update(cx, |s, cx| {
                    if let Some(r) = s.rooms.iter().find(|r| r.room == value).cloned() {
                        s.open_room(r, cx);
                    }
                });
                ex.timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |s, _| eprintln!("test: after rail click: {}", layout(s)));
            }
        }
    })
    .detach();
}

/// STREAMER_APP_TEST_ROOMS="room B": 5 s after joining goes to room B, 5 s later back to the
/// first room through its button in the rail, 4 s later opens room B's menu.
pub fn test_rooms(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(other) = std::env::var("STREAMER_APP_TEST_ROOMS") else { return };
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let ex = cx.background_executor().clone();
        for _ in 0..80 {
            if this.update(cx, |s, _| s.snap.connected && s.current.is_some()).unwrap_or(false) {
                break;
            }
            ex.timer(Duration::from_millis(250)).await;
        }
        ex.timer(Duration::from_secs(5)).await;
        let first = this.update(cx, |s, _| s.current.clone()).ok().flatten();
        let Some(first) = first else { return };
        let _ = this.update(cx, |s, cx| {
            let mut b = first.clone();
            b.room = other.clone();
            b.title = None;
            s.open_room(b, cx);
        });
        ex.timer(Duration::from_secs(5)).await;
        let _ = this.update(cx, |s, cx| {
            if let Some(r) = s.rooms.iter().find(|r| r.same(&first)).cloned() {
                s.open_room(r, cx);
            }
        });
        ex.timer(Duration::from_secs(4)).await;
        let _ = this.update(cx, |s, cx| {
            if let Some(r) = s.rooms.iter().find(|r| r.room == other).cloned() {
                s.room_menu = Some((r, point(px(60.), px(80.))));
                cx.notify();
            }
        });
        eprintln!("test: rooms done");
    })
    .detach();
}

/// STREAMER_APP_UI_STRESS keeps the UI busy while in a room: every 1.5 s the go-live dialog
/// (with its source pictures) opens or closes, and every other time the language changes.
/// "windows" opens the dialog on the program window tab.
pub fn test_ui_stress(window: &mut Window, cx: &mut Context<Shell>) {
    let Ok(v) = std::env::var("STREAMER_APP_UI_STRESS") else { return };
    let windows_tab = v == "windows";
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let ex = cx.background_executor().clone();
        let mut n = 0usize;
        loop {
            ex.timer(Duration::from_millis(1500)).await;
            n += 1;
            let alive = this.update_in(cx, |s, window, cx| {
                if !s.snap.connected {
                    return;
                }
                if s.dialogs.open == Some(dialogs::Open::GoLive) {
                    s.close_dialog(cx);
                } else {
                    s.open_golive(false, window, cx);
                    if let Some(g) = s.dialogs.golive.as_mut() {
                        g.screen = !windows_tab;
                    }
                    s.refresh_thumbs(cx);
                }
                if n % 2 == 0 {
                    let (id, _) = i18n::LANGS[(n / 2) % i18n::LANGS.len()];
                    i18n::set_language(id);
                }
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// STREAMER_APP_TEST_VOICE=<Hz> (with STREAMER_APP_AUTOJOIN): once in the room, presses
/// "Join voice" (a tone instead of the microphone, nothing played; see core::join_voice),
/// turns the microphone off at +8 s and on at +12 s as its button does, and prints once a
/// second who is in voice and who is speaking, by name.
pub fn test_voice(window: &mut Window, cx: &mut Context<Shell>) {
    if std::env::var_os("STREAMER_APP_TEST_VOICE").is_none() {
        return;
    }
    cx.spawn_in(window, async move |this: WeakEntity<Shell>, cx: &mut AsyncWindowContext| {
        let mut joined_at = None::<u32>;
        for tick in 0..60u32 {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this.update_in(cx, |s, _, cx| {
                if !s.snap.connected {
                    return;
                }
                let Some(at) = joined_at else {
                    joined_at = Some(tick);
                    eprintln!("test voice: pressing join");
                    s.join_voice(cx);
                    return;
                };
                let since = tick - at;
                if since == 8 || since == 12 {
                    let off = since == 8;
                    core::set_mic_muted(&s.core, off);
                    s.snap.voice = core::voice_view(&s.core);
                    cx.notify();
                    eprintln!("test voice: microphone {}", if off { "off" } else { "on" });
                }
                let v = &s.snap.voice;
                let name = |id: &u64| s.snap.peers.iter().find(|p| p.id == *id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
                let levels: Vec<String> = core::voice_levels(&s.core).iter().map(|(id, db)| format!("{}:{db:.1}", name(id))).collect();
                eprintln!(
                    "test voice: t {since} joined {} members [{}] speaking [{}] muted [{}] deaf [{}] talking {} levels [{}]",
                    v.joined,
                    v.members.iter().map(name).collect::<Vec<_>>().join(","),
                    v.speaking.iter().map(name).collect::<Vec<_>>().join(","),
                    v.muted.iter().map(name).collect::<Vec<_>>().join(","),
                    v.deaf.iter().map(name).collect::<Vec<_>>().join(","),
                    v.talking,
                    levels.join(",")
                );
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();
}
