//! Windows presentation: a Win32 window, a D3D11 device shared with the decoder, and a
//! flip-discard swapchain with tearing. Frames arrive as D3D11 textures through appsink and
//! are copied into the back buffer on the GPU. Stage 0: adds ~0.1 ms over decode, where
//! d3d11videosink capped at 30 fps on the test machine.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::WindowsAndMessaging::*;

const GST_MAP_D3D11: u32 = gst::ffi::GST_MAP_FLAG_LAST << 1;

#[link(name = "gstd3d11-1.0")]
extern "C" {
    fn gst_d3d11_device_new_wrapped(device: *mut c_void) -> *mut c_void;
    fn gst_d3d11_context_new(device: *mut c_void) -> *mut gst::ffi::GstContext;
    fn gst_d3d11_device_lock(device: *mut c_void);
    fn gst_d3d11_device_unlock(device: *mut c_void);
    fn gst_is_d3d11_memory(mem: *mut gst::ffi::GstMemory) -> i32;
    fn gst_d3d11_memory_get_resource_handle(mem: *mut c_void) -> *mut c_void;
    fn gst_d3d11_memory_get_subresource_index(mem: *mut c_void) -> u32;
}

pub struct Renderer {
    /// Set on the viewer pipeline so the decoder allocates on our device.
    pub context: gst::Context,
    /// The appsink frames come from, and the capsfilter that sets their size.
    pub frames: mpsc::Sender<(gst_app::AppSink, gst::Element)>,
    pub closed: Arc<AtomicBool>,
    pub stats: Arc<RenderStats>,
    pub window: Window,
}

/// Commands for the video window, carried out on its own thread.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    hwnd: isize,
}

const WM_SHOW: u32 = WM_APP + 1;
/// wparam: 0 off, 1 on, 2 toggle.
const WM_FULLSCREEN: u32 = WM_APP + 2;
/// wparam: 0 off, 1 on.
const WM_TOPMOST: u32 = WM_APP + 3;
/// Apply the slot, popped-out and hidden state from WinState.
const WM_PLACE: u32 = WM_APP + 4;
/// Close for good (the watch ended). A user closing a popped-out window sends WM_CLOSE,
/// which puts it back into the app instead.
const WM_END: u32 = WM_APP + 5;

impl Window {
    fn post(&self, msg: u32, w: usize) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut c_void)), msg, WPARAM(w), LPARAM(0));
        }
    }

    /// Restores the window if minimized and brings it to the front.
    pub fn show(&self) {
        self.post(WM_SHOW, 0);
    }

    /// Borderless over the whole monitor the window is on, or back.
    pub fn set_fullscreen(&self, on: bool) {
        self.post(WM_FULLSCREEN, on as usize);
    }

    pub fn toggle_fullscreen(&self) {
        self.post(WM_FULLSCREEN, 2);
    }

    /// Keeps the window above all others.
    pub fn set_topmost(&self, on: bool) {
        self.post(WM_TOPMOST, on as usize);
    }

    /// Closes the window; its thread then ends.
    pub fn close(&self) {
        self.post(WM_END, 0);
    }

    fn update(&self, f: impl FnOnce(&mut WinState)) {
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&self.hwnd) {
            f(st);
        }
        self.post(WM_PLACE, 0);
    }

    /// Where the video goes inside the app's main window, in its client pixels; None
    /// hides it (no slot on screen).
    pub fn place(&self, rect: Option<(i32, i32, i32, i32)>) {
        self.update(|st| st.slot = rect.map(|(x, y, w, h)| RECT { left: x, top: y, right: x + w, bottom: y + h }));
    }

    /// Takes the video out into a window of its own, or puts it back into the app.
    pub fn set_popped(&self, popped: bool) {
        self.update(|st| st.popped = popped);
    }

    /// Hides embedded video while the app shows something over it (a dialog): the
    /// webview cannot draw above a child window.
    pub fn set_hidden(&self, hidden: bool) {
        self.update(|st| st.hidden = hidden);
    }

    /// Whether the window still exists.
    /// Places the video where it already is (tests: the UI's `place` calls).
    pub fn place_again(&self) {
        self.update(|_| ());
    }

    pub fn set_audio_handler(&self, handler: AudioHandler) {
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&self.hwnd) {
            st.audio = Some(handler);
        }
    }

    pub fn alive(&self) -> bool {
        WINDOWS.lock().unwrap().contains_key(&self.hwnd)
    }

    pub fn set_title(&self, title: &str) {
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = SetWindowTextW(HWND(self.hwnd as *mut c_void), windows::core::PCWSTR(wide.as_ptr()));
        }
    }

    pub fn state(&self) -> WindowState {
        WINDOWS
            .lock()
            .unwrap()
            .get(&self.hwnd)
            .map(|w| WindowState {
                fullscreen: w.fullscreen.is_some(),
                topmost: w.topmost,
                overlay: w.overlay,
                embedded: w.parent != 0 && !w.popped,
            })
            .unwrap_or_default()
    }

    /// Shows or hides the diagnostic overlay (F3 in the window does the same).
    pub fn set_overlay(&self, on: bool) {
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&self.hwnd) {
            st.overlay = on;
        }
    }

    /// A line shown in the middle of the window until the first frame ("Connecting...");
    /// empty clears it.
    pub fn set_status(&self, text: &str) {
        let changed = match WINDOWS.lock().unwrap().get_mut(&self.hwnd) {
            Some(st) if st.status != text => {
                st.status = text.to_string();
                true
            }
            _ => false,
        };
        if changed {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(HWND(self.hwnd as *mut c_void)), None, true);
            }
        }
    }

    /// The overlay's text; drawn while the overlay is on.
    pub fn set_overlay_text(&self, text: &str) {
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&self.hwnd) {
            if st.overlay_text != text {
                st.overlay_text = text.to_string();
                st.overlay_version += 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct WindowState {
    pub fullscreen: bool,
    pub topmost: bool,
    pub overlay: bool,
    /// Shown inside the app's main window (not popped out).
    pub embedded: bool,
}

/// Per-window state, by HWND: an app may show several streams at once.
struct WinState {
    closed: Arc<AtomicBool>,
    /// Placement and style before fullscreen, while fullscreen.
    fullscreen: Option<(WINDOWPLACEMENT, i32)>,
    topmost: bool,
    overlay: bool,
    overlay_text: String,
    overlay_version: u64,
    /// Shown before the first frame; see `Window::set_status`.
    status: String,
    /// The app's main window when the video lives inside it, else 0 (own window).
    parent: isize,
    /// Its place in the parent's client area; None: not on screen.
    slot: Option<RECT>,
    /// Out in a window of its own, by the user's choice.
    popped: bool,
    /// Hidden for a dialog.
    hidden: bool,
    /// Fullscreen was entered from inside the main window: go back there on exit.
    fs_from_embedded: bool,
    /// Sets and reads the stream's sound for the controls drawn over the video.
    audio: Option<AudioHandler>,
    /// The controls show until then (the mouse moved).
    controls_until: Option<std::time::Instant>,
    /// The volume bar is being dragged.
    dragging: bool,
}

/// Sets mute and/or volume (0..1) when given and returns both as they now are.
pub type AudioHandler = Arc<dyn Fn(Option<bool>, Option<f32>) -> (bool, f32) + Send + Sync>;

/// How long the controls stay after the mouse stops.
const CONTROLS_FOR: std::time::Duration = std::time::Duration::from_millis(2500);

unsafe fn hide_controls(hwnd: HWND) {
    if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
        st.controls_until = None;
    }
}

/// The controls' look while they show.
fn controls_of(hwnd: HWND) -> Option<crate::controls::Look> {
    let (audio, fullscreen) = {
        let map = WINDOWS.lock().unwrap();
        let st = map.get(&(hwnd.0 as isize))?;
        if !st.dragging && st.controls_until.is_none_or(|t| t < std::time::Instant::now()) {
            return None;
        }
        (st.audio.clone()?, st.fullscreen.is_some())
    };
    let (muted, volume) = audio(None, None);
    Some(crate::controls::Look { muted, volume, fullscreen })
}

/// The client size and DPI scale of a window.
unsafe fn client_and_scale(hwnd: HWND) -> ((u32, u32), f32) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    (((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32), GetDpiForWindow(hwnd).max(96) as f32 / 96.0)
}

/// Sets the hand cursor when the pointer is on a control (mute, volume bar, fullscreen) of
/// a window that has them. True when it did.
unsafe fn controls_cursor(hwnd: HWND) -> bool {
    use crate::controls::Hit;
    let has = WINDOWS.lock().unwrap().get(&(hwnd.0 as isize)).is_some_and(|st| st.audio.is_some());
    if !has {
        return false;
    }
    let mut p = windows::Win32::Foundation::POINT::default();
    if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p).is_err() || !windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p).as_bool() {
        return false;
    }
    let (size, scale) = client_and_scale(hwnd);
    match crate::controls::hit(size, scale, p.x, p.y) {
        Hit::Mute | Hit::Bar(_) | Hit::Fullscreen => {
            if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                SetCursor(Some(c));
            }
            true
        }
        _ => false,
    }
}

/// STREAMER_DEBUG_CONTROLS_TIMING: when the last input to the controls came, to report how
/// long it took to show (`shown_input`).
static INPUT_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn controls_timing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STREAMER_DEBUG_CONTROLS_TIMING").is_some())
}

fn mark_input() {
    if controls_timing() {
        INPUT_AT.lock().unwrap().get_or_insert_with(std::time::Instant::now);
    }
}

/// A frame with a changed panel was just presented: how long since the input.
fn shown_input() {
    if controls_timing() {
        if let Some(t) = INPUT_AT.lock().unwrap().take() {
            eprintln!("controls: input to screen {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// Mouse input for the controls. True when it was theirs.
unsafe fn controls_mouse(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> bool {
    use crate::controls::Hit;
    use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
    let (x, y) = ((l.0 & 0xFFFF) as i16 as i32, ((l.0 >> 16) & 0xFFFF) as i16 as i32);
    let (audio, showing, dragging) = {
        let mut map = WINDOWS.lock().unwrap();
        let Some(st) = map.get_mut(&(hwnd.0 as isize)) else { return false };
        let showing = st.controls_until.is_some_and(|t| t > std::time::Instant::now());
        if msg == WM_MOUSEMOVE || msg == WM_MOUSEWHEEL {
            st.controls_until = Some(std::time::Instant::now() + CONTROLS_FOR);
        }
        if msg == WM_LBUTTONUP {
            st.dragging = false;
        }
        (st.audio.clone(), showing, st.dragging)
    };
    let Some(audio) = audio else { return false };
    let (size, scale) = client_and_scale(hwnd);
    match msg {
        WM_MOUSEWHEEL => {
            mark_input();
            let notches = ((w.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
            let (_, v) = audio(None, None);
            audio(Some(false), Some(v + notches * 0.05));
            true
        }
        WM_MOUSEMOVE if dragging => {
            audio(Some(false), Some(crate::controls::bar_level(size, scale, x)));
            true
        }
        WM_LBUTTONUP if dragging => {
            let _ = ReleaseCapture();
            true
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if showing => match crate::controls::hit(size, scale, x, y) {
            Hit::Mute => {
                let (m, _) = audio(None, None);
                audio(Some(!m), None);
                true
            }
            Hit::Bar(level) => {
                audio(Some(false), Some(level));
                if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
                    st.dragging = true;
                }
                SetCapture(hwnd);
                true
            }
            Hit::Fullscreen => {
                fullscreen(hwnd, 2);
                true
            }
            Hit::Panel => true,
            Hit::Outside => false,
        },
        _ => false,
    }
}

/// The overlay text and its version while the overlay is on.
fn overlay_of(hwnd: HWND) -> Option<(u64, String)> {
    let map = WINDOWS.lock().unwrap();
    let st = map.get(&(hwnd.0 as isize))?;
    (st.overlay && !st.overlay_text.is_empty()).then(|| (st.overlay_version, st.overlay_text.clone()))
}

/// The app's main window: new video windows are created inside it. 0: each video gets a
/// window of its own (the command-line viewer).
static DEFAULT_PARENT: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_default_parent(hwnd: isize) {
    DEFAULT_PARENT.store(hwnd, Ordering::SeqCst);
}

pub fn default_parent() -> isize {
    DEFAULT_PARENT.load(Ordering::SeqCst)
}

/// Puts the window where WinState says: a child of the app's main window at its slot,
/// or a window of its own. Never holds the state lock while calling into Win32, since
/// those calls can send messages back to this window.
unsafe fn apply_place(hwnd: HWND) {
    let (parent, slot, popped, hidden, fs) = {
        let map = WINDOWS.lock().unwrap();
        let Some(st) = map.get(&(hwnd.0 as isize)) else { return };
        (st.parent, st.slot, st.popped, st.hidden, st.fullscreen.is_some())
    };
    if parent == 0 || fs {
        return;
    }
    let parent_hwnd = HWND(parent as *mut c_void);
    let is_child = GetParent(hwnd).map(|p| p == parent_hwnd).unwrap_or(false);
    if std::env::var_os("STREAMER_DEBUG_PLACE").is_some() {
        eprintln!("place: popped {popped} hidden {hidden} child {is_child} slot {:?}", slot.map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top)));
    }
    if !popped {
        if !is_child {
            SetWindowLongW(hwnd, GWL_STYLE, (WS_CHILD | WS_CLIPSIBLINGS).0 as i32);
            SetWindowLongW(hwnd, GWL_EXSTYLE, WS_EX_NOPARENTNOTIFY.0 as i32);
            let _ = SetParent(hwnd, Some(parent_hwnd));
        }
        match slot {
            Some(r) if !hidden && r.right > r.left && r.bottom > r.top => {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            _ => {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    } else if is_child || GetWindowLongW(hwnd, GWL_STYLE) & WS_POPUP.0 as i32 != 0 {
        // Out into its own window, near the main window, 1280x720 of picture (also when
        // it was popped out while fullscreen from inside the app).
        let _ = SetParent(hwnd, None);
        SetWindowLongW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as i32);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent_hwnd, &mut pr);
        let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(parent_hwnd).max(96) as i32;
        let mut r = RECT { left: 0, top: 0, right: 1280 * scale / 96, bottom: 720 * scale / 96 };
        let _ = AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            pr.left + 48,
            pr.top + 48,
            r.right - r.left,
            r.bottom - r.top,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

static WINDOWS: std::sync::LazyLock<Mutex<HashMap<isize, WinState>>> = std::sync::LazyLock::new(Default::default);

/// Virtual-key codes, without the keyboard feature of the windows crate.
const KEY_ESCAPE: usize = 0x1B;
const KEY_F11: usize = 0x7A;
const KEY_F3: usize = 0x72;

/// Black, with the status line in the middle while there is one. Once frames come, the
/// swapchain covers the window and the status is empty.
unsafe fn paint_status(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::*;
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    FillRect(dc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
    let status = WINDOWS.lock().unwrap().get(&(hwnd.0 as isize)).map(|s| s.status.clone()).unwrap_or_default();
    if !status.is_empty() {
        let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
        let font = CreateFontW(-(18 * scale / 96), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));
        let old = SelectObject(dc, font.into());
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00DCDCDC));
        SetBkMode(dc, TRANSPARENT);
        let mut wide: Vec<u16> = status.encode_utf16().collect();
        // Measure the wrapped text, then centre it vertically.
        let pad = 24 * scale / 96;
        let mut box_ = RECT { left: rc.left + pad, top: 0, right: rc.right - pad, bottom: 0 };
        DrawTextW(dc, &mut wide, &mut box_, DT_CALCRECT | DT_WORDBREAK | DT_CENTER);
        let h = box_.bottom - box_.top;
        let mut place = RECT { left: rc.left + pad, top: (rc.bottom - h) / 2, right: rc.right - pad, bottom: (rc.bottom + h) / 2 };
        DrawTextW(dc, &mut wide, &mut place, DT_WORDBREAK | DT_CENTER);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
    }
    let _ = EndPaint(hwnd, &ps);
}

/// The fullscreen switch before the fix: holds the state lock across the Win32 calls.
/// STREAMER_DEBUG_FS_OLD selects it; the spoiled variant for the fullscreen gate.
unsafe fn fullscreen_old(hwnd: HWND, mode: usize) {
    let mut map = WINDOWS.lock().unwrap();
    let Some(st) = map.get_mut(&(hwnd.0 as isize)) else { return };
    let on = match mode {
        0 => false,
        1 => true,
        _ => st.fullscreen.is_none(),
    };
    if on == st.fullscreen.is_some() {
        return;
    }
    if on {
        let embedded = st.parent != 0 && !st.popped;
        st.fs_from_embedded = embedded;
        if embedded {
            let _ = SetParent(hwnd, None);
            SetWindowLongW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as i32);
        }
        let mut place = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut place);
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let r = mi.rcMonitor;
        SetWindowLongW(hwnd, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW.0 as i32)) | WS_POPUP.0 as i32);
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
        st.fullscreen = Some((place, style));
    } else if st.fs_from_embedded {
        st.fullscreen = None;
        st.fs_from_embedded = false;
        drop(map);
        apply_place(hwnd);
    } else if let Some((place, style)) = st.fullscreen.take() {
        SetWindowLongW(hwnd, GWL_STYLE, style);
        let _ = SetWindowPlacement(hwnd, &place);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER);
    }
}
/// Fullscreen on (1), off (0) or toggled (2). Like `apply_place`, never holds the state lock
/// while calling into Win32: taking an embedded video out of the main window sends messages
/// to the main window's thread, and that thread takes the lock for every `place` call. Held
/// across those calls, the lock froze both the app and the video (found going in and out
/// of fullscreen several times while watching).
unsafe fn fullscreen(hwnd: HWND, mode: usize) {
    if std::env::var_os("STREAMER_DEBUG_FS_OLD").is_some() {
        return fullscreen_old(hwnd, mode);
    }
    let (on, embedded) = {
        let map = WINDOWS.lock().unwrap();
        let Some(st) = map.get(&(hwnd.0 as isize)) else { return };
        let on = match mode {
            0 => false,
            1 => true,
            _ => st.fullscreen.is_none(),
        };
        if on == st.fullscreen.is_some() {
            return;
        }
        (on, st.parent != 0 && !st.popped)
    };
    if on {
        // Marked first, so a `place` arriving meanwhile leaves the window alone.
        let mark = |f: &mut WinState| {
            f.fullscreen = Some((WINDOWPLACEMENT::default(), 0));
            f.fs_from_embedded = embedded;
        };
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
            mark(st);
        }
        // The monitor it is on now, before leaving the main window: taken out, its position
        // relative to the main window turns into one on the primary monitor, and the video
        // went fullscreen there instead of where the app was (any other monitor).
        // STREAMER_DEBUG_FS_MONITOR_AFTER: asked afterwards, the spoiled variant for the gate.
        let monitor_after = std::env::var_os("STREAMER_DEBUG_FS_MONITOR_AFTER").is_some();
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !monitor_after {
            let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        }
        // From inside the main window: become a window of its own first.
        if embedded {
            SetWindowLongW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as i32);
            let _ = SetParent(hwnd, None);
        }
        let mut place = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut place);
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        if monitor_after {
            let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        }
        let r = mi.rcMonitor;
        SetWindowLongW(hwnd, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW.0 as i32)) | WS_POPUP.0 as i32);
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
        if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
            st.fullscreen = Some((place, style));
        }
    } else {
        let (from_embedded, saved) = {
            let mut map = WINDOWS.lock().unwrap();
            let Some(st) = map.get_mut(&(hwnd.0 as isize)) else { return };
            let from = std::mem::take(&mut st.fs_from_embedded);
            (from, st.fullscreen.take())
        };
        if from_embedded {
            // Back into its place in the main window (or a window of its own if it was
            // popped out meanwhile).
            apply_place(hwnd);
        } else if let Some((place, style)) = saved {
            SetWindowLongW(hwnd, GWL_STYLE, style);
            let _ = SetWindowPlacement(hwnd, &place);
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER);
            // Put back into the app meanwhile: go there now.
            apply_place(hwnd);
        }
    }
    hide_controls(hwnd);
}

#[derive(Default)]
pub struct RenderStats {
    pub presented: AtomicU64,
    /// Frames dropped because the swapchain had no free slot.
    pub skipped: AtomicU64,
    pub max_present_us: AtomicU64,
    /// Longest hold of the lock shared with the decoder.
    pub max_lock_us: AtomicU64,
}

struct Send_<T>(T);
unsafe impl<T> Send for Send_<T> {}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                // The user closed a popped-out video: it goes back into the app.
                let back = {
                    let mut map = WINDOWS.lock().unwrap();
                    match map.get_mut(&(hwnd.0 as isize)) {
                        Some(st) if st.parent != 0 => {
                            st.popped = false;
                            st.fullscreen = None;
                            st.fs_from_embedded = false;
                            true
                        }
                        _ => false,
                    }
                };
                if back {
                    apply_place(hwnd);
                    return LRESULT(0);
                }
                wndproc(hwnd, WM_END, w, l)
            }
            WM_END => {
                let map = WINDOWS.lock().unwrap();
                // STREAMER_DEBUG_SHARED_CLOSE: one window closing marks every window closed,
                // as the single process-wide flag did; the spoiled variant for the gate.
                if std::env::var_os("STREAMER_DEBUG_SHARED_CLOSE").is_some() {
                    map.values().for_each(|st| st.closed.store(true, Ordering::SeqCst));
                } else if let Some(st) = map.get(&(hwnd.0 as isize)) {
                    st.closed.store(true, Ordering::SeqCst);
                }
                drop(map);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                WINDOWS.lock().unwrap().remove(&(hwnd.0 as isize));
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_SHOW => {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
                LRESULT(0)
            }
            WM_FULLSCREEN => {
                fullscreen(hwnd, w.0);
                LRESULT(0)
            }
            WM_PLACE => {
                apply_place(hwnd);
                LRESULT(0)
            }
            // The hand over the panel's buttons and volume bar, as over the app's own buttons.
            WM_SETCURSOR if (l.0 & 0xFFFF) as u32 == HTCLIENT && controls_cursor(hwnd) => LRESULT(1),
            WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_LBUTTONUP if controls_mouse(hwnd, msg, w, l) => LRESULT(0),
            WM_MOUSEMOVE | WM_LBUTTONUP => DefWindowProcW(hwnd, msg, w, l),
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if controls_mouse(hwnd, msg, w, l) => LRESULT(0),
            WM_LBUTTONDOWN => {
                // Inside the main window, a click gives the video the keys (F11, Esc, F3).
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
                LRESULT(0)
            }
            WM_TOPMOST => {
                let on = w.0 != 0;
                if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
                    st.topmost = on;
                }
                let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
                let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == KEY_F11 => {
                fullscreen(hwnd, 2);
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == KEY_F3 => {
                if let Some(st) = WINDOWS.lock().unwrap().get_mut(&(hwnd.0 as isize)) {
                    st.overlay = !st.overlay;
                }
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == KEY_ESCAPE => {
                fullscreen(hwnd, 0);
                LRESULT(0)
            }
            WM_PAINT => {
                paint_status(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                fullscreen(hwnd, 2);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

pub fn start() -> Renderer {
    let closed = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let (frames_tx, frames_rx) = mpsc::channel::<(gst_app::AppSink, gst::Element)>();
    let stats = Arc::new(RenderStats::default());
    let (s, c) = (stats.clone(), closed.clone());
    std::thread::Builder::new()
        .name("present".into())
        .spawn(move || unsafe { run(ready_tx, frames_rx, s, c) })
        .unwrap();
    let Send_((context, hwnd)) = ready_rx.recv().expect("present thread failed to start");
    Renderer { context, frames: frames_tx, closed, stats, window: Window { hwnd } }
}

/// The adapter that drives a display, so presenting needs no cross-adapter copy.
unsafe fn display_adapter(factory: &IDXGIFactory6) -> IDXGIAdapter1 {
    let mut i = 0;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        if adapter.EnumOutputs(0).is_ok() {
            return adapter;
        }
        i += 1;
    }
    panic!("no adapter with a display output");
}

/// One BGRA texture seen from both devices, handed over with a keyed mutex:
/// key 0 = the decoder side may write, key 1 = the presentation side may read.
struct Shared {
    dec: ID3D11Texture2D,
    km_dec: IDXGIKeyedMutex,
    present: ID3D11Texture2D,
    km_present: IDXGIKeyedMutex,
}

impl Shared {
    unsafe fn new(dec_device: &ID3D11Device, present_device: &ID3D11Device, size: (u32, u32)) -> Self {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
        };
        let mut dec = None;
        dec_device.CreateTexture2D(&desc, None, Some(&mut dec)).unwrap();
        let dec: ID3D11Texture2D = dec.unwrap();
        let handle = dec.cast::<IDXGIResource>().unwrap().GetSharedHandle().unwrap();
        let mut present: Option<ID3D11Texture2D> = None;
        present_device.OpenSharedResource(handle, &mut present).unwrap();
        let present = present.unwrap();
        Shared {
            km_dec: dec.cast().unwrap(),
            km_present: present.cast().unwrap(),
            dec,
            present,
        }
    }
}

fn frame_size(sample: &gst::SampleRef) -> Option<(u32, u32)> {
    let s = sample.caps()?.structure(0)?;
    Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
}

unsafe fn run(
    ready: mpsc::Sender<Send_<(gst::Context, isize)>>,
    frames: mpsc::Receiver<(gst_app::AppSink, gst::Element)>,
    stats: Arc<RenderStats>,
    closed: Arc<AtomicBool>,
) {
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
    let adapter = display_adapter(&factory);
    let (mut device, mut context) = (None, None);
    D3D11CreateDevice(
        &adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        Default::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .unwrap();
    let device: ID3D11Device = device.unwrap();
    let context: ID3D11DeviceContext = context.unwrap();
    let gst_device = gst_d3d11_device_new_wrapped(device.as_raw());
    assert!(!gst_device.is_null(), "gst_d3d11_device_new_wrapped failed");
    let gst_context: gst::Context = gst::glib::translate::from_glib_full(gst_d3d11_context_new(gst_device));

    // A second device only for presentation. DWM throttles partly covered windows and
    // Present then blocks for whole refreshes while holding its device's lock; on a shared
    // device that stalled the decoder (stage 3: 30 fps, latency growing to tens of seconds).
    // Frames cross over through a shared texture guarded by a keyed mutex.
    let (mut pdevice, mut pcontext) = (None, None);
    D3D11CreateDevice(
        &adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        Default::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut pdevice),
        None,
        Some(&mut pcontext),
    )
    .unwrap();
    let pdevice: ID3D11Device = pdevice.unwrap();
    let pcontext: ID3D11DeviceContext = pcontext.unwrap();

    let mut allow = windows::core::BOOL(0);
    let tearing = factory
        .CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            &mut allow as *mut _ as *mut c_void,
            std::mem::size_of_val(&allow) as u32,
        )
        .is_ok()
        && allow.as_bool();

    let instance = GetModuleHandleW(None).unwrap();
    RegisterClassW(&WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("streamer_viewer"),
        // Double click toggles fullscreen.
        style: CS_DBLCLKS,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        // Black until the first frame; the default was white, which looked like a fault.
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH).0),
        ..Default::default()
    });
    // Inside the app's main window when it has one (hidden until the UI gives it a
    // place); else a window of its own.
    let parent = default_parent();
    let hwnd = if parent != 0 {
        // NOPARENTNOTIFY: creating and closing a child otherwise sends WM_PARENTNOTIFY to
        // the main window and waits for its thread; when that thread waits for us (a
        // command run on it), the app freezes.
        CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            w!("streamer_viewer"),
            w!("Stream"),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            16,
            9,
            Some(HWND(parent as *mut c_void)),
            None,
            Some(instance.into()),
            None,
        )
        .unwrap()
    } else {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("streamer_viewer"),
            w!("Stream"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            720,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap()
    };
    // STREAMER_OVERLAY=1: windows open with the overlay on (diagnosis and tests).
    let overlay_on = std::env::var_os("STREAMER_OVERLAY").is_some();
    WINDOWS.lock().unwrap().insert(
        hwnd.0 as isize,
        WinState {
            closed,
            fullscreen: None,
            topmost: false,
            overlay: overlay_on,
            overlay_text: String::new(),
            overlay_version: 0,
            status: String::new(),
            parent,
            slot: None,
            popped: false,
            hidden: false,
            fs_from_embedded: false,
            audio: None,
            controls_until: None,
            dragging: false,
        },
    );

    // Context calls from this thread and from GStreamer are then serialized by D3D11 itself,
    // so Present can run without holding GStreamer's device lock.
    if let Ok(mt) = context.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }

    ready.send(Send_((gst_context, hwnd.0 as isize))).unwrap();

    // Debug knobs for the isolation gate: a slow Present outside the lock must not reach the
    // decoder; the same delay inside the lock is the spoiled variant that must.
    let slow_ms: u64 = std::env::var("STREAMER_DEBUG_SLOW_PRESENT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let slow_in_lock = std::env::var_os("STREAMER_DEBUG_SLOW_IN_LOCK").is_some();

    let swap_flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
        | if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
    // DO_NOT_WAIT: DWM throttles partly covered windows and Present then blocks for whole
    // refreshes while D3D11 holds the device lock the decoder needs (stage 3: 33 ms per
    // frame, decoder at 30 fps, latency growing without bound). Busy means skip instead.
    let present_flags = DXGI_PRESENT_DO_NOT_WAIT | if tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
    let mut swapchain: Option<IDXGISwapChain1> = None;
    let mut shared: Option<Shared> = None;
    let mut waitable = windows::Win32::Foundation::HANDLE::default();
    let mut size = (0u32, 0u32);
    let mut sink: Option<gst_app::AppSink> = None;
    let mut fit: Option<gst::Element> = None;
    // Frames come at the window's client size, with black borders where the aspect ratios
    // differ. Stretching the stream's own frame to the window distorted 16:9 on a 21:9
    // monitor. A new size waits until it holds for a moment, so dragging the border does
    // not renegotiate on every pixel. STREAMER_DEBUG_NO_FIT keeps the stretch: the spoiled
    // variant.
    let no_fit = std::env::var_os("STREAMER_DEBUG_NO_FIT").is_some();
    let mut applied = (0u32, 0u32);
    let mut pending = ((0u32, 0u32), std::time::Instant::now());
    let mut overlay: Option<crate::overlay::Overlay> = None;
    let mut controls: Option<crate::controls::Controls> = None;
    let mut drawn_look: Option<crate::controls::Look> = None;
    // The last frame, kept while the controls show, to draw them again without a new frame.
    let mut last: Option<ID3D11Texture2D> = None;
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok((s, f)) = frames.try_recv() {
            sink = Some(s);
            fit = Some(f);
            applied = (0, 0);
        }
        if let (Some(f), false) = (&fit, no_fit) {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let want = ((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32);
            if want.0 > 0 && want.1 > 0 && want != applied {
                if want != pending.0 {
                    pending = (want, std::time::Instant::now());
                } else if applied == (0, 0) || pending.1.elapsed() > std::time::Duration::from_millis(120) {
                    let caps = gst::Caps::builder("video/x-raw")
                        .features(["memory:D3D11Memory"])
                        .field("format", "BGRA")
                        .field("width", want.0 as i32)
                        .field("height", want.1 as i32)
                        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                        .build();
                    gst::prelude::ObjectExt::set_property(f, "caps", &caps);
                    applied = want;
                }
            }
        }
        let Some(appsink) = &sink else {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        };
        // Latest frame wins: the appsink holds one buffer and drops older ones. While the
        // controls show, a short wait: the loop also takes the mouse and redraws them.
        // STREAMER_DEBUG_CONTROLS_WITH_FRAMES: the old way (20 ms, drawn only with a frame).
        let old_way = std::env::var_os("STREAMER_DEBUG_CONTROLS_WITH_FRAMES").is_some();
        let active = controls_of(hwnd).is_some() || drawn_look.is_some();
        let wait = if active && !old_way { 4 } else { 20 };
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(wait)) else {
            // No new frame, but the controls changed (a click, the volume, hiding): the last
            // frame again with them. Drawn only with frames, they lagged a frame or more (30
            // fps: p90 33 ms, max 59) and stood still while the stream did.
            let look = controls_of(hwnd);
            if !old_way && look != drawn_look {
                if let (Some(sc), Some(prev)) = (&swapchain, &last) {
                    let free = waitable.is_invalid() || windows::Win32::System::Threading::WaitForSingleObjectEx(waitable, 0, false) == windows::Win32::Foundation::WAIT_OBJECT_0;
                    if free {
                        let back: ID3D11Texture2D = sc.GetBuffer(0).unwrap();
                        pcontext.CopyResource(&back, prev);
                        if let (Some((version, text)), Some(o)) = (overlay_of(hwnd), overlay.as_mut()) {
                            o.draw(&pcontext, &back, size, version, &text);
                        }
                        if let (Some(l), Some(c)) = (look, controls.as_mut()) {
                            c.draw(&pcontext, &back, size, l);
                        }
                        drop(back);
                        if sc.Present(0, present_flags) != DXGI_ERROR_WAS_STILL_DRAWING {
                            if look.is_some() {
                                shown_input();
                            }
                            drawn_look = look;
                        }
                    }
                }
            }
            continue;
        };
        let (Some(buffer), Some(frame)) = (sample.buffer(), frame_size(&sample)) else { continue };

        if swapchain.is_none() || frame != size {
            gst_d3d11_device_lock(gst_device);
            shared = Some(Shared::new(&device, &pdevice, frame));
            gst_d3d11_device_unlock(gst_device);
            if let Some(sc) = &swapchain {
                let _ = sc.ResizeBuffers(0, frame.0, frame.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(swap_flags as i32));
            } else {
                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: frame.0,
                    Height: frame.1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    Stereo: false.into(),
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    // Several buffers, so DWM holding one never blocks Present (as in Moonlight).
                    BufferCount: 3,
                    // Frames already match the window; stretch only covers the moment
                    // between a resize and the next frame of the new size.
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: swap_flags,
                };
                let sc = factory.CreateSwapChainForHwnd(&pdevice, hwnd, &desc, None, None).unwrap();
                // A frame queue slot is free when this handle is signalled; checking it with a
                // zero timeout lets us skip a frame instead of blocking inside Present.
                if let Ok(sc2) = sc.cast::<IDXGISwapChain2>() {
                    let _ = sc2.SetMaximumFrameLatency(2);
                    waitable = sc2.GetFrameLatencyWaitableObject();
                }
                swapchain = Some(sc);
                let mut r = RECT { left: 0, top: 0, right: frame.0 as i32, bottom: frame.1 as i32 };
                if parent == 0 && AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false).is_ok() {
                    let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
                }
            }
            size = frame;
        }
        let sc = swapchain.as_ref().unwrap();
        let sh = shared.as_ref().unwrap();

        if !waitable.is_invalid()
            && windows::Win32::System::Threading::WaitForSingleObjectEx(waitable, 0, false)
                != windows::Win32::Foundation::WAIT_OBJECT_0
        {
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let mem = buffer.peek_memory(0).as_mut_ptr();
        if gst_is_d3d11_memory(mem) == 0 {
            continue;
        }
        let mut info = std::mem::MaybeUninit::<gst::ffi::GstMapInfo>::zeroed();
        // The D3D11 map waits for the decoder's pending GPU work on this texture.
        if gst::ffi::gst_memory_map(mem, info.as_mut_ptr(), gst::ffi::GST_MAP_READ | GST_MAP_D3D11) == 0 {
            continue;
        }
        let raw = gst_d3d11_memory_get_resource_handle(mem as *mut c_void);
        let sub = gst_d3d11_memory_get_subresource_index(mem as *mut c_void);
        let src = ID3D11Resource::from_raw_borrowed(&raw).unwrap();
        let src_box = D3D11_BOX { left: 0, top: 0, front: 0, right: size.0, bottom: size.1, back: 1 };
        // On the decoder's device only the copy into the shared texture happens, under
        // GStreamer's lock (stage 1: holding it during Present grew latency without bound).
        let locked = std::time::Instant::now();
        gst_d3d11_device_lock(gst_device);
        let _ = sh.km_dec.AcquireSync(0, 100);
        context.CopySubresourceRegion(&sh.dec, 0, 0, 0, 0, src, sub, Some(&src_box));
        let _ = sh.km_dec.ReleaseSync(1);
        if slow_in_lock && slow_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(slow_ms));
        }
        gst_d3d11_device_unlock(gst_device);
        stats.max_lock_us.fetch_max(locked.elapsed().as_micros() as u64, Ordering::Relaxed);
        gst::ffi::gst_memory_unmap(mem, info.as_mut_ptr());

        // Everything below runs on the presentation device and cannot stall the decoder.
        let back: ID3D11Texture2D = sc.GetBuffer(0).unwrap();
        let _ = sh.km_present.AcquireSync(1, 100);
        pcontext.CopyResource(&back, &sh.present);
        // Kept for redrawing the controls between frames (only while they show).
        if controls_of(hwnd).is_some() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            sh.present.GetDesc(&mut d);
            if last.as_ref().is_none_or(|t| {
                let mut ld = D3D11_TEXTURE2D_DESC::default();
                t.GetDesc(&mut ld);
                (ld.Width, ld.Height) != (d.Width, d.Height)
            }) {
                d.MiscFlags = 0;
                let mut t = None;
                last = pdevice.CreateTexture2D(&d, None, Some(&mut t)).ok().and(t);
            }
            if let Some(t) = &last {
                pcontext.CopyResource(t, &sh.present);
            }
        }
        let _ = sh.km_present.ReleaseSync(0);
        if let Some((version, text)) = overlay_of(hwnd) {
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            if overlay.as_ref().is_some_and(|o| o.scale() != scale) {
                overlay = None;
            }
            if overlay.is_none() {
                overlay = crate::overlay::Overlay::new(&pdevice, scale).map_err(|e| eprintln!("overlay: {e}")).ok();
            }
            if let Some(o) = overlay.as_mut() {
                o.draw(&pcontext, &back, size, version, &text);
            }
        }
        let frame_look = controls_of(hwnd);
        if let Some(look) = frame_look {
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            if controls.as_ref().is_some_and(|c| c.scale() != scale) {
                controls = None;
            }
            if controls.is_none() {
                controls = crate::controls::Controls::new(&pdevice, scale).map_err(|e| eprintln!("controls: {e}")).ok();
            }
            if let Some(c) = controls.as_mut() {
                c.draw(&pcontext, &back, size, look);
            }
        }
        drop(back);

        let presenting = std::time::Instant::now();
        let hr = sc.Present(0, present_flags);
        if hr != DXGI_ERROR_WAS_STILL_DRAWING && frame_look != drawn_look {
            if frame_look.is_some() {
                shown_input();
            }
            drawn_look = frame_look;
        }
        if !slow_in_lock && slow_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(slow_ms));
        }
        stats.max_present_us.fetch_max(presenting.elapsed().as_micros() as u64, Ordering::Relaxed);
        if hr == DXGI_ERROR_WAS_STILL_DRAWING {
            stats.skipped.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.presented.fetch_add(1, Ordering::Relaxed);
        }
    }
}
