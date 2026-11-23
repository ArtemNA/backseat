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
fn work(mut model: DfTract, rx: Receiver<Vec<f32>>, tx: Sender<Vec<f32>>) {
    let hop = model.hop_size;
    let (mut inq, mut outq) = (VecDeque::<f32>::new(), VecDeque::<f32>::new());
    let mut noisy = ndarray::Array2::<f32>::zeros((1, hop));
    let mut enh = ndarray::Array2::<f32>::zeros((1, hop));
    let (mut short, mut errors, mut dithered) = (0u64, 0u64, 0u64);
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
            if model.process(noisy.view(), enh.view_mut()).is_err() {
                errors += 1;
                enh.assign(&noisy);
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
    eprintln!("voice: DeepFilterNet stopped ({short} samples before the first 10 ms, {errors} failed steps, {dithered} silent steps)");
}

/// An `identity` that denoises what passes through it (F32LE, mono, 48 kHz). The network
/// loads on the worker (0.15-0.4 s) while the caller goes on (the settings' "Hear yourself"
/// is built on the UI thread); until it is ready, or if it fails, the sound passes as it is.
pub fn element(s: u8) -> Result<gst::Element, String> {
    let (to_w, w_rx) = channel::<Vec<f32>>();
    let (w_tx, from_w) = channel::<Vec<f32>>();
    let ready = Arc::new(AtomicBool::new(false));
    let r2 = ready.clone();
    std::thread::Builder::new()
        .name("deepfilter".into())
        .spawn(move || match load(s) {
            Ok(m) => {
                r2.store(true, Ordering::SeqCst);
                work(m, w_rx, w_tx);
            }
            Err(e) => eprintln!("voice: no denoiser: {e}"),
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
        if pass || !ready.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(b) = info.buffer_mut() else { return gst::PadProbeReturn::Ok };
        let Ok(mut m) = b.make_mut().map_writable() else { return gst::PadProbeReturn::Ok };
        let data = m.as_mut_slice();
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
    /// The element comes back at once and the network loads behind it: "Hear yourself" is
    /// built on the UI thread, which a 0.15-0.4 s load froze. Spoiled:
    /// STREAMER_DEBUG_DFN_WAIT_LOAD (waits for the load, as before).
    #[test]
    fn element_does_not_wait_for_the_network() {
        crate::init();
        let t0 = std::time::Instant::now();
        let el = super::element(1).expect("element");
        let took = t0.elapsed();
        drop(el);
        assert!(took < std::time::Duration::from_millis(50), "element() took {took:?}");
    }
}
