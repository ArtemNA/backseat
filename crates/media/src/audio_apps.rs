//! The programs that have sound sessions on any output device, for capturing "all sound" one
//! program at a time. Windows can capture one program's sound (with its child processes), or
//! everything but one program; a sound router (SteelSeries Sonar, Voicemeeter) plays other
//! programs' sound, ours included, as its own, so "everything but Backseat" still took in our
//! voice chat through it.

/// A program with a sound session.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioApp {
    pub pid: u32,
    /// Executable file name ("game.exe"); empty when it cannot be read.
    pub exe: String,
    /// Some session of it is playing now.
    pub active: bool,
}

/// Programs that play other programs' sound as their own (virtual devices, mixers). Their
/// sound is the others' over again, Backseat's included: never captured.
const ROUTERS: [&str; 11] = [
    "steelseriessonar",
    "steelseriesgg",
    "steelseriesengine",
    "voicemeeter",
    "vbvmaux",
    "nahimic",
    "sonicstudio",
    "fxsound",
    "equalizerapo",
    "goxlr",
    "wavelink",
];

/// Whether a program is a sound router (by its executable's name).
pub fn is_router(exe: &str) -> bool {
    let e = exe.to_ascii_lowercase();
    ROUTERS.iter().any(|r| e.starts_with(r))
}

#[cfg(windows)]
pub fn exe_name(pid: u32) -> String {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut path = [0u16; 520];
        let mut len = path.len() as u32;
        let mut exe = String::new();
        if QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, windows::core::PWSTR(path.as_mut_ptr()), &mut len).is_ok() {
            let full = String::from_utf16_lossy(&path[..len as usize]);
            exe = full.rsplit(['\\', '/']).next().unwrap_or("").to_string();
        }
        let _ = CloseHandle(p);
        exe
    }
}

/// Parent of each running process.
#[cfg(windows)]
pub fn parents() -> std::collections::HashMap<u32, u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut out = std::collections::HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                out.insert(e.th32ProcessID, e.th32ParentProcessID);
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Every program with a sound session on an active output device (not the system sounds).
#[cfg(windows)]
pub fn audio_apps() -> Vec<AudioApp> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    let mut out: Vec<AudioApp> = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(e) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(devices) = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
        let n = devices.GetCount().unwrap_or(0);
        for i in 0..n {
            let Ok(dev) = devices.Item(i) else { continue };
            let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(sessions) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(ctl) = sessions.GetSession(j) else { continue };
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                if ctl2.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK {
                    continue;
                }
                let Ok(pid) = ctl2.GetProcessId() else { continue };
                if pid == 0 {
                    continue;
                }
                let active = ctl.GetState().is_ok_and(|s| s == AudioSessionStateActive);
                match out.iter_mut().find(|a| a.pid == pid) {
                    Some(a) => a.active |= active,
                    None => out.push(AudioApp { pid, exe: exe_name(pid), active }),
                }
            }
        }
    }
    out
}

/// What "all sound" captures, one program each: every program with sound but Backseat (and
/// its children), the routers, and programs whose parent is already in the list (a program's
/// capture takes in its children). STREAMER_DEBUG_CAPTURE_ROUTERS keeps the routers (the
/// spoiled variant).
#[cfg(windows)]
pub fn to_capture(apps: &[AudioApp]) -> Vec<AudioApp> {
    let me = std::process::id();
    let parents = parents();
    let under = |pid: u32, root: u32| {
        let mut p = pid;
        for _ in 0..64 {
            if p == root {
                return true;
            }
            match parents.get(&p) {
                Some(&q) if q != p && q != 0 => p = q,
                _ => return false,
            }
        }
        false
    };
    let routers = std::env::var_os("STREAMER_DEBUG_CAPTURE_ROUTERS").is_none();
    let keep: Vec<AudioApp> = apps.iter().filter(|a| !under(a.pid, me) && !(routers && is_router(&a.exe))).cloned().collect();
    keep.iter().filter(|a| !keep.iter().any(|b| b.pid != a.pid && under(a.pid, b.pid))).cloned().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn routers_by_name() {
        assert!(super::is_router("SteelSeriesSonar.exe"));
        assert!(super::is_router("voicemeeter8x64.exe"));
        assert!(!super::is_router("game.exe"));
        assert!(!super::is_router("chrome.exe"));
    }
}
