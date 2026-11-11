//! Short notification sounds, made here from sine tones (no sound files): someone goes live
//! or stops, someone starts or stops watching our stream, someone joins or leaves voice,
//! push-to-talk on and off, our microphone off and on. They play through the shared output,
//! so the voice chat's echo canceller keeps them out of our microphone.

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    LiveStart,
    LiveEnd,
    ViewerJoined,
    ViewerLeft,
    VoiceJoined,
    VoiceLeft,
    TalkOn,
    TalkOff,
    MicOff,
    MicOn,
}

const RATE: usize = crate::playback::RATE as usize;

/// Notes: (frequency Hz, start ms, length ms). Each note: a sine with its octave, a 5 ms
/// attack and an exponential decay; the whole cue peaks near -16 dBFS.
fn notes(cue: Cue) -> &'static [(f32, u32, u32)] {
    match cue {
        // Two notes up / down, a fourth apart: something began / ended.
        Cue::LiveStart => &[(659.3, 0, 140), (880.0, 110, 260)],
        Cue::LiveEnd => &[(880.0, 0, 140), (659.3, 110, 260)],
        // One soft note with a slide up / down: a person came / went.
        Cue::ViewerJoined => &[(587.3, 0, 90), (784.0, 70, 170)],
        Cue::ViewerLeft => &[(784.0, 0, 90), (587.3, 70, 170)],
        // Three quick notes, a major chord up / down: voice.
        Cue::VoiceJoined => &[(523.3, 0, 80), (659.3, 60, 80), (784.0, 120, 200)],
        Cue::VoiceLeft => &[(784.0, 0, 80), (659.3, 60, 80), (523.3, 120, 200)],
        // A short high / lower tick: the key is down / up.
        Cue::TalkOn => &[(1318.5, 0, 45)],
        Cue::TalkOff => &[(987.8, 0, 45)],
        Cue::MicOff => &[(659.3, 0, 70), (440.0, 55, 120)],
        Cue::MicOn => &[(440.0, 0, 70), (659.3, 55, 120)],
    }
}

/// The cue as 48 kHz stereo S16 samples, interleaved.
pub fn render(cue: Cue) -> Vec<i16> {
    let ns = notes(cue);
    let end_ms = ns.iter().map(|(_, s, l)| s + l).max().unwrap_or(0) as usize;
    let n = end_ms * RATE / 1000;
    let mut mono = vec![0f32; n];
    for &(f, start, len) in ns {
        let (s0, len) = (start as usize * RATE / 1000, len as usize * RATE / 1000);
        for i in 0..len.min(n.saturating_sub(s0)) {
            let t = i as f32 / RATE as f32;
            let attack = (i as f32 / (0.005 * RATE as f32)).min(1.0);
            let decay = (-5.0 * i as f32 / len as f32).exp();
            let w = std::f32::consts::TAU * f * t;
            mono[s0 + i] += (w.sin() + 0.25 * (2.0 * w).sin()) * attack * decay;
        }
    }
    let peak = mono.iter().fold(0f32, |a, v| a.max(v.abs())).max(1e-6);
    let gain = 0.16 / peak; // about -16 dBFS
    mono.iter().flat_map(|v| {
        let s = (v * gain * 32767.0) as i16;
        [s, s]
    })
    .collect()
}

/// Plays `cue` (does not wait). Cues that come together play one after another.
pub fn play(cue: Cue) {
    static TX: OnceLock<Mutex<mpsc::Sender<Cue>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Cue>();
        std::thread::Builder::new()
            .name("sounds".into())
            .spawn(move || player(rx))
            .expect("sounds thread");
        Mutex::new(tx)
    });
    eprintln!("sound: {cue:?}");
    let _ = tx.lock().unwrap().send(cue);
}

/// Feeds cues to the shared output in 10 ms pieces at real time (each piece is timestamped
/// on arrival there).
fn player(rx: mpsc::Receiver<Cue>) {
    let input = match crate::playback::get().and_then(|p| p.input("sounds")) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("sounds: no output: {e}");
            return;
        }
    };
    let chunk = RATE / 100 * 2; // 10 ms, stereo
    while let Ok(cue) = rx.recv() {
        let pcm = render(cue);
        let t0 = Instant::now();
        for (k, piece) in pcm.chunks(chunk).enumerate() {
            while t0.elapsed() < Duration::from_millis(10 * k as u64) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let bytes: Vec<u8> = piece.iter().flat_map(|s| s.to_le_bytes()).collect();
            input.push(gst::Buffer::from_mut_slice(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_are_short_and_not_too_loud() {
        for cue in [Cue::LiveStart, Cue::LiveEnd, Cue::ViewerJoined, Cue::ViewerLeft, Cue::VoiceJoined, Cue::VoiceLeft, Cue::TalkOn, Cue::TalkOff, Cue::MicOff, Cue::MicOn] {
            let pcm = render(cue);
            let secs = pcm.len() as f32 / 2.0 / RATE as f32;
            let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap() as f32 / 32768.0;
            assert!(secs > 0.03 && secs < 0.5, "{cue:?} lasts {secs} s");
            assert!(peak > 0.1 && peak < 0.2, "{cue:?} peaks at {peak}");
        }
    }
}
