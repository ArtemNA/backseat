//! While the window's size changes, the frames still come at the old size until the decoder's
//! output is fitted again (it waits until the size holds, so dragging a border does not
//! renegotiate on every pixel). Stretched to the window by the swapchain they were distorted
//! and then jumped into place. Instead the swapchain takes the window's size at once and the
//! video part of each old-size frame is drawn into it with the stream's aspect, where the
//! fitted frame will put it.

use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

pub struct Scaler {
    dc: ID2D1DeviceContext,
}

/// The largest rectangle of `aspect` (width / height) centred in `w`×`h`.
pub fn fit(aspect: f32, w: f32, h: f32) -> D2D_RECT_F {
    let (fw, fh) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
    let (l, t) = ((w - fw) / 2.0, (h - fh) / 2.0);
    D2D_RECT_F { left: l, top: t, right: l + fw, bottom: t + fh }
}

impl Scaler {
    pub unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
        let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        Ok(Self { dc })
    }

    /// `src` (`from` pixels, the video `aspect` fitted inside it with black around) into
    /// `back` (`size` pixels): black, and the video part where a frame of `size` has it.
    pub unsafe fn draw(&mut self, back: &ID3D11Texture2D, size: (u32, u32), src: &ID3D11Texture2D, from: (u32, u32), aspect: f32) -> Result<()> {
        let pf = D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE };
        let target = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: pf,
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let source = D2D1_BITMAP_PROPERTIES1 { pixelFormat: pf, dpiX: 96.0, dpiY: 96.0, bitmapOptions: D2D1_BITMAP_OPTIONS_NONE, colorContext: std::mem::ManuallyDrop::new(None) };
        let out = self.dc.CreateBitmapFromDxgiSurface(&back.cast::<IDXGISurface>()?, Some(&target))?;
        let input = self.dc.CreateBitmapFromDxgiSurface(&src.cast::<IDXGISurface>()?, Some(&source))?;
        let from_rect = fit(aspect, from.0 as f32, from.1 as f32);
        let to_rect = fit(aspect, size.0 as f32, size.1 as f32);
        self.dc.SetTarget(&out);
        self.dc.BeginDraw();
        self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
        self.dc.DrawBitmap(&input, Some(&to_rect), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, Some(&from_rect), None);
        let r = self.dc.EndDraw(None, None);
        self.dc.SetTarget(None);
        r
    }
}
