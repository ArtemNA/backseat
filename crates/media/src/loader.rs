//! What the video window shows before its first frame: a ring of dots going round, and the
//! status line ("Connecting to …") under it. Drawn with Direct2D into the swapchain, as the
//! frames are: the app's main window is composed with DirectComposition, so GDI painting in
//! the embedded video window never showed (the window stayed black until the first frame).

use windows::core::{w, Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

pub struct Loader {
    dc: ID2D1DeviceContext,
    text: IDWriteTextFormat,
    brush: ID2D1SolidColorBrush,
    scale: f32,
}

impl Loader {
    pub unsafe fn new(device: &ID3D11Device, scale: f32) -> Result<Self> {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
        let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let text = dwrite.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 15.0 * scale, w!("en-us"))?;
        text.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        text.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        text.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
        let brush = dc.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, None)?;
        Ok(Self { dc, text, brush, scale })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Black, the ring at `step` (0..8, the lit dot), the status under it, into `back`.
    pub unsafe fn draw(&mut self, back: &ID3D11Texture2D, size: (u32, u32), step: u32, status: &str) -> Result<()> {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bitmap = self.dc.CreateBitmapFromDxgiSurface(&back.cast::<IDXGISurface>()?, Some(&props))?;
        self.dc.SetTarget(&bitmap);
        let s = self.scale;
        let (w, h) = (size.0 as f32, size.1 as f32);
        let lift = if status.is_empty() { 0.0 } else { 22.0 * s };
        let (cx, cy, ring, dot) = (w / 2.0, h / 2.0 - lift, 16.0 * s, 3.2 * s);
        self.dc.BeginDraw();
        self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
        for i in 0..8u32 {
            let a = i as f32 * std::f32::consts::TAU / 8.0 - std::f32::consts::FRAC_PI_2;
            let age = (step + 8 - i) % 8;
            let v = (0.92 - age as f32 * 0.1).max(0.22);
            self.brush.SetColor(&D2D1_COLOR_F { r: v, g: v, b: v, a: 1.0 });
            let e = D2D1_ELLIPSE { point: windows_numerics::Vector2 { X: cx + ring * a.cos(), Y: cy + ring * a.sin() }, radiusX: dot, radiusY: dot };
            self.dc.FillEllipse(&e, &self.brush);
        }
        if !status.is_empty() {
            self.brush.SetColor(&D2D1_COLOR_F { r: 0.86, g: 0.86, b: 0.86, a: 1.0 });
            let wide: Vec<u16> = status.encode_utf16().collect();
            let pad = 24.0 * s;
            let top = cy + ring + 18.0 * s;
            let rect = D2D_RECT_F { left: pad, top, right: (w - pad).max(pad + 1.0), bottom: h };
            self.dc.DrawText(&wide, &self.text, &rect, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
        let r = self.dc.EndDraw(None, None);
        self.dc.SetTarget(None);
        r
    }
}
