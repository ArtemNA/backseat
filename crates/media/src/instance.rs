//! One running app per user. The first start claims a named pipe; any later start (an invite
//! link clicked while the app runs) finds it taken, hands its message over and exits. The
//! first gets each message on a callback. Windows only; elsewhere every start runs.
//!
//! Earlier the pipe took one client at a time and a start that could not reach it ran as a
//! second app: two links clicked together put the same person in a room twice.

/// The pipe of this user's running app.
#[cfg(windows)]
fn pipe_name() -> String {
    // Per user, so two users on one machine each have their own app.
    let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!(r"\\.\pipe\backseat-{user}")
}

/// The claim of being the running app; `listen` serves it.
#[cfg(windows)]
pub struct Claim(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for Claim {}

#[cfg(windows)]
fn new_pipe(first: bool) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    let mode = if first { PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_INBOUND };
    let h = unsafe { CreateNamedPipeW(&HSTRING::from(pipe_name()), mode, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES, 0, 4096, 0, None) };
    (!h.is_invalid()).then_some(h)
}

/// Claims the pipe if no app of this user runs yet. None: one already runs.
#[cfg(windows)]
pub fn claim() -> Option<Claim> {
    new_pipe(true).map(Claim)
}

/// Sends `msg` to the running app, waiting up to a few seconds for it. True if it took it.
#[cfg(windows)]
pub fn send_to_running(msg: &str) -> bool {
    use std::io::Write;
    use windows::core::HSTRING;
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    let name = pipe_name();
    for _ in 0..10 {
        match std::fs::OpenOptions::new().write(true).open(&name) {
            Ok(mut f) => return f.write_all(msg.as_bytes()).is_ok(),
            // ERROR_PIPE_BUSY: every instance is taken right now; wait for one.
            Err(e) if e.raw_os_error() == Some(231) => unsafe {
                let _ = WaitNamedPipeW(&HSTRING::from(name.as_str()), 500);
            },
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
    false
}

/// Takes messages from later starts, each on `on_message`, for as long as the app runs.
#[cfg(windows)]
pub fn listen(claim: Claim, on_message: impl Fn(String) + Send + Sync + 'static) {
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::System::Pipes::ConnectNamedPipe;
    let on_message = std::sync::Arc::new(on_message);
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || {
            // The whole claim moves in (it is Send), not just its handle field.
            let claim = claim;
            let mut h = claim.0;
            loop {
                // Fails with ERROR_PIPE_CONNECTED when the client was quicker; that is fine.
                let _ = unsafe { ConnectNamedPipe(h, None) };
                // Read this one on its own thread and open the next instance at once, so a
                // second start never finds the pipe busy for long.
                let conn = h.0 as isize;
                let cb = on_message.clone();
                std::thread::spawn(move || {
                    let f = unsafe { std::fs::File::from_raw_handle(conn as *mut std::ffi::c_void) };
                    let mut msg = String::new();
                    let _ = f.take(4096).read_to_string(&mut msg);
                    if !msg.is_empty() {
                        cb(msg);
                    }
                });
                match new_pipe(false) {
                    Some(next) => h = next,
                    None => {
                        eprintln!("instance: cannot open the next pipe instance");
                        return;
                    }
                }
            }
        })
        .expect("instance thread");
}

#[cfg(not(windows))]
pub struct Claim;

#[cfg(not(windows))]
pub fn claim() -> Option<Claim> {
    Some(Claim)
}

#[cfg(not(windows))]
pub fn send_to_running(_msg: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub fn listen(_claim: Claim, _on_message: impl Fn(String) + Send + Sync + 'static) {}
