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
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
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
        self.post(WM_CLOSE, 0);
    }

    /// Whether the window still exists.
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
        WINDOWS.lock().unwrap().get(&self.hwnd).map(|w| WindowState { fullscreen: w.fullscreen.is_some(), topmost: w.topmost }).unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct WindowState {
    pub fullscreen: bool,
    pub topmost: bool,
}

/// Per-window state, by HWND: an app may show several streams at once.
struct WinState {
    closed: Arc<AtomicBool>,
    /// Placement and style before fullscreen, while fullscreen.
    fullscreen: Option<(WINDOWPLACEMENT, i32)>,
    topmost: bool,
}

static WINDOWS: std::sync::LazyLock<Mutex<HashMap<isize, WinState>>> = std::sync::LazyLock::new(Default::default);

/// Virtual-key codes, without the keyboard feature of the windows crate.
const KEY_ESCAPE: usize = 0x1B;
const KEY_F11: usize = 0x7A;

unsafe fn fullscreen(hwnd: HWND, mode: usize) {
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
        let mut place = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut place);
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let r = mi.rcMonitor;
        SetWindowLongW(hwnd, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW.0 as i32)) | WS_POPUP.0 as i32);
        let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW);
        st.fullscreen = Some((place, style));
    } else if let Some((place, style)) = st.fullscreen.take() {
        SetWindowLongW(hwnd, GWL_STYLE, style);
        let _ = SetWindowPlacement(hwnd, &place);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER);
    }
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
            WM_KEYDOWN if w.0 == KEY_ESCAPE => {
                fullscreen(hwnd, 0);
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
        ..Default::default()
    });
    let hwnd = CreateWindowExW(
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
    .unwrap();
    WINDOWS.lock().unwrap().insert(hwnd.0 as isize, WinState { closed, fullscreen: None, topmost: false });

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
        // Latest frame wins: the appsink holds one buffer and drops older ones.
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(20)) else { continue };
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
                if AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false).is_ok() {
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
        let _ = sh.km_present.ReleaseSync(0);
        drop(back);

        let presenting = std::time::Instant::now();
        let hr = sc.Present(0, present_flags);
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
