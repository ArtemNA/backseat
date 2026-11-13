//! "Hear yourself": the microphone through the same processing as in voice (noise, gain),
//! straight to the speakers, with its level for the settings' meter. No echo cancelling here:
//! the speakers play our own voice, which it would take for echo.

use crate::voice::DspConfig;
use gst::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct MicTest {
    pipeline: gst::Pipeline,
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
        let level = make("level")?;
        level.set_property("interval", 100_000_000u64);
        level.set_property("post-messages", true);
        let conv2 = make("audioconvert")?;
        let res2 = make("audioresample")?;
        let sink = if silent {
            gst::ElementFactory::make("fakesink").property("sync", true).build().map_err(|e| e.to_string())?
        } else {
            let s = make("wasapi2sink")?;
            if s.has_property("low-latency") {
                s.set_property("low-latency", true);
            }
            if let (Some(d), true) = (&speakers, s.has_property("device")) {
                s.set_property("device", d);
            }
            s
        };
        let all = [&src, &conv, &res, &caps, &proc, &level, &conv2, &res2, &sink];
        pipeline.add_many(all).map_err(|e| e.to_string())?;
        gst::Element::link_many(all).map_err(|e| e.to_string())?;
        let store = Arc::new(AtomicU64::new((-100f64).to_bits()));
        let st = store.clone();
        pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
            if let gst::MessageView::Element(e) = msg.view() {
                if let Some(s) = e.structure().filter(|s| s.name() == "level") {
                    if let Ok(rms) = s.get::<gst::glib::ValueArray>("rms") {
                        let db = rms.iter().filter_map(|v| v.get::<f64>().ok()).fold(-100f64, f64::max);
                        st.store(db.max(-100.0).to_bits(), Ordering::Relaxed);
                    }
                }
            }
            gst::BusSyncReply::Drop
        });
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("microphone test: {e}"))?;
        eprintln!("mic test: on");
        Ok(Self { pipeline, level: store })
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
