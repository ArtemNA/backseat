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
    // STREAMER_DEBUG_TONE_SQUARE: a 500 Hz square, a comb of tones like a ground whine.
    let square = std::env::var_os("STREAMER_DEBUG_TONE_SQUARE").is_some();
    let tone = if square { 500.0 } else { 1000.0 };
    if std::env::var_os("STREAMER_DEBUG_HIGHCUT_CHECK").is_some() {
        let plain = media::voice::DspConfig { echo_cancel: false, noise: None, auto_gain: false, rnnoise: false, notches: vec![], high_cut: false };
        let hc = media::voice::DspConfig { high_cut: true, ..plain.clone() };
        for f in [1000.0, 4000.0, 10000.0, 15000.0] {
            println!("{f:>6.0} Hz: plain {:.1} dB, high cut {:.1} dB", level(&plain, f), level(&hc, f));
        }
        return;
    }
    let found = media::mictest::find_whine(None, 2, Some(tone)).expect("find");
    println!("found: {found:?}");
    let plain = media::voice::DspConfig { echo_cancel: false, noise: None, auto_gain: false, rnnoise: false, notches: vec![], high_cut: false };
    let cut = media::voice::DspConfig { notches: found.clone(), ..plain.clone() };
    // The spoiled variant: cuts somewhere else (the tone stays).
    let elsewhere = media::voice::DspConfig { notches: vec![3000.0], ..plain.clone() };
    if square {
        println!("square level without cuts: {:.1} dB", level(&plain, tone));
        println!("square level with the found cuts: {:.1} dB", level(&cut, tone));
        let first12 = media::voice::DspConfig { notches: found.iter().copied().take(12).collect(), ..plain.clone() };
        println!("square level with only the 12 lowest cuts: {:.1} dB", level(&first12, tone));
        return;
    }
    println!("tone level without cuts: {:.1} dB", level(&plain, tone));
    println!("tone level with the found cuts: {:.1} dB", level(&cut, tone));
    println!("tone level with a cut elsewhere (control): {:.1} dB", level(&elsewhere, tone));
    // A voice-like tone between two cuts stays: 1250 Hz with cuts at 1000 and 1500.
    let near = media::voice::DspConfig { notches: vec![1000.0, 1500.0], ..plain };
    println!("1250 Hz between cuts at 1000 and 1500: {:.1} dB", level(&near, 1250.0));
}
