//! Windows presentation: a Win32 window, a D3D11 device shared with the decoder, and a
//! flip-discard swapchain with tearing. Frames arrive as D3D11 textures through appsink and
//! are copied into the back buffer on the GPU. Stage 0: adds ~0.1 ms over decode, where
//! d3d11videosink capped at 30 fps on the test machine.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
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
    pub frames: mpsc::Sender<gst_app::AppSink>,
    pub closed: Arc<AtomicBool>,
    pub stats: Arc<RenderStats>,
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

static CLOSED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

struct Send_<T>(T);
unsafe impl<T> Send for Send_<T> {}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                if let Some(c) = CLOSED.get() {
                    c.store(true, Ordering::SeqCst);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

pub fn start() -> Renderer {
    let closed = CLOSED.get_or_init(|| Arc::new(AtomicBool::new(false))).clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (frames_tx, frames_rx) = mpsc::channel::<gst_app::AppSink>();
    let stats = Arc::new(RenderStats::default());
    let s = stats.clone();
    std::thread::Builder::new()
        .name("present".into())
        .spawn(move || unsafe { run(ready_tx, frames_rx, s) })
        .unwrap();
    let Send_(context) = ready_rx.recv().expect("present thread failed to start");
    Renderer { context, frames: frames_tx, closed, stats }
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

fn frame_size(sample: &gst::SampleRef) -> Option<(u32, u32)> {
    let s = sample.caps()?.structure(0)?;
    Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
}

unsafe fn run(
    ready: mpsc::Sender<Send_<gst::Context>>,
    frames: mpsc::Receiver<gst_app::AppSink>,
    stats: Arc<RenderStats>,
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

    // Context calls from this thread and from GStreamer are then serialized by D3D11 itself,
    // so Present can run without holding GStreamer's device lock.
    if let Ok(mt) = context.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }

    ready.send(Send_(gst_context)).unwrap();

    // Debug knobs for the isolation gate: a slow Present outside the lock must not reach the
    // decoder; the same delay inside the lock is the spoiled variant that must.
    let slow_ms: u64 = std::env::var("STREAMER_DEBUG_SLOW_PRESENT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let slow_in_lock = std::env::var_os("STREAMER_DEBUG_SLOW_IN_LOCK").is_some();

    let swap_flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
        | if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
    let present_flags = if tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
    let mut swapchain: Option<IDXGISwapChain1> = None;
    let mut waitable = windows::Win32::Foundation::HANDLE::default();
    let mut size = (0u32, 0u32);
    let mut sink: Option<gst_app::AppSink> = None;
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(s) = frames.try_recv() {
            sink = Some(s);
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
                    // The window may be any size; DWM scales the stream to it.
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: swap_flags,
                };
                let sc = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None).unwrap();
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
            gst_d3d11_device_unlock(gst_device);
            size = frame;
        }
        let sc = swapchain.as_ref().unwrap();

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
        let back: ID3D11Texture2D = sc.GetBuffer(0).unwrap();
        let src_box = D3D11_BOX { left: 0, top: 0, front: 0, right: size.0, bottom: size.1, back: 1 };
        // GStreamer's lock is held only for the copy, never for Present: a slow Present must
        // not stall the decoder (stage 1: stalls grew latency without bound).
        let locked = std::time::Instant::now();
        gst_d3d11_device_lock(gst_device);
        context.CopySubresourceRegion(&back, 0, 0, 0, 0, src, sub, Some(&src_box));
        if slow_in_lock && slow_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(slow_ms));
        }
        gst_d3d11_device_unlock(gst_device);
        stats.max_lock_us.fetch_max(locked.elapsed().as_micros() as u64, Ordering::Relaxed);
        gst::ffi::gst_memory_unmap(mem, info.as_mut_ptr());
        drop(back);

        let presenting = std::time::Instant::now();
        let _ = sc.Present(0, present_flags);
        if !slow_in_lock && slow_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(slow_ms));
        }
        stats.max_present_us.fetch_max(presenting.elapsed().as_micros() as u64, Ordering::Relaxed);
        stats.presented.fetch_add(1, Ordering::Relaxed);
    }
}
