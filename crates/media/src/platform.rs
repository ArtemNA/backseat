//! Platform choices: timer resolution, encoders, decoders, audio devices.

use crate::config::Codec;

/// Windows timers tick at ~15.6 ms by default; receive timers then fire late, which in
/// stage 0 cost +4.5 ms of latency, 31 ms frame gaps and hundreds of false NACKs.
pub fn init_timer() {
    #[cfg(windows)]
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    Nvenc,
    Amf,
    Qsv,
}

/// Hardware encoders in preference order, by element factory name.
pub fn encoder_candidates(codec: Codec) -> Vec<(String, EncoderKind)> {
    let c = codec.short();
    vec![
        (format!("nvd3d11{c}enc"), EncoderKind::Nvenc),
        (format!("amf{c}enc"), EncoderKind::Amf),
        (format!("qsv{c}enc"), EncoderKind::Qsv),
    ]
}

/// Low-latency CBR settings without B-frames, per encoder family.
pub fn encoder_properties(kind: EncoderKind, kbit: u32, fps: u32, gop: u32) -> String {
    match kind {
        // Sunshine-like: ultra-low-latency, CBR, VBV of one frame, no reordering.
        // The element multiplies its kbit value by 1024, so convert to get 1000-based kbit/s.
        EncoderKind::Nvenc => {
            let k = (kbit as u64 * 1000 / 1024) as u32;
            format!(
                "preset=p4 tune=ultra-low-latency rc-mode=cbr bitrate={k} vbv-buffer-size={} gop-size={gop} \
                 bframes=0 zerolatency=true repeat-sequence-header=true aud=false",
                k / fps
            )
        }
        EncoderKind::Amf => format!("usage=ultra-low-latency rate-control=cbr bitrate={kbit} gop-size={gop}"),
        EncoderKind::Qsv => format!("low-latency=true rate-control=cbr bitrate={kbit} gop-size={gop}"),
    }
}

pub fn video_decoder(codec: Codec) -> String {
    if cfg!(target_os = "macos") {
        // Note: vtdec holds 16 frames on H.265 (stage 0), so macOS viewers want H.264.
        "vtdec_hw".into()
    } else {
        format!("d3d11{}dec", codec.short())
    }
}

pub fn audio_sink() -> &'static str {
    if cfg!(windows) {
        "wasapi2sink"
    } else if cfg!(target_os = "macos") {
        "osxaudiosink"
    } else {
        "autoaudiosink"
    }
}
