//! UI strings in English, Ukrainian and Russian (strings.json). The language is the saved
//! one ("lang" in the state file), else the system's when it is one of these, else English.
//! STREAMER_APP_LANG forces it (tests, screenshots). English fills any missing key.

use backseat_core as core;
use gpui_kit::SharedString;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub const LANGS: [(&str, &str); 3] = [("en", "English"), ("uk", "Українська"), ("ru", "Русский")];

type Table = HashMap<String, HashMap<String, String>>;

static STRINGS: LazyLock<Table> = LazyLock::new(|| {
    let v: serde_json::Value = serde_json::from_str(include_str!("strings.json")).expect("strings.json");
    let mut t = Table::new();
    for (lang, _) in LANGS {
        let m = v[lang].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default();
        t.insert(lang.to_string(), m);
    }
    t
});

static LANG: Mutex<String> = Mutex::new(String::new());

/// The first of the user's Windows display languages this app has, two letters ("uk" from
/// "uk-UA"). The display languages, not the regional format: the webview UI follows them too.
fn system_language() -> Option<&'static str> {
    use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let (mut count, mut len) = (0u32, 0u32);
    unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len).ok()? };
    let mut buf = vec![0u16; len as usize];
    unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len).ok()? };
    String::from_utf16_lossy(&buf).split('\0').filter(|l| !l.is_empty()).find_map(|l| known(&l.chars().take(2).collect::<String>().to_lowercase()))
}

fn known(id: &str) -> Option<&'static str> {
    LANGS.iter().find(|(l, _)| *l == id).map(|(l, _)| *l)
}

/// Picks the language at start.
pub fn init() {
    let forced = std::env::var("STREAMER_APP_LANG").ok().and_then(|l| known(&l));
    let saved = crate::state_value("lang").and_then(|v| v.as_str().and_then(known));
    let id = forced.or(saved).or_else(system_language).unwrap_or("en");
    set_current(id);
    if std::env::var_os("STREAMER_APP_TEST_GPUI").is_some() {
        let (show, quit) = core::tray_text();
        eprintln!("test: language {id} (forced {forced:?}, saved {saved:?}, system {:?}); tray {show} / {quit}", system_language());
    }
}

fn set_current(id: &str) {
    *LANG.lock().unwrap() = id.to_string();
    // Text drawn natively (the video window's overlay, the tray) follows.
    *core::sampler::UI_LANG.lock().unwrap() = id.to_string();
    crate::tray::relabel();
}

pub fn current() -> String {
    LANG.lock().unwrap().clone()
}

/// The language picker: saves it like the webview does ("lang" in the state file).
pub fn set_language(id: &str) {
    let Some(id) = known(id) else { return };
    set_current(id);
    let mut state = core::state_load();
    state.insert("lang".into(), serde_json::Value::String(id.into()));
    let _ = core::state_save(&serde_json::Value::Object(state).to_string());
}

fn lookup(key: &str) -> String {
    let lang = current();
    STRINGS.get(&lang).and_then(|m| m.get(key)).or_else(|| STRINGS.get("en").and_then(|m| m.get(key))).cloned().unwrap_or_else(|| key.to_string())
}

/// t("room.live")
pub fn t(key: &str) -> SharedString {
    lookup(key).into()
}

/// tf("room.live", &[("n", "2")]): {n} in the string becomes 2.
pub fn tf(key: &str, vars: &[(&str, &dyn std::fmt::Display)]) -> SharedString {
    let mut s = lookup(key);
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), &v.to_string());
    }
    s.into()
}

/// Upper case, as the webview's CSS shows labels ("text-transform: uppercase").
pub fn tu(key: &str) -> SharedString {
    lookup(key).to_uppercase().into()
}

/// A number the way the language writes it: 2.5 or 2,5.
pub fn num(v: f64) -> String {
    let s = if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    if current() == "en" { s } else { s.replace('.', ",") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key is in all three languages, and every key the code asks for exists.
    #[test]
    fn keys_complete() {
        let en: Vec<&String> = STRINGS["en"].keys().collect();
        let mut missing = vec![];
        for (lang, _) in LANGS {
            for k in &en {
                if !STRINGS[lang].contains_key(*k) || STRINGS[lang][*k].trim().is_empty() {
                    missing.push(format!("{lang}:{k}"));
                }
            }
            for k in STRINGS[lang].keys() {
                if !STRINGS["en"].contains_key(k) {
                    missing.push(format!("en lacks {lang}:{k}"));
                }
            }
        }
        // Keys used in the code: t("..."), tf("...", tu("...").
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for f in std::fs::read_dir(dir).unwrap().flatten() {
            // This file only defines them (and its comments show examples).
            if f.file_name() == "i18n.rs" {
                continue;
            }
            let text = std::fs::read_to_string(f.path()).unwrap_or_default();
            for call in ["t(\"", "tf(\"", "tu(\""] {
                for (at, _) in text.match_indices(call) {
                    // A call of its own, not the end of another name ("set_setting(\"").
                    if text[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == ':') {
                        continue;
                    }
                    let key = text[at + call.len()..].split('"').next().unwrap_or_default();
                    if !STRINGS["en"].contains_key(key) {
                        missing.push(format!("{}: {key}", f.file_name().to_string_lossy()));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "missing strings: {missing:?}");
    }

    /// Placeholders match across languages: "{n}" in English is "{n}" in the others.
    #[test]
    fn placeholders_match() {
        let vars = |s: &str| {
            let mut v: Vec<String> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|(a, _)| a.to_string())).collect();
            v.sort();
            v
        };
        let mut bad = vec![];
        for (k, en) in &STRINGS["en"] {
            for (lang, _) in LANGS {
                if let Some(s) = STRINGS[lang].get(k) {
                    if vars(s) != vars(en) {
                        bad.push(format!("{lang}:{k}"));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "placeholders differ: {bad:?}");
    }
}
