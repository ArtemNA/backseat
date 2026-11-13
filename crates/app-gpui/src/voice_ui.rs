//! Voice chat in the UI beyond the side panel: the voice settings applied to the core, and a
//! member's volume (right click on them in the voice list), kept by name in the settings.

use crate::i18n::{t, tf};
use crate::{color, set_setting, settings, Shell};
use backseat_core as core;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Push-to-talk keys to pick from: Windows virtual-key codes and their names.
pub const PTT_KEYS: [(&str, &str); 10] = [
    ("0x05", "Mouse 4"),
    ("0x06", "Mouse 5"),
    ("0xC0", "` ~"),
    ("0x14", "Caps Lock"),
    ("0x56", "V"),
    ("0x42", "B"),
    ("0x47", "G"),
    ("0xA4", "Left Alt"),
    ("0x7C", "F13"),
    ("0x7D", "F14"),
];

fn voice_pref(key: &str) -> Option<String> {
    settings().get("voice").and_then(|s| s.get(key)).and_then(|v| v.as_str()).map(String::from).filter(|v| !v.is_empty())
}

fn flag(key: &str) -> bool {
    settings().get(key).and_then(|v| v.as_bool()) != Some(false)
}

/// The voice settings as saved, for the core.
pub fn saved() -> core::VoiceSettings {
    let ptt = voice_pref("mode").as_deref() == Some("ptt");
    let key = voice_pref("key").unwrap_or_else(|| PTT_KEYS[0].0.to_string());
    core::VoiceSettings {
        mic: voice_pref("mic"),
        speakers: voice_pref("out"),
        ptt_key: ptt.then(|| i32::from_str_radix(key.trim_start_matches("0x"), 16).ok()).flatten(),
        event_sounds: flag("voiceSounds"),
        ptt_sounds: flag("pttSounds"),
        threshold: threshold(),
        // Off unless chosen: the stream's sound stays exactly as without voice chat.
        stream_echo: settings().get("voiceStreamEcho").and_then(|v| v.as_bool()) == Some(true),
        dsp: media::voice::DspConfig {
            echo_cancel: flag("voiceEcho"),
            // Unset: the usual middle level; "off": none.
            noise: match voice_pref("noise").as_deref() {
                Some("off") | Some("rnnoise") => None,
                None => Some("moderate".into()),
                Some(v) => Some(v.to_string()),
            },
            auto_gain: flag("voiceGain"),
            rnnoise: voice_pref("noise").as_deref() == Some("rnnoise"),
        },
    }
}

/// The lowest end of the threshold slider means "off".
pub const THRESHOLD_OFF: f32 = -80.0;

/// The saved voice activity threshold (dBFS), None for off. New installs start at -50.
pub fn threshold() -> Option<f64> {
    let v = settings().get("voiceThreshold").and_then(|v| v.as_f64()).unwrap_or(-50.0);
    (v > THRESHOLD_OFF as f64).then_some(v)
}

impl Shell {
    /// The threshold slider for the settings, made when they open.
    pub fn threshold_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (Entity<SliderState>, Subscription) {
        let now = threshold().map_or(THRESHOLD_OFF, |v| v as f32);
        let slider = cx.new(|_| SliderState::new().min(THRESHOLD_OFF).max(-20.0).step(1.0).default_value(now));
        let sub = cx.subscribe_in(&slider, window, |this: &mut Shell, _, e: &SliderEvent, _, cx| {
            let (v, done) = match e {
                SliderEvent::Change(v) => (v.start(), false),
                SliderEvent::Release(v) => (v.start(), true),
            };
            set_setting("voiceThreshold", serde_json::json!(v.round()));
            if done {
                apply_settings(&this.core);
            } else {
                // Moving the slider is heard at once, without rewriting everything else.
                let voice = this.core.session.lock().unwrap().clone().and_then(|s| s.voice());
                if let Some(voice) = voice {
                    voice.set_threshold((v > THRESHOLD_OFF).then_some(v as f64));
                }
            }
            cx.notify();
        });
        (slider, sub)
    }
}

/// The threshold row: its value, the slider, and (in voice) the microphone's level now,
/// green where it would open.
pub fn threshold_view(slider: &Entity<SliderState>, level: Option<f64>) -> Div {
    let th = threshold();
    let label = match th {
        Some(v) => tf("voice.threshold_at", &[("db", &format!("{v:.0}"))]),
        None => t("voice.threshold_off"),
    };
    let meter = level.map(|db| {
        let frac = ((db as f32 - THRESHOLD_OFF) / (-20.0 - THRESHOLD_OFF)).clamp(0.0, 1.0);
        let open = th.is_none_or(|t| db >= t);
        div()
            .h(px(6.))
            .w_full()
            .rounded_full()
            .bg(rgb(color::RAIL))
            .child(div().h_full().rounded_full().w(relative(frac)).bg(rgb(if open { color::GREEN } else { 0x6d6f78 })))
    });
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::MUTED)).child(label))
        .child(div().cursor_pointer().child(Slider::new(slider)))
        .children(meter)
        .child(div().text_xs().text_color(rgb(color::MUTED)).child(if level.is_some() { t("voice.threshold_hint") } else { t("voice.threshold_join") }))
}

/// The microphone's level alone (push-to-talk has no threshold to show it against).
pub fn level_meter(level: Option<f64>) -> Option<Div> {
    let db = level?;
    let frac = ((db as f32 - THRESHOLD_OFF) / (-20.0 - THRESHOLD_OFF)).clamp(0.0, 1.0);
    Some(div().h(px(6.)).w_full().rounded_full().bg(rgb(color::RAIL)).child(div().h_full().rounded_full().w(relative(frac)).bg(rgb(color::GREEN))))
}

pub fn apply_settings(c: &std::sync::Arc<core::App>) {
    core::refresh_audio_devices();
    let s = saved();
    eprintln!("voice settings: mic {} speakers {} push-to-talk {:?} sounds {}/{}", if s.mic.is_some() { "chosen" } else { "default" }, if s.speakers.is_some() { "chosen" } else { "default" }, s.ptt_key, s.event_sounds, s.ptt_sounds);
    core::set_voice_settings(c, s);
}

/// Saved volumes, percent by member name.
fn volumes() -> serde_json::Map<String, serde_json::Value> {
    match settings().get("voiceVolumes") {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => Default::default(),
    }
}

fn save_volume(name: &str, percent: f32) {
    let mut m = volumes();
    if (percent - 100.0).abs() < 0.5 {
        m.remove(name);
    } else {
        m.insert(name.into(), serde_json::json!(percent.round()));
    }
    set_setting("voiceVolumes", serde_json::Value::Object(m));
}

pub struct VoiceMenu {
    id: u64,
    name: String,
    at: Point<Pixels>,
    slider: Entity<SliderState>,
    percent: f32,
    _sub: Subscription,
}

impl Shell {
    /// Gives each member in voice the volume saved for their name (when the members change).
    pub fn apply_volumes(&mut self) {
        let members: Vec<u64> = self.snap.voice.members.clone();
        if members == self.volumes_for {
            return;
        }
        let saved = volumes();
        for id in &members {
            if *id == self.snap.me {
                continue;
            }
            if let Some(p) = self.snap.peers.iter().find(|p| p.id == *id) {
                let v = saved.get(&p.name).and_then(|v| v.as_f64()).unwrap_or(100.0);
                core::set_peer_volume(&self.core, *id, v / 100.0);
            }
        }
        self.volumes_for = members;
    }

    pub fn open_voice_menu(&mut self, id: u64, name: String, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let percent = (core::peer_volume(&self.core, id) * 100.0) as f32;
        let slider = cx.new(|_| SliderState::new().min(0.0).max(200.0).step(5.0).default_value(percent));
        let n = name.clone();
        let sub = cx.subscribe_in(&slider, window, move |this: &mut Shell, _, e: &SliderEvent, _, cx| {
            let (v, done) = match e {
                SliderEvent::Change(v) => (v.start(), false),
                SliderEvent::Release(v) => (v.start(), true),
            };
            core::set_peer_volume(&this.core, id, v as f64 / 100.0);
            if let Some(m) = this.voice_menu.as_mut() {
                m.percent = v;
            }
            if done {
                save_volume(&n, v);
            }
            cx.notify();
        });
        self.voice_menu = Some(VoiceMenu { id, name, at, slider, percent, _sub: sub });
        cx.notify();
    }

    pub fn render_voice_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.voice_menu.as_ref()?;
        let close = |this: &mut Shell, cx: &mut Context<Shell>| {
            this.voice_menu = None;
            cx.notify();
        };
        let (id, name) = (m.id, m.name.clone());
        Some(
            div()
                .id("voice-menu-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| close(this, cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| close(this, cx)))
                .child(
                    div()
                        .id("voice-menu")
                        .absolute()
                        .left(m.at.x)
                        .top(m.at.y)
                        .w(px(240.))
                        .p_3()
                        .gap_2()
                        .rounded_lg()
                        .bg(rgb(0x111214))
                        .border_1()
                        .border_color(rgb(color::LINE))
                        .flex()
                        .flex_col()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(div().truncate().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color::BRIGHT)).child(name.clone()))
                        .child(div().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(color::MUTED)).child(tf("voice.volume", &[("n", &format!("{:.0}", m.percent))])))
                        .child(div().cursor_pointer().child(Slider::new(&m.slider)))
                        .when((m.percent - 100.0).abs() >= 0.5, |d| {
                            d.child(
                                div()
                                    .id("voice-volume-reset")
                                    .self_start()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(rgb(color::MUTED))
                                    .hover(|s| s.bg(rgb(0x35373c)).text_color(rgb(color::BRIGHT)))
                                    .child(t("voice.reset"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        core::set_peer_volume(&this.core, id, 1.0);
                                        save_volume(&name, 100.0);
                                        if let Some(m) = this.voice_menu.as_mut() {
                                            m.percent = 100.0;
                                            m.slider.update(cx, |s, cx| s.set_value(100.0, window, cx));
                                        }
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

/// Tests only: STREAMER_APP_TEST_VOLUME="<name>=<percent>" sets that member's volume the way
/// the slider does once they are in voice, and saves it.
pub fn test_volume(shell: &mut Shell) {
    let Ok(spec) = std::env::var("STREAMER_APP_TEST_VOLUME") else { return };
    let Some((name, pct)) = spec.split_once('=') else { return };
    let Some(p) = shell.snap.peers.iter().find(|p| p.name == name && shell.snap.voice.members.contains(&p.id)) else { return };
    let v: f32 = pct.parse().unwrap_or(100.0);
    if (core::peer_volume(&shell.core, p.id) * 100.0 - v as f64).abs() > 0.5 {
        core::set_peer_volume(&shell.core, p.id, v as f64 / 100.0);
        save_volume(name, v);
        eprintln!("test voice: volume of {name} set to {v}%");
    }
}
