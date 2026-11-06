//! One connection to the signaling server in one room: who is there, our own stream, and the
//! streams we watch. Shared by the desktop app and the command-line host and viewer, so the
//! gates test the same code the app runs.
//!
//! Two people may watch each other, so the same message kinds (ice, ping) flow both ways
//! between two peers. Kinds carry the sender's role: "v/" from a viewer to the streamer it
//! watches, "s/" from a streamer to its viewer.

use crate::clock::HostClock;
use crate::config::{HostConfig, Quality, ViewerConfig};
use crate::host::Host;
use crate::server_client::ServerClient;
use crate::signal::Outbox;
use crate::viewer::Viewer;
use protocol::{ClientMsg, PeerId, PeerInfo, ServerMsg};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const TO_STREAMER: &str = "v/";
const TO_VIEWER: &str = "s/";

/// What changed, for the UI or a command-line report.
#[derive(Debug, Clone)]
pub enum Event {
    /// Everyone in the room, us included, after any change.
    Peers(Vec<PeerInfo>),
    ViewerJoined { viewer: PeerId, quality: Option<Quality> },
    /// `took_ms`: how long tearing down its branch took.
    ViewerLeft { viewer: PeerId, took_ms: u64 },
    /// A watched stream ended: the streamer stopped or left, or our window was closed.
    WatchEnded { streamer: PeerId, reason: String },
    /// The connection to the signaling server is gone.
    Lost,
    /// A recoverable problem worth showing.
    Error(String),
}

/// Prefixes every kind it sends with the sender's role.
struct Tagged {
    inner: Arc<dyn Outbox>,
    tag: &'static str,
}

impl Outbox for Tagged {
    fn send(&self, kind: &str, payload: &str) {
        if no_tags() {
            self.inner.send(kind, payload);
        } else {
            self.inner.send(&format!("{}{kind}", self.tag), payload);
        }
    }
}

/// STREAMER_DEBUG_NO_TAGS sends kinds without roles and routes by "is it our viewer": the
/// spoiled variant for the mutual-watch gate.
fn no_tags() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STREAMER_DEBUG_NO_TAGS").is_some())
}

/// One stream we watch.
pub struct Watch {
    pub streamer: PeerId,
    pub viewer: Arc<Viewer>,
    pub clock: HostClock,
    /// The quality last asked for ("720p30 2000"), None for the streamer's own.
    pub quality: Mutex<Option<String>>,
    link: Arc<dyn Outbox>,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    peers: Vec<PeerInfo>,
    host: Option<Arc<Host>>,
    /// Our viewers and the outboxes our host answers them through.
    viewers: HashMap<PeerId, Arc<dyn Outbox>>,
    watches: HashMap<PeerId, Arc<Watch>>,
}

pub struct Session {
    client: Arc<ServerClient>,
    pub id: PeerId,
    pub room: String,
    state: Mutex<State>,
    events: mpsc::Sender<Event>,
}

impl Session {
    /// Joins `room` as `name`. Events arrive on the returned receiver.
    pub fn join(url: &str, room: &str, name: &str) -> Result<(Arc<Self>, mpsc::Receiver<Event>), String> {
        let (client, rx) = ServerClient::join(url, room, name)?;
        let (tx, events) = mpsc::channel();
        let me = PeerInfo { id: client.id, name: name.into(), streaming: false };
        let mut peers = client.peers.clone();
        peers.push(me);
        let s = Arc::new(Self {
            id: client.id,
            room: room.into(),
            client,
            state: Mutex::new(State { peers: peers.clone(), ..Default::default() }),
            events: tx,
        });
        let _ = s.events.send(Event::Peers(peers));
        let weak = Arc::downgrade(&s);
        std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                while let Ok(m) = rx.recv() {
                    let Some(s) = weak.upgrade() else { return };
                    s.dispatch(m);
                }
                if let Some(s) = weak.upgrade() {
                    let _ = s.events.send(Event::Lost);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok((s, events))
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        self.state.lock().unwrap().peers.clone()
    }

    /// Our stream, while we stream.
    pub fn host(&self) -> Option<Arc<Host>> {
        self.state.lock().unwrap().host.clone()
    }

    /// Starts streaming with `cfg`. The encoder only runs while someone watches.
    pub fn start_streaming(&self, cfg: HostConfig) -> Result<Arc<Host>, String> {
        let host = Arc::new(Host::new(cfg)?);
        {
            let mut st = self.state.lock().unwrap();
            if st.host.is_some() {
                return Err("already streaming".into());
            }
            st.host = Some(host.clone());
        }
        self.client.send(ClientMsg::SetStreaming { streaming: true });
        self.set_streaming_flag(true);
        Ok(host)
    }

    /// Stops our stream; its viewers get told by the server.
    pub fn stop_streaming(&self) {
        let host = {
            let mut st = self.state.lock().unwrap();
            st.viewers.clear();
            st.host.take()
        };
        self.client.send(ClientMsg::SetStreaming { streaming: false });
        self.set_streaming_flag(false);
        if let Some(h) = host {
            h.stop();
        }
    }

    fn set_streaming_flag(&self, on: bool) {
        let peers = {
            let mut st = self.state.lock().unwrap();
            if let Some(me) = st.peers.iter_mut().find(|p| p.id == self.id) {
                me.streaming = on;
            }
            st.peers.clone()
        };
        let _ = self.events.send(Event::Peers(peers));
    }

    /// Starts watching `streamer`, at `quality` if given ("720p30 2000"). The video opens in
    /// its own native window unless `cfg.headless`.
    pub fn watch(self: &Arc<Self>, streamer: PeerId, cfg: ViewerConfig, quality: Option<String>) -> Result<Arc<Watch>, String> {
        if self.state.lock().unwrap().watches.contains_key(&streamer) {
            return Err(format!("already watching {streamer}"));
        }
        let link: Arc<dyn Outbox> = Arc::new(Tagged { inner: self.client.to_peer(streamer), tag: TO_STREAMER });
        let clock = HostClock::default();
        let viewer = Arc::new(Viewer::new(cfg, clock.clone(), link.clone())?);
        viewer.start()?;
        #[cfg(windows)]
        if let (Some(win), Some(p)) = (viewer.window, self.peers().into_iter().find(|p| p.id == streamer)) {
            win.set_title(&p.name);
        }
        let w = Arc::new(Watch {
            streamer,
            viewer,
            clock,
            quality: Mutex::new(quality.clone()),
            link,
            stop: Arc::new(AtomicBool::new(false)),
        });
        self.state.lock().unwrap().watches.insert(streamer, w.clone());
        self.client.send(ClientMsg::Subscribe { streamer, quality });
        // Clock sync for latency figures; also notices a closed window.
        let (weak, w2) = (Arc::downgrade(self), w.clone());
        std::thread::spawn(move || {
            while !w2.stop.load(Ordering::SeqCst) {
                w2.link.send("ping", &w2.clock.local_ns().to_string());
                std::thread::sleep(Duration::from_millis(200));
                if w2.viewer.window_closed() {
                    if let Some(s) = weak.upgrade() {
                        s.end_watch(w2.streamer, "window closed", true);
                    }
                    return;
                }
            }
        });
        Ok(w)
    }

    /// Asks the streamer for another quality for this watch.
    pub fn set_watch_quality(&self, streamer: PeerId, quality: &str) {
        if let Some(w) = self.state.lock().unwrap().watches.get(&streamer) {
            w.link.send(crate::host::QUALITY_MSG, quality);
            *w.quality.lock().unwrap() = Some(quality.to_string());
        }
    }

    pub fn unwatch(&self, streamer: PeerId) {
        self.end_watch(streamer, "stopped", true);
    }

    pub fn watching(&self) -> Vec<Arc<Watch>> {
        self.state.lock().unwrap().watches.values().cloned().collect()
    }

    fn end_watch(&self, streamer: PeerId, reason: &str, unsubscribe: bool) {
        let Some(w) = self.state.lock().unwrap().watches.remove(&streamer) else { return };
        w.stop.store(true, Ordering::SeqCst);
        if unsubscribe {
            self.client.send(ClientMsg::Unsubscribe { streamer });
        }
        use gst::prelude::*;
        let _ = w.viewer.pipeline.set_state(gst::State::Null);
        // The video window goes with the watch.
        #[cfg(windows)]
        if let Some(win) = w.viewer.window {
            win.close();
        }
        let _ = self.events.send(Event::WatchEnded { streamer, reason: reason.into() });
    }

    /// Leaves the room: ends watches and our stream.
    pub fn leave(&self) {
        let ids: Vec<PeerId> = self.state.lock().unwrap().watches.keys().copied().collect();
        for id in ids {
            self.end_watch(id, "left the room", true);
        }
        if self.host().is_some() {
            self.stop_streaming();
        }
    }

    fn dispatch(&self, m: ServerMsg) {
        match m {
            ServerMsg::PeerJoined { peer } => self.update_peer(peer),
            ServerMsg::PeerUpdated { peer } => {
                let stopped = !peer.streaming;
                let id = peer.id;
                self.update_peer(peer);
                if stopped {
                    self.end_watch(id, "the streamer stopped", false);
                }
            }
            ServerMsg::PeerLeft { id } => {
                let peers = {
                    let mut st = self.state.lock().unwrap();
                    st.peers.retain(|p| p.id != id);
                    st.peers.clone()
                };
                let _ = self.events.send(Event::Peers(peers));
                self.end_watch(id, "the streamer left", false);
                self.drop_viewer(id);
            }
            ServerMsg::SubscribeRequest { viewer, quality } => self.add_viewer(viewer, quality),
            ServerMsg::UnsubscribeRequest { viewer } => self.drop_viewer(viewer),
            ServerMsg::Signal { from, kind, payload } if no_tags() => {
                let st = self.state.lock().unwrap();
                let to_host = st.viewers.get(&from).cloned().zip(st.host.clone());
                let watch = st.watches.get(&from).cloned();
                drop(st);
                match (to_host, watch) {
                    (Some((out, host)), _) => host.on_message(from, out.as_ref(), &kind, &payload),
                    (None, Some(w)) => w.viewer.on_message(&w.link, &w.clock, &kind, &payload),
                    _ => {}
                }
            }
            ServerMsg::Signal { from, kind, payload } => {
                if let Some(k) = kind.strip_prefix(TO_STREAMER) {
                    let st = self.state.lock().unwrap();
                    if let (Some(host), Some(out)) = (st.host.clone(), st.viewers.get(&from).cloned()) {
                        drop(st);
                        host.on_message(from, out.as_ref(), k, &payload);
                    }
                } else if let Some(k) = kind.strip_prefix(TO_VIEWER) {
                    let w = self.state.lock().unwrap().watches.get(&from).cloned();
                    if let Some(w) = w {
                        w.viewer.on_message(&w.link, &w.clock, k, &payload);
                    }
                }
            }
            ServerMsg::Error { message } => {
                let _ = self.events.send(Event::Error(message));
            }
            ServerMsg::Welcome { .. } => {}
        }
    }

    fn update_peer(&self, peer: PeerInfo) {
        let peers = {
            let mut st = self.state.lock().unwrap();
            match st.peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => *p = peer,
                None => st.peers.push(peer),
            }
            st.peers.clone()
        };
        let _ = self.events.send(Event::Peers(peers));
    }

    fn add_viewer(&self, viewer: PeerId, quality: Option<String>) {
        let Some(host) = self.host() else { return };
        // A bad request falls back to the host's own quality.
        let want = quality.as_deref().and_then(|q| match Quality::parse_request(q, host.ceiling) {
            Ok(w) => Some(w),
            Err(e) => {
                let _ = self.events.send(Event::Error(format!("viewer {viewer} quality {q}: {e}")));
                None
            }
        });
        let out: Arc<dyn Outbox> = Arc::new(Tagged { inner: self.client.to_peer(viewer), tag: TO_VIEWER });
        match host.add_viewer(viewer, out.clone(), want) {
            Ok(()) => {
                self.state.lock().unwrap().viewers.insert(viewer, out);
                let _ = self.events.send(Event::ViewerJoined { viewer, quality: want.map(|w| w.clamp_to(host.ceiling)) });
            }
            Err(e) => {
                let _ = self.events.send(Event::Error(format!("cannot add viewer {viewer}: {e}")));
            }
        }
    }

    fn drop_viewer(&self, viewer: PeerId) {
        let host = {
            let mut st = self.state.lock().unwrap();
            if st.viewers.remove(&viewer).is_none() {
                return;
            }
            st.host.clone()
        };
        let t = Instant::now();
        if let Some(h) = host {
            h.remove_viewer(viewer);
        }
        let _ = self.events.send(Event::ViewerLeft { viewer, took_ms: t.elapsed().as_millis() as u64 });
    }
}
