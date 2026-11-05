//! Signaling server binary. Listens on localhost by default; pass another address to expose
//! it on a network.

use std::time::Duration;

#[tokio::main]
async fn main() {
    let listen = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7800".into());
    let server = server::Server::new(server::Config::default());
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("server: cannot listen on {listen}: {e}");
            std::process::exit(1);
        }
    };
    println!("server: listening on ws://{listen}/ws");
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
