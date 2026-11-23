//! The DeepFilterNet3 voice denoiser: a pad probe that rewrites F32LE mono 48 kHz buffers in
//! place, 10 ms at a time (30 ms behind: 10 of the window, 20 of the model looking ahead).
//! The model is built into the executable (crates/deep_filter). The network (tract) may not
//! leave its thread, so each element has a worker thread that owns it; the probe hands it
//! each buffer and waits for the result.
use df::tract::{DfParams, DfTract, RuntimeParams};
use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// How hard the network cuts: (lowest local SNR it still works on, dB; the most it takes
/// away, dB). Below that SNR a 10 ms step counts as no speech and is muted outright, which
/// cut quiet word ends at -15 by ear; the limit mixes that much of the input back in.
/// The strength the workers follow: set by the voice settings, applied from the next step
/// without rebuilding anything (a rebuild rejoined voice: the others heard you leave and
/// come back).
static STRENGTH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Every running denoiser takes this strength from its next 10 ms step.
pub fn set_strength(s: u8) {
    STRENGTH.store(s, Ordering::SeqCst);
}

fn strength(s: u8) -> (f32, f32, &'static str) {
    match s {
        0 => (-15.0, 100.0, "strong"),
        2 => (-30.0, 25.0, "soft"),
        _ => (-25.0, 40.0, "medium"),
    }
}

fn load(s: u8) -> Result<DfTract, String> {
    let t0 = std::time::Instant::now();
    let params = DfParams::default();
    // The thresholds of the authors' own deep-filter tool: the library's defaults
    // (-10/30/20) left more noise than no denoiser at all (KNOWLEDGE/pitfalls.md).
    // The upper two are the tool's 35/35; the lower one and the limit by strength.
    let (min_db, limit, name) = strength(s);
    let rp = RuntimeParams::default_with_ch(1).with_atten_lim(limit).with_thresholds(min_db, 35.0, 35.0);
    let m = DfTract::new(params, &rp).map_err(|e| format!("DeepFilterNet: {e}"))?;
    eprintln!("voice: DeepFilterNet, {name} ({min_db} dB, limit {limit} dB), ready in {} ms (hop {}, delay {} samples)", t0.elapsed().as_millis(), m.hop_size, m.fft_size - m.hop_size + m.lookahead * m.hop_size);
    Ok(m)
}

/// The worker: takes buffers of samples, returns as many denoised ones. The output runs
/// behind by the model's own delay; samples not processed yet (the start, when buffers are
/// shorter than 10 ms) come out as zeros.
fn work(mut model: DfTract, rx: Receiver<Vec<f32>>, tx: Sender<Vec<f32>>, s: u8, load: fn(u8) -> Result<DfTract, String>) {
    let hop = model.hop_size;
    let (mut inq, mut outq) = (VecDeque::<f32>::new(), VecDeque::<f32>::new());
    let mut noisy = ndarray::Array2::<f32>::zeros((1, hop));
    let mut enh = ndarray::Array2::<f32>::zeros((1, hop));
    let (mut short, mut errors, mut dithered, mut resets) = (0u64, 0u64, 0u64, 0u64);
    // One NaN or infinity in the network's state stays there for good (its running means and
    // recurrent state): the microphone would stay silent or noisy until rebuilt. The input is
    // made finite and bounded; an output that is not finite anyway reloads the network.
    // STREAMER_DEBUG_DFN_NO_GUARD: neither (the spoiled variant).
    let guard = std::env::var_os("STREAMER_DEBUG_DFN_NO_GUARD").is_none();
    let mut cur = s;
    // The network skips a step whose mean square is below 1e-7 (about -70 dBFS): it outputs
    // zeros without moving its buffers, so the 30 ms still in them were lost (word ends cut)
    // and came back stale when sound did (a piece of the last word, a click). Such a step
    // (digital silence: a muted or gated microphone, the echo canceller) gets a faint noise
    // (about -68 dBFS) it then removes. STREAMER_DEBUG_DFN_NO_DITHER: none (spoiled run).
    let dither = std::env::var_os("STREAMER_DEBUG_DFN_NO_DITHER").is_none();
    let mut seed = 0x2545_f491_u32;
    while let Ok(mut buf) = rx.recv() {
        inq.extend(buf.iter().copied());
        while inq.len() >= hop {
            let want = STRENGTH.load(Ordering::SeqCst);
            if want != cur {
                cur = want;
                let (min_db, limit, name) = strength(cur);
                model.min_db_thresh = min_db;
                model.set_atten_lim(limit);
                eprintln!("voice: DeepFilterNet now {name}");
            }
            for (d, s) in noisy.iter_mut().zip(inq.drain(..hop)) {
                *d = s;
            }
            if dither && noisy.iter().map(|v| v * v).sum::<f32>() / (hop as f32) < 2e-7 {
                dithered += 1;
                for v in noisy.iter_mut() {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *v += (seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * 7e-4;
                }
            }
            if guard {
                for v in noisy.iter_mut() {
                    *v = if v.is_finite() { v.clamp(-4.0, 4.0) } else { 0.0 };
                }
            }
            // A failed step gives silence, not the raw input: that would jump 30 ms ahead.
            if model.process(noisy.view(), enh.view_mut()).is_err() {
                errors += 1;
                enh.fill(0.0);
            }
            if guard && enh.iter().any(|v| !v.is_finite()) {
                resets += 1;
                enh.fill(0.0);
                match std::panic::catch_unwind(|| load(s)) {
                    Ok(Ok(m)) => {
                        model = m;
                        let (min_db, limit, _) = strength(cur);
                        model.min_db_thresh = min_db;
                        model.set_atten_lim(limit);
                    }
                    _ => eprintln!("voice: DeepFilterNet could not be reloaded"),
                }
            }
            outq.extend(enh.iter());
        }
        for v in buf.iter_mut() {
            *v = outq.pop_front().unwrap_or_else(|| {
                short += 1;
                0.0
            });
        }
        if tx.send(buf).is_err() {
            break;
        }
    }
    eprintln!("voice: DeepFilterNet stopped ({short} samples before the first 10 ms, {errors} failed steps, {dithered} silent steps, {resets} reloads)");
}

/// An `identity` that denoises what passes through it (F32LE, mono, 48 kHz). The network
/// loads on the worker (0.15-0.4 s) while the caller goes on (the settings' "Hear yourself"
/// is built on the UI thread). Until it is ready the output is silence: passing the sound on
/// meanwhile cut from live sound to the network's first 30 ms of zeros, a click mid-word.
/// If it fails to load, the sound passes as it is.
pub fn element(s: u8) -> Result<gst::Element, String> {
    element_with(s, load)
}

fn element_with(s: u8, load: fn(u8) -> Result<DfTract, String>) -> Result<gst::Element, String> {
    let (to_w, w_rx) = channel::<Vec<f32>>();
    let (w_tx, from_w) = channel::<Vec<f32>>();
    let ready = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let (r2, f2) = (ready.clone(), failed.clone());
    std::thread::Builder::new()
        .name("deepfilter".into())
        // The library panics on some failures (unwrap) instead of returning them: caught, so
        // that a failed load lets the sound through instead of leaving the microphone silent.
        // STREAMER_DEBUG_DFN_NO_CATCH: not caught, as before (the spoiled variant).
        .spawn(move || match if std::env::var_os("STREAMER_DEBUG_DFN_NO_CATCH").is_some() {
            load(s)
        } else {
            std::panic::catch_unwind(|| load(s)).unwrap_or_else(|_| Err("the network panicked while loading".into()))
        } {
            Ok(m) => {
                r2.store(true, Ordering::SeqCst);
                work(m, w_rx, w_tx, s, load);
            }
            Err(e) => {
                f2.store(true, Ordering::SeqCst);
                eprintln!("voice: no denoiser: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    // STREAMER_DEBUG_DFN_WAIT_LOAD: wait here until the network is ready (a file pushed
    // faster than real time would otherwise pass through before it is).
    if std::env::var_os("STREAMER_DEBUG_DFN_WAIT_LOAD").is_some() {
        let t0 = std::time::Instant::now();
        while !ready.load(Ordering::SeqCst) && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let el = gst::ElementFactory::make("identity").name("deepfilter").build().map_err(|e| e.to_string())?;
    // STREAMER_DEBUG_DFN_PASS: the samples go through untouched (a gate's spoiled run).
    let pass = std::env::var_os("STREAMER_DEBUG_DFN_PASS").is_some();
    let link = Mutex::new((to_w, from_w));
    el.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if pass || failed.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(b) = info.buffer_mut() else { return gst::PadProbeReturn::Ok };
        let Ok(mut m) = b.make_mut().map_writable() else { return gst::PadProbeReturn::Ok };
        let data = m.as_mut_slice();
        // STREAMER_DEBUG_DFN_LIVE_UNTIL_READY: the sound passes until ready, as before (spoiled).
        if !ready.load(Ordering::SeqCst) {
            if std::env::var_os("STREAMER_DEBUG_DFN_LIVE_UNTIL_READY").is_none() {
                data.fill(0);
            }
            return gst::PadProbeReturn::Ok;
        }
        let samples: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let l = link.lock().unwrap();
        if l.0.send(samples).is_err() {
            return gst::PadProbeReturn::Ok;
        }
        if let Ok(out) = l.1.recv() {
            for (c, v) in data.chunks_exact_mut(4).zip(out) {
                c.copy_from_slice(&v.to_le_bytes());
            }
        }
        gst::PadProbeReturn::Ok
    });
    Ok(el)
}

#[cfg(test)]
mod tests {
    /// The real load, 2 s late: the tests do not depend on how fast the machine loads it.
    fn slow_load(s: u8) -> Result<super::DfTract, String> {
        std::thread::sleep(std::time::Duration::from_secs(2));
        super::load(s)
    }

    /// The element comes back at once and the network loads behind it: "Hear yourself" is
    /// built on the UI thread, which a 0.15-0.4 s load froze. Spoiled:
    /// STREAMER_DEBUG_DFN_WAIT_LOAD (waits for the load, as before).
    #[test]
    fn element_does_not_wait_for_the_network() {
        crate::init();
        // A load that takes 2 s: the element must come back long before it ends.
        let t0 = std::time::Instant::now();
        let el = super::element_with(1, slow_load).expect("element");
        let took = t0.elapsed();
        drop(el);
        assert!(took < std::time::Duration::from_millis(1000), "element() took {took:?}");
    }

    /// A network that panics while loading lets the sound through (the library unwraps):
    /// before, the worker died with "ready" never set and the microphone stayed silent.
    #[test]
    fn a_panicking_load_lets_the_sound_through() {
        use gst::prelude::*;
        crate::init();
        let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", 48000i32).field("channels", 1i32).field("layout", "interleaved").build();
        let src = gst_app::AppSrc::builder().caps(&caps).format(gst::Format::Time).build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let el = super::element_with(1, |_| panic!("test: the network does not load")).expect("element");
        let p = gst::Pipeline::new();
        p.add_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        gst::Element::link_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        // Steps until one comes through (the failure is noticed on the worker), up to 5 s.
        let mut loud = 0;
        for _ in 0..500 {
            let tone: Vec<u8> = (0..480).flat_map(|n| (0.5 * ((n as f32) * 0.1).sin()).to_le_bytes()).collect();
            src.push_buffer(gst::Buffer::from_slice(tone)).unwrap();
            let s = sink.pull_sample().expect("sample");
            let m = s.buffer().unwrap().map_readable().unwrap();
            loud = m.as_slice().chunks_exact(4).filter(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs() > 1e-6).count();
            if loud > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        p.set_state(gst::State::Null).unwrap();
        assert!(loud > 400, "only {loud} of 480 samples came through");
    }

    /// NaN and infinite samples do not poison the network: after them a tone comes through
    /// again. Spoiled: STREAMER_DEBUG_DFN_NO_GUARD.
    #[test]
    fn nan_input_does_not_silence_the_microphone() {
        use gst::prelude::*;
        crate::init();
        let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", 48000i32).field("channels", 1i32).field("layout", "interleaved").build();
        let src = gst_app::AppSrc::builder().caps(&caps).format(gst::Format::Time).build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let el = super::element_with(1, super::load).expect("element");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let p = gst::Pipeline::new();
        p.add_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        gst::Element::link_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let step = |f: &dyn Fn(usize) -> f32| -> Vec<u8> { (0..480).flat_map(|n| f(n).to_le_bytes()).collect() };
        let mut steps = vec![];
        for i in 0..100 {
            let bad = (20..23).contains(&i);
            steps.push(step(&|n| if bad { if n % 2 == 0 { f32::NAN } else { f32::INFINITY } } else { 0.3 * ((n as f32) * 0.05).sin() + 0.05 * ((n * 7919 % 97) as f32 / 97.0 - 0.5) }));
        }
        for s in steps {
            src.push_buffer(gst::Buffer::from_slice(s)).unwrap();
        }
        let (mut bad, mut last_loud) = (0usize, 0f32);
        for i in 0..100 {
            let s = sink.pull_sample().expect("sample");
            let m = s.buffer().unwrap().map_readable().unwrap();
            let v: Vec<f32> = m.as_slice().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            bad += v.iter().filter(|x| !x.is_finite()).count();
            if i >= 90 {
                last_loud = last_loud.max(v.iter().map(|x| if x.is_finite() { x.abs() } else { 0.0 }).fold(0.0, f32::max));
            }
        }
        p.set_state(gst::State::Null).unwrap();
        assert_eq!(bad, 0, "{bad} samples were not finite");
        assert!(last_loud > 1e-4, "the microphone stayed silent after the bad samples (peak {last_loud})");
    }

    /// Until the network is ready the element gives silence, not the live sound: the switch
    /// from live sound to the network's first 30 ms of zeros was a click. Spoiled:
    /// STREAMER_DEBUG_DFN_LIVE_UNTIL_READY.
    #[test]
    fn silent_until_the_network_is_ready() {
        use gst::prelude::*;
        crate::init();
        let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", 48000i32).field("channels", 1i32).field("layout", "interleaved").build();
        let src = gst_app::AppSrc::builder().caps(&caps).format(gst::Format::Time).build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let el = super::element_with(1, slow_load).expect("element");
        let p = gst::Pipeline::new();
        p.add_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        gst::Element::link_many([src.upcast_ref(), &el, sink.upcast_ref()]).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        // 10 steps of a loud tone, pushed at once: well before the (2 s) load ends.
        for i in 0..10u64 {
            let tone: Vec<u8> = (0..480).flat_map(|n| (0.5 * ((n as f32) * 0.1).sin()).to_le_bytes()).collect();
            let mut b = gst::Buffer::from_slice(tone);
            b.get_mut().unwrap().set_pts(gst::ClockTime::from_mseconds(i * 10));
            src.push_buffer(b).unwrap();
        }
        let mut loud = 0;
        for _ in 0..10 {
            let s = sink.pull_sample().expect("sample");
            let m = s.buffer().unwrap().map_readable().unwrap();
            loud += m.as_slice().chunks_exact(4).filter(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs() > 1e-6).count();
        }
        p.set_state(gst::State::Null).unwrap();
        assert_eq!(loud, 0, "{loud} samples came through before the network was ready");
    }
}
