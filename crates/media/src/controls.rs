//! Sound and fullscreen controls drawn over the video when the mouse moves: in fullscreen
//! and in a popped-out window the app's own buttons are out of reach. Drawn like the
//! overlay: Direct2D into a texture of its own when something changes, then a GPU copy
//! into the bottom middle of each frame. The window procedure hit-tests the same layout.

use windows::core::{w, Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

/// Panel size and parts, in pixels at 100% scale.
const W: f32 = 340.0;
const H: f32 = 52.0;
const BOTTOM: f32 = 28.0;
const MUTE: (f32, f32) = (8.0, 52.0);
const BAR: (f32, f32) = (64.0, 236.0);
const PCT: (f32, f32) = (244.0, 292.0);
const FS: (f32, f32) = (294.0, 336.0);

/// What a point in the window is over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Mute,
    /// The volume bar, at this level (0..1).
    Bar(f32),
    Fullscreen,
    /// The panel, between its parts.
    Panel,
    Outside,
}

/// Where the panel is in a client area of `size` pixels: left, top, scale.
fn origin(size: (u32, u32), scale: f32) -> (f32, f32) {
    ((size.0 as f32 - W * scale) / 2.0, size.1 as f32 - (H + BOTTOM) * scale)
}

/// The part of the panel at (x, y) in a client area of `size` pixels.
pub fn hit(size: (u32, u32), scale: f32, x: i32, y: i32) -> Hit {
    let (l, t) = origin(size, scale);
    let (px, py) = ((x as f32 - l) / scale, (y as f32 - t) / scale);
    if px < 0.0 || py < 0.0 || px > W || py > H {
        return Hit::Outside;
    }
    if px < MUTE.1 {
        Hit::Mute
    } else if px >= BAR.0 - 6.0 && px <= BAR.1 + 6.0 {
        Hit::Bar(((px - BAR.0) / (BAR.1 - BAR.0)).clamp(0.0, 1.0))
    } else if px >= FS.0 {
        Hit::Fullscreen
    } else {
        Hit::Panel
    }
}

/// The bar's level for a point left or right of it, while dragging.
pub fn bar_level(size: (u32, u32), scale: f32, x: i32) -> f32 {
    let (l, _) = origin(size, scale);
    (((x as f32 - l) / scale - BAR.0) / (BAR.1 - BAR.0)).clamp(0.0, 1.0)
}

/// What the panel shows.
#[derive(Clone, Copy, PartialEq)]
pub struct Look {
    pub muted: bool,
    pub volume: f32,
    pub fullscreen: bool,
}

pub struct Controls {
    dc: ID2D1DeviceContext,
    tex: ID3D11Texture2D,
    icons: IDWriteTextFormat,
    text: IDWriteTextFormat,
    fg: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    scale: f32,
    drawn: Option<Look>,
}

impl Controls {
    pub unsafe fn new(device: &ID3D11Device, scale: f32) -> Result<Self> {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
        let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: (W * scale).ceil() as u32,
            Height: (H * scale).ceil() as u32,
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
        let format = |family, size: f32| -> Result<IDWriteTextFormat> {
            let f = dwrite.CreateTextFormat(family, None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size * scale, w!("en-us"))?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(f)
        };
        // Segoe MDL2 Assets ships with Windows 10 and 11.
        let icons = format(w!("Segoe MDL2 Assets"), 20.0)?;
        let text = format(w!("Segoe UI"), 14.0)?;
        let brush = |r, g, b| dc.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a: 1.0 }, None);
        Ok(Self {
            fg: brush(0.95, 0.96, 0.97)?,
            dim: brush(0.35, 0.37, 0.40)?,
            accent: brush(0.35, 0.40, 0.95)?,
            dc,
            tex,
            icons,
            text,
            scale,
            drawn: None,
        })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    unsafe fn redraw(&mut self, look: Look) -> Result<()> {
        if self.drawn == Some(look) {
            return Ok(());
        }
        let s = self.scale;
        let rect = |l: f32, t: f32, r: f32, b: f32| D2D_RECT_F { left: l * s, top: t * s, right: r * s, bottom: b * s };
        let put = |dc: &ID2D1DeviceContext, glyph: &str, fmt: &IDWriteTextFormat, (l, r): (f32, f32), brush: &ID2D1SolidColorBrush| {
            let wide: Vec<u16> = glyph.encode_utf16().collect();
            dc.DrawText(&wide, fmt, &rect(l, 0.0, r, H), brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        };
        self.dc.BeginDraw();
        // Opaque: copied over the video, no blending.
        self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.07, g: 0.08, b: 0.09, a: 1.0 }));
        // Volume E767, Mute E74F; FullScreen E740, BackToWindow E73F.
        put(&self.dc, if look.muted { "\u{E74F}" } else { "\u{E767}" }, &self.icons, MUTE, &self.fg);
        let mid = H / 2.0;
        self.dc.FillRectangle(&rect(BAR.0, mid - 3.0, BAR.1, mid + 3.0), &self.dim);
        let level = if look.muted { 0.0 } else { look.volume };
        let x = BAR.0 + (BAR.1 - BAR.0) * level;
        self.dc.FillRectangle(&rect(BAR.0, mid - 3.0, x, mid + 3.0), &self.accent);
        self.dc.FillRectangle(&rect(x - 3.0, mid - 9.0, x + 3.0, mid + 9.0), &self.fg);
        put(&self.dc, &format!("{}%", (level * 100.0).round()), &self.text, PCT, &self.fg);
        put(&self.dc, if look.fullscreen { "\u{E73F}" } else { "\u{E740}" }, &self.icons, FS, &self.fg);
        self.dc.EndDraw(None, None)?;
        self.drawn = Some(look);
        Ok(())
    }

    /// Puts the panel into the bottom middle of `back` (`size` pixels).
    pub unsafe fn draw(&mut self, context: &ID3D11DeviceContext, back: &ID3D11Texture2D, size: (u32, u32), look: Look) {
        if self.redraw(look).is_err() {
            return;
        }
        let (l, t) = origin(size, self.scale);
        let (w, h) = ((W * self.scale).ceil() as u32, (H * self.scale).ceil() as u32);
        if l < 0.0 || t < 0.0 || l as u32 + w > size.0 || t as u32 + h > size.1 {
            return;
        }
        let src = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        context.CopySubresourceRegion(back, 0, l as u32, t as u32, 0, &self.tex, 0, Some(&src));
    }
}
