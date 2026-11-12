//! Blocking client for the signaling server. One thread owns the WebSocket: it sends queued
//! messages and reads with a short timeout, which also answers the server's pings.
//!
//! A broken connection is not the end: the thread reconnects for a while and takes our place
//! back with the token from Welcome (`Join::resume`), so our id stays and nobody in the room
//! is told we left. Direct streams never pass through the server and go on meanwhile; a
//! friend's stream ended (and the app crashed) when Render's TLS connection broke once.

use crate::signal::Outbox;
use protocol::{ClientMsg, PeerId, PeerInfo, Resume, ServerMsg};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// How long to keep trying to take our place back; the server keeps it for 20 s.
const RESUME_FOR: Duration = Duration::from_secs(15);

/// The TCP socket under a plain or TLS WebSocket stream.
fn tcp(s: &mut MaybeTlsStream<TcpStream>) -> Option<&mut TcpStream> {
    match s {
        MaybeTlsStream::Plain(t) => Some(t),
        MaybeTlsStream::NativeTls(t) => Some(t.get_mut()),
        _ => None,
    }
}

struct Welcomed {
    id: PeerId,
    peers: Vec<PeerInfo>,
    title: Option<String>,
    token: Option<String>,
}

/// Connects and sends `join`; returns the socket and the server's Welcome.
fn connect(url: &str, join: &ClientMsg) -> Result<(Ws, Welcomed), String> {
    let (mut ws, _) = tungstenite::connect(url).map_err(|e| format!("cannot reach server {url}: {e}"))?;
    if let Some(s) = tcp(ws.get_mut()) {
        s.set_nodelay(true).ok();
        // Bounded, so a stalled reconnect cannot hang the thread.
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    }
    ws.send(Message::Text(join.to_json().into())).map_err(|e| e.to_string())?;
    let w = loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => match protocol::parse_server(&t)? {
                ServerMsg::Welcome { id, peers, title, token, .. } => break Welcomed { id, peers, title, token },
                ServerMsg::Error { message } => return Err(format!("server refused join: {message}")),
                _ => continue,
            },
            _ => continue,
        }
    };
    // Outgoing messages wait for the read timeout. A long timeout delays our pings one
    // way only, which skews the clock estimate (10 ms gave 0 ms "latency" in stage 2).
    if let Some(s) = tcp(ws.get_mut()) {
        s.set_read_timeout(Some(Duration::from_millis(1))).ok();
    }
    Ok((ws, w))
}

/// Why `pump` returned.
enum End {
    /// We left on purpose, or nobody uses the client any more.
    Left,
    /// The connection broke.
    Broken,
    /// Nobody reads server messages any more.
    Gone,
}

/// Moves messages both ways until the connection ends.
fn pump(ws: &mut Ws, out_rx: &mpsc::Receiver<ClientMsg>, in_tx: &mpsc::Sender<ServerMsg>, break_at: &mut Option<Instant>) -> End {
    let mut alive = Instant::now() - Duration::from_secs(1);
    loop {
        // Test only: STREAMER_DEBUG_SIGNALING_BREAK_AT cuts the connection once, as a
        // broken TLS session did.
        if break_at.is_some_and(|t| Instant::now() >= t) {
            *break_at = None;
            eprintln!("signaling: test: cutting the connection");
            if let Some(s) = tcp(ws.get_mut()) {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        // Proves to the server that we are here; see ClientMsg::Alive.
        if alive.elapsed() >= Duration::from_secs(1) && std::env::var_os("STREAMER_DEBUG_NO_ALIVE").is_none() {
            alive = Instant::now();
            if let Err(e) = ws.send(Message::Text(ClientMsg::Alive.to_json().into())) {
                eprintln!("signaling: send failed: {e}");
                return End::Broken;
            }
        }
        loop {
            match out_rx.try_recv() {
                Ok(m) => {
                    let leave = m == ClientMsg::Leave;
                    if let Err(e) = ws.send(Message::Text(m.to_json().into())) {
                        eprintln!("signaling: send failed: {e}");
                        return if leave { End::Left } else { End::Broken };
                    }
                    if leave {
                        let _ = ws.close(None);
                        let _ = ws.flush();
                        return End::Left;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = ws.send(Message::Text(ClientMsg::Leave.to_json().into()));
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return End::Left;
                }
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => match protocol::parse_server(&t) {
                Ok(m) => {
                    if in_tx.send(m).is_err() {
                        return End::Gone;
                    }
                }
                Err(e) => eprintln!("signaling: bad server message: {e}"),
            },
            Ok(Message::Close(f)) => {
                eprintln!("signaling: server closed the connection: {f:?}");
                return End::Broken;
            }
            Ok(_) => {}
            // On Windows a read that times out can also report ERROR_IO_PENDING (997);
            // treating it as a lost connection dropped a host after ~5 min of churn.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) || e.raw_os_error() == Some(997) =>
            {
                // Flushes a queued pong, if the read produced one.
                let _ = ws.flush();
            }
            Err(e) => {
                eprintln!("signaling: connection lost: {e}");
                return End::Broken;
            }
        }
    }
}

pub struct ServerClient {
    out: mpsc::Sender<ClientMsg>,
    pub id: PeerId,
    pub peers: Vec<PeerInfo>,
    /// The room's display name from the server, if it has one.
    pub title: Option<String>,
    /// For taking this place back later, also from a new start of the app.
    pub resume: Option<Resume>,
    /// The join took back a place kept from an earlier start (`join_resuming`).
    pub resumed: bool,
    /// The connection broke and is being made again.
    pub reconnecting: Arc<std::sync::atomic::AtomicBool>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ServerClient {
    /// Connects, joins the room and returns the client plus the stream of server messages.
    /// A later Welcome on that stream means the connection broke and we took our place
    /// back: same id, its peers are the room as it is now.
    pub fn join(url: &str, room: &str, name: &str) -> Result<(Arc<Self>, mpsc::Receiver<ServerMsg>), String> {
        Self::join_titled(url, room, name, None)
    }

    /// Like `join`; `title` names the room if it is new.
    pub fn join_titled(url: &str, room: &str, name: &str, title: Option<&str>) -> Result<(Arc<Self>, mpsc::Receiver<ServerMsg>), String> {
        Self::join_resuming(url, room, name, title, None)
    }

    /// Like `join_titled`; `resume` takes back the place an earlier start of the app held
    /// (it closed without leaving: an update, a crash), so the room does not see us twice.
    /// The server gives a new place when that one is gone.
    pub fn join_resuming(url: &str, room: &str, name: &str, title: Option<&str>, resume: Option<Resume>) -> Result<(Arc<Self>, mpsc::Receiver<ServerMsg>), String> {
        let wanted = resume.as_ref().map(|r| r.id);
        let first = ClientMsg::Join { room: room.into(), name: name.into(), title: title.map(String::from), resume };
        let (mut ws, w) = connect(url, &first)?;
        let join = ClientMsg::Join { room: room.into(), name: name.into(), title: title.map(String::from), resume: None };
        let (id, token) = (w.id, w.token.clone());
        let resumed = wanted == Some(id);
        let (out_tx, out_rx) = mpsc::channel::<ClientMsg>();
        let (in_tx, in_rx) = mpsc::channel::<ServerMsg>();
        let url = url.to_string();
        let reconnecting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let again_flag = reconnecting.clone();
        let mut break_at = std::env::var("STREAMER_DEBUG_SIGNALING_BREAK_AT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|s| Instant::now() + Duration::from_secs(s));
        let thread = std::thread::Builder::new()
            .name("signaling".into())
            .spawn(move || loop {
                match pump(&mut ws, &out_rx, &in_tx, &mut break_at) {
                    End::Left | End::Gone => return,
                    End::Broken => {}
                }
                // A server older than 0.1.6 gives no token: nothing to resume.
                let Some(token) = token.clone() else { return };
                let ClientMsg::Join { room, name, title, .. } = join.clone() else { return };
                let again = ClientMsg::Join { room, name, title, resume: Some(Resume { id, token }) };
                let until = Instant::now() + RESUME_FOR;
                again_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let resumed = loop {
                    if Instant::now() >= until {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(700));
                    match connect(&url, &again) {
                        Ok((s, w)) if w.id == id => break Some((s, w)),
                        Ok((mut s, w)) => {
                            // The server no longer had our place: this is a new member.
                            eprintln!("signaling: our place was gone (new id {}), giving up", w.id);
                            let _ = s.send(Message::Text(ClientMsg::Leave.to_json().into()));
                            let _ = s.close(None);
                            break None;
                        }
                        Err(e) => eprintln!("signaling: reconnecting: {e}"),
                    }
                };
                again_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                let Some((s, w)) = resumed else { return };
                eprintln!("signaling: reconnected, still peer {id}");
                ws = s;
                let welcome = ServerMsg::Welcome { id, room: String::new(), peers: w.peers, title: w.title, token: w.token };
                if in_tx.send(welcome).is_err() {
                    return;
                }
            })
            .unwrap();
        let resume = w.token.clone().map(|token| Resume { id, token });
        Ok((Arc::new(Self { out: out_tx, id, peers: w.peers, title: w.title, resume, resumed, reconnecting, thread: std::sync::Mutex::new(Some(thread)) }), in_rx))
    }

    /// A client with no server, for a direct session: what it is asked to send comes out of
    /// the returned receiver, for the direct link to carry.
    pub fn local(id: PeerId, peers: Vec<PeerInfo>) -> (Arc<Self>, mpsc::Receiver<ClientMsg>) {
        let (out, rx) = mpsc::channel();
        (Arc::new(Self { out, id, peers, title: None, resume: None, resumed: false, reconnecting: Arc::default(), thread: std::sync::Mutex::new(None) }), rx)
    }

    pub fn send(&self, m: ClientMsg) {
        let _ = self.out.send(m);
    }

    /// Leaves and waits up to `wait` for the message to go out: the app is about to exit,
    /// and a Leave that never left would keep our place (and our name in the room) for 20 s.
    pub fn leave_and_wait(&self, wait: Duration) {
        self.send(ClientMsg::Leave);
        let Some(t) = self.thread.lock().unwrap().take() else { return };
        let until = Instant::now() + wait;
        while !t.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// An outbox that delivers signaling messages to one peer through the server.
    pub fn to_peer(self: &Arc<Self>, peer: PeerId) -> Arc<dyn Outbox> {
        Arc::new(PeerOutbox { client: self.clone(), peer })
    }
}

struct PeerOutbox {
    client: Arc<ServerClient>,
    peer: PeerId,
}

impl Outbox for PeerOutbox {
    fn send(&self, kind: &str, payload: &str) {
        self.client.send(ClientMsg::Signal { to: self.peer, kind: kind.into(), payload: payload.into() });
    }
}
