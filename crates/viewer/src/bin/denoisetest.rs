//! Silent check of the voice denoisers: a noisy recording goes through the same elements as
//! the microphone after webrtcdsp (`DspConfig::after`), and the result is compared with the
//! clean recording (SI-SDR, best lag within 100 ms), at each DeepFilterNet strength. Nothing
//! is played. STREAMER_DEBUG_DFN_PASS: the denoiser passes the sound untouched (spoiled run).
//!   denoisetest CLEAN.wav NOISY.wav [OUT_PREFIX]
//! STREAMER_DEBUG_DENOISE_SPLIT=1: 10 ms buffers, as webrtcdsp gives; else the file's own sizes.
use gst::prelude::*;

fn run(path: &str, dsp: &media::voice::DspConfig) -> Vec<f32> {
    let p = gst::Pipeline::new();
    let mk = |f: &str| gst::ElementFactory::make(f).build().expect(f);
    let src = gst::ElementFactory::make("filesrc").property("location", path).build().unwrap();
    let parse = mk("wavparse");
    let conv = mk("audioconvert");
    let res = mk("audioresample");
    let s16 = gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("rate", 48000i32).field("channels", 1i32).field("layout", "interleaved").build();
    let cf = gst::ElementFactory::make("capsfilter").property("caps", &s16).build().unwrap();
    let mut chain = vec![src, parse, conv, res, cf];
    if std::env::var_os("STREAMER_DEBUG_DENOISE_SPLIT").is_some() {
        let split = mk("audiobuffersplit");
        split.set_property("output-buffer-duration", gst::Fraction::new(1, 100));
        chain.push(split);
    }
    chain.extend(dsp.after(false).expect("after"));
    let sink = gst_app::AppSink::builder().caps(&s16).sync(false).build();
    chain.push(sink.clone().upcast());
    p.add_many(&chain).unwrap();
    gst::Element::link_many(&chain).unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let mut out = vec![];
    while let Ok(s) = sink.pull_sample() {
        let m = s.buffer().unwrap().map_readable().unwrap();
        out.extend(m.as_slice().chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0));
    }
    p.set_state(gst::State::Null).unwrap();
    out
}

fn si_sdr(s: &[f32], e: &[f32]) -> f64 {
    let n = s.len().min(e.len());
    let (s, e) = (&s[..n], &e[..n]);
    let dot: f64 = s.iter().zip(e).map(|(a, b)| *a as f64 * *b as f64).sum();
    let ss: f64 = s.iter().map(|a| (*a as f64).powi(2)).sum();
    let a = dot / ss;
    let t: f64 = s.iter().map(|v| (a * *v as f64).powi(2)).sum();
    let r: f64 = s.iter().zip(e).map(|(v, w)| (*w as f64 - a * *v as f64).powi(2)).sum();
    10.0 * (t / r).log10()
}

fn best(c: &[f32], e: &[f32]) -> (f64, i64) {
    let at = |lag: i64| {
        let (cs, es): (&[f32], &[f32]) = if lag >= 0 { (&c[..c.len() - lag as usize], &e[lag as usize..]) } else { (&c[(-lag) as usize..], &e[..e.len() - (-lag) as usize]) };
        si_sdr(cs, es)
    };
    let mut b = (f64::MIN, 0i64);
    for lag in (-4800i64..=4800).step_by(16) {
        let v = at(lag);
        if v > b.0 { b = (v, lag); }
    }
    for lag in b.1 - 16..=b.1 + 16 {
        let v = at(lag);
        if v > b.0 { b = (v, lag); }
    }
    b
}

fn write(path: &str, x: &[f32]) {
    let mut d = Vec::with_capacity(44 + x.len() * 2);
    let n = (x.len() * 2) as u32;
    d.extend(b"RIFF");
    d.extend((36 + n).to_le_bytes());
    d.extend(b"WAVEfmt ");
    for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 48000u32.to_le_bytes().to_vec(), 96000u32.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] {
        d.extend(v);
    }
    d.extend(b"data");
    d.extend(n.to_le_bytes());
    for v in x {
        d.extend(((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, d).unwrap();
}

fn main() {
    media::init();
    let a: Vec<String> = std::env::args().collect();
    let plain = media::voice::DspConfig { echo_cancel: false, noise: None, auto_gain: false, deepfilter: false, deepfilter_strength: 1 };
    let clean = run(&a[1], &plain);
    let mut scores = vec![];
    for (name, dsp) in [
        ("none", plain.clone()),
        ("strong", media::voice::DspConfig { deepfilter: true, deepfilter_strength: 0, ..plain.clone() }),
        ("medium", media::voice::DspConfig { deepfilter: true, deepfilter_strength: 1, ..plain.clone() }),
        ("soft", media::voice::DspConfig { deepfilter: true, deepfilter_strength: 2, ..plain.clone() }),
    ] {
        let t0 = std::time::Instant::now();
        let out = run(&a[2], &dsp);
        let (v, lag) = best(&clean, &out);
        println!("{name}: SI-SDR {v:.2} dB, lag {lag} samples, {} samples, {} ms", out.len(), t0.elapsed().as_millis());
        if let Some(pre) = a.get(3) {
            write(&format!("{pre}-{name}.wav"), &out);
        }
        scores.push(v);
    }
    // Passes when every strength gains at least 5 dB over the noisy input.
    let ok = scores[1..].iter().all(|v| *v >= scores[0] + 5.0);
    println!("DENOISE GATE: {} (none {:.2}, strong {:.2}, medium {:.2}, soft {:.2})", if ok { "PASS" } else { "FAIL" }, scores[0], scores[1], scores[2], scores[3]);
}
