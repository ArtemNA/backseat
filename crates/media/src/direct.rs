//! Direct connection without a signaling server (beta). Two people swap one text each (an
//! invite, then an answer) through any messenger. The texts set up a control connection:
//! WebRTC with only a data channel. It then carries what the server would, for these two
//! only (see `Session::direct`): who streams, requests to watch, the opening of each stream
//! and everything after it. Media goes in its own connections, as in a room.

use crate::config::IceConfig;
use base64::Engine;
use gst::prelude::*;
use gst_webrtc::{WebRTCDataChannel, WebRTCDataChannelState, WebRTCSDPType, WebRTCSessionDescription};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Marks a text as ours, and its format.
const PREFIX: &str = "BACKSEAT1.";

type Handler = Arc<dyn Fn(String) + Send + Sync>;

/// The control connection to the other person.
pub struct Control {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// What goes into our text: the SDP and our ICE candidates, as they come.
    opening: Arc<Mutex<Vec<(String, String)>>>,
    last: Arc<Mutex<Option<Instant>>>,
    channel: Arc<Mutex<Option<WebRTCDataChannel>>>,
    /// Sent before the channel opened; sent when it does.
    pending: Arc<Mutex<Vec<String>>>,
    on_message: Arc<Mutex<Option<Handler>>>,
    on_close: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    open: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl Control {
    pub fn new(ice: &IceConfig) -> Result<Arc<Self>, String> {
        let pipeline = gst::Pipeline::with_name("direct-control");
        let webrtc = gst::ElementFactory::make("webrtcbin").build().map_err(|e| format!("webrtcbin: {e}"))?;
        webrtc.set_property_from_str("bundle-policy", "max-bundle");
        crate::ice::configure(&webrtc, ice);
        pipeline.add(&webrtc).map_err(|e| e.to_string())?;
        let c = Arc::new(Self {
            pipeline,
            webrtc: webrtc.clone(),
            opening: Arc::default(),
            last: Arc::default(),
            channel: Arc::default(),
            pending: Arc::default(),
            on_message: Arc::default(),
            on_close: Arc::default(),
            open: Arc::default(),
            closed: Arc::default(),
        });
        let (o, l) = (c.opening.clone(), c.last.clone());
        webrtc.connect("on-ice-candidate", false, move |v| {
            let mline = v[1].get::<u32>().unwrap_or(0);
            let cand = v[2].get::<String>().unwrap_or_default();
            o.lock().unwrap().push(("ice".into(), format!("{mline} {cand}")));
            *l.lock().unwrap() = Some(Instant::now());
            None
        });
        let me = Arc::downgrade(&c);
        webrtc.connect("on-data-channel", false, move |v| {
            if let (Some(me), Ok(ch)) = (me.upgrade(), v[1].get::<WebRTCDataChannel>()) {
                me.attach(ch);
            }
            None
        });
        // Losing the control connection after it worked ends the direct session.
        let me = Arc::downgrade(&c);
        webrtc.connect_notify(Some("ice-connection-state"), move |w, _| {
            use gst_webrtc::WebRTCICEConnectionState as S;
            if w.property::<S>("ice-connection-state") == S::Failed {
                if let Some(me) = me.upgrade().filter(|m| m.open.load(Ordering::SeqCst)) {
                    me.close();
                }
            }
        });
        crate::drain_bus(&c.pipeline, "direct");
        c.pipeline.set_state(gst::State::Playing).map_err(|e| format!("control connection: {e}"))?;
        Ok(c)
    }

    fn attach(&self, ch: WebRTCDataChannel) {
        let h = self.on_message.clone();
        ch.connect("on-message-string", false, move |v| {
            if let Ok(Some(text)) = v[1].get::<Option<String>>() {
                let f = h.lock().unwrap().clone();
                if let Some(f) = f {
                    f(text);
                }
            }
            None
        });
        let (pending, open) = (self.pending.clone(), self.open.clone());
        ch.connect("on-open", false, move |v| {
            open.store(true, Ordering::SeqCst);
            if let Ok(ch) = v[0].get::<WebRTCDataChannel>() {
                for t in pending.lock().unwrap().drain(..) {
                    ch.emit_by_name::<()>("send-string", &[&Some(t.as_str())]);
                }
            }
            None
        });
        let (closed, on_close) = (self.closed.clone(), self.on_close.clone());
        ch.connect("on-close", false, move |_| {
            if !closed.swap(true, Ordering::SeqCst) {
                let f = on_close.lock().unwrap().clone();
                if let Some(f) = f {
                    f();
                }
            }
            None
        });
        // The channel may already be open by the time this runs (the other side's).
        if ch.ready_state() == WebRTCDataChannelState::Open {
            self.open.store(true, Ordering::SeqCst);
        }
        *self.channel.lock().unwrap() = Some(ch);
    }

    /// Whether the other person is connected.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst) && !self.closed.load(Ordering::SeqCst)
    }

    pub fn on_message(&self, f: impl Fn(String) + Send + Sync + 'static) {
        *self.on_message.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn on_close(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_close.lock().unwrap() = Some(Arc::new(f));
    }

    /// Sends a text to the other person; kept until the channel opens.
    pub fn send(&self, text: &str) {
        let ch = self.channel.lock().unwrap().clone();
        match ch.filter(|c| c.ready_state() == WebRTCDataChannelState::Open) {
            Some(c) => c.emit_by_name::<()>("send-string", &[&Some(text)]),
            None => {
                let mut p = self.pending.lock().unwrap();
                p.push(text.to_string());
                let extra = p.len().saturating_sub(256);
                p.drain(..extra);
            }
        }
    }

    /// Ends the connection; `on_close` runs once.
    pub fn close(&self) {
        if let Some(ch) = self.channel.lock().unwrap().as_ref() {
            ch.close();
        }
        if !self.closed.swap(true, Ordering::SeqCst) {
            let f = self.on_close.lock().unwrap().clone();
            if let Some(f) = f {
                f();
            }
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Our text, once the SDP is set and ICE gathering went quiet (or `wait` passed).
    fn text(&self, name: &str, wait: Duration) -> Result<String, String> {
        let until = Instant::now() + wait;
        loop {
            let has_sdp = self.opening.lock().unwrap().iter().any(|(k, _)| k == "offer" || k == "answer");
            let quiet = self.last.lock().unwrap().is_some_and(|t| t.elapsed() > Duration::from_millis(1200));
            if has_sdp && (quiet || Instant::now() > until) {
                break;
            }
            if Instant::now() > until + Duration::from_secs(5) {
                return Err("the connection text could not be made".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut msgs = vec![("name".to_string(), name.to_string())];
        let skip = crate::platform::virtual_adapter_addresses();
        msgs.extend(self.opening.lock().unwrap().iter().filter(|(k, v)| k != "ice" || useful(v, &skip)).cloned());
        Ok(pack(&msgs))
    }

    fn describe(&self, kind: WebRTCSDPType, sdp: &str) -> Result<(), String> {
        let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).map_err(|_| "the text is damaged (SDP)")?;
        let d = WebRTCSessionDescription::new(kind, sdp);
        self.webrtc.emit_by_name::<()>("set-remote-description", &[&d, &None::<gst::Promise>]);
        Ok(())
    }

    /// Makes our local description with `create` ("create-offer" or "create-answer").
    fn make(&self, create: &str, kind: &'static str) {
        let (w, o) = (self.webrtc.clone(), self.opening.clone());
        let promise = gst::Promise::with_change_func(move |reply| {
            let Ok(Some(reply)) = reply else { return };
            let Ok(desc) = reply.get::<WebRTCSessionDescription>(kind) else { return };
            w.emit_by_name::<()>("set-local-description", &[&desc, &None::<gst::Promise>]);
            o.lock().unwrap().insert(0, (kind.to_string(), desc.sdp().as_text().unwrap_or_default()));
        });
        self.webrtc.emit_by_name::<()>(create, &[&None::<gst::Structure>, &promise]);
    }

    fn apply_ice(&self, msgs: &[(String, String)]) {
        for (_, c) in msgs.iter().filter(|(k, _)| k == "ice") {
            crate::host::add_ice(&self.webrtc, c);
        }
    }

    /// The inviting side: our invite text.
    pub fn invite(&self, name: &str, wait: Duration) -> Result<String, String> {
        let ch = self
            .webrtc
            .emit_by_name::<Option<WebRTCDataChannel>>("create-data-channel", &[&"backseat", &None::<gst::Structure>])
            .ok_or("no data channel")?;
        self.attach(ch);
        self.make("create-offer", "offer");
        self.text(name, wait)
    }

    /// The invited side: takes the invite, returns our answer text and their name.
    pub fn answer(&self, invite: &str, name: &str, wait: Duration) -> Result<(String, String), String> {
        let msgs = unpack(invite)?;
        let offer = msgs.iter().find(|(k, _)| k == "offer").ok_or("this is not an invite")?;
        self.describe(WebRTCSDPType::Offer, &offer.1)?;
        self.apply_ice(&msgs);
        self.make("create-answer", "answer");
        Ok((self.text(name, wait)?, name_of(&msgs)))
    }

    /// The inviting side: takes the answer; returns their name. Connects in a moment.
    pub fn accept(&self, answer: &str) -> Result<String, String> {
        let msgs = unpack(answer)?;
        let a = msgs.iter().find(|(k, _)| k == "answer").ok_or("this is not an answer")?;
        self.describe(WebRTCSDPType::Answer, &a.1)?;
        self.apply_ice(&msgs);
        Ok(name_of(&msgs))
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Whether an ICE candidate ("MLINE candidate:F C PROTO PRIO ADDR PORT typ TYPE ...") is
/// worth a place in the text. Left out: IPv6 link-local addresses (one cable or Wi-Fi only;
/// the home IPv4 address covers that case) and addresses of virtual machine adapters
/// (`skip`), which the other person cannot reach. Everything else stays, TCP included.
fn useful(candidate: &str, skip: &[std::net::IpAddr]) -> bool {
    let Some(addr) = candidate.split_whitespace().nth(5) else { return true };
    let Ok(ip) = addr.parse::<std::net::IpAddr>() else { return true };
    match ip {
        std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => false,
        _ => !skip.contains(&ip),
    }
}

fn name_of(msgs: &[(String, String)]) -> String {
    msgs.iter().find(|(k, _)| k == "name").map(|(_, v)| v.chars().take(protocol::MAX_NAME_LEN).collect()).unwrap_or_else(|| "?".into())
}

/// Messages as text: JSON, deflated, base64url, with a prefix.
pub fn pack(msgs: &[(String, String)]) -> String {
    let json = serde_json::to_vec(msgs).expect("serializable");
    let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&json).expect("in memory");
    let bytes = z.finish().expect("in memory");
    format!("{PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The messages in a text from `pack`. Whitespace (a messenger's line breaks) is ignored.
pub fn unpack(text: &str) -> Result<Vec<(String, String)>, String> {
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let body = clean.strip_prefix(PREFIX).ok_or("not a Backseat connection text")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).map_err(|_| "the text is damaged (not base64)")?;
    let mut json = Vec::new();
    flate2::read::DeflateDecoder::new(&bytes[..]).take(1 << 20).read_to_end(&mut json).map_err(|_| "the text is damaged")?;
    serde_json::from_slice(&json).map_err(|_| "the text is damaged".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_line_breaks() {
        let m = vec![("offer".to_string(), "v=0\r\n".to_string()), ("ice".into(), "0 candidate:1 1 UDP 1 1.2.3.4 5 typ host".into())];
        let t = pack(&m);
        let broken = format!("{}\n{}", &t[..20], &t[20..]);
        assert_eq!(unpack(&broken).unwrap(), m);
        assert!(unpack("hello").is_err());
    }

    #[test]
    fn useful_candidates() {
        let vm: Vec<std::net::IpAddr> = vec!["172.30.1.1".parse().unwrap()];
        assert!(useful("0 candidate:1 1 UDP 2015363327 192.168.1.5 57451 typ host", &vm));
        assert!(useful("0 candidate:13 1 UDP 1679819007 203.0.113.7 57451 typ srflx raddr 192.168.1.5 rport 57451", &vm));
        assert!(useful("0 candidate:2 1 TCP 1015021823 192.168.1.5 9 typ host tcptype active", &vm));
        assert!(useful("0 candidate:4 1 UDP 2015363583 2001:db8::5 57452 typ host", &vm));
        assert!(!useful("0 candidate:4 1 UDP 2015363583 fe80::1234:5678 57452 typ host", &vm));
        assert!(!useful("0 candidate:10 1 UDP 2015364607 172.30.1.1 57454 typ host", &vm));
    }
}
