//! Media core shared by the host and viewer binaries: GStreamer pipelines, the direct
//! signaling link, clock sync for latency measurement, and platform specifics.

pub mod clock;
pub mod config;
pub mod host;
pub mod ice;
pub mod netsim;
pub mod platform;
pub mod server_client;
pub mod signal;
pub mod stats;
pub mod viewer;

#[cfg(windows)]
mod present;

pub fn init() {
    platform::init_timer();
    gst::init().expect("GStreamer init failed");
}

/// Drains a pipeline's bus on its own thread, printing errors and warnings. Undrained
/// messages hold references to their source elements, so removed elements were never
/// freed (stage 3: ~0.6 MB and 2 handles per viewer that left).
pub(crate) fn drain_bus(pipeline: &gst::Pipeline, label: &'static str) {
    use gst::prelude::*;
    let bus = pipeline.bus().expect("pipeline has a bus");
    std::thread::Builder::new()
        .name(format!("{label}-bus"))
        .spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        eprintln!("{label}: error from {:?}: {} ({:?})", e.src().map(|s| s.name()), e.error(), e.debug())
                    }
                    gst::MessageView::Warning(w) => {
                        eprintln!("{label}: warning from {:?}: {}", w.src().map(|s| s.name()), w.error())
                    }
                    _ => {}
                }
            }
        })
        .expect("bus thread");
}
