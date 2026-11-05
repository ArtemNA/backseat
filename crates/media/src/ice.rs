//! ICE setup and connection monitoring shared by host and viewer.

use crate::config::IceConfig;
use gst::prelude::*;
use gst_webrtc::WebRTCICEConnectionState as State;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long a peer may stay unconnected before we call the attempt failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub fn configure(webrtc: &gst::Element, ice: &IceConfig) {
    if let Some(stun) = &ice.stun {
        webrtc.set_property("stun-server", stun);
    }
    if let Some(turn) = &ice.turn {
        let ok = webrtc.emit_by_name::<bool>("add-turn-server", &[turn]);
        if !ok {
            eprintln!("ice: the TURN server address was rejected (check its format: turn://user:pass@host:port)");
        }
    }
}

/// Reports connection progress for one peer. `label` names the other side in messages.
/// Calls `on_failed` once if ICE fails or does not connect in time.
pub fn watch(webrtc: &gst::Element, label: String, has_turn: bool, on_failed: impl Fn() + Send + Sync + 'static) {
    let connected = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let on_failed = Arc::new(on_failed);
    let hint = if has_turn {
        "the TURN server did not help either; check its address and credentials"
    } else {
        "both networks probably block direct connections (run natcheck on both sides); \
         another network on one side, or your own TURN server (--turn), can help"
    };
    let report = {
        let (failed, on_failed, label) = (failed.clone(), on_failed.clone(), label.clone());
        move |why: &str| {
            if !failed.swap(true, Ordering::SeqCst) {
                eprintln!("ice: no direct connection with {label}: {why}; {hint}");
                on_failed();
            }
        }
    };
    let (c, r, l) = (connected.clone(), report.clone(), label.clone());
    webrtc.connect_notify(Some("ice-connection-state"), move |w, _| {
        let state = w.property::<State>("ice-connection-state");
        match state {
            State::Connected | State::Completed => {
                if !c.swap(true, Ordering::SeqCst) {
                    println!("ice: connected with {l}");
                }
            }
            State::Failed => r("ICE failed"),
            State::Disconnected => eprintln!("ice: connection with {l} interrupted, waiting for it to recover"),
            _ => {}
        }
    });
    std::thread::spawn(move || {
        std::thread::sleep(CONNECT_TIMEOUT);
        if !connected.load(Ordering::SeqCst) {
            report(&format!("not connected after {} s", CONNECT_TIMEOUT.as_secs()));
        }
    });
}
