//! Measures a microphone's steady noise: listens for N seconds (the user stays quiet) and
//! prints the strongest narrow peaks of the averaged spectrum. Nothing is recorded.
//!
//!   micspectrum [--seconds 5] [--device ID]

use gst::prelude::*;
use std::sync::{Arc, Mutex};

const BANDS: u32 = 4096;
const RATE: i32 = 48_000;

fn main() {
    media::init();
    let mut args = std::env::args().skip(1);
    let (mut secs, mut device, mut compare) = (5u64, None::<String>, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(5),
            "--device" => device = args.next(),
            // Also through RNNoise, from the same sound: the peaks before and after.
            "--rnnoise" => compare = true,
            _ => {}
        }
    }
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("wasapi2src").property("low-latency", true).build().unwrap();
    if let Some(d) = &device {
        src.set_property("device", d);
    }
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let res = gst::ElementFactory::make("audioresample").build().unwrap();
    let caps = gst::ElementFactory::make("capsfilter").property("caps", gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", RATE).field("channels", 1i32).build()).build().unwrap();
    let spectrum = |name: &str| gst::ElementFactory::make("spectrum").name(name).property("bands", BANDS).property("interval", 1_000_000_000u64).property("threshold", -120i32).property("post-messages", true).build().unwrap();
    let fakesink = || gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
    let tee = gst::ElementFactory::make("tee").build().unwrap();
    let q1 = gst::ElementFactory::make("queue").build().unwrap();
    let raw = spectrum("raw");
    let s1 = fakesink();
    pipeline.add_many([&src, &conv, &res, &caps, &tee, &q1, &raw, &s1]).unwrap();
    gst::Element::link_many([&src, &conv, &res, &caps, &tee]).unwrap();
    gst::Element::link_many([&tee, &q1, &raw, &s1]).unwrap();
    if compare {
        let q2 = gst::ElementFactory::make("queue").build().unwrap();
        let rn = gst::ElementFactory::make("audiornnoise").build().expect("audiornnoise");
        let den = spectrum("rnnoise");
        let s2 = fakesink();
        pipeline.add_many([&q2, &rn, &den, &s2]).unwrap();
        gst::Element::link_many([&tee, &q2, &rn, &den, &s2]).unwrap();
    }
    let sums: Arc<Mutex<std::collections::HashMap<String, (Vec<f64>, u32)>>> = Arc::default();
    let s2 = sums.clone();
    pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
        if let gst::MessageView::Element(e) = msg.view() {
            if let Some(st) = e.structure().filter(|s| s.name() == "spectrum") {
                if let (Ok(list), Some(src)) = (st.get::<gst::List>("magnitude"), msg.src()) {
                    let mut all = s2.lock().unwrap();
                    let s = all.entry(src.name().to_string()).or_insert_with(|| (vec![0f64; BANDS as usize], 0));
                    for (i, v) in list.iter().enumerate().take(BANDS as usize) {
                        if let Ok(db) = v.get::<f32>() {
                            // Averaged as power, shown in dB.
                            s.0[i] += 10f64.powf(db as f64 / 10.0);
                        }
                    }
                    s.1 += 1;
                }
            }
        }
        gst::BusSyncReply::Drop
    });
    pipeline.set_state(gst::State::Playing).expect("microphone");
    println!("listening {secs} s, stay quiet...");
    std::thread::sleep(std::time::Duration::from_secs(secs + 1));
    let _ = pipeline.set_state(gst::State::Null);
    let all = sums.lock().unwrap().clone();
    let Some((pow, n)) = all.get("raw").cloned() else {
        println!("no data");
        return;
    };
    let den = all.get("rnnoise").map(|(p, k)| p.iter().map(|v| 10.0 * (v / *k as f64).max(1e-14).log10()).collect::<Vec<f64>>());

    let hz = RATE as f64 / 2.0 / BANDS as f64;
    let db: Vec<f64> = pow.iter().map(|p| 10.0 * (p / n as f64).max(1e-14).log10()).collect();
    // The floor around each band: the median of +-40 bands; a peak stands out from it.
    let floor = |i: usize| {
        let lo = i.saturating_sub(40);
        let hi = (i + 40).min(db.len() - 1);
        let mut w: Vec<f64> = db[lo..=hi].to_vec();
        w.sort_by(|a, b| a.partial_cmp(b).unwrap());
        w[w.len() / 2]
    };
    let mut peaks: Vec<(f64, f64, f64)> = (2..db.len() - 2)
        .filter(|&i| db[i] >= db[i - 1] && db[i] >= db[i + 1] && db[i] >= db[i - 2] && db[i] >= db[i + 2])
        .map(|i| (i as f64 * hz, db[i], db[i] - floor(i)))
        .filter(|p| p.2 > 8.0 && p.0 > 20.0)
        .collect();
    peaks.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    println!("seconds averaged: {n}; bands {hz:.1} Hz wide");
    for (f, l, over) in peaks.iter().take(12) {
        let after = den.as_ref().map(|d| format!("   after RNNoise {:>6.1} dB", d[(f / hz).round() as usize])).unwrap_or_default();
        println!("peak {f:>7.0} Hz  {l:>6.1} dB  {over:>5.1} dB above the floor around it{after}");
    }
    let total = 10.0 * (pow.iter().sum::<f64>() / n as f64).max(1e-14).log10();
    println!("whole spectrum: {total:.1} dB");
    if let Some((p, k)) = all.get("rnnoise") {
        println!("whole spectrum after RNNoise: {:.1} dB", 10.0 * (p.iter().sum::<f64>() / *k as f64).max(1e-14).log10());
    }
}
