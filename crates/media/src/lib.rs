//! Media core shared by the host and viewer binaries: GStreamer pipelines, the direct
//! signaling link, clock sync for latency measurement, and platform specifics.

pub mod catchup;
pub mod clock;
pub mod config;
pub mod host;
pub mod ice;
pub mod instance;
pub mod logfile;
pub mod marker;
pub mod netsim;
pub mod platform;
pub mod playback;
pub mod server_client;
pub mod session;
pub mod sounds;
pub mod direct;
pub mod signal;
pub mod stats;
pub mod thumbs;
pub mod viewer;
pub mod voice;

#[cfg(windows)]
mod controls;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
pub mod present;

pub fn init() {
    platform::init_timer();
    use_bundled_plugins();
    gst::init().expect("GStreamer init failed");
}

/// An installed copy carries its own plugins in `gstreamer-1.0` next to the executable
/// (the libraries themselves sit next to it, so Windows finds them first). Point
/// GStreamer at that folder only, so a GStreamer installed elsewhere on the machine is
/// never mixed in, and keep the plugin registry in the user's local app data. Without
/// that folder (a developer build) nothing changes.
fn use_bundled_plugins() {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("gstreamer-1.0"))) else { return };
    if !dir.is_dir() {
        return;
    }
    let registry = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Backseat")
        .join(format!("gst-registry-{}.bin", env!("CARGO_PKG_VERSION")));
    if let Some(parent) = registry.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Runs before GStreamer or any other thread of ours starts.
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &dir);
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH", &dir);
    std::env::remove_var("GST_PLUGIN_PATH_1_0");
    std::env::remove_var("GST_PLUGIN_PATH");
    std::env::set_var("GST_REGISTRY_1_0", &registry);
    std::env::set_var("GST_REGISTRY", &registry);
    // Scan in-process: no separate scanner executable to ship.
    std::env::set_var("GST_REGISTRY_FORK", "no");
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
