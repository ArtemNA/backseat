//! Limits against a client that floods or lies: a frame header naming a huge length, a
//! client that stops reading while others send to it, and too many messages a second.

use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, ServerMsg};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

async fn start() -> (String, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = server::Config { resume_grace: Duration::from_millis(300), ..server::Config::default() };
    tokio::spawn(server::serve(listener, server::Server::new(cfg)));
    (format!("ws://{addr}/ws"), addr)
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn join(url: &str, room: &str, name: &str) -> (Ws, u64) {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let j = ClientMsg::Join { room: room.into(), name: name.into(), title: None, resume: None };
    ws.send(Message::Text(j.to_json().into())).await.unwrap();
    loop {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            if let Ok(ServerMsg::Welcome { id, .. }) = protocol::parse_server(&t) {
                return (ws, id);
            }
        }
    }
}

/// A frame header naming 16 MB: the server must refuse it at once, not wait for (and hold
/// room for) 16 MB. Spoiled: STREAMER_DEBUG_ANY_FRAME_SIZE.
#[tokio::test]
async fn huge_frame_header_is_refused_at_once() {
    let (_, addr) = start().await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 101"), "no upgrade");
    // Text frame, masked, 64-bit length 16_000_000, then 4 mask bytes and nothing else.
    let mut h = vec![0x81u8, 0x80 | 127];
    h.extend_from_slice(&16_000_000u64.to_be_bytes());
    h.extend_from_slice(&[1, 2, 3, 4]);
    s.write_all(&h).await.unwrap();
    // The server closes: a close frame then EOF, or EOF. Waiting 2 s for data means it waits
    // for the 16 MB.
    let t = Instant::now();
    let mut closed = false;
    while t.elapsed() < Duration::from_secs(2) {
        match tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {
                closed = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Err(_) => break,
        }
    }
    assert!(closed, "the server kept waiting for the 16 MB frame");
    assert!(t.elapsed() < Duration::from_millis(1500), "closed only after {:?}", t.elapsed());
}

/// B stops reading while A sends it 64 KB signals: B is cut off once 1 MB waits for it
/// (others see it leave), well before its idle timeout (4 s). Spoiled:
/// STREAMER_DEBUG_QUEUE_LIMIT=1000000000.
#[tokio::test]
async fn a_client_that_stops_reading_is_cut_off() {
    let (url, _) = start().await;
    let (mut a, _) = join(&url, "flood", "A").await;
    let joined = Instant::now();
    let (_b, b) = join(&url, "flood", "B").await;
    let payload = "x".repeat(60_000);
    let mut left_at = None;
    'outer: for _ in 0..400 {
        a.send(Message::Text(ClientMsg::Signal { to: b, kind: "offer".into(), payload: payload.clone() }.to_json().into())).await.unwrap();
        // A reads what comes (and answers pings) between sends.
        while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(5), a.next()).await {
            if let Message::Text(t) = m {
                if let Ok(ServerMsg::PeerLeft { id }) = protocol::parse_server(&t) {
                    if id == b {
                        left_at = Some(joined.elapsed());
                        break 'outer;
                    }
                }
            }
        }
        if joined.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    let left_at = left_at.expect("B was not cut off within 3 s");
    eprintln!("B cut off after {left_at:?}");
}

/// 1000 messages in a burst: the connection is closed. Spoiled: STREAMER_DEBUG_NO_RATE_LIMIT.
#[tokio::test]
async fn a_message_flood_closes_the_connection() {
    let (url, _) = start().await;
    let (mut a, _) = join(&url, "burst", "A").await;
    for _ in 0..1000 {
        if a.send(Message::Text(ClientMsg::Alive.to_json().into())).await.is_err() {
            break;
        }
    }
    let t = Instant::now();
    let mut closed = false;
    while t.elapsed() < Duration::from_secs(3) {
        match tokio::time::timeout(Duration::from_secs(3), a.next()).await {
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => {
                closed = true;
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Err(_) => break,
        }
    }
    assert!(closed, "a flood of 1000 messages was let through");
}
