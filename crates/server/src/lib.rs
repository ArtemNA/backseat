//! Signaling server: rooms, presence, subscriptions and SDP/ICE relay over WebSocket.
//! It never carries media; it only forwards small JSON messages between peers in a room.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, PeerId, PeerInfo, ServerMsg, MAX_NAME_LEN, MAX_PAYLOAD_LEN, MAX_ROOM_LEN};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How often the server pings each client.
    pub ping_interval: Duration,
    /// A client silent for this long is dropped from its room.
    pub idle_timeout: Duration,
    pub max_room_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        // Detects a vanished client in 4-6 s, inside the 10 s stage 2 target.
        Self { ping_interval: Duration::from_secs(2), idle_timeout: Duration::from_secs(4), max_room_size: 16 }
    }
}

struct Peer {
    info: PeerInfo,
    room: String,
    tx: mpsc::UnboundedSender<ServerMsg>,
    /// Streamers this peer watches.
    watching: HashSet<PeerId>,
}

#[derive(Default)]
struct Rooms {
    peers: HashMap<PeerId, Peer>,
}

impl Rooms {
    fn room_of(&self, id: PeerId) -> Option<&str> {
        self.peers.get(&id).map(|p| p.room.as_str())
    }

    fn send(&self, id: PeerId, msg: ServerMsg) {
        if let Some(p) = self.peers.get(&id) {
            let _ = p.tx.send(msg);
        }
    }

    fn broadcast(&self, room: &str, except: PeerId, msg: &ServerMsg) {
        for (id, p) in &self.peers {
            if *id != except && p.room == room {
                let _ = p.tx.send(msg.clone());
            }
        }
    }

    fn same_room(&self, a: PeerId, b: PeerId) -> bool {
        matches!((self.room_of(a), self.room_of(b)), (Some(x), Some(y)) if x == y)
    }

    fn remove(&mut self, id: PeerId) {
        let Some(peer) = self.peers.remove(&id) else { return };
        // Streamers this peer watched stop sending to it.
        for streamer in &peer.watching {
            self.send(*streamer, ServerMsg::UnsubscribeRequest { viewer: id });
        }
        for p in self.peers.values_mut() {
            p.watching.remove(&id);
        }
        self.broadcast(&peer.room, id, &ServerMsg::PeerLeft { id });
    }
}

pub struct Server {
    cfg: Config,
    rooms: Mutex<Rooms>,
    next_id: AtomicU64,
    /// Bytes received from all clients, for checking that no media passes through.
    pub bytes_in: AtomicU64,
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self { cfg, rooms: Mutex::default(), next_id: AtomicU64::new(1), bytes_in: AtomicU64::new(0) })
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/ws", get(upgrade))
            // Health check for the host, and the target of the keep-alive request.
            .route("/live", get(|| async { "ok" }))
            .with_state(self.clone())
    }

    pub fn peer_count(&self) -> usize {
        self.rooms.lock().unwrap().peers.len()
    }

    /// Applies one message from `id`. Returns an error to send back, if any.
    fn handle(&self, id: PeerId, msg: ClientMsg) -> Option<String> {
        let mut rooms = self.rooms.lock().unwrap();
        match msg {
            ClientMsg::Join { .. } => Some("already joined".into()),
            ClientMsg::SetStreaming { streaming } => {
                let p = rooms.peers.get_mut(&id)?;
                p.info.streaming = streaming;
                let (room, info) = (p.room.clone(), p.info.clone());
                let watchers: Vec<PeerId> = if streaming {
                    vec![]
                } else {
                    rooms.peers.iter().filter(|(_, q)| q.watching.contains(&id)).map(|(k, _)| *k).collect()
                };
                for w in watchers {
                    if let Some(q) = rooms.peers.get_mut(&w) {
                        q.watching.remove(&id);
                    }
                }
                rooms.broadcast(&room, id, &ServerMsg::PeerUpdated { peer: info });
                None
            }
            ClientMsg::Subscribe { streamer, quality } => {
                if streamer == id || !rooms.same_room(id, streamer) {
                    return Some(format!("no peer {streamer} in this room"));
                }
                if !rooms.peers[&streamer].info.streaming {
                    return Some(format!("peer {streamer} is not streaming"));
                }
                if rooms.peers.get_mut(&id)?.watching.insert(streamer) {
                    rooms.send(streamer, ServerMsg::SubscribeRequest { viewer: id, quality });
                }
                None
            }
            ClientMsg::Unsubscribe { streamer } => {
                if rooms.peers.get_mut(&id)?.watching.remove(&streamer) {
                    rooms.send(streamer, ServerMsg::UnsubscribeRequest { viewer: id });
                }
                None
            }
            ClientMsg::Signal { to, kind, payload } => {
                if payload.len() > MAX_PAYLOAD_LEN {
                    return Some("payload too large".into());
                }
                if !rooms.same_room(id, to) {
                    return Some(format!("no peer {to} in this room"));
                }
                rooms.send(to, ServerMsg::Signal { from: id, kind, payload });
                None
            }
        }
    }

    fn join(&self, room: String, name: String, tx: mpsc::UnboundedSender<ServerMsg>) -> Result<PeerId, String> {
        if room.is_empty() || room.len() > MAX_ROOM_LEN || name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err("room or name is empty or too long".into());
        }
        let mut rooms = self.rooms.lock().unwrap();
        let members: Vec<PeerInfo> =
            rooms.peers.values().filter(|p| p.room == room).map(|p| p.info.clone()).collect();
        if members.len() >= self.cfg.max_room_size {
            return Err("room is full".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = PeerInfo { id, name, streaming: false };
        let _ = tx.send(ServerMsg::Welcome { id, room: room.clone(), peers: members });
        rooms.broadcast(&room, id, &ServerMsg::PeerJoined { peer: info.clone() });
        rooms.peers.insert(id, Peer { info, room, tx, watching: HashSet::new() });
        Ok(id)
    }

    fn leave(&self, id: PeerId) {
        self.rooms.lock().unwrap().remove(id);
    }
}

async fn upgrade(ws: WebSocketUpgrade, State(server): State<Arc<Server>>) -> axum::response::Response {
    ws.max_message_size(MAX_PAYLOAD_LEN * 2).on_upgrade(move |socket| session(server, socket))
}

/// One client connection: Join first, then messages until close or silence.
async fn session(server: Arc<Server>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let cfg = server.cfg;

    // Writer task: queued server messages and periodic pings.
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(cfg.ping_interval);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => if sink.send(Message::Text(m.to_json().into())).await.is_err() { break },
                    None => break,
                },
                _ = ping.tick() => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
            }
        }
        let _ = sink.close().await;
    });

    let mut id: Option<PeerId> = None;
    loop {
        // Any frame, including the pong to our ping, proves the client is alive.
        let next = tokio::time::timeout(cfg.idle_timeout, stream.next()).await;
        let frame = match next {
            Ok(Some(Ok(f))) => f,
            _ => break,
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
            Message::Binary(_) => {
                let _ = tx.send(ServerMsg::Error { message: "binary frames are not accepted".into() });
                continue;
            }
            _ => continue,
        };
        server.bytes_in.fetch_add(text.len() as u64, Ordering::Relaxed);
        let msg = match protocol::parse_client(&text) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(ServerMsg::Error { message: format!("bad message: {e}") });
                continue;
            }
        };
        match (id, msg) {
            (None, ClientMsg::Join { room, name }) => match server.join(room, name, tx.clone()) {
                Ok(new_id) => id = Some(new_id),
                Err(e) => {
                    let _ = tx.send(ServerMsg::Error { message: e });
                }
            },
            (None, _) => {
                let _ = tx.send(ServerMsg::Error { message: "join first".into() });
            }
            (Some(me), m) => {
                if let Some(e) = server.handle(me, m) {
                    let _ = tx.send(ServerMsg::Error { message: e });
                }
            }
        }
    }
    if let Some(me) = id {
        server.leave(me);
    }
    drop(tx);
    let _ = writer.await;
}

/// Serves until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, server: Arc<Server>) -> std::io::Result<()> {
    axum::serve(listener, server.router()).await
}
