//! Integration tests against a real server on localhost with real WebSocket clients.

use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, PeerId, ServerMsg};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The default config with a short resume grace, so a broken client goes in 0.3 s here.
fn cfg() -> server::Config {
    server::Config { resume_grace: Duration::from_millis(300), ..server::Config::default() }
}

async fn start(cfg: server::Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = server::Server::new(cfg);
    tokio::spawn(server::serve(listener, srv));
    format!("ws://{addr}/ws")
}

struct Client {
    ws: Ws,
    id: PeerId,
    token: Option<String>,
}

impl Client {
    async fn join(url: &str, room: &str, name: &str) -> (Self, Vec<protocol::PeerInfo>) {
        Self::join_as(url, room, name, None).await
    }

    async fn join_as(url: &str, room: &str, name: &str, resume: Option<protocol::Resume>) -> (Self, Vec<protocol::PeerInfo>) {
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let join = ClientMsg::Join { room: room.into(), name: name.into(), title: None, resume };
        ws.send(Message::Text(join.to_json().into())).await.unwrap();
        let mut c = Client { ws, id: 0, token: None };
        match c.next().await {
            ServerMsg::Welcome { id, peers, token, .. } => {
                c.id = id;
                c.token = token;
                (c, peers)
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    async fn send(&mut self, m: ClientMsg) {
        self.ws.send(Message::Text(m.to_json().into())).await.unwrap();
    }

    /// Next server message within 2 s. Reading also answers the server's pings.
    async fn next(&mut self) -> ServerMsg {
        let deadline = Duration::from_secs(2);
        loop {
            let frame = tokio::time::timeout(deadline, self.ws.next())
                .await
                .expect("no message within 2 s")
                .expect("stream ended")
                .expect("websocket error");
            if let Message::Text(t) = frame {
                return protocol::parse_server(&t).unwrap();
            }
        }
    }

    /// Skips messages until one matches.
    async fn until(&mut self, f: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        loop {
            let m = self.next().await;
            if f(&m) {
                return m;
            }
        }
    }
}

#[tokio::test]
async fn eight_clients_see_each_other() {
    let url = start(cfg()).await;
    let mut clients = vec![];
    for i in 0..8 {
        let (c, peers) = Client::join(&url, "room", &format!("p{i}")).await;
        assert_eq!(peers.len(), i, "client {i} should see the {i} earlier peers");
        clients.push(c);
    }
    // Each earlier client hears about every later one.
    for i in 0..8 {
        for j in (i + 1)..8 {
            let id = clients[j].id;
            let m = clients[i].until(|m| matches!(m, ServerMsg::PeerJoined { .. })).await;
            assert_eq!(m, ServerMsg::PeerJoined { peer: protocol::PeerInfo { id, name: format!("p{j}"), streaming: false } });
        }
    }
}

#[tokio::test]
async fn subscribe_relay_and_unsubscribe() {
    let url = start(cfg()).await;
    let (mut host, _) = Client::join(&url, "r", "host").await;
    let (mut viewer, _) = Client::join(&url, "r", "viewer").await;
    host.until(|m| matches!(m, ServerMsg::PeerJoined { .. })).await;

    // Not streaming yet: refused.
    viewer.send(ClientMsg::Subscribe { streamer: host.id, quality: None }).await;
    assert!(matches!(viewer.next().await, ServerMsg::Error { .. }));

    host.send(ClientMsg::SetStreaming { streaming: true }).await;
    let m = viewer.until(|m| matches!(m, ServerMsg::PeerUpdated { .. })).await;
    assert!(matches!(m, ServerMsg::PeerUpdated { peer } if peer.streaming));

    viewer.send(ClientMsg::Subscribe { streamer: host.id, quality: None }).await;
    assert_eq!(host.next().await, ServerMsg::SubscribeRequest { viewer: viewer.id, quality: None });

    host.send(ClientMsg::Signal { to: viewer.id, kind: "offer".into(), payload: "v=0".into() }).await;
    assert_eq!(viewer.next().await, ServerMsg::Signal { from: host.id, kind: "offer".into(), payload: "v=0".into() });
    viewer.send(ClientMsg::Signal { to: host.id, kind: "answer".into(), payload: "v=0 a".into() }).await;
    assert_eq!(host.next().await, ServerMsg::Signal { from: viewer.id, kind: "answer".into(), payload: "v=0 a".into() });

    viewer.send(ClientMsg::Unsubscribe { streamer: host.id }).await;
    assert_eq!(host.next().await, ServerMsg::UnsubscribeRequest { viewer: viewer.id });
}

#[tokio::test]
async fn rooms_are_isolated() {
    let url = start(cfg()).await;
    let (mut a, _) = Client::join(&url, "one", "a").await;
    let (mut b, peers) = Client::join(&url, "two", "b").await;
    assert!(peers.is_empty());
    b.send(ClientMsg::Signal { to: a.id, kind: "offer".into(), payload: "x".into() }).await;
    assert!(matches!(b.next().await, ServerMsg::Error { .. }));
    // a got no message (server pings are not messages), checked over a short wait.
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(left, a.ws.next()).await {
            panic!("peer in another room received {t}");
        }
    }
}

#[tokio::test]
async fn leaving_viewer_releases_its_subscription() {
    let url = start(cfg()).await;
    let (mut host, _) = Client::join(&url, "r", "host").await;
    let (mut viewer, _) = Client::join(&url, "r", "viewer").await;
    host.send(ClientMsg::SetStreaming { streaming: true }).await;
    viewer.until(|m| matches!(m, ServerMsg::PeerUpdated { .. })).await;
    viewer.send(ClientMsg::Subscribe { streamer: host.id, quality: None }).await;
    host.until(|m| matches!(m, ServerMsg::SubscribeRequest { .. })).await;

    let vid = viewer.id;
    viewer.ws.close(None).await.unwrap();
    assert_eq!(host.next().await, ServerMsg::UnsubscribeRequest { viewer: vid });
    assert_eq!(host.next().await, ServerMsg::PeerLeft { id: vid });
}

/// A client whose connection stays open but which stops responding (pulled cable, lost
/// Wi-Fi) must leave the room in under 10 s. STREAMER_TEST_IDLE_TIMEOUT_SECS spoils the
/// server timeout to prove this test fails when detection is too slow.
#[tokio::test]
async fn silent_client_is_dropped_within_10_s() {
    let mut cfg = cfg();
    if let Some(secs) = std::env::var("STREAMER_TEST_IDLE_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()) {
        cfg.idle_timeout = Duration::from_secs(secs);
    }
    let url = start(cfg).await;
    let (mut watcher, _) = Client::join(&url, "r", "watcher").await;
    let (frozen, _) = Client::join(&url, "r", "frozen").await;
    watcher.until(|m| matches!(m, ServerMsg::PeerJoined { .. })).await;

    // From here on `frozen` never reads, so it never answers pings.
    let frozen_id = frozen.id;
    let t0 = Instant::now();
    let left = loop {
        assert!(t0.elapsed() < Duration::from_secs(10), "silent client not dropped within 10 s");
        match tokio::time::timeout(Duration::from_millis(500), watcher.ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if protocol::parse_server(&t).unwrap() == (ServerMsg::PeerLeft { id: frozen_id }) {
                    break t0.elapsed();
                }
            }
            _ => continue,
        }
    };
    println!("silent client dropped after {:.2} s", left.as_secs_f64());
    drop(frozen);
}

/// Imitates a reverse proxy that keeps a dead client's connection open and answers pings:
/// the client reads (so pongs go back) but stops sending Alive. It must leave the room.
#[tokio::test]
async fn silent_heartbeat_client_is_dropped() {
    let url = start(cfg()).await;
    let (mut ghost, _) = Client::join(&url, "room", "ghost").await;
    ghost.send(ClientMsg::Alive).await;
    let (mut other, peers) = Client::join(&url, "room", "other").await;
    assert_eq!(peers.len(), 1);
    let ghost_id = ghost.id;
    // The ghost keeps reading, so its pongs keep coming, but sends nothing else.
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = ghost.ws.next().await {} });
    let started = Instant::now();
    let m = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            other.send(ClientMsg::Alive).await;
            if let Ok(m) = tokio::time::timeout(Duration::from_millis(900), other.next()).await {
                if matches!(m, ServerMsg::PeerLeft { id } if id == ghost_id) {
                    return m;
                }
            }
        }
    })
    .await
    .expect("the silent client was not dropped within 10 s");
    eprintln!("dropped after {:?}: {m:?}", started.elapsed());
    reader.abort();
}

/// A client that never sent Alive (older builds) is kept while it answers pings.
#[tokio::test]
async fn client_without_heartbeat_stays() {
    let url = start(cfg()).await;
    let (old, _) = Client::join(&url, "room", "old").await;
    let mut ws = old.ws;
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} });
    tokio::time::sleep(Duration::from_secs(7)).await;
    let (_probe, peers) = Client::join(&url, "room", "probe").await;
    assert_eq!(peers.len(), 1, "the older client should still be in the room");
    reader.abort();
}

/// The first member's title names the room for everyone after; it goes with the room.
#[tokio::test]
async fn room_title_is_kept_while_the_room_lives() {
    let url = start(cfg()).await;
    let join = |title: Option<&str>| {
        let (url, title) = (url.clone(), title.map(String::from));
        async move {
            let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.unwrap();
            let m = ClientMsg::Join { room: "K7Q29XMB4TQ1".into(), name: "n".into(), title, resume: None };
            ws.send(Message::Text(m.to_json().into())).await.unwrap();
            let mut c = Client { ws, id: 0, token: None };
            match c.next().await {
                ServerMsg::Welcome { title, .. } => (c, title),
                other => panic!("expected welcome, got {other:?}"),
            }
        }
    };
    let (first, t1) = join(Some("Friends RPG")).await;
    assert_eq!(t1.as_deref(), Some("Friends RPG"));
    let (second, t2) = join(None).await;
    assert_eq!(t2.as_deref(), Some("Friends RPG"), "a joiner by code gets the title");
    let (third, t3) = join(Some("Other")).await;
    assert_eq!(t3.as_deref(), Some("Friends RPG"), "a later title does not replace it");
    drop((first, second, third));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_fresh, t4) = join(None).await;
    assert_eq!(t4, None, "the title goes when the room empties");
}

#[tokio::test]
async fn broken_connection_resumes_unnoticed() {
    // The real grace, so the place is surely kept across the reconnect.
    let url = start(server::Config::default()).await;
    let (mut a, _) = Client::join(&url, "r", "a").await;
    let (b, _) = Client::join(&url, "r", "b").await;
    a.until(|m| matches!(m, ServerMsg::PeerJoined { .. })).await;
    let (id, token) = (b.id, b.token.clone().expect("a token"));
    drop(b); // the connection breaks without a Leave
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (b2, peers) = Client::join_as(&url, "r", "b", Some(protocol::Resume { id, token })).await;
    assert_eq!(b2.id, id, "the same id comes back");
    assert!(peers.iter().any(|p| p.id == a.id) && !peers.iter().any(|p| p.id == id), "peers are the others");
    // a hears nothing about b leaving or joining.
    let quiet = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            if let Some(Ok(Message::Text(t))) = a.ws.next().await {
                return protocol::parse_server(&t).unwrap();
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a was told something: {quiet:?}");
}

#[tokio::test]
async fn leave_drops_at_once_and_a_wrong_token_gets_a_new_id() {
    let url = start(server::Config::default()).await;
    let (mut a, _) = Client::join(&url, "r", "a").await;
    let (mut b, _) = Client::join(&url, "r", "b").await;
    a.until(|m| matches!(m, ServerMsg::PeerJoined { .. })).await;
    let id = b.id;
    b.send(ClientMsg::Leave).await;
    let t = Instant::now();
    let m = a.until(|m| matches!(m, ServerMsg::PeerLeft { .. })).await;
    assert_eq!(m, ServerMsg::PeerLeft { id });
    assert!(t.elapsed() < Duration::from_secs(1), "Leave is at once, not after the grace");
    let (c, _) = Client::join_as(&url, "r", "b", Some(protocol::Resume { id, token: "guess".into() })).await;
    assert_ne!(c.id, id, "a wrong token does not take a place");
}
/// Names are limited in characters: a 32-letter Cyrillic name (64 bytes) joins, 33 letters do
/// not.
#[tokio::test]
async fn long_cyrillic_name_joins() {
    let url = start(cfg()).await;
    let name: String = "Олександр".chars().cycle().take(32).collect();
    assert_eq!(name.len(), 64);
    let (a, peers) = Client::join(&url, "names", &name).await;
    assert!(peers.is_empty());
    let (_b, peers) = Client::join(&url, "names", "Bob").await;
    assert_eq!(peers.iter().find(|p| p.id == a.id).map(|p| p.name.clone()), Some(name.clone()));

    let too_long: String = "Олександр".chars().cycle().take(33).collect();
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.unwrap();
    let join = ClientMsg::Join { room: "names".into(), name: too_long, title: None, resume: None };
    ws.send(Message::Text(join.to_json().into())).await.unwrap();
    let mut c = Client { ws, id: 0, token: None };
    assert!(matches!(c.next().await, ServerMsg::Error { .. }), "a 33-letter name must be refused");
}
