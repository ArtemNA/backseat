//! Signaling messages between clients and the server, as JSON over WebSocket.
//! The server handles rooms, presence and subscriptions, and relays SDP/ICE between two
//! peers. Media never passes through it.

use serde::{Deserialize, Serialize};

pub type PeerId = u64;

/// Limits in characters (not bytes).
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_ROOM_LEN: usize = 64;
/// A room's display name; the room itself is its code (or, in older clients, its name).
pub const MAX_TITLE_LEN: usize = 48;
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
    /// First message on a connection. `title` names a new room for everyone who joins it
    /// later (by code); it is ignored when the room already has one.
    Join {
        room: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Takes back our place after the connection broke: same id, nobody else told.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<Resume>,
    },
    /// Leaving on purpose: the server drops us at once instead of keeping our place for a
    /// reconnect.
    Leave,
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
    /// Sent every second after joining. A reverse proxy in front of the server answers its
    /// WebSocket pings itself and keeps a dead client's connection open, which left ghosts
    /// in rooms; a client that sends this is dropped when it stops.
    Alive,
}

/// What a client keeps from its Welcome to take its place back after a broken connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub id: PeerId,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        id: PeerId,
        room: String,
        peers: Vec<PeerInfo>,
        /// The room's display name, if its first member gave one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Secret for `Join::resume`; servers before 0.1.6 send none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
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

/// Temporary rooms. Their code is a token that carries the room's end time and a signature
/// only the server (and the owner's link maker) can make, so the server needs no storage:
/// it survives a restart. 20 symbols of Crockford base32 (100 bits): a 20-bit nonce, the
/// end as 35-bit unix seconds and a 45-bit truncated HMAC-SHA256. Ordinary codes have 12.
pub mod temp {
    pub const ABC: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    pub const LEN: usize = 20;
    pub const NONCE_BITS: u32 = 20;
    pub const END_BITS: u32 = 35;
    pub const MAC_BITS: u32 = 45;

    /// Error texts the server sends, which the app recognises (`ServerMsg::Error`).
    pub const ENDED: &str = "temporary room ended";
    pub const INVALID: &str = "invalid temporary room link";
    pub const OFF: &str = "temporary rooms are off on this server";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Parts {
        pub nonce: u32,
        /// Unix seconds.
        pub ends: u64,
        pub mac: u64,
    }

    /// Shaped like a temporary room's code (the signature is not checked).
    pub fn looks_temp(room: &str) -> bool {
        room.len() == LEN && room.bytes().all(|c| ABC.contains(&c))
    }

    pub fn decode(room: &str) -> Option<Parts> {
        if !looks_temp(room) {
            return None;
        }
        let v = room.bytes().fold(0u128, |v, c| (v << 5) | ABC.iter().position(|x| *x == c).unwrap() as u128);
        let mask = |bits: u32| (1u128 << bits) - 1;
        Some(Parts {
            nonce: ((v >> (END_BITS + MAC_BITS)) & mask(NONCE_BITS)) as u32,
            ends: ((v >> MAC_BITS) & mask(END_BITS)) as u64,
            mac: (v & mask(MAC_BITS)) as u64,
        })
    }

    pub fn encode(p: Parts) -> String {
        let mask = |bits: u32| (1u128 << bits) - 1;
        let v = ((p.nonce as u128 & mask(NONCE_BITS)) << (END_BITS + MAC_BITS)) | ((p.ends as u128 & mask(END_BITS)) << MAC_BITS) | (p.mac as u128 & mask(MAC_BITS));
        (0..LEN).rev().map(|i| ABC[((v >> (5 * i)) & 31) as usize] as char).collect()
    }

    /// What the signature covers.
    pub fn signed_bytes(nonce: u32, ends: u64) -> Vec<u8> {
        let mut b = b"backseat-temp-v1:".to_vec();
        b.extend_from_slice(&nonce.to_be_bytes());
        b.extend_from_slice(&ends.to_be_bytes());
        b
    }

    /// When a temporary room ends (unix seconds), or None for any other room.
    pub fn ends(room: &str) -> Option<u64> {
        decode(room).map(|p| p.ends)
    }

    pub fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = ClientMsg::Signal { to: 3, kind: "offer".into(), payload: "v=0\r\n".into() };
        assert_eq!(parse_client(&m.to_json()).unwrap(), m);
        let s = ServerMsg::Welcome { id: 1, room: "r".into(), peers: vec![], title: Some("Friends".into()), token: Some("t".into()) };
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
        assert_eq!(
            parse_client(r#"{"type":"join","room":"r","name":"n"}"#).unwrap(),
            ClientMsg::Join { room: "r".into(), name: "n".into(), title: None, resume: None }
        );
        assert!(matches!(parse_server(r#"{"type":"welcome","id":1,"room":"r","peers":[]}"#).unwrap(), ServerMsg::Welcome { title: None, .. }));
    }

    #[test]
    fn temp_code_round_trip() {
        let p = temp::Parts { nonce: 0xABCDE, ends: 1_800_000_000, mac: (1 << 45) - 3 };
        let c = temp::encode(p);
        assert_eq!(c.len(), 20);
        assert!(temp::looks_temp(&c));
        assert_eq!(temp::decode(&c), Some(p));
        assert_eq!(temp::ends(&c), Some(1_800_000_000));
        // An ordinary 12-symbol code is not one.
        assert_eq!(temp::ends("8TJB5QCWEATF"), None);
    }
}
