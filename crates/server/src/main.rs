//! Signaling server binary. Listens on localhost by default; pass another address to expose
//! it on a network. A hosting service that sets PORT: then it listens on 0.0.0.0:PORT.

use std::time::Duration;

/// An address requested every few minutes (optional, for the deployment's own needs).
/// Empty: off. STREAMER_KEEP_ALIVE_URL overrides it.
const KEEP_ALIVE_URL: &str = "";
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(4 * 60);

fn keep_alive() {
    let url = std::env::var("STREAMER_KEEP_ALIVE_URL").unwrap_or_else(|_| KEEP_ALIVE_URL.to_string());
    if url.is_empty() {
        return;
    }
    // Tests only: a shorter interval.
    let every = std::env::var("STREAMER_KEEP_ALIVE_SECS").ok().and_then(|s| s.parse().ok()).map(Duration::from_secs).unwrap_or(KEEP_ALIVE_EVERY);
    println!("server: keep-alive request to {url} every {} s", every.as_secs());
    std::thread::spawn(move || loop {
        std::thread::sleep(every);
        match ureq::get(&url).call() {
            Ok(r) if r.status().is_success() => println!("server: keep-alive ok"),
            Ok(r) => eprintln!("server: keep-alive got {}", r.status()),
            Err(e) => eprintln!("server: keep-alive failed: {e}"),
        }
    });
}

#[tokio::main]
async fn main() {
    let listen = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("PORT").ok().map(|p| format!("0.0.0.0:{p}")))
        .unwrap_or_else(|| "127.0.0.1:7800".into());
    let temp = match server::TempKey::from_env() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("server: {e}; temporary rooms are off");
            None
        }
    };
    println!("server: temporary rooms {}", if temp.is_some() { "on" } else { "off (no STREAMER_TEMP_SECRET)" });
    let server = server::Server::with_temp(server::Config::default(), temp);
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("server: cannot listen on {listen}: {e}");
            std::process::exit(1);
        }
    };
    println!("server: listening on ws://{listen}/ws");
    keep_alive();
    let s = server.clone();
    tokio::spawn(async move {
        let mut last = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let now = s.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
            println!("server: {} peers, {} bytes received in the last 30 s", s.peer_count(), now - last);
            last = now;
        }
    });
    if let Err(e) = server::serve(listener, server).await {
        eprintln!("server: {e}");
    }
}
