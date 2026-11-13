//! "Hear yourself": the microphone through the same processing as in voice (noise, gain),
//! straight to the speakers, with its level for the settings' meter. No echo cancelling here:
//! the speakers play our own voice, which it would take for echo.

use crate::voice::DspConfig;
use gst::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct MicTest {
    pipeline: gst::Pipeline,
    /// Our input of the shared sound output, while the test plays there.
    _out: Option<Arc<crate::playback::Input>>,
    /// dBFS of the last 100 ms (after processing), as bits.
    level: Arc<AtomicU64>,
}

fn make(name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(name).build().map_err(|e| format!("{name}: {e}"))
}

impl MicTest {
    /// `mic`, `speakers`: device ids, None for the system's. Tests: `tone` instead of the
    /// microphone, and `silent` plays nothing.
    pub fn start(mic: Option<String>, speakers: Option<String>, dsp: &DspConfig, tone: Option<f64>, silent: bool) -> Result<Self, String> {
        crate::init();
        let pipeline = gst::Pipeline::new();
        let src = match tone {
            Some(f) => gst::ElementFactory::make("audiotestsrc").property_from_str("wave", "sine").property("freq", f).property("volume", 0.3f64).property("is-live", true).build().map_err(|e| e.to_string())?,
            None => {
                let s = make("wasapi2src")?;
                if s.has_property("low-latency") {
                    s.set_property("low-latency", true);
                }
                if let (Some(d), true) = (&mic, s.has_property("device")) {
                    s.set_property("device", d);
                }
                s
            }
        };
        let conv = make("audioconvert")?;
        let res = make("audioresample")?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("rate", 48_000i32).field("channels", 1i32).build())
            .build()
            .map_err(|e| e.to_string())?;
        let proc = make("webrtcdsp")?;
        proc.set_property("echo-cancel", false);
        dsp.apply(&proc, tone.is_some());
        // Played through the app's shared sound output, where voices and streams are heard (a
        // sink of its own went elsewhere with sound routers, and the test was silent).
        let (tail, out) = if silent {
            (vec![gst::ElementFactory::make("fakesink").property("sync", true).build().map_err(|e| e.to_string())?], None)
        } else {
            let _ = &speakers;
            let (t, input) = crate::playback::tail("mic test")?;
            (t, Some(input))
        };
        let after = dsp.after(tone.is_some())?;
        let mut all: Vec<&gst::Element> = vec![&src, &conv, &res, &caps, &proc];
        all.extend(after.iter());
        all.extend(tail.iter());
        pipeline.add_many(all.iter().copied()).map_err(|e| e.to_string())?;
        gst::Element::link_many(all.iter().copied()).map_err(|e| e.to_string())?;
        // The level of every 100 ms after processing, from the samples themselves: the
        // "level" plugin is not among the ones the installer ships (0.2.12 had no meter and
        // the test did not start).
        let store = Arc::new(AtomicU64::new((-100f64).to_bits()));
        let st = store.clone();
        let acc = std::sync::Mutex::new((0f64, 0usize));
        after.last().unwrap_or(&proc).static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let Ok(map) = buf.map_readable() {
                    let mut a = acc.lock().unwrap();
                    for c in map.as_slice().chunks_exact(2) {
                        let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
                        a.0 += v * v;
                        a.1 += 1;
                    }
                    if a.1 >= 4800 {
                        let db = if a.0 > 0.0 { 10.0 * (a.0 / a.1 as f64).log10() } else { -100.0 };
                        st.store(db.max(-100.0).to_bits(), Ordering::Relaxed);
                        *a = (0.0, 0);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        crate::drain_bus(&pipeline, "mic test");
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("microphone test: {e}"))?;
        eprintln!("mic test: on");
        Ok(Self { pipeline, _out: out, level: store })
    }

    /// The processed microphone's level now, dBFS.
    pub fn level_db(&self) -> f64 {
        f64::from_bits(self.level.load(Ordering::Relaxed))
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        eprintln!("mic test: off");
    }
}

/// Listens to the microphone for `secs` (the user stays quiet) and returns the frequencies of
/// a steady whine: narrow peaks well above the noise around them, strongest first, at most 16.
/// Blocks. Tests: `tone` instead of the microphone.
pub fn find_whine(mic: Option<String>, secs: u64, tone: Option<f64>) -> Result<Vec<f32>, String> {
    const BANDS: u32 = 4096;
    const RATE: i32 = 48_000;
    crate::init();
    let pipeline = gst::Pipeline::new();
    let src = match tone {
        Some(f) => gst::ElementFactory::make("audiotestsrc").property_from_str("wave", "sine").property("freq", f).property("volume", 0.01f64).property("is-live", true).build().map_err(|e| e.to_string())?,
        None => {
            let s = make("wasapi2src")?;
            if let (Some(d), true) = (&mic, s.has_property("device")) {
                s.set_property("device", d);
            }
            s
        }
    };
    let caps = gst::ElementFactory::make("capsfilter").property("caps", gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", RATE).field("channels", 1i32).build()).build().unwrap();
    let spec = gst::ElementFactory::make("spectrum").property("bands", BANDS).property("interval", 500_000_000u64).property("threshold", -120i32).property("post-messages", true).build().map_err(|e| format!("spectrum: {e}"))?;
    let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
    let all = [&src, &make("audioconvert")?, &make("audioresample")?, &caps, &spec, &sink];
    pipeline.add_many(all).map_err(|e| e.to_string())?;
    gst::Element::link_many(all).map_err(|e| e.to_string())?;
    let sum = Arc::new(std::sync::Mutex::new((vec![0f64; BANDS as usize], 0u32)));
    let s2 = sum.clone();
    pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
        if let gst::MessageView::Element(e) = msg.view() {
            if let Some(list) = e.structure().filter(|s| s.name() == "spectrum").and_then(|s| s.get::<gst::List>("magnitude").ok()) {
                let mut s = s2.lock().unwrap();
                for (i, v) in list.iter().enumerate().take(BANDS as usize) {
                    if let Ok(db) = v.get::<f32>() {
                        s.0[i] += 10f64.powf(db as f64 / 10.0);
                    }
                }
                s.1 += 1;
            }
        }
        gst::BusSyncReply::Drop
    });
    pipeline.set_state(gst::State::Playing).map_err(|e| format!("microphone: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(secs * 1000 + 300));
    let _ = pipeline.set_state(gst::State::Null);
    let (pow, n) = sum.lock().unwrap().clone();
    if n == 0 {
        return Err("no sound from the microphone".into());
    }
    let hz = RATE as f64 / 2.0 / BANDS as f64;
    let db: Vec<f64> = pow.iter().map(|p| 10.0 * (p / n as f64).max(1e-14).log10()).collect();
    let floor = |i: usize| {
        let (lo, hi) = (i.saturating_sub(40), (i + 40).min(db.len() - 1));
        let mut w = db[lo..=hi].to_vec();
        w.sort_by(|a, b| a.partial_cmp(b).unwrap());
        w[w.len() / 2]
    };
    let mut peaks: Vec<(f64, f64)> = (2..db.len() - 2)
        .filter(|&i| db[i] >= db[i - 1] && db[i] >= db[i + 1] && db[i] >= db[i - 2] && db[i] >= db[i + 2])
        .map(|i| (i as f64 * hz, db[i] - floor(i)))
        .filter(|&(f, over)| over > 9.0 && (40.0..20_000.0).contains(&f))
        .collect();
    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    // A peak's own side lobes (and a tone drifting a little) are the same tone: the strongest
    // within 60 Hz stands for them.
    let mut kept: Vec<(f64, f64)> = Vec::new();
    for p in peaks {
        if !kept.iter().any(|k| (k.0 - p.0).abs() < 60.0) {
            kept.push(p);
        }
    }
    let mut out: Vec<f32> = kept.into_iter().take(16).map(|(f, _)| f.round() as f32).collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!("whine: {} peaks at {:?} Hz", out.len(), out);
    Ok(out)
}
