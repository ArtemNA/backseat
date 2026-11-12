//! Temporary rooms against a real server on localhost: a signed code joins, a forged or
//! expired one does not, and at the end everyone in the room is told and dropped.

use futures_util::{SinkExt, StreamExt};
use protocol::{temp, ClientMsg, ServerMsg};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SECRET: &[u8] = b"test-secret-only-for-these-tests";

fn key() -> server::TempKey {
    server::TempKey::new(SECRET).unwrap()
}

async fn start(key: Option<server::TempKey>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = server::Config { resume_grace: Duration::from_secs(5), ..server::Config::default() };
    tokio::spawn(server::serve(listener, server::Server::with_temp(cfg, key)));
    format!("ws://{addr}/ws")
}

/// What the server says to a join: Ok(welcome id, resume token) or Err(error text).
async fn join(url: &str, room: &str, name: &str, resume: Option<protocol::Resume>) -> (Ws, Result<(u64, String), String>) {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let m = ClientMsg::Join { room: room.into(), name: name.into(), title: None, resume };
    ws.send(Message::Text(m.to_json().into())).await.unwrap();
    let r = match next(&mut ws, Duration::from_secs(2)).await {
        Some(ServerMsg::Welcome { id, token, .. }) => Ok((id, token.unwrap())),
        Some(ServerMsg::Error { message }) => Err(message),
        other => panic!("unexpected {other:?}"),
    };
    (ws, r)
}

/// Next server message within `wait`; None when the connection closed or nothing came.
async fn next(ws: &mut Ws, wait: Duration) -> Option<ServerMsg> {
    let until = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(until, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(protocol::parse_server(&t).unwrap()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Reads until the connection closes or `wait` passes; returns the messages and whether
/// it closed.
async fn drain(ws: &mut Ws, wait: Duration) -> (Vec<ServerMsg>, bool) {
    let until = tokio::time::Instant::now() + wait;
    let mut got = vec![];
    loop {
        match tokio::time::timeout_at(until, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => got.push(protocol::parse_server(&t).unwrap()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => return (got, true),
            Ok(Some(Ok(_))) => {}
            Err(_) => return (got, false),
        }
        let _ = ws.send(Message::Text(ClientMsg::Alive.to_json().into())).await;
    }
}

#[tokio::test]
async fn valid_code_joins_and_others_see_each_other() {
    let url = start(Some(key())).await;
    let code = key().make(temp::now() + 3600);
    let (_a, ra) = join(&url, &code, "Ann", None).await;
    let (_b, rb) = join(&url, &code, "Bob", None).await;
    assert!(ra.is_ok() && rb.is_ok(), "{ra:?} {rb:?}");
}

#[tokio::test]
async fn forged_code_is_refused() {
    let url = start(Some(key())).await;
    let real = temp::decode(&key().make(temp::now() + 3600)).unwrap();
    // A longer life with the old signature, and a code made with another secret.
    let longer = temp::encode(temp::Parts { ends: real.ends + 86400, ..real });
    let other = server::TempKey::new(b"somebody-elses-secret-123").unwrap().make(temp::now() + 3600);
    for code in [longer, other] {
        let (_ws, r) = join(&url, &code, "Eve", None).await;
        assert_eq!(r, Err(temp::INVALID.to_string()), "{code}");
    }
}

#[tokio::test]
async fn expired_code_is_refused() {
    let url = start(Some(key())).await;
    let code = key().make(temp::now() - 1);
    let (mut ws, r) = join(&url, &code, "Late", None).await;
    assert_eq!(r, Err(temp::ENDED.to_string()));
    // And the connection closes after it.
    assert!(drain(&mut ws, Duration::from_secs(3)).await.1, "connection stayed open");
}

#[tokio::test]
async fn without_a_secret_temp_codes_are_refused_and_others_work() {
    let url = start(None).await;
    let code = key().make(temp::now() + 3600);
    let (_ws, r) = join(&url, &code, "Ann", None).await;
    assert_eq!(r, Err(temp::OFF.to_string()));
    let (_ws2, r2) = join(&url, "8TJB5QCWEATF", "Ann", None).await;
    assert!(r2.is_ok());
}

#[tokio::test]
async fn everyone_is_dropped_at_the_end_with_no_resume() {
    let url = start(Some(key())).await;
    let code = key().make(temp::now() + 3);
    let started = Instant::now();
    let (mut a, ra) = join(&url, &code, "Ann", None).await;
    let (mut b, rb) = join(&url, &code, "Bob", None).await;
    let (ida, tokena) = ra.unwrap();
    rb.unwrap();
    // A room with another code is left alone.
    let (mut c, rc) = join(&url, "8TJB5QCWEATF", "Cat", None).await;
    rc.unwrap();
    for ws in [&mut a, &mut b] {
        let (got, closed) = drain(ws, Duration::from_secs(6)).await;
        assert!(got.iter().any(|m| matches!(m, ServerMsg::Error { message } if message == temp::ENDED)), "not told: {got:?}");
        assert!(closed, "still connected after the end");
    }
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "ended after {took:?}");
    // Taking the place back, or a new join, is refused.
    let (_r, back) = join(&url, &code, "Ann", Some(protocol::Resume { id: ida, token: tokena })).await;
    assert_eq!(back, Err(temp::ENDED.to_string()));
    let (_n, new) = join(&url, &code, "New", None).await;
    assert_eq!(new, Err(temp::ENDED.to_string()));
    // The ordinary room's member is still there.
    let (got, closed) = drain(&mut c, Duration::from_millis(500)).await;
    assert!(!closed, "the other room's member was dropped: {got:?}");
    let _ = next(&mut c, Duration::from_millis(10)).await;
}

#[tokio::test]
async fn invite_page_for_temp_codes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, server::Server::with_temp(server::Config::default(), Some(key()))));
    let code = key().make(temp::now() + 3600);
    let get = |path: String| async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    };
    let shown = code.as_bytes().chunks(4).map(|x| String::from_utf8_lossy(x).into_owned()).collect::<Vec<_>>().join("-");
    let page = get(format!("/t/{shown}?title=%3Cb%3EFriday")).await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains(&format!("backseat://join/{code}?title=%3Cb%3EFriday")));
    assert!(page.contains("&lt;b&gt;Friday") && !page.contains("<b>Friday"));
    let forged = temp::encode(temp::Parts { ends: temp::now() + 99999, ..temp::decode(&code).unwrap() });
    assert!(get(format!("/t/{forged}")).await.starts_with("HTTP/1.1 404"));
}
