//! Small pictures for the source picker: a frame of each window or monitor and each window's
//! icon, as `data:` URLs the UI can show directly. GDI only, no encoder dependency: the
//! pictures are 24-bit BMP, base64.

/// A BMP `data:` URL from top-down BGR rows of `w`×`h` pixels, each row `stride` bytes.
pub fn bmp_data_url(w: u32, h: u32, bgr: &[u8], stride: usize) -> String {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let size = 54 + row * h as usize;
    let mut f = Vec::with_capacity(size);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&(size as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    // Negative height: rows are top-down.
    f.extend_from_slice(&(-(h as i32)).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&[0u8; 24]);
    for y in 0..h as usize {
        let src = &bgr[y * stride..y * stride + w as usize * 3];
        f.extend_from_slice(src);
        f.resize(f.len() + (row - w as usize * 3), 0);
    }
    format!("data:image/bmp;base64,{}", base64(&f))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A synthetic picture for test sources: colour bars, so test runs never show a real screen.
pub fn test_pattern(w: u32, h: u32) -> String {
    const BARS: [[u8; 3]; 7] = [[192, 192, 192], [0, 192, 192], [192, 192, 0], [0, 192, 0], [192, 0, 192], [0, 0, 192], [192, 0, 0]];
    let stride = w as usize * 3;
    let mut px = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let b = BARS[x * 7 / w as usize];
            px[y * stride + x * 3..y * stride + x * 3 + 3].copy_from_slice(&b);
        }
    }
    bmp_data_url(w, h, &px, stride)
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Reads a DIB section's pixels as top-down BGR.
    unsafe fn read(dc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<(Vec<u8>, usize)> {
        let stride = ((w * 3 + 3) / 4 * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 24;
        let n = GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        (n > 0).then_some((buf, stride))
    }

    /// Fits `w`×`h` into `max_w`×`max_h`, keeping the aspect ratio.
    fn fit(w: i32, h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
        let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
        (((w as f64 * s) as i32).max(1), ((h as f64 * s) as i32).max(1))
    }

    /// Scales `src` (a DC holding a `w`×`h` picture at `x`,`y`) into a new small picture.
    unsafe fn shrink(src: HDC, x: i32, y: i32, w: i32, h: i32, max_w: i32, max_h: i32) -> Option<String> {
        let (tw, th) = fit(w, h, max_w, max_h);
        let dc = CreateCompatibleDC(Some(src));
        let bmp = CreateCompatibleBitmap(src, tw, th);
        let old = SelectObject(dc, bmp.into());
        SetStretchBltMode(dc, HALFTONE);
        let ok = StretchBlt(dc, 0, 0, tw, th, Some(src), x, y, w, h, SRCCOPY).as_bool();
        SelectObject(dc, old);
        let px = if ok { read(dc, bmp, tw, th) } else { None };
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        px.map(|(p, stride)| super::bmp_data_url(tw as u32, th as u32, &p, stride))
    }

    pub fn window(hwnd: u64, max_w: i32, max_h: i32) -> Option<String> {
        unsafe {
            let h = HWND(hwnd as *mut _);
            if IsIconic(h).as_bool() {
                return None;
            }
            let mut r = RECT::default();
            GetWindowRect(h, &mut r).ok()?;
            let (w, ht) = (r.right - r.left, r.bottom - r.top);
            if w <= 0 || ht <= 0 {
                return None;
            }
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, ht);
            let old = SelectObject(dc, bmp.into());
            // PW_RENDERFULLCONTENT (2) also captures windows that draw with DirectX.
            let ok = PrintWindow(h, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
            let out = if ok { shrink(dc, 0, 0, w, ht, max_w, max_h) } else { None };
            SelectObject(dc, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            out
        }
    }

    pub fn monitor(handle: u64, max_w: i32, max_h: i32) -> Option<String> {
        unsafe {
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(HMONITOR(handle as *mut _), &mut info).as_bool() {
                return None;
            }
            let r = info.rcMonitor;
            let screen = GetDC(None);
            let out = shrink(screen, r.left, r.top, r.right - r.left, r.bottom - r.top, max_w, max_h);
            ReleaseDC(None, screen);
            out
        }
    }

    pub fn icon(hwnd: u64, size: i32) -> Option<String> {
        unsafe {
            let h = HWND(hwnd as *mut _);
            let mut ic: usize = 0;
            for which in [ICON_SMALL2 as usize, ICON_SMALL as usize, ICON_BIG as usize] {
                let mut res = 0usize;
                let _ = SendMessageTimeoutW(h, WM_GETICON, WPARAM(which), LPARAM(0), SMTO_ABORTIFHUNG, 50, Some(&mut res));
                if res != 0 {
                    ic = res;
                    break;
                }
            }
            if ic == 0 {
                ic = GetClassLongPtrW(h, GCLP_HICONSM);
            }
            if ic == 0 {
                ic = GetClassLongPtrW(h, GCLP_HICON);
            }
            if ic == 0 {
                return None;
            }
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, size, size);
            let old = SelectObject(dc, bmp.into());
            // The card's own background, as BMP has no transparency here.
            let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00312d2b));
            let rect = RECT { left: 0, top: 0, right: size, bottom: size };
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            let ok = DrawIconEx(dc, 0, 0, HICON(ic as *mut _), size, size, 0, None, DI_NORMAL).is_ok();
            SelectObject(dc, old);
            let px = if ok { read(dc, bmp, size, size) } else { None };
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            px.map(|(p, stride)| super::bmp_data_url(size as u32, size as u32, &p, stride))
        }
    }
}

/// A picture of a window, None if it is minimized or cannot be drawn.
pub fn window(hwnd: u64, max_w: u32, max_h: u32) -> Option<String> {
    #[cfg(windows)]
    return win::window(hwnd, max_w as i32, max_h as i32);
    #[cfg(not(windows))]
    {
        let _ = (hwnd, max_w, max_h);
        None
    }
}

/// A picture of a monitor's whole area.
pub fn monitor(handle: u64, max_w: u32, max_h: u32) -> Option<String> {
    #[cfg(windows)]
    return win::monitor(handle, max_w as i32, max_h as i32);
    #[cfg(not(windows))]
    {
        let _ = (handle, max_w, max_h);
        None
    }
}

/// The window's program icon.
pub fn icon(hwnd: u64, size: u32) -> Option<String> {
    #[cfg(windows)]
    return win::icon(hwnd, size as i32);
    #[cfg(not(windows))]
    {
        let _ = (hwnd, size);
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_known_values() {
        assert_eq!(super::base64(b"Man"), "TWFu");
        assert_eq!(super::base64(b"Ma"), "TWE=");
        assert_eq!(super::base64(b"M"), "TQ==");
    }

    #[test]
    fn bmp_has_right_size_and_header() {
        let url = super::test_pattern(5, 2);
        // "Qk" is base64 for the "BM" magic.
        assert!(url.starts_with("data:image/bmp;base64,Qk"));
        // 54-byte header + 2 rows of 15 bytes padded to 16.
        let len = (54 + 32usize).div_ceil(3) * 4;
        assert_eq!(url.len(), "data:image/bmp;base64,".len() + len);
    }
}
