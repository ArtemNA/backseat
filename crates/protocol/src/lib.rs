//! Signaling messages between clients and the server, as JSON over WebSocket.
//! The server handles rooms, presence and subscriptions, and relays SDP/ICE between two
//! peers. Media never passes through it.

use serde::{Deserialize, Serialize};

pub type PeerId = u64;

pub const MAX_NAME_LEN: usize = 32;
pub const MAX_ROOM_LEN: usize = 64;
/// SDP offers are a few KB; anything far larger is not signaling.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
    pub streaming: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on a connection.
    Join { room: String, name: String },
    /// Announces whether this peer offers a stream others can watch.
    SetStreaming { streaming: bool },
    /// Asks a streamer to start sending to us, optionally at a lower quality ("720p30 2000"),
    /// so the stream starts there instead of at the host's own quality first.
    Subscribe {
        streamer: PeerId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<String>,
    },
    Unsubscribe { streamer: PeerId },
    /// Opaque message to one peer in the same room: offer, answer, ice, ping, pong.
    Signal { to: PeerId, kind: String, payload: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { id: PeerId, room: String, peers: Vec<PeerInfo> },
    PeerJoined { peer: PeerInfo },
    PeerUpdated { peer: PeerInfo },
    PeerLeft { id: PeerId },
    /// To a streamer: this viewer wants the stream, at this quality if given.
    SubscribeRequest {
        viewer: PeerId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<String>,
    },
    /// To a streamer: this viewer no longer wants the stream, or left.
    UnsubscribeRequest { viewer: PeerId },
    Signal { from: PeerId, kind: String, payload: String },
    Error { message: String },
}

impl ClientMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }
}

pub fn parse_client(s: &str) -> Result<ClientMsg, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

pub fn parse_server(s: &str) -> Result<ServerMsg, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = ClientMsg::Signal { to: 3, kind: "offer".into(), payload: "v=0\r\n".into() };
        assert_eq!(parse_client(&m.to_json()).unwrap(), m);
        let s = ServerMsg::Welcome { id: 1, room: "r".into(), peers: vec![] };
        assert_eq!(parse_server(&s.to_json()).unwrap(), s);
    }

    #[test]
    fn wire_format_is_tagged() {
        let m = ClientMsg::Subscribe { streamer: 7, quality: None };
        assert_eq!(m.to_json(), r#"{"type":"subscribe","streamer":7}"#);
        let q = ClientMsg::Subscribe { streamer: 7, quality: Some("720p30 2000".into()) };
        assert_eq!(parse_client(&q.to_json()).unwrap(), q);
        // Older clients without the field still parse.
        assert_eq!(parse_client(r#"{"type":"subscribe","streamer":7}"#).unwrap(), m);
    }
}
