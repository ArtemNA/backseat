//! Voice chat from the command line, for the gates: joins a room, joins its voice, and prints
//! once a second how loud each other member arrives.

use media::config::IceConfig;
use media::session::Session;
use media::voice::VoiceConfig;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: voicechat --server ws://HOST:PORT/ws [--room NAME] [--name NAME] [--tone HZ] [--silent] [--stun stun://HOST:PORT|none] [--seconds 0] [--leave-after 0]";

fn main() {
    media::init();
    let (mut server, mut room, mut name) = (None::<String>, "default".to_string(), "voice".to_string());
    let mut cfg = VoiceConfig::default();
    let (mut seconds, mut leave_after) = (0u64, 0u64);
    // Also watch this streamer (no window, sound to the shared output).
    let mut watch = None::<String>;
    // Voice activity threshold, dBFS.
    let mut threshold = None::<f64>;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let key = argv[i].as_str();
        if key == "--silent" {
            cfg.silent = true;
            i += 1;
            continue;
        }
        let Some(val) = argv.get(i + 1).cloned() else {
            eprintln!("missing value for {key}\n{USAGE}");
            std::process::exit(2);
        };
        match key {
            "--server" => server = Some(val),
            "--room" => room = val,
            "--name" => name = val,
            "--tone" => cfg.test_tone = val.parse().ok(),
            "--watch" => watch = Some(val),
            "--threshold" => threshold = val.parse().ok(),
            "--seconds" => seconds = val.parse().unwrap_or(0),
            "--leave-after" => leave_after = val.parse().unwrap_or(0),
            "--stun" => cfg.ice = IceConfig { stun: (val != "none").then_some(val), turn: None },
            _ => {
                eprintln!("unknown {key}\n{USAGE}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    let Some(server) = server else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let (session, _events) = Session::join(&server, &room, &name).unwrap_or_else(|e| {
        eprintln!("cannot join: {e}");
        std::process::exit(1);
    });
    let voice = session.join_voice(cfg).unwrap_or_else(|e| {
        eprintln!("cannot join voice: {e}");
        std::process::exit(1);
    });
    voice.set_threshold(threshold);
    println!("voice: in room {room} as {name} (id {})", session.id);
    let t0 = Instant::now();
    let mut watching = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let s = t0.elapsed().as_secs();
        if leave_after > 0 && s >= leave_after {
            session.leave_voice();
            println!("voice: left at {s} s");
            std::thread::sleep(Duration::from_secs(2));
            break;
        }
        let peers = session.peers();
        if let (Some(w), false) = (&watch, watching) {
            if let Some(p) = peers.iter().find(|p| p.streaming && &p.name == w) {
                let cfg = media::config::ViewerConfig { headless: true, ..Default::default() };
                match session.watch(p.id, cfg, None) {
                    Ok(_) => println!("voice: watching {w}"),
                    Err(e) => eprintln!("voice: cannot watch {w}: {e}"),
                }
                watching = true;
            }
        }
        let mut line = format!("t {s:>3} mic {:>6.1} max {:>6.1} raw {:>6.1}", voice.mic_db(), voice.mic_max_db(), voice.raw_max_db());
        let mut levels = voice.peer_db();
        levels.sort_by_key(|(id, _)| *id);
        for (id, db) in levels {
            let n = peers.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
            line += &format!(" | {n} {db:>6.1}");
        }
        println!("{line}");
        if seconds > 0 && s >= seconds {
            break;
        }
    }
    session.leave_and_wait(Duration::from_secs(1));
}
