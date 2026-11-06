//! Diagnostic overlay for the video window. The text is drawn with Direct2D into a small
//! texture of its own, only when it changes (about once a second); every frame then gets
//! a GPU copy of that texture into the corner of the back buffer. All of it runs on the
//! presentation device, which the decoder never waits for.

use windows::core::{w, Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

/// The largest box, in pixels at 100% scale; the texture is made this big times the scale.
const MAX_W: f32 = 520.0;
const MAX_H: f32 = 140.0;
const PAD: f32 = 8.0;
/// Distance from the window's corner, at 100% scale.
const MARGIN: f32 = 12.0;

pub struct Overlay {
    dc: ID2D1DeviceContext,
    tex: ID3D11Texture2D,
    dwrite: IDWriteFactory,
    format: IDWriteTextFormat,
    text: ID2D1SolidColorBrush,
    scale: f32,
    /// Version of the text now in the texture, and the size of the box drawn.
    drawn: u64,
    box_size: (u32, u32),
}

impl Overlay {
    pub unsafe fn new(device: &ID3D11Device, scale: f32) -> Result<Self> {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
        let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: (MAX_W * scale).ceil() as u32,
            Height: (MAX_H * scale).ceil() as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.unwrap();
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bitmap = dc.CreateBitmapFromDxgiSurface(&tex.cast::<IDXGISurface>()?, Some(&props))?;
        dc.SetTarget(&bitmap);
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let format = dwrite.CreateTextFormat(
            w!("Consolas"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            14.0 * scale,
            w!("en-us"),
        )?;
        let text = dc.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.95, g: 0.96, b: 0.97, a: 1.0 }, None)?;
        Ok(Self { dc, tex, dwrite, format, text, scale, drawn: u64::MAX, box_size: (0, 0) })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Redraws the texture when `version` differs from the one drawn.
    unsafe fn redraw(&mut self, version: u64, text: &str) -> Result<()> {
        if version == self.drawn {
            return Ok(());
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let s = self.scale;
        let layout = self.dwrite.CreateTextLayout(&wide, &self.format, (MAX_W - 2.0 * PAD) * s, (MAX_H - 2.0 * PAD) * s)?;
        let mut m = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut m)?;
        let bw = (m.widthIncludingTrailingWhitespace + 2.0 * PAD * s).min(MAX_W * s).ceil();
        let bh = (m.height + 2.0 * PAD * s).min(MAX_H * s).ceil();
        self.dc.BeginDraw();
        // Opaque: the box is copied over the video, there is no blending.
        self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.07, g: 0.08, b: 0.09, a: 1.0 }));
        self.dc.DrawTextLayout(
            windows_numerics::Vector2 { X: PAD * s, Y: PAD * s },
            &layout,
            &self.text,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
        self.dc.EndDraw(None, None)?;
        self.drawn = version;
        self.box_size = (bw as u32, bh as u32);
        Ok(())
    }

    /// Puts the box into the top left corner of `back` (`size` pixels).
    pub unsafe fn draw(&mut self, context: &ID3D11DeviceContext, back: &ID3D11Texture2D, size: (u32, u32), version: u64, text: &str) {
        if self.redraw(version, text).is_err() {
            return;
        }
        let m = (MARGIN * self.scale) as u32;
        let w = self.box_size.0.min(size.0.saturating_sub(m));
        let h = self.box_size.1.min(size.1.saturating_sub(m));
        if w == 0 || h == 0 {
            return;
        }
        let src = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        context.CopySubresourceRegion(back, 0, m, m, 0, &self.tex, 0, Some(&src));
    }
}
