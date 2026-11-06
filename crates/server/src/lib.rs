//! Signaling server: rooms, presence, subscriptions and SDP/ICE relay over WebSocket.
//! It never carries media; it only forwards small JSON messages between peers in a room.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, PeerId, PeerInfo, ServerMsg, MAX_NAME_LEN, MAX_PAYLOAD_LEN, MAX_ROOM_LEN, MAX_TITLE_LEN};
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
    /// Display names of rooms that have members; dropped with the last one.
    titles: HashMap<String, String>,
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
        if !self.peers.values().any(|p| p.room == peer.room) {
            self.titles.remove(&peer.room);
        }
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
            // Invite links: a page that opens the app on that room, or shows the code.
            .route("/j/{code}", get(invite_page))
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
            ClientMsg::Alive => None,
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

    fn join(&self, room: String, name: String, title: Option<String>, tx: mpsc::UnboundedSender<ServerMsg>) -> Result<PeerId, String> {
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
        if let Some(t) = title.map(|t| t.trim().chars().take(MAX_TITLE_LEN).collect::<String>()).filter(|t| !t.is_empty()) {
            rooms.titles.entry(room.clone()).or_insert(t);
        }
        let title = rooms.titles.get(&room).cloned();
        let _ = tx.send(ServerMsg::Welcome { id, room: room.clone(), peers: members, title });
        rooms.broadcast(&room, id, &ServerMsg::PeerJoined { peer: info.clone() });
        rooms.peers.insert(id, Peer { info, room, tx, watching: HashSet::new() });
        Ok(id)
    }

    fn leave(&self, id: PeerId) {
        self.rooms.lock().unwrap().remove(id);
    }
}

/// The page behind an invite link (`/j/K7Q2-9XMB-4TQ1`). Messengers make https links
/// clickable but not `backseat://` ones, so the link comes here and the page hands the
/// code to the app. Self-contained: no scripts or styles from anywhere else.
async fn invite_page(axum::extract::Path(code): axum::extract::Path<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let clean: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    if clean.len() != 12 {
        return (axum::http::StatusCode::NOT_FOUND, "no such invite").into_response();
    }
    let shown = format!("{}-{}-{}", &clean[0..4], &clean[4..8], &clean[8..12]);
    let html = INVITE_PAGE.replace("{CODE}", &clean).replace("{SHOWN}", &shown);
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::HeaderName::from_static("content-security-policy"), "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'"),
        ],
        html,
    )
        .into_response()
}

const INVITE_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Backseat</title>
<style>
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#1e1f22;color:#dbdee1;font:15px/1.5 "Segoe UI",system-ui,sans-serif}
main{max-width:420px;margin:16px;background:#313338;border-radius:12px;padding:28px;text-align:center}
h1{margin:0 0 6px;font-size:22px;color:#f2f3f5}
a.btn{display:inline-block;margin:18px 0 10px;padding:11px 22px;border-radius:8px;background:#3b6fd8;color:#fff;font-weight:600;text-decoration:none}
code{display:inline-block;margin-top:6px;padding:6px 10px;border-radius:6px;background:#1e1f22;color:#f2f3f5;font-size:18px;letter-spacing:.06em}
p{margin:6px 0;color:#b5bac1}
</style></head>
<body><main>
<h1 id="h">Backseat</h1>
<p id="lead">You are invited to a room.</p>
<a class="btn" id="open" href="backseat://join/{CODE}">Open in Backseat</a>
<p id="manual">No app or nothing happens? In Backseat press “+”, choose “I have an invite” and enter:</p>
<code>{SHOWN}</code>
</main>
<script>
var L={uk:["Вас запрошують у кімнату.","Відкрити в Backseat","Немає застосунку або нічого не сталося? У Backseat натисніть «+», виберіть «Маю запрошення» і введіть:"],
ru:["Вас приглашают в комнату.","Открыть в Backseat","Нет приложения или ничего не произошло? В Backseat нажмите «+», выберите «Есть приглашение» и введите:"]};
var l=L[(navigator.language||"").slice(0,2)];
if(l){document.getElementById("lead").textContent=l[0];document.getElementById("open").textContent=l[1];document.getElementById("manual").textContent=l[2];}
setTimeout(function(){location.href="backseat://join/{CODE}"},300);
</script>
</body></html>
"#;

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
    // Set by the first Alive: from then on only the client's own messages prove it is
    // there, since a proxy may answer our pings for a client that is gone.
    let mut heartbeat = false;
    let mut last_text = tokio::time::Instant::now();
    loop {
        // Any frame, including the pong to our ping, proves the client is alive.
        let next = tokio::time::timeout(cfg.idle_timeout, stream.next()).await;
        let frame = match next {
            Ok(Some(Ok(f))) => f,
            _ => break,
        };
        if heartbeat && last_text.elapsed() > cfg.idle_timeout && std::env::var_os("STREAMER_DEBUG_NO_HEARTBEAT").is_none() {
            break;
        }
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
            Message::Binary(_) => {
                let _ = tx.send(ServerMsg::Error { message: "binary frames are not accepted".into() });
                continue;
            }
            _ => continue,
        };
        last_text = tokio::time::Instant::now();
        server.bytes_in.fetch_add(text.len() as u64, Ordering::Relaxed);
        let msg = match protocol::parse_client(&text) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(ServerMsg::Error { message: format!("bad message: {e}") });
                continue;
            }
        };
        match (id, msg) {
            (Some(_), ClientMsg::Alive) => heartbeat = true,
            (None, ClientMsg::Join { room, name, title }) => match server.join(room, name, title, tx.clone()) {
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
