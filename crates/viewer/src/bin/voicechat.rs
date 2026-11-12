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
    // Watch for this many seconds before joining voice (the echo probe has played that long).
    let mut voice_after = 0u64;
    // Voice activity threshold, dBFS.
    let mut threshold = None::<f64>;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let key = argv[i].as_str();
        // An echo bot: sends back what it hears, a second later, plays nothing.
        if key == "--echo" {
            std::env::set_var("STREAMER_DEBUG_VOICE_ROOM", "1000:1");
            std::env::set_var("STREAMER_DEBUG_VOICE_NO_AEC", "1");
            cfg.silent = true;
            i += 1;
            continue;
        }
        // How long listing the sound devices takes (the settings list them).
        if key == "--list-devices" {
            for input in [true, false] {
                let t = Instant::now();
                let d = media::playback::devices(input);
                println!("{} devices: {} in {} ms", if input { "input" } else { "output" }, d.len(), t.elapsed().as_millis());
            }
            return;
        }
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
            "--voice-after" => voice_after = val.parse().unwrap_or(0),
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
    let mut watching = false;
    if voice_after > 0 {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(voice_after) {
            std::thread::sleep(Duration::from_millis(500));
            if let (Some(w), false) = (&watch, watching) {
                if let Some(p) = session.peers().iter().find(|p| p.streaming && &p.name == w) {
                    if cfg.silent {
                        media::playback::set_silent();
                    }
                    let vc = media::config::ViewerConfig { headless: true, ..Default::default() };
                    match session.watch(p.id, vc, None) {
                        Ok(_) => println!("voice: watching {w} before voice"),
                        Err(e) => eprintln!("voice: cannot watch {w}: {e}"),
                    }
                    watching = true;
                }
            }
        }
    }
    let voice = session.join_voice(cfg).unwrap_or_else(|e| {
        eprintln!("cannot join voice: {e}");
        std::process::exit(1);
    });
    voice.set_threshold(threshold);
    // What goes to the speakers (the shared output), loudest 10 ms per second: the voices of
    // the others must arrive there, not only at the receiver.
    let out_max = std::sync::Arc::new(std::sync::Mutex::new(-100f64));
    {
        let m = out_max.clone();
        let _ = media::playback::get().and_then(|p| {
            p.tap(move |b| {
                let Ok(map) = b.map_readable() else { return };
                let x = map.as_slice();
                let n = x.len() / 2;
                let sum: f64 = x.chunks_exact(2).map(|c| { let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0; v * v }).sum();
                let db = if n > 0 { 20.0 * (sum / n as f64).sqrt().max(1e-5).log10() } else { -100.0 };
                let mut g = m.lock().unwrap();
                if db > *g {
                    *g = db;
                }
            })
        });
    }
    println!("voice: in room {room} as {name} (id {})", session.id);
    let t0 = Instant::now();
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
        let out = std::mem::replace(&mut *out_max.lock().unwrap(), -100.0);
        if s % 5 == 0 {
            if let Ok(p) = media::playback::get() {
                eprintln!("shared output latency {} ms", p.render_latency().mseconds());
                use gst::prelude::*;
                for pad in p.mixer().sink_pads() {
                    let mut q = gst::query::Latency::new();
                    let ok = pad.peer_query(&mut q);
                    let (live, min, max) = q.result();
                    let up = pad.peer().and_then(|x| x.parent_element()).map(|e| e.name().to_string()).unwrap_or_default();
                    eprintln!("  mixer input {} from {up}: ok {ok} live {live} min {} ms max {:?}", pad.name(), min.mseconds(), max.map(|m| m.mseconds()));
                }
            }
        }
        let mut line = format!("t {s:>3} mic {:>6.1} max {:>6.1} raw {:>6.1} out {out:>6.1}", voice.mic_db(), voice.mic_max_db(), voice.raw_max_db());
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
