//! Log file for the installed app. Started from a shortcut, the app has no stderr, so its
//! messages and GStreamer's warnings would be lost. `start` points stderr at a pipe whose
//! reader writes lines, with a time stamp, to `%LOCALAPPDATA%\Backseat\logs`. The newest
//! few files are kept, each is capped, and credentials in URLs (`user:pass@`) are masked.

use std::path::PathBuf;

/// Files kept, the new one included.
pub const KEEP: usize = 5;
/// Bytes written to one file at most; the rest of that run is dropped.
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

pub fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Backseat")
        .join("logs")
}

/// Masks the user and password in every `scheme://user:pass@host`.
pub fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || c == '/' || c == '"' || c == '\'').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) if at > 0 => {
                out.push_str("***");
                rest = &tail[at..];
            }
            _ => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// "HH:MM:SS.mmm" UTC.
fn stamp() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600 % 24, s / 60 % 60, s % 60, d.subsec_millis())
}

/// "YYYYMMDD-HHMMSS" UTC, sortable.
fn file_stamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, secs) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Deletes the oldest log files so that, with the one about to be made, `KEEP` remain.
fn rotate(dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut logs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("backseat-") && n.ends_with(".log")))
        .collect();
    logs.sort();
    let extra = logs.len().saturating_sub(KEEP - 1);
    for p in logs.into_iter().take(extra) {
        let _ = std::fs::remove_file(p);
    }
}

/// Starts the log file when stderr goes nowhere (or `STREAMER_LOG_TO_FILE` is set) and
/// returns its path. Call first thing in `main`, before other threads start.
#[cfg(windows)]
pub fn start() -> Option<PathBuf> {
    use std::io::{Read, Write};
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    use windows::Win32::System::Pipes::CreatePipe;

    let attached = unsafe { GetStdHandle(STD_ERROR_HANDLE) }.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
    if attached && std::env::var_os("STREAMER_LOG_TO_FILE").is_none() {
        return None;
    }
    let dir = dir();
    std::fs::create_dir_all(&dir).ok()?;
    rotate(&dir);
    let path = dir.join(format!("backseat-{}.log", file_stamp()));
    let mut file = std::fs::File::create(&path).ok()?;
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    unsafe { CreatePipe(&mut r, &mut w, None, 64 * 1024) }.ok()?;
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, w).ok()?;
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, w);
    }
    let mut pipe = unsafe { std::fs::File::from_raw_handle(r.0) };
    let _ = writeln!(file, "Backseat {} log, times UTC, started {}", env!("CARGO_PKG_VERSION"), file_stamp());
    std::thread::Builder::new()
        .name("logfile".into())
        .spawn(move || {
            let mut written = 0u64;
            let mut pending = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                pending.extend_from_slice(&buf[..n]);
                while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=i).collect();
                    if written >= MAX_BYTES {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&raw);
                    let line = format!("{} {}\n", stamp(), redact(text.trim_end()));
                    written += line.len() as u64;
                    if written >= MAX_BYTES {
                        let _ = writeln!(file, "{} log limit of {} MB reached; the rest of this run is not written", stamp(), MAX_BYTES >> 20);
                    } else {
                        let _ = file.write_all(line.as_bytes());
                    }
                }
                let _ = file.flush();
            }
        })
        .ok()?;
    Some(path)
}

#[cfg(not(windows))]
pub fn start() -> Option<PathBuf> {
    None
}

/// GStreamer prints its own messages with the C runtime, which has no stderr in a
/// windowed app. Unless GST_DEBUG asks for something else, route its warnings and errors
/// through ours. Call after `media::init`.
pub fn gst_warnings() {
    if std::env::var_os("GST_DEBUG").is_some() {
        return;
    }
    gst::log::remove_default_log_function();
    gst::log::set_default_threshold(gst::DebugLevel::Warning);
    gst::log::add_log_function(|cat, level, _file, _func, _line, obj, msg| {
        if level > gst::DebugLevel::Warning {
            return;
        }
        let what = obj.map(|o| o.to_string()).unwrap_or_default();
        let text = msg.get().map(|m| m.to_string()).unwrap_or_default();
        eprintln!("gst {level:?} {} {what}: {text}", cat.name());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials_only() {
        assert_eq!(redact("turn://alice:s3cret@turn.example.org:3478 x"), "turn://***@turn.example.org:3478 x");
        assert_eq!(redact("wss://host.example/ws?a=b"), "wss://host.example/ws?a=b");
        assert_eq!(redact("a ws://u:p@h/ws and turns://x:y@z:5349"), "a ws://***@h/ws and turns://***@z:5349");
        assert_eq!(redact("mail me@example.org"), "mail me@example.org");
        assert_eq!(redact("ws://h/path@x"), "ws://h/path@x");
    }

    #[test]
    fn file_stamp_is_a_date() {
        let s = file_stamp();
        assert_eq!(s.len(), 15, "{s}");
        assert!(s.starts_with("20"), "{s}");
    }
}
