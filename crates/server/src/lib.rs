//! Signaling server: rooms, presence, subscriptions and SDP/ICE relay over WebSocket.
//! It never carries media; it only forwards small JSON messages between peers in a room.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, PeerId, PeerInfo, Resume, ServerMsg, MAX_NAME_LEN, MAX_PAYLOAD_LEN, MAX_ROOM_LEN, MAX_TITLE_LEN};
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
    /// A peer whose connection broke keeps its place this long, so it can come back with
    /// `Join::resume` while its direct streams go on; nobody is told it went.
    pub resume_grace: Duration,
}

impl Default for Config {
    fn default() -> Self {
        // Detects a vanished client in 4-6 s, inside the 10 s stage 2 target.
        Self { ping_interval: Duration::from_secs(2), idle_timeout: Duration::from_secs(4), max_room_size: 16, resume_grace: Duration::from_secs(20) }
    }
}

struct Peer {
    info: PeerInfo,
    room: String,
    tx: mpsc::UnboundedSender<ServerMsg>,
    /// Streamers this peer watches.
    watching: HashSet<PeerId>,
    /// Secret for taking this place back (`Join::resume`).
    token: String,
    /// The connection that holds the place now; a broken older one must not remove it.
    conn: u64,
    /// The connection broke and the peer has not come back yet.
    away: bool,
}

/// A secret nobody else can guess: two hashes keyed from the OS's randomness.
fn new_token(id: PeerId) -> String {
    use std::hash::BuildHasher;
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let a = std::collections::hash_map::RandomState::new().hash_one((id, t));
    let b = std::collections::hash_map::RandomState::new().hash_one((t, id, a));
    format!("{a:016x}{b:016x}")
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
    next_conn: AtomicU64,
    /// Bytes received from all clients, for checking that no media passes through.
    pub bytes_in: AtomicU64,
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self { cfg, rooms: Mutex::default(), next_id: AtomicU64::new(1), next_conn: AtomicU64::new(1), bytes_in: AtomicU64::new(0) })
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
            ClientMsg::Alive | ClientMsg::Leave => None,
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

    /// Joins `room`, or takes back a kept place with `resume`. Returns the peer id and the
    /// connection number that now holds it.
    fn join(&self, room: String, name: String, title: Option<String>, resume: Option<Resume>, tx: mpsc::UnboundedSender<ServerMsg>) -> Result<(PeerId, u64), String> {
        // Counted in characters, not bytes: a Cyrillic letter is two bytes, so a 17-letter
        // Ukrainian name was refused as too long.
        if room.is_empty() || room.chars().count() > MAX_ROOM_LEN || name.is_empty() || name.chars().count() > MAX_NAME_LEN {
            return Err("room or name is empty or too long".into());
        }
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = resume {
            let others: Vec<PeerInfo> = rooms.peers.iter().filter(|(k, p)| **k != r.id && p.room == room).map(|(_, p)| p.info.clone()).collect();
            let title = rooms.titles.get(&room).cloned();
            if let Some(p) = rooms.peers.get_mut(&r.id).filter(|p| p.token == r.token && p.room == room) {
                p.tx = tx.clone();
                p.conn = conn;
                p.away = false;
                let _ = tx.send(ServerMsg::Welcome { id: r.id, room, peers: others, title, token: Some(r.token) });
                return Ok((r.id, conn));
            }
            // Too late or unknown: an ordinary join below, with a new id.
        }
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
        let token = new_token(id);
        let _ = tx.send(ServerMsg::Welcome { id, room: room.clone(), peers: members, title, token: Some(token.clone()) });
        rooms.broadcast(&room, id, &ServerMsg::PeerJoined { peer: info.clone() });
        rooms.peers.insert(id, Peer { info, room, tx, watching: HashSet::new(), token, conn, away: false });
        Ok((id, conn))
    }

    fn leave(&self, id: PeerId) {
        self.rooms.lock().unwrap().remove(id);
    }

    /// Connection `conn` of `id` broke: keep the place for the grace period, then drop it
    /// unless a newer connection took it back.
    fn broke(self: &Arc<Self>, id: PeerId, conn: u64) {
        {
            let mut rooms = self.rooms.lock().unwrap();
            match rooms.peers.get_mut(&id) {
                Some(p) if p.conn == conn => p.away = true,
                _ => return,
            }
        }
        let (me, grace) = (self.clone(), self.cfg.resume_grace);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let mut rooms = me.rooms.lock().unwrap();
            if rooms.peers.get(&id).is_some_and(|p| p.away && p.conn == conn) {
                rooms.remove(id);
            }
        });
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
    let server = &server;
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
    let mut conn = 0u64;
    // Left on purpose (ClientMsg::Leave): no place kept.
    let mut left = false;
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
            (_, ClientMsg::Leave) => {
                left = true;
                break;
            }
            (None, ClientMsg::Join { room, name, title, resume }) => match server.join(room, name, title, resume, tx.clone()) {
                Ok((new_id, c)) => {
                    id = Some(new_id);
                    conn = c;
                }
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
    // STREAMER_DEBUG_NO_RESUME drops a broken peer at once, as before: the spoiled variant.
    match id {
        Some(me) if left || std::env::var_os("STREAMER_DEBUG_NO_RESUME").is_some() => server.leave(me),
        Some(me) => server.broke(me, conn),
        None => {}
    }
    drop(tx);
    let _ = writer.await;
}

/// Serves until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, server: Arc<Server>) -> std::io::Result<()> {
    axum::serve(listener, server.router()).await
}
