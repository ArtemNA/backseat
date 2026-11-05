//! Media core shared by the host and viewer binaries: GStreamer pipelines, the direct
//! signaling link, clock sync for latency measurement, and platform specifics.

pub mod clock;
pub mod config;
pub mod host;
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
