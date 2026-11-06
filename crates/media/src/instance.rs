//! One running app per user. A second start (an invite link clicked while the app runs)
//! hands its message to the first through a named pipe and exits; the first gets it on a
//! callback. Windows only; elsewhere every start runs on its own.

/// The pipe of this user's running app.
#[cfg(windows)]
fn pipe_name() -> String {
    // Per user, so two users on one machine each have their own app.
    let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!(r"\\.\pipe\backseat-{user}")
}

/// Sends `msg` to an app that is already running. True if one took it.
#[cfg(windows)]
pub fn send_to_running(msg: &str) -> bool {
    use std::io::Write;
    for _ in 0..20 {
        match std::fs::OpenOptions::new().write(true).open(pipe_name()) {
            Ok(mut f) => return f.write_all(msg.as_bytes()).is_ok(),
            // ERROR_PIPE_BUSY: the running app is between two connections.
            Err(e) if e.raw_os_error() == Some(231) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

/// Takes messages from later starts, each on `on_message`, for as long as the app runs.
#[cfg(windows)]
pub fn listen(on_message: impl Fn(String) + Send + 'static) {
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};
    let name = HSTRING::from(pipe_name());
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || loop {
            let h = unsafe { CreateNamedPipeW(&name, PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 0, 4096, 0, None) };
            if h.is_invalid() {
                eprintln!("instance: cannot create the pipe");
                return;
            }
            // Fails with ERROR_PIPE_CONNECTED when the client was quicker; that is fine.
            let _ = unsafe { ConnectNamedPipe(h, None) };
            // Dropping the reader closes the pipe; the next loop makes a fresh one.
            let f = unsafe { std::fs::File::from_raw_handle(h.0) };
            let mut msg = String::new();
            let _ = f.take(4096).read_to_string(&mut msg);
            if !msg.is_empty() {
                on_message(msg);
            }
        })
        .expect("instance thread");
}

#[cfg(not(windows))]
pub fn send_to_running(_msg: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub fn listen(_on_message: impl Fn(String) + Send + 'static) {}
