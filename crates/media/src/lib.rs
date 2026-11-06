//! Media core shared by the host and viewer binaries: GStreamer pipelines, the direct
//! signaling link, clock sync for latency measurement, and platform specifics.

pub mod catchup;
pub mod clock;
pub mod config;
pub mod host;
pub mod ice;
pub mod marker;
pub mod netsim;
pub mod platform;
pub mod server_client;
pub mod session;
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
    let weak = pipeline.downgrade();
    std::thread::Builder::new()
        .name(format!("{label}-bus"))
        .spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    // Branches added at runtime change the latency. Without recalculating
                    // it, rtpsession cannot map packets to running time, sends no sender
                    // reports, and the viewers' receiver reports carry no round-trip time.
                    gst::MessageView::Latency(_) if std::env::var_os("STREAMER_DEBUG_NO_LATENCY").is_none() => {
                        if let Some(p) = weak.upgrade() {
                            let _ = p.recalculate_latency();
                        }
                    }
                    gst::MessageView::Error(e) => {
                        eprintln!("{label}: error from {:?}: {} ({:?})", e.src().map(|s| s.name()), e.error(), e.debug())
                    }
                    // STREAMER_DEBUG_STATES traces the pipeline's own state changes and the
                    // async (preroll) messages that drive them.
                    gst::MessageView::StateChanged(s)
                        if std::env::var_os("STREAMER_DEBUG_STATES").is_some()
                            && s.src().is_some_and(|o| o.type_().is_a(gst::Pipeline::static_type())) =>
                    {
                        eprintln!("{label}: pipeline {:?} -> {:?} (pending {:?})", s.old(), s.current(), s.pending())
                    }
                    gst::MessageView::AsyncStart(_) | gst::MessageView::AsyncDone(_)
                        if std::env::var_os("STREAMER_DEBUG_STATES").is_some() =>
                    {
                        eprintln!("{label}: {:?} from {:?}", msg.type_(), msg.src().map(|s| s.path_string()))
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
