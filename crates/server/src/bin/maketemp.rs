//! Makes a temporary room's invite link. For the server's owner only: it needs the same
//! STREAMER_TEMP_SECRET the server has. The room ends after the given time; then nobody
//! joins it and everyone in it is dropped.
//!
//!   maketemp [--hours 24] [--minutes N] [--title "Friday games"] [--base https://host]

const DEFAULT_BASE: &str = "https://streamer-signaling.onrender.com";

fn usage() -> ! {
    eprintln!("usage: maketemp [--hours H] [--minutes M] [--title TEXT] [--base URL]");
    eprintln!("  needs STREAMER_TEMP_SECRET (the server's); the room lasts 24 hours by default");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut secs, mut title, mut base) = (None::<u64>, None::<String>, DEFAULT_BASE.to_string());
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--hours" => secs = Some(value().parse::<f64>().map(|h| (h * 3600.0) as u64).unwrap_or_else(|_| usage())),
            "--minutes" => secs = Some(value().parse::<f64>().map(|m| (m * 60.0) as u64).unwrap_or_else(|_| usage())),
            "--title" => title = Some(value()),
            "--base" => base = value().trim_end_matches('/').to_string(),
            _ => usage(),
        }
    }
    let secs = secs.unwrap_or(24 * 3600);
    if secs == 0 {
        usage();
    }
    let key = match server::TempKey::from_env() {
        Ok(Some(k)) => k,
        Ok(None) => {
            eprintln!("maketemp: set STREAMER_TEMP_SECRET to the server's secret first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("maketemp: {e}");
            std::process::exit(1);
        }
    };
    let ends = protocol::temp::now() + secs;
    let code = key.make(ends);
    let shown = code.as_bytes().chunks(4).map(|x| String::from_utf8_lossy(x).into_owned()).collect::<Vec<_>>().join("-");
    let title = title.map(|t| t.trim().chars().take(protocol::MAX_TITLE_LEN).collect::<String>()).filter(|t| !t.is_empty());
    let query = title.as_deref().map(|t| format!("?title={}", server::percent_encode(t))).unwrap_or_default();
    println!("{base}/t/{shown}{query}");
    eprintln!("code {shown}, ends in {} h {} min (unix {ends})", secs / 3600, secs % 3600 / 60);
}
