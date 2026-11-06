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

    pub fn watches(&self) -> Vec<WatchStats> {
        self.0.lock().unwrap().watches.clone()
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
        let mut i = inner.lock().unwrap();
        let (f0, p0, t0) = i.last.get(&w.streamer).copied().unwrap_or((frames, shown, now));
        let dt = now.duration_since(t0).as_secs_f64().max(0.001);
        i.last.insert(w.streamer, (frames, shown, now));
        seen.push(w.streamer);
        watches.push(WatchStats {
            streamer: w.streamer,
            received_fps: (frames - f0) as f64 / dt,
            shown_fps: (shown.saturating_sub(p0)) as f64 / dt,
            latency_ms: if lat.is_empty() { 0.0 } else { lat.ms(0.5) },
            window_closed: w.viewer.window_closed(),
            quality: w.quality.lock().unwrap().clone(),
            fullscreen: w.viewer.window.map(|x| x.state().fullscreen).unwrap_or(false),
            topmost: w.viewer.window.map(|x| x.state().topmost).unwrap_or(false),
        });
    }
    inner.lock().unwrap().last.retain(|k, _| seen.contains(k));
    if std::env::var_os("STREAMER_APP_DEBUG_STATS").is_some() {
        for w in &watches {
            eprintln!("stats: watch {} received {:.1} shown {:.1} fps, fullscreen {} topmost {}", w.streamer, w.received_fps, w.shown_fps, w.fullscreen, w.topmost);
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
                "stats: live viewers {} upload {:.2} Mbit/s layers {}",
                viewers.len(),
                upload_mbit,
                layers.iter().map(|l| format!("{}={:.2}x{}", l.quality, l.mbit, l.viewers)).collect::<Vec<_>>().join(", ")
            );
        }
        LiveStats {
            ceiling_mbit: h.ceiling.kbit as f64 / 1000.0,
            ceiling: h.ceiling.to_string(),
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
