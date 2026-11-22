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

    /// The part `crop` (in `src`'s pixels) of `src` over the whole of `back` (`size` pixels):
    /// the fullscreen zoom.
    pub unsafe fn draw_crop(&mut self, back: &ID3D11Texture2D, size: (u32, u32), src: &ID3D11Texture2D, crop: D2D_RECT_F) -> Result<()> {
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
        let to = D2D_RECT_F { left: 0.0, top: 0.0, right: size.0 as f32, bottom: size.1 as f32 };
        self.dc.SetTarget(&out);
        self.dc.BeginDraw();
        self.dc.DrawBitmap(&input, Some(&to), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, Some(&crop), None);
        let r = self.dc.EndDraw(None, None);
        self.dc.SetTarget(None);
        r
    }
}

/// The fullscreen zoom: how much (1 = none) and the view's top left corner, as shares of the
/// frame (0..1). The view is `1 / zoom` of the frame each way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zoom {
    pub z: f32,
    pub l: f32,
    pub t: f32,
}

impl Default for Zoom {
    fn default() -> Self {
        Self { z: 1.0, l: 0.0, t: 0.0 }
    }
}

/// At most this much zoom.
pub const MAX_ZOOM: f32 = 4.0;

impl Zoom {
    /// `notches` of the wheel (positive: in) at `at`, the cursor as shares of the window (0..1):
    /// the point under the cursor stays there. 1.25 times per notch, from 1 to `MAX_ZOOM`.
    pub fn wheel(self, notches: f32, at: (f32, f32)) -> Self {
        let z = (self.z * 1.25f32.powf(notches)).clamp(1.0, MAX_ZOOM);
        let (px, py) = (self.l + at.0 / self.z, self.t + at.1 / self.z);
        let span = 1.0 / z;
        Self { z, l: (px - at.0 * span).clamp(0.0, 1.0 - span), t: (py - at.1 * span).clamp(0.0, 1.0 - span) }
    }

    pub fn active(&self) -> bool {
        self.z > 1.0001
    }

    /// The view in a frame of `w`×`h` pixels.
    pub fn crop(&self, w: f32, h: f32) -> D2D_RECT_F {
        let span = 1.0 / self.z;
        D2D_RECT_F { left: self.l * w, top: self.t * h, right: (self.l + span) * w, bottom: (self.t + span) * h }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_stays_in_bounds_and_keeps_the_point_under_the_cursor() {
        let z = Zoom::default().wheel(1.0, (0.25, 0.75));
        assert!((z.z - 1.25).abs() < 1e-4);
        // The frame point under the cursor before (0.25, 0.75) is still under it.
        assert!((z.l + 0.25 / z.z - 0.25).abs() < 1e-4 && (z.t + 0.75 / z.z - 0.75).abs() < 1e-4);
        // Never past 4x, never out of the frame, back to 1 when wheeled out.
        let mut z = Zoom::default();
        for _ in 0..40 {
            z = z.wheel(1.0, (1.0, 1.0));
        }
        assert_eq!(z.z, MAX_ZOOM);
        let c = z.crop(1000.0, 500.0);
        assert!(c.left >= 0.0 && c.right <= 1000.0 + 1e-3 && c.top >= 0.0 && c.bottom <= 500.0 + 1e-3, "{c:?}");
        for _ in 0..40 {
            z = z.wheel(-1.0, (0.3, 0.3));
        }
        assert_eq!(z, Zoom { z: 1.0, l: 0.0, t: 0.0 });
        assert!(!z.active());
    }
}
