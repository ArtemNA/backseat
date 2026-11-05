//! Own presentation path for the viewer: a Win32 window, a D3D11 device shared with the
//! GStreamer decoder, and a flip-discard swapchain with tearing. Frames arrive through
//! appsink as D3D11 textures and are copied into the back buffer on the GPU. No CPU copy.

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
const GST_MAP_D3D11: u32 = (gst::ffi::GST_MAP_FLAG_LAST as u32) << 1;

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

/// Called after each Present with the presented buffer's PTS.
pub type OnPresent = Arc<dyn Fn(Option<u64>) + Send + Sync>;

pub struct Renderer {
    /// Context to set on the viewer pipeline so the decoder uses our device.
    pub context: gst::Context,
    /// Hand the appsink to the render thread once the decode branch exists.
    pub sinks: mpsc::Sender<(gst_app::AppSink, OnPresent)>,
    pub tearing: bool,
}

struct Send_<T>(T);
unsafe impl<T> Send for Send_<T> {}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Picks the adapter that drives a display, so presentation needs no cross-adapter copy.
unsafe fn display_adapter(factory: &IDXGIFactory6) -> IDXGIAdapter1 {
    let mut i = 0;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        if adapter.EnumOutputs(0).is_ok() {
            let desc = adapter.GetDesc1().unwrap();
            let name = String::from_utf16_lossy(&desc.Description);
            println!("render adapter: {}", name.trim_end_matches('\0'));
            return adapter;
        }
        i += 1;
    }
    panic!("no adapter with a display output");
}

pub fn start() -> Renderer {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (sink_tx, sink_rx) = mpsc::channel::<(gst_app::AppSink, OnPresent)>();
    std::thread::spawn(move || unsafe { run(ready_tx, sink_rx) });
    let (ctx, tearing) = ready_rx.recv().expect("render thread failed to start");
    let Send_(ctx) = ctx;
    Renderer { context: ctx, sinks: sink_tx, tearing }
}

unsafe fn run(ready: mpsc::Sender<(Send_<gst::Context>, bool)>, sinks: mpsc::Receiver<(gst_app::AppSink, OnPresent)>) {
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
    let adapter = display_adapter(&factory);
    let mut device = None;
    let mut context = None;
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
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("streamer_probe"),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&class);
    let mut rect = RECT { left: 0, top: 0, right: WIDTH as i32, bottom: HEIGHT as i32 };
    AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false).unwrap();
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("streamer_probe"),
        w!("probe viewer"),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        40,
        40,
        rect.right - rect.left,
        rect.bottom - rect.top,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .unwrap();

    let flags = if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: WIDTH,
        Height: HEIGHT,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        // Moonlight uses several buffers so DWM holding one never blocks Present.
        BufferCount: 3,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: flags,
    };
    let swapchain = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None).unwrap();
    let present_flags = if tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };

    ready.send((Send_(gst_context), tearing)).unwrap();

    let mut active: Option<(gst_app::AppSink, OnPresent)> = None;
    let mut msg = MSG::default();
    let started = Instant::now();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(s) = sinks.try_recv() {
            active = Some(s);
        }
        let Some((sink, on_present)) = &active else {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        // Latest frame wins: the appsink keeps one buffer and drops older ones.
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) else {
            if sink.is_eos() || started.elapsed().as_secs() > 3600 {
                break;
            }
            continue;
        };
        let Some(buffer) = sample.buffer() else { continue };
        let mem = buffer.peek_memory(0);
        let mem_ptr = mem.as_mut_ptr();
        if gst_is_d3d11_memory(mem_ptr) == 0 {
            eprintln!("render: buffer is not D3D11 memory, skipping");
            continue;
        }
        let mut info = std::mem::MaybeUninit::<gst::ffi::GstMapInfo>::zeroed();
        // The D3D11 map waits for the decoder's pending GPU work on this texture.
        if gst::ffi::gst_memory_map(mem_ptr, info.as_mut_ptr(), gst::ffi::GST_MAP_READ | GST_MAP_D3D11) == 0 {
            eprintln!("render: map failed");
            continue;
        }
        let raw = gst_d3d11_memory_get_resource_handle(mem_ptr as *mut c_void);
        let sub = gst_d3d11_memory_get_subresource_index(mem_ptr as *mut c_void);
        let src = ID3D11Resource::from_raw_borrowed(&raw).unwrap();
        let back: ID3D11Texture2D = swapchain.GetBuffer(0).unwrap();
        let src_box = D3D11_BOX { left: 0, top: 0, front: 0, right: WIDTH, bottom: HEIGHT, back: 1 };
        gst_d3d11_device_lock(gst_device);
        context.CopySubresourceRegion(&back, 0, 0, 0, 0, src, sub, Some(&src_box));
        gst_d3d11_device_unlock(gst_device);
        gst::ffi::gst_memory_unmap(mem_ptr, info.as_mut_ptr());
        drop(back);

        gst_d3d11_device_lock(gst_device);
        let hr = swapchain.Present(0, present_flags);
        gst_d3d11_device_unlock(gst_device);
        if hr.is_err() {
            eprintln!("render: Present failed: {hr:?}");
        }
        on_present(buffer.pts().map(|p| p.nseconds()));
    }
    let _ = DestroyWindow(hwnd);
}
