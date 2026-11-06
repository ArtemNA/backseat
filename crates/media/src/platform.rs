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

/// Experiment for the host: raise this process's GPU scheduling priority so that other GPU
/// work (for example a viewer starting to decode) delays capture and encoding less.
/// STREAMER_GPU_PRIORITY=high|above|normal; returns a description of what happened.
pub fn set_gpu_priority_from_env() -> Option<String> {
    let want = std::env::var("STREAMER_GPU_PRIORITY").ok()?;
    #[cfg(windows)]
    {
        use windows::Wdk::Graphics::Direct3D::*;
        let class = match want.as_str() {
            "high" => D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH,
            "above" => D3DKMT_SCHEDULINGPRIORITYCLASS_ABOVE_NORMAL,
            _ => D3DKMT_SCHEDULINGPRIORITYCLASS_NORMAL,
        };
        let status = unsafe {
            D3DKMTSetProcessSchedulingPriorityClass(windows::Win32::System::Threading::GetCurrentProcess(), class)
        };
        Some(format!("GPU scheduling priority {want}: NTSTATUS {:#x}", status.0))
    }
    #[cfg(not(windows))]
    Some(format!("GPU priority {want} is not supported on this platform"))
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

/// Codecs this machine can stream: those with a hardware encoder.
pub fn encodable_codecs() -> Vec<Codec> {
    [Codec::Av1, Codec::H265, Codec::H264]
        .into_iter()
        .filter(|c| encoder_candidates(*c).iter().any(|(name, _)| gst::ElementFactory::find(name).is_some()))
        // Tests only: act like a GPU without an AV1 encoder.
        .filter(|c| !(*c == Codec::Av1 && std::env::var_os("STREAMER_DEBUG_NO_AV1").is_some()))
        .collect()
}

/// Low-latency CBR settings without B-frames, per encoder family.
pub fn encoder_properties(kind: EncoderKind, codec: Codec, kbit: u32, fps: u32, gop: u32) -> String {
    match kind {
        // Sunshine-like: ultra-low-latency, CBR, VBV of one frame, no reordering.
        // The element multiplies its kbit value by 1024, so convert to get 1000-based kbit/s.
        EncoderKind::Nvenc => {
            let k = (kbit as u64 * 1000 / 1024) as u32;
            // AV1 repeats its sequence header with every key frame and has no AUDs.
            let h26x = if codec == Codec::Av1 { "" } else { " repeat-sequence-header=true aud=false" };
            format!(
                "preset=p4 tune=ultra-low-latency rc-mode=cbr bitrate={k} vbv-buffer-size={} gop-size={gop} \
                 bframes=0 zerolatency=true{h26x}",
                k / fps
            )
        }
        EncoderKind::Amf => format!("usage=ultra-low-latency rate-control=cbr bitrate={kbit} gop-size={gop}"),
        EncoderKind::Qsv => format!("low-latency=true rate-control=cbr bitrate={kbit} gop-size={gop}"),
    }
}

pub fn video_decoder(codec: Codec) -> String {
    // AV1: on the GPU when it can (the element exists only then), else dav1d on the CPU.
    if codec == Codec::Av1 && !cfg!(target_os = "macos") && gst::ElementFactory::find("d3d11av1dec").is_none() {
        return "dav1ddec".into();
    }
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

/// A monitor a host can stream, for a source picker.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MonitorInfo {
    /// HMONITOR, for `Source::Monitor`.
    pub handle: u64,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

/// A window a host can stream, for a source picker.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WindowInfo {
    /// HWND, for `Source::Window`.
    pub hwnd: u64,
    pub title: String,
    /// Owning process, for capturing that program's audio.
    pub pid: u32,
    /// The program's file name, e.g. "eldenring.exe".
    pub exe: String,
    pub minimized: bool,
}

#[cfg(windows)]
pub fn monitors() -> Vec<MonitorInfo> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(m, &mut info as *mut MONITORINFOEXW as *mut _).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            out.push(MonitorInfo {
                handle: m.0 as u64,
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        true.into()
    }
    let mut out: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

#[cfg(windows)]
pub fn windows() -> Vec<WindowInfo> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe extern "system" fn each(h: HWND, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<WindowInfo>);
        if !IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return true.into();
        }
        if GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, &mut buf);
        if n <= 0 {
            return true.into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == std::process::id() {
            return true.into();
        }
        let mut exe = String::new();
        if let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut path = [0u16; 520];
            let mut len = path.len() as u32;
            if QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, windows::core::PWSTR(path.as_mut_ptr()), &mut len).is_ok() {
                let full = String::from_utf16_lossy(&path[..len as usize]);
                exe = full.rsplit(['\\', '/']).next().unwrap_or("").to_string();
            }
            let _ = CloseHandle(p);
        }
        out.push(WindowInfo {
            hwnd: h.0 as u64,
            title: String::from_utf16_lossy(&buf[..n as usize]),
            pid,
            exe,
            minimized: IsIconic(h).as_bool(),
        });
        true.into()
    }
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

#[cfg(not(windows))]
pub fn monitors() -> Vec<MonitorInfo> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn windows() -> Vec<WindowInfo> {
    Vec::new()
}
