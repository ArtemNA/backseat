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
        let mut all: Vec<&gst::Element> = vec![&src, &conv, &res, &caps, &proc];
        all.extend(tail.iter());
        pipeline.add_many(all.iter().copied()).map_err(|e| e.to_string())?;
        gst::Element::link_many(all.iter().copied()).map_err(|e| e.to_string())?;
        // The level of every 100 ms after processing, from the samples themselves: the
        // "level" plugin is not among the ones the installer ships (0.2.12 had no meter and
        // the test did not start).
        let store = Arc::new(AtomicU64::new((-100f64).to_bits()));
        let st = store.clone();
        let acc = std::sync::Mutex::new((0f64, 0usize));
        proc.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
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
