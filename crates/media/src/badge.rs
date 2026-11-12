//! The stream badge in the video's top right corner: a dark see-through pill with a status
//! dot (green: the frames come as they should; amber: fewer, or packets lost) and one line,
//! "1080p60 · 12.4 Mbit/s · AV1". Drawn with Direct2D straight onto the back buffer, so it
//! blends over the picture; the text layout is rebuilt only when the text changes.

use windows::core::{w, Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};

/// Separator between the parts; drawn dimmer than the values.
pub const SEP: &str = " · ";

const MARGIN: f32 = 10.0;
const PAD_L: f32 = 8.0;
const PAD_R: f32 = 10.0;
const PAD_Y: f32 = 5.0;
const DOT: f32 = 8.0;
const GAP: f32 = 7.0;

fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a }
}

pub struct Badge {
    dc: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    format: IDWriteTextFormat,
    brush: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    scale: f32,
    /// The text the layout was made for, the layout and its size.
    laid: Option<(String, IDWriteTextLayout, f32, f32)>,
}

impl Badge {
    pub unsafe fn new(device: &ID3D11Device, scale: f32) -> Result<Self> {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
        let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let format = dwrite.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 12.0 * scale, w!("en-us"))?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let brush = dc.CreateSolidColorBrush(&rgba(0xf2, 0xf3, 0xf5, 1.0), None)?;
        let dim = dc.CreateSolidColorBrush(&rgba(0x80, 0x84, 0x8e, 1.0), None)?;
        Ok(Self { dc, dwrite, format, brush, dim, scale, laid: None })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    unsafe fn layout(&mut self, text: &str) -> Result<(IDWriteTextLayout, f32, f32)> {
        if let Some((t, l, w, h)) = &self.laid {
            if t == text {
                return Ok((l.clone(), *w, *h));
            }
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let layout = self.dwrite.CreateTextLayout(&wide, &self.format, 4000.0, 400.0)?;
        // The separators dimmer.
        let dim: windows::core::IUnknown = self.dim.cast()?;
        let sep: Vec<u16> = SEP.encode_utf16().collect();
        let mut i = 0;
        while i + sep.len() <= wide.len() {
            if wide[i..i + sep.len()] == sep[..] {
                layout.SetDrawingEffect(&dim, DWRITE_TEXT_RANGE { startPosition: i as u32, length: sep.len() as u32 })?;
                i += sep.len();
            } else {
                i += 1;
            }
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut m)?;
        self.laid = Some((text.to_string(), layout.clone(), m.widthIncludingTrailingWhitespace, m.height));
        Ok((layout, m.widthIncludingTrailingWhitespace, m.height))
    }

    /// The badge in the top right corner of `back` (`size` pixels).
    pub unsafe fn draw(&mut self, back: &ID3D11Texture2D, size: (u32, u32), text: &str, ok: bool) -> Result<()> {
        let (layout, tw, th) = self.layout(text)?;
        let s = self.scale;
        let bw = PAD_L * s + DOT * s + GAP * s + tw + PAD_R * s;
        let bh = th + 2.0 * PAD_Y * s;
        let right = size.0 as f32 - MARGIN * s;
        let left = right - bw;
        let top = MARGIN * s;
        if left < MARGIN * s || top + bh > size.1 as f32 {
            return Ok(());
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bitmap = self.dc.CreateBitmapFromDxgiSurface(&back.cast::<IDXGISurface>()?, Some(&props))?;
        self.dc.SetTarget(&bitmap);
        self.dc.BeginDraw();
        let pill = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left, top, right, bottom: top + bh }, radiusX: bh / 2.0, radiusY: bh / 2.0 };
        let base = self.brush.GetColor();
        self.brush.SetColor(&rgba(0x11, 0x12, 0x14, 0.72));
        self.dc.FillRoundedRectangle(&pill, &self.brush);
        let (r, g, b) = if ok { (0x23, 0xa5, 0x5a) } else { (0xf0, 0xb2, 0x32) };
        let c = windows_numerics::Vector2 { X: left + PAD_L * s + DOT * s / 2.0, Y: top + bh / 2.0 };
        self.brush.SetColor(&rgba(r, g, b, 0.25));
        self.dc.FillEllipse(&D2D1_ELLIPSE { point: c, radiusX: DOT * s / 2.0 + 2.0 * s, radiusY: DOT * s / 2.0 + 2.0 * s }, &self.brush);
        self.brush.SetColor(&rgba(r, g, b, 1.0));
        self.dc.FillEllipse(&D2D1_ELLIPSE { point: c, radiusX: DOT * s / 2.0, radiusY: DOT * s / 2.0 }, &self.brush);
        self.brush.SetColor(&base);
        let at = windows_numerics::Vector2 { X: left + PAD_L * s + DOT * s + GAP * s, Y: top + PAD_Y * s };
        self.dc.DrawTextLayout(at, &layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        let r = self.dc.EndDraw(None, None);
        self.dc.SetTarget(None);
        r
    }
}
