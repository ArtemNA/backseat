//! Blocking client for the signaling server. One thread owns the WebSocket: it sends queued
//! messages and reads with a short timeout, which also answers the server's pings.

use crate::signal::Outbox;
use protocol::{ClientMsg, PeerId, PeerInfo, ServerMsg};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

/// The TCP socket under a plain or TLS WebSocket stream.
fn tcp(s: &mut MaybeTlsStream<std::net::TcpStream>) -> Option<&mut std::net::TcpStream> {
    match s {
        MaybeTlsStream::Plain(t) => Some(t),
        MaybeTlsStream::NativeTls(t) => Some(t.get_mut()),
        _ => None,
    }
}

pub struct ServerClient {
    out: mpsc::Sender<ClientMsg>,
    pub id: PeerId,
    pub peers: Vec<PeerInfo>,
}

impl ServerClient {
    /// Connects, joins the room and returns the client plus the stream of server messages.
    pub fn join(url: &str, room: &str, name: &str) -> Result<(Arc<Self>, mpsc::Receiver<ServerMsg>), String> {
        let (mut ws, _) = tungstenite::connect(url).map_err(|e| format!("cannot reach server {url}: {e}"))?;
        if let Some(s) = tcp(ws.get_mut()) {
            s.set_nodelay(true).ok();
        }
        let join = ClientMsg::Join { room: room.into(), name: name.into() };
        ws.send(Message::Text(join.to_json().into())).map_err(|e| e.to_string())?;
        let (id, peers) = loop {
            match ws.read().map_err(|e| e.to_string())? {
                Message::Text(t) => match protocol::parse_server(&t)? {
                    ServerMsg::Welcome { id, peers, .. } => break (id, peers),
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

        let (out_tx, out_rx) = mpsc::channel::<ClientMsg>();
        let (in_tx, in_rx) = mpsc::channel::<ServerMsg>();
        std::thread::Builder::new()
            .name("signaling".into())
            .spawn(move || loop {
                while let Ok(m) = out_rx.try_recv() {
                    if ws.send(Message::Text(m.to_json().into())).is_err() {
                        return;
                    }
                }
                match ws.read() {
                    Ok(Message::Text(t)) => match protocol::parse_server(&t) {
                        Ok(m) => {
                            if in_tx.send(m).is_err() {
                                return;
                            }
                        }
                        Err(e) => eprintln!("signaling: bad server message: {e}"),
                    },
                    Ok(Message::Close(_)) => return,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                    {
                        // Flushes a queued pong, if the read produced one.
                        let _ = ws.flush();
                    }
                    Err(_) => return,
                }
            })
            .unwrap();
        Ok((Arc::new(Self { out: out_tx, id, peers }), in_rx))
    }

    pub fn send(&self, m: ClientMsg) {
        let _ = self.out.send(m);
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
