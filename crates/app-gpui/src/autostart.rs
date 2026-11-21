//! Start with Windows: a value under HKCU\...\Run that starts the app hidden in the tray. On by
//! default; the setting "autostart" false turns it off. Only the installed app (backseat.exe)
//! writes it: dev builds and test runs never touch the registry, they log what they would do.

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "Backseat";
/// The argument the Run value passes: start hidden in the tray.
pub const ARG: &str = "--autostart";

/// This start came from Windows' autostart.
pub fn started_by_windows() -> bool {
    std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case(ARG))
}

/// Whether the setting is on (the default).
pub fn wanted() -> bool {
    crate::settings().get("autostart").and_then(|v| v.as_bool()) != Some(false)
}

/// The Run value's command for `exe`.
fn command(exe: &std::path::Path) -> String {
    format!("\"{}\" {ARG}", exe.display())
}

/// Brings the Run value in line with the setting: on writes it (with this exe's path, so a
/// move or reinstall elsewhere follows), off deletes it.
pub fn sync() {
    let on = wanted();
    let Ok(exe) = std::env::current_exe() else { return };
    let installed = exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("backseat.exe"));
    if !installed || backseat_core::testing() || std::env::var_os("STREAMER_DEBUG_AUTOSTART_DRY").is_some() {
        eprintln!("autostart: {} (not written: {})", if on { command(&exe) } else { "off".into() }, if installed { "test run" } else { "not the installed app" });
        return;
    }
    let r = if on { write(&command(&exe)) } else { delete() };
    match r {
        Ok(()) => eprintln!("autostart: {}", if on { "on" } else { "off" }),
        Err(e) => eprintln!("autostart: {e}"),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open() -> Result<windows::Win32::System::Registry::HKEY, String> {
    use windows::Win32::System::Registry::*;
    let key = wide(RUN_KEY);
    let mut h = HKEY::default();
    let e = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, windows::core::PCWSTR(key.as_ptr()), None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut h, None) };
    if e.is_err() {
        return Err(format!("open Run: {e:?}"));
    }
    Ok(h)
}

fn write(cmd: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::*;
    let h = open()?;
    let name = wide(VALUE);
    let data = wide(cmd);
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
    let e = unsafe { RegSetValueExW(h, windows::core::PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) };
    unsafe {
        let _ = RegCloseKey(h);
    }
    if e.is_err() {
        return Err(format!("write: {e:?}"));
    }
    Ok(())
}

fn delete() -> Result<(), String> {
    use windows::Win32::System::Registry::*;
    let h = open()?;
    let name = wide(VALUE);
    let e = unsafe { RegDeleteValueW(h, windows::core::PCWSTR(name.as_ptr())) };
    unsafe {
        let _ = RegCloseKey(h);
    }
    // Already gone is fine.
    if e.is_err() && e != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        return Err(format!("delete: {e:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_quotes_the_path() {
        let c = super::command(std::path::Path::new(r"C:\Program Files\Backseat\backseat.exe"));
        assert_eq!(c, r#""C:\Program Files\Backseat\backseat.exe" --autostart"#);
    }
}
