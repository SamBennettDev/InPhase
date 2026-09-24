//! Still of the primary desktop for the play-page monitor bezel.
//!
//! GDI `StretchBlt` from the screen DC — independent of the GStreamer capture
//! pipeline, so the library can show a live thumbnail while idle. Exclusive
//! fullscreen games may come out black; that page is not on screen then.
//! Encoded as BMP so we need no extra crates (LAN thumbnail, ~100 KB).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const MAX_EDGE: u32 = 320;
const CACHE_TTL: Duration = Duration::from_millis(1500);

static CACHE: std::sync::Mutex<Option<(Instant, Vec<u8>)>> = std::sync::Mutex::new(None);

pub fn desktop_preview_image() -> anyhow::Result<Vec<u8>> {
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, bytes)) = guard.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(bytes.clone());
            }
        }
    }
    let bmp = capture()?;
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((Instant::now(), bmp.clone()));
    }
    Ok(bmp)
}

fn capture() -> anyhow::Result<Vec<u8>> {
    let src_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let src_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if src_w <= 0 || src_h <= 0 {
        anyhow::bail!("primary desktop has no size");
    }
    let (dst_w, dst_h) = fit(src_w as u32, src_h as u32, MAX_EDGE);

    unsafe {
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.is_invalid() {
            anyhow::bail!("GetDC failed");
        }
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            ReleaseDC(HWND::default(), hdc_screen);
            anyhow::bail!("CreateCompatibleDC failed");
        }
        let hbmp = CreateCompatibleBitmap(hdc_screen, dst_w as i32, dst_h as i32);
        if hbmp.is_invalid() {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            anyhow::bail!("CreateCompatibleBitmap failed");
        }
        let prev = SelectObject(hdc_mem, HGDIOBJ(hbmp.0));
        let _ = SetStretchBltMode(hdc_mem, HALFTONE);
        let _ = SetBrushOrgEx(hdc_mem, 0, 0, None);
        let blit_ok = StretchBlt(
            hdc_mem,
            0,
            0,
            dst_w as i32,
            dst_h as i32,
            hdc_screen,
            0,
            0,
            src_w,
            src_h,
            SRCCOPY,
        )
        .as_bool();
        if !blit_ok {
            SelectObject(hdc_mem, prev);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            anyhow::bail!("StretchBlt failed");
        }

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dst_w as i32,
                biHeight: -(dst_h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (dst_w * dst_h * 4) as usize];
        let lines = GetDIBits(
            hdc_mem,
            hbmp,
            0,
            dst_h,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(hdc_mem, prev);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(HWND::default(), hdc_screen);
        if lines == 0 {
            anyhow::bail!("GetDIBits failed");
        }
        Ok(encode_bmp24(dst_w, dst_h, &bgra))
    }
}

fn encode_bmp24(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let stride = ((w * 3 + 3) / 4) * 4;
    let pixel_bytes = stride * h;
    let file_size = 14 + 40 + pixel_bytes;
    let mut out = vec![0u8; file_size as usize];
    out[0] = b'B';
    out[1] = b'M';
    out[2..6].copy_from_slice(&file_size.to_le_bytes());
    out[10..14].copy_from_slice(&54u32.to_le_bytes());
    out[14..18].copy_from_slice(&40u32.to_le_bytes());
    out[18..22].copy_from_slice(&w.to_le_bytes());
    out[22..26].copy_from_slice(&(-(h as i32)).to_le_bytes());
    out[26..28].copy_from_slice(&1u16.to_le_bytes());
    out[28..30].copy_from_slice(&24u16.to_le_bytes());
    out[34..38].copy_from_slice(&pixel_bytes.to_le_bytes());
    let pixels = &mut out[54..];
    for y in 0..h {
        let src = (y * w * 4) as usize;
        let dst = (y * stride) as usize;
        for x in 0..w {
            let s = src + (x * 4) as usize;
            let d = dst + (x * 3) as usize;
            pixels[d] = bgra[s];
            pixels[d + 1] = bgra[s + 1];
            pixels[d + 2] = bgra[s + 2];
        }
    }
    out
}

fn fit(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    if w >= h {
        let dw = max_edge.min(w);
        let dh = ((h as u64 * dw as u64) / w as u64).max(1) as u32;
        (dw, dh)
    } else {
        let dh = max_edge.min(h);
        let dw = ((w as u64 * dh as u64) / h as u64).max(1) as u32;
        (dw, dh)
    }
}

#[cfg(test)]
mod tests {
    use super::{encode_bmp24, fit};

    #[test]
    fn fit_keeps_aspect_and_caps_the_long_edge() {
        assert_eq!(fit(1920, 1080, 480), (480, 270));
        assert_eq!(fit(1080, 1920, 480), (270, 480));
        assert_eq!(fit(100, 50, 480), (100, 50));
    }

    #[test]
    fn bmp_header_is_well_formed() {
        let bgra = vec![0u8; 2 * 2 * 4];
        let bmp = encode_bmp24(2, 2, &bgra);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 54);
        assert_eq!(u32::from_le_bytes(bmp[18..22].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(bmp[22..26].try_into().unwrap()), -2);
        assert_eq!(u16::from_le_bytes(bmp[28..30].try_into().unwrap()), 24);
    }
}
