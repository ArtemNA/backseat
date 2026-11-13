//! Silent check of "Remove the whine": a 1 kHz tone stands in for the whine. Finds its
//! frequency, then measures the tone through the microphone test's processing without and
//! with the cuts (nothing is played: a fake sink).

fn level(dsp: &media::voice::DspConfig, tone: f64) -> f64 {
    let t = media::mictest::MicTest::start(None, None, dsp, Some(tone), true).expect("mic test");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    t.level_db()
}

fn main() {
    media::init();
    let tone = 1000.0;
    let found = media::mictest::find_whine(None, 2, Some(tone)).expect("find");
    println!("found: {found:?}");
    let plain = media::voice::DspConfig { echo_cancel: false, noise: None, auto_gain: false, rnnoise: false, notches: vec![] };
    let cut = media::voice::DspConfig { notches: found.clone(), ..plain.clone() };
    // The spoiled variant: cuts somewhere else (the tone stays).
    let elsewhere = media::voice::DspConfig { notches: vec![3000.0], ..plain.clone() };
    println!("tone level without cuts: {:.1} dB", level(&plain, tone));
    println!("tone level with the found cuts: {:.1} dB", level(&cut, tone));
    println!("tone level with a cut elsewhere (control): {:.1} dB", level(&elsewhere, tone));
    // A voice-like tone between two cuts stays: 1250 Hz with cuts at 1000 and 1500.
    let near = media::voice::DspConfig { notches: vec![1000.0, 1500.0], ..plain };
    println!("1250 Hz between cuts at 1000 and 1500: {:.1} dB", level(&near, 1250.0));
}
