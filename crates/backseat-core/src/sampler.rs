//! Once a second: frame rates, latency and upload for the UI. Counters are read here only,
//! so the UI never takes a series that another reader also drains.

use media::marker::Marker;
use media::session::Session;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize)]
pub struct WatchStats {
    pub streamer: u64,
    /// Frames decoded per second.
    pub received_fps: f64,
    /// Frames presented per second; below `received_fps` means the window is throttled.
    pub shown_fps: f64,
    pub latency_ms: f64,
    pub window_closed: bool,
    /// What we asked for ("720p30 2000"), None for the streamer's own quality.
    pub quality: Option<String>,
    pub fullscreen: bool,
    pub topmost: bool,
    /// Time between decoded frames in the last second, ms.
    pub gap_p99_ms: f64,
    pub gap_max_ms: f64,
    /// Video received, Mbit/s.
    pub mbit: f64,
    /// Video packets lost in the last second, percent.
    pub loss_pct: f64,
    /// Round trip of the clock ping (through signaling), ms; 0 before the first.
    pub ping_ms: f64,
    pub overlay: bool,
    /// The small badge in the video's corner (F3); the overlay is the detailed stats.
    pub badge: bool,
    /// Shown inside the main window (else popped out into its own).
    pub embedded: bool,
    pub muted: bool,
    /// "AV1 · GPU", "AV1 · CPU (dav1d)", "H265 · GPU"; empty before video arrives.
    pub codec: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ViewerRow {
    pub id: u64,
    pub name: String,
    pub quality: String,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub catchups: u64,
    /// Short codes the UI translates: "loss:4.7", "queue:1748".
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LayerRow {
    pub quality: String,
    pub viewers: usize,
    pub fps: f64,
    pub mbit: f64,
    pub parked: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LiveStats {
    pub ceiling: String,
    pub encoder: String,
    pub viewers: Vec<ViewerRow>,
    pub layers: Vec<LayerRow>,
    /// Measured: what is actually sent to all viewers, video and audio, per second.
    pub upload_mbit: f64,
    /// The bitrate of the host's own layer: what one more viewer at full quality adds.
    pub ceiling_mbit: f64,
    pub since_secs: u64,
}

#[derive(Default)]
struct Inner {
    session: Option<Weak<Session>>,
    watches: Vec<WatchStats>,
    live: Option<LiveStats>,
    live_since: Option<Instant>,
    /// Per watch: frames decoded and presented at the last sample.
    last: HashMap<u64, (u64, u64, Instant)>,
    /// Per watch: video bytes, packets received and lost at the last sample.
    net: HashMap<u64, (i64, i64, i64)>,
    /// Per viewer of our stream: bytes sent at the last sample.
    sent: HashMap<u64, (u64, Instant)>,
    marker: Marker,
}

#[derive(Default)]
pub struct Sampler(Arc<Mutex<Inner>>);

impl Sampler {
    pub fn attach(&self, s: Arc<Session>) {
        let mut i = self.0.lock().unwrap();
        *i = Inner { session: Some(Arc::downgrade(&s)), ..Default::default() };
    }

    /// The last sample, with the window's own state (stats, badge, fullscreen, popped out,
    /// sound) read now: a button reflects a click at once, not up to a second later.
    pub fn watches(&self) -> Vec<WatchStats> {
        let (mut watches, session) = {
            let i = self.0.lock().unwrap();
            (i.watches.clone(), i.session.as_ref().and_then(|w| w.upgrade()))
        };
        if let Some(s) = session {
            for w in s.watching() {
                let Some(x) = watches.iter_mut().find(|x| x.streamer == w.streamer) else { continue };
                if let Some(win) = w.viewer.window {
                    let st = win.state();
                    x.overlay = st.overlay;
                    x.badge = st.badge;
                    x.fullscreen = st.fullscreen;
                    x.topmost = st.topmost;
                    x.embedded = st.embedded;
                }
                x.muted = w.viewer.muted();
            }
        }
        watches
    }

    pub fn live(&self) -> Option<LiveStats> {
        self.0.lock().unwrap().live.clone()
    }

    pub fn start(&self) {
        let inner = self.0.clone();
        std::thread::Builder::new()
            .name("sampler".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                sample(&inner);
            })
            .expect("sampler thread");
    }
}

/// The video window's diagnostic overlay, in English like other technical readouts.
fn overlay_text(w: &WatchStats) -> String {
    let quality = w.quality.as_deref().unwrap_or("streamer's quality");
    let codec = if w.codec.is_empty() { String::new() } else { format!(" · {}", w.codec) };
    format!(
        "{quality}{codec}\n{:.0} fps shown / {:.0} received\n{:.1} Mbit/s   loss {:.1}%\nlatency {:.0} ms   ping {:.0} ms\nframe gap p99 {:.1} ms   Shift+F3 hides",
        w.shown_fps, w.received_fps, w.mbit, w.loss_pct, w.latency_ms, w.ping_ms, w.gap_p99_ms
    )
}

/// The badge's line, "1080p60 · 12.4 Mbit/s · AV1", and whether all is well (the dot).
/// Modes are 30 or 60 fps: within a few frames of one (58 of 60 is normal) the mode is named;
/// further off, the frames shown are ("1080p · 48 fps") and the dot turns amber, as it does
/// at 2% packets lost or more. Empty before the first frame.
fn badge_line(size: Option<(u32, u32)>, w: &WatchStats, codec: &str) -> (String, bool) {
    let Some((_, h)) = size else { return (String::new(), true) };
    if w.received_fps <= 0.0 {
        return (String::new(), true);
    }
    let sep = media::present::badge_separator();
    let fps = w.shown_fps;
    let mode = if (fps - 60.0).abs() <= 4.0 {
        Some(60)
    } else if (fps - 30.0).abs() <= 3.0 {
        Some(30)
    } else {
        None
    };
    let lang = UI_LANG.lock().unwrap().clone();
    let unit = match lang.as_str() {
        "uk" => "Мбіт/с",
        "ru" => "Мбит/с",
        _ => "Mbit/s",
    };
    let quality = match mode {
        Some(m) => format!("{h}p{m}"),
        None => format!("{h}p{sep}{fps:.0} fps"),
    };
    let mut text = format!("{quality}{sep}{:.1} {unit}", w.mbit);
    if !codec.is_empty() {
        text.push_str(sep);
        text.push_str(codec);
    }
    (text, mode.is_some() && w.loss_pct < 2.0)
}

/// The UI language ("en", "uk", "ru"), set by the webview; for text drawn natively.
pub static UI_LANG: Mutex<String> = Mutex::new(String::new());

/// The video window's line before the first frame.
/// What went wrong before the first frame.
#[derive(Clone, Copy)]
enum Pending {
    Connecting,
    /// The streamer never sent its offer.
    NoAnswer,
    /// Offer and answer went through, the direct connection did not.
    Network,
}

fn window_status(name: &str, p: Pending) -> String {
    let lang = UI_LANG.lock().unwrap().clone();
    match (lang.as_str(), p) {
        ("uk", Pending::Connecting) => format!("З'єднання з {name}…"),
        ("uk", Pending::NoAnswer) => format!("{name} не відповів на запит. Спробуйте ще раз; якщо не допоможе, хай {name} перезапустить стрім."),
        ("uk", Pending::Network) => format!("Немає прямого з'єднання з {name}. Мережа однієї зі сторін його блокує; допомогти може сервер TURN."),
        ("ru", Pending::Connecting) => format!("Соединение с {name}…"),
        ("ru", Pending::NoAnswer) => format!("{name} не ответил на запрос. Попробуйте ещё раз; если не поможет, пусть {name} перезапустит стрим."),
        ("ru", Pending::Network) => format!("Нет прямого соединения с {name}. Сеть одной из сторон его блокирует; помочь может сервер TURN."),
        (_, Pending::Connecting) => format!("Connecting to {name}…"),
        (_, Pending::NoAnswer) => format!("{name} did not answer. Try again; if that does not help, {name} should restart the stream."),
        (_, Pending::Network) => format!("No direct connection to {name}. A network on one side blocks it; a TURN server can help."),
    }
}

fn sample(inner: &Mutex<Inner>) {
    let session = inner.lock().unwrap().session.as_ref().and_then(|w| w.upgrade());
    let Some(s) = session else { return };
    let now = Instant::now();

    let mut watches = Vec::new();
    let mut seen = Vec::new();
    for w in s.watching() {
        let frames = w.viewer.stats.frames.load(Ordering::Relaxed);
        let shown = w.viewer.render_report().map(|r| r.presented).unwrap_or(0);
        let lat = w.viewer.stats.video_latency.take();
        let gaps = w.viewer.stats.frame_interval.take();
        let net = w.viewer.inbound().map(|n| (n.bytes, n.received, n.lost));
        let mut i = inner.lock().unwrap();
        let (f0, p0, t0) = i.last.get(&w.streamer).copied().unwrap_or((frames, shown, now));
        let dt = now.duration_since(t0).as_secs_f64().max(0.001);
        i.last.insert(w.streamer, (frames, shown, now));
        let (mbit, loss_pct) = match (net, i.net.get(&w.streamer).copied()) {
            (Some((b, r, l)), Some((b0, r0, l0))) => {
                let (dr, dl) = ((r - r0).max(0) as f64, (l - l0).max(0) as f64);
                ((b - b0).max(0) as f64 * 8.0 / dt / 1e6, if dr + dl > 0.0 { dl * 100.0 / (dr + dl) } else { 0.0 })
            }
            _ => (0.0, 0.0),
        };
        if let Some(n) = net {
            i.net.insert(w.streamer, n);
        }
        seen.push(w.streamer);
        let st = w.viewer.window.map(|x| x.state()).unwrap_or_default();
        watches.push(WatchStats {
            mbit,
            loss_pct,
            ping_ms: w.clock.last_rtt_ns().map(|n| n as f64 / 1e6).unwrap_or(0.0),
            overlay: st.overlay,
            badge: st.badge,
            embedded: st.embedded,
            muted: w.viewer.muted(),
            codec: w
                .viewer
                .video_codec
                .lock()
                .unwrap()
                .as_ref()
                .map(|(c, dec)| if dec == "dav1ddec" { format!("{c} · CPU (dav1d)") } else { format!("{c} · GPU") })
                .unwrap_or_default(),
            streamer: w.streamer,
            received_fps: (frames - f0) as f64 / dt,
            shown_fps: (shown.saturating_sub(p0)) as f64 / dt,
            latency_ms: if lat.is_empty() { 0.0 } else { lat.ms(0.5) },
            window_closed: w.viewer.window_closed(),
            quality: w.quality.lock().unwrap().clone(),
            fullscreen: w.viewer.window.map(|x| x.state().fullscreen).unwrap_or(false),
            topmost: w.viewer.window.map(|x| x.state().topmost).unwrap_or(false),
            gap_p99_ms: if gaps.is_empty() { 0.0 } else { gaps.ms(0.99) },
            gap_max_ms: if gaps.is_empty() { 0.0 } else { gaps.ms(1.0) },
        });
    }
    {
        let mut i = inner.lock().unwrap();
        i.last.retain(|k, _| seen.contains(k));
        i.net.retain(|k, _| seen.contains(k));
    }
    let names: HashMap<u64, String> = s.peers().into_iter().map(|p| (p.id, p.name)).collect();
    for w in s.watching() {
        if let Some(win) = w.viewer.window {
            if w.viewer.stats.frames.load(Ordering::Relaxed) == 0 {
                let name = names.get(&w.streamer).cloned().unwrap_or_default();
                let p = match (w.viewer.ice_failed.load(Ordering::Relaxed), w.viewer.offered.load(Ordering::Relaxed)) {
                    (false, _) => Pending::Connecting,
                    (true, false) => Pending::NoAnswer,
                    (true, true) => Pending::Network,
                };
                win.set_status(&window_status(&name, p));
            } else {
                win.set_status("");
            }
        }
        let Some(st) = watches.iter().find(|x| x.streamer == w.streamer) else { continue };
        if let (Some(win), true) = (w.viewer.window, st.overlay) {
            win.set_overlay_text(&overlay_text(st));
        }
        if let (Some(win), true) = (w.viewer.window, st.badge) {
            let codec = w.viewer.video_codec.lock().unwrap().as_ref().map(|(c, _)| c.clone()).unwrap_or_default();
            let size = *w.viewer.stats.video_size.lock().unwrap();
            let (text, ok) = badge_line(size, st, &codec);
            win.set_badge_info(&text, ok);
        }
    }
    if std::env::var_os("STREAMER_APP_DEBUG_STATS").is_some() {
        for w in &watches {
            eprintln!(
                "stats: watch {} received {:.1} shown {:.1} fps, gap p99 {:.1} max {:.1} ms, fullscreen {} topmost {}",
                w.streamer, w.received_fps, w.shown_fps, w.gap_p99_ms, w.gap_max_ms, w.fullscreen, w.topmost
            );
        }
    }

    let live = s.host().map(|h| {
        let names: HashMap<u64, String> = s.peers().into_iter().map(|p| (p.id, p.name)).collect();
        let dt = 1.0;
        let layers: Vec<LayerRow> = h
            .layers()
            .into_iter()
            .map(|l| {
                let st = &l.stats;
                LayerRow {
                    quality: l.quality.to_string(),
                    viewers: l.viewers,
                    fps: st.frames.swap(0, Ordering::Relaxed) as f64 / dt,
                    mbit: st.bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / dt / 1e6,
                    parked: l.parked,
                }
            })
            .collect();
        let rate: HashMap<String, f64> = layers.iter().map(|l| (l.quality.clone(), l.mbit)).collect();
        let layer_of: HashMap<u64, String> = h.viewer_layers().into_iter().map(|(id, q)| (id, q.to_string())).collect();
        let mut i = inner.lock().unwrap();
        let viewers: Vec<ViewerRow> = h
            .viewer_quality()
            .iter()
            .map(|q| ViewerRow {
                id: q.id,
                name: names.get(&q.id).cloned().unwrap_or_else(|| format!("#{}", q.id)),
                quality: layer_of.get(&q.id).cloned().unwrap_or_default(),
                rtt_ms: q.rtt_ms,
                loss_pct: q.fraction_lost * 100.0,
                catchups: q.catchups,
                problems: i
                    .marker
                    .assess(q)
                    .iter()
                    .map(|p| match p {
                        media::marker::Problem::Loss { pct } => format!("loss:{pct:.1}"),
                        media::marker::Problem::Queue { ms } => format!("queue:{ms:.0}"),
                    })
                    .collect(),
            })
            .collect();
        // STREAMER_DEBUG_UPLOAD_PER_LAYER counts each layer once however many watch it: the
        // spoiled variant for the upload gate.
        let upload_mbit = if std::env::var_os("STREAMER_DEBUG_UPLOAD_PER_LAYER").is_some() {
            rate.values().sum()
        } else {
            let qs = h.viewer_quality();
            let mut total = 0.0;
            for q in &qs {
                let (b0, t0) = i.sent.get(&q.id).copied().unwrap_or((q.bytes_sent, now));
                let dt = now.duration_since(t0).as_secs_f64();
                if dt > 0.0 && q.bytes_sent >= b0 {
                    total += (q.bytes_sent - b0) as f64 * 8.0 / dt / 1e6;
                }
                i.sent.insert(q.id, (q.bytes_sent, now));
            }
            i.sent.retain(|k, _| qs.iter().any(|q| q.id == *k));
            total
        };
        let since = *i.live_since.get_or_insert(now);
        h.housekeeping();
        if std::env::var_os("STREAMER_APP_DEBUG_STATS").is_some() {
            eprintln!(
                "stats: live encoder {} viewers {} upload {:.2} Mbit/s layers {}",
                h.encoder,
                viewers.len(),
                upload_mbit,
                layers.iter().map(|l| format!("{}={:.2}x{}", l.quality, l.mbit, l.viewers)).collect::<Vec<_>>().join(", ")
            );
        }
        LiveStats {
            ceiling_mbit: h.ceiling().kbit as f64 / 1000.0,
            ceiling: h.ceiling().to_string(),
            encoder: h.encoder.clone(),
            viewers,
            layers,
            upload_mbit,
            since_secs: now.duration_since(since).as_secs(),
        }
    });
    let mut i = inner.lock().unwrap();
    if live.is_none() {
        i.live_since = None;
    }
    i.watches = watches;
    i.live = live;
}
