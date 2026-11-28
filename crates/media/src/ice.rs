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
    // A punched hole: the agent takes exactly its port (the NAT keeps the hole on it).
    if let Some(p) = ice.pinned {
        let agent = webrtc.property::<gst::glib::Object>("ice-agent");
        agent.set_property("min-rtp-port", p.port as u32);
        agent.set_property("max-rtp-port", p.port as u32);
    }
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

/// After the remote description: the other side's punched address as one more candidate
/// (theirs through the hole), when this connection has one.
pub fn add_pinned(webrtc: &gst::Element, hole: Option<crate::punch::Pinned>) {
    if let Some(p) = hole {
        let cand = format!("candidate:9 1 UDP 1694498815 {} {} typ srflx raddr 0.0.0.0 rport 9", p.peer.ip(), p.peer.port());
        webrtc.emit_by_name::<()>("add-ice-candidate", &[&0u32, &cand]);
    }
}

/// Reports connection progress for one peer. `label` names the other side in messages.
/// Calls `on_failed` once if ICE fails or does not connect in time, telling whether the
/// connection had been up before. A connection that comes up after that (late, while the
/// failure is being handled) is watched again: its next failure is told too. Returns
/// whether it is (or was) up, as it goes.
pub fn watch(webrtc: &gst::Element, label: String, has_turn: bool, on_failed: impl Fn(bool) + Send + Sync + 'static) -> Arc<AtomicBool> {
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
        let (failed, on_failed, label, was_up) = (failed.clone(), on_failed.clone(), label.clone(), connected.clone());
        move |why: &str| {
            if !failed.swap(true, Ordering::SeqCst) {
                eprintln!("ice: no direct connection with {label}: {why}; {hint}");
                on_failed(was_up.load(Ordering::SeqCst));
            }
        }
    };
    let (c, r, l, f) = (connected.clone(), report.clone(), label.clone(), failed.clone());
    webrtc.connect_notify(Some("ice-connection-state"), move |w, _| {
        let state = w.property::<State>("ice-connection-state");
        match state {
            State::Connected | State::Completed => {
                if !c.swap(true, Ordering::SeqCst) {
                    println!("ice: connected with {l}");
                    f.store(false, Ordering::SeqCst);
                }
            }
            State::Failed => r("ICE failed"),
            State::Disconnected => eprintln!("ice: connection with {l} interrupted, waiting for it to recover"),
            _ => {}
        }
    });
    let up = connected.clone();
    // Tests only: STREAMER_DEBUG_ICE_TIMEOUT_SECS: another wait (two sides that notice a
    // failure far apart).
    let wait = std::env::var("STREAMER_DEBUG_ICE_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(CONNECT_TIMEOUT);
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        if !connected.load(Ordering::SeqCst) {
            report(&format!("not connected after {} s", wait.as_secs()));
        }
    });
    up
}
