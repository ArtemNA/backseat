//! Direct signaling link between host and viewer: SDP, ICE and clock pings over TCP.
//! Frame: "<kind> <len>\n" followed by <len> bytes of UTF-8 payload. Media never uses it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

pub struct Link {
    writer: Mutex<TcpStream>,
}

pub type Reader = BufReader<TcpStream>;

impl Link {
    pub fn new(stream: TcpStream) -> (Self, Reader) {
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(stream.try_clone().expect("socket clone"));
        (Self { writer: Mutex::new(stream) }, reader)
    }

    pub fn send(&self, kind: &str, payload: &str) {
        let mut w = self.writer.lock().unwrap();
        let _ = writeln!(w, "{kind} {}", payload.len());
        let _ = w.write_all(payload.as_bytes());
        let _ = w.flush();
    }
}

/// Blocking read of one message; None when the peer closed the link.
pub fn recv(reader: &mut Reader) -> Option<(String, String)> {
    let mut header = String::new();
    if reader.read_line(&mut header).ok()? == 0 {
        return None;
    }
    let (kind, len) = header.trim_end().split_once(' ')?;
    let mut buf = vec![0u8; len.parse().ok()?];
    reader.read_exact(&mut buf).ok()?;
    Some((kind.to_string(), String::from_utf8(buf).ok()?))
}
