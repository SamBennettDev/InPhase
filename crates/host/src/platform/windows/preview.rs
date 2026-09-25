//! Frames of the primary desktop for the library's live desktop tile.
//!
//! GDI `StretchBlt` from the screen DC — independent of the GStreamer capture
//! pipeline, so the library can show the desktop while no stream runs.
//! Exclusive fullscreen games may come out black; that page is not on screen
//! then. The page pulls frames back to back at up to 30 fps; at up to 960 px
//! (sharp on a 3x phone) each is a JPEG of roughly 40-90 KB - as BMP it would
//! be ~1.5 MB, ~45 MB/s at that rate.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const MAX_EDGE: u32 = 960;
const JPEG_QUALITY: u8 = 78;
/// Frames a second the producer delivers, at most.
const FPS: u64 = 30;
/// The producer stops this long after the last request (the tile left view).
const IDLE_STOP: Duration = Duration::from_secs(3);

/// Latest encoded frame from the producer thread, shared by requests.
struct Producer {
    frame: Option<std::sync::Arc<Vec<u8>>>,
    /// Bumped for every new frame, so a client can ask for "newer than N".
    seq: u64,
    last_request: Instant,
    running: bool,
}

static PRODUCER: std::sync::Mutex<Option<Producer>> = std::sync::Mutex::new(None);
static FRAME_READY: std::sync::Condvar = std::sync::Condvar::new();

/// A desktop frame for the tile: the JPEG and its sequence number, or `None`
/// when nothing newer than the client's last frame arrived in time.
pub type PreviewFrame = Option<(Vec<u8>, u64)>;

/// The desktop as a JPEG of up to `MAX_EDGE` px, newer than frame `after`.
///
/// A still desktop produces no new frames, so a client that already has the
/// latest waits here (up to ~1 s) instead of being sent the same image again
/// thirty times a second.
///
/// A background thread keeps the latest frame encoded while the tile is being
/// watched, so a request returns at once instead of waiting out a ~30 ms GDI
/// read-back (which capped a request-driven tile at ~15 fps, 2026-09-25).
///
/// Not DXGI desktop duplication, though it is far cheaper: a process gets one
/// duplication per output, so a preview still holding it made the stream's
/// own capture fail to start, and the host then crashed in that failure path
/// (2026-09-25 03:36Z).
pub fn desktop_preview_image(after: Option<u64>) -> anyhow::Result<PreviewFrame> {
    let mut g = PRODUCER
        .lock()
        .map_err(|_| anyhow::anyhow!("preview lock poisoned"))?;
    let p = g.get_or_insert_with(|| Producer {
        frame: None,
        seq: 0,
        last_request: Instant::now(),
        running: false,
    });
    p.last_request = Instant::now();
    {
        if !p.running {
            p.running = true;
            p.frame = None;
            std::thread::Builder::new()
                .name("desktop-preview".into())
                .spawn(produce)?;
        }
        // Wait for a frame the client has not seen (the first one after start
        // takes one acquire).
        let deadline = Instant::now() + Duration::from_millis(1000);
        let stale = |p: &Producer| p.frame.is_none() || after.is_some_and(|a| p.seq <= a);
        while g.as_ref().is_some_and(|p| stale(p) && p.running) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = FRAME_READY
                .wait_timeout(g, left)
                .map_err(|_| anyhow::anyhow!("preview lock poisoned"))?
                .0;
        }
        match g.as_ref() {
            Some(p) if p.frame.is_some() && !stale(p) => {
                let f = p.frame.clone().unwrap_or_default();
                return Ok(Some((f.to_vec(), p.seq)));
            }
            // The producer runs but nothing changed: the client keeps its frame.
            Some(p) if p.running && p.frame.is_some() => return Ok(None),
            _ => {}
        }
    }
    drop(g);
    // GDI has no change detection: every call is a fresh capture.
    capture_gdi().map(|jpg| Some((jpg, 0)))
}

/// Producer thread: capture, encode, publish only when the image changed;
/// stop when idle.
fn produce() {
    use std::hash::{Hash, Hasher};
    let period = Duration::from_millis(1000 / FPS);
    let mut last_hash = None;
    loop {
        let started = Instant::now();
        {
            let mut g = match PRODUCER.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let Some(p) = g.as_mut() else { return };
            if p.last_request.elapsed() > IDLE_STOP {
                p.running = false;
                p.frame = None;
                return;
            }
        }
        match capture_gdi() {
            Ok(jpg) => {
                // Same pixels encode to the same bytes: an unchanged desktop
                // publishes nothing, and waiting clients stay waiting.
                let mut h = std::collections::hash_map::DefaultHasher::new();
                jpg.hash(&mut h);
                let hash = h.finish();
                if last_hash != Some(hash) {
                    last_hash = Some(hash);
                    if let Ok(mut g) = PRODUCER.lock() {
                        if let Some(p) = g.as_mut() {
                            p.frame = Some(std::sync::Arc::new(jpg));
                            p.seq += 1;
                        }
                    }
                    FRAME_READY.notify_all();
                }
            }
            Err(e) => {
                tracing::debug!("desktop preview capture failed: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        if let Some(rest) = period.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// A 1:1 GDI `BitBlt` of the screen, box-averaged and JPEG-encoded (~34 ms
/// at 1440p, most of it the compositor read-back).
fn capture_gdi() -> anyhow::Result<Vec<u8>> {
    let src_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let src_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if src_w <= 0 || src_h <= 0 {
        anyhow::bail!("primary desktop has no size");
    }
    let (sw, sh) = (src_w as u32, src_h as u32);
    let k = scale_factor(sw, sh, MAX_EDGE);

    // A plain BitBlt into a DIB section, then a k x k box average in Rust.
    // `StretchBlt` in HALFTONE mode did the scaling in GDI at ~43 ms a frame
    // for a 2560x1440 desktop (the whole budget of a 30 fps tile, measured
    // 2026-09-25); a 1:1 blit plus the average is a fraction of that and
    // looks the same.
    let small = unsafe {
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.is_invalid() {
            anyhow::bail!("GetDC failed");
        }
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            ReleaseDC(HWND::default(), hdc_screen);
            anyhow::bail!("CreateCompatibleDC failed");
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: src_w,
                biHeight: -src_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(hdc_screen, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) if !bits.is_null() => d,
            _ => {
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(HWND::default(), hdc_screen);
                anyhow::bail!("CreateDIBSection failed");
            }
        };
        let prev = SelectObject(hdc_mem, HGDIOBJ(dib.0));
        let blit_ok = BitBlt(hdc_mem, 0, 0, src_w, src_h, hdc_screen, 0, 0, SRCCOPY).is_ok();
        let _ = GdiFlush();
        let result = blit_ok.then(|| {
            let px = std::slice::from_raw_parts(bits as *const u8, (sw * sh * 4) as usize);
            box_downscale(px, sw, sh, sw as usize * 4, k)
        });
        SelectObject(hdc_mem, prev);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(HWND::default(), hdc_screen);
        result.ok_or_else(|| anyhow::anyhow!("BitBlt failed"))?
    };
    let jpg = encode_jpeg(sw / k, sh / k, &small);
    jpg
}

/// Smallest whole factor that brings the long edge to `max_edge` or below.
fn scale_factor(w: u32, h: u32, max_edge: u32) -> u32 {
    w.max(h).div_ceil(max_edge.max(1)).max(1)
}

/// Average each k x k block of a BGRA image (rows `stride` bytes apart) into
/// one pixel. Output is `(w / k) x (h / k)` BGRA; leftover edge pixels are
/// dropped.
fn box_downscale(src: &[u8], w: u32, h: u32, stride: usize, k: u32) -> Vec<u8> {
    let (dw, dh, k) = ((w / k) as usize, (h / k) as usize, k as usize);
    let mut out = vec![0u8; dw * dh * 4];
    let mut sums = vec![0u32; dw * 3];
    let area = (k * k) as u32;
    for dy in 0..dh {
        sums.iter_mut().for_each(|s| *s = 0);
        for y in dy * k..dy * k + k {
            let row = &src[y * stride..y * stride + dw * k * 4];
            for (dx, block) in row.chunks_exact(k * 4).enumerate() {
                let acc = &mut sums[dx * 3..dx * 3 + 3];
                for p in block.chunks_exact(4) {
                    acc[0] += p[0] as u32;
                    acc[1] += p[1] as u32;
                    acc[2] += p[2] as u32;
                }
            }
        }
        let dst = &mut out[dy * dw * 4..(dy + 1) * dw * 4];
        for (dx, px) in dst.chunks_exact_mut(4).enumerate() {
            px[0] = (sums[dx * 3] / area) as u8;
            px[1] = (sums[dx * 3 + 1] / area) as u8;
            px[2] = (sums[dx * 3 + 2] / area) as u8;
            px[3] = 255;
        }
    }
    out
}

fn encode_jpeg(w: u32, h: u32, bgra: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(96 * 1024);
    jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY).encode(
        bgra,
        u16::try_from(w)?,
        u16::try_from(h)?,
        jpeg_encoder::ColorType::Bgra,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{box_downscale, encode_jpeg, scale_factor};

    #[test]
    fn the_scale_factor_brings_the_long_edge_under_the_cap() {
        assert_eq!(scale_factor(1920, 1080, 960), 2);
        assert_eq!(scale_factor(2560, 1440, 960), 3);
        assert_eq!(scale_factor(3840, 2160, 960), 4);
        assert_eq!(scale_factor(800, 600, 960), 1);
        assert_eq!(scale_factor(1080, 1920, 960), 2);
    }

    #[test]
    fn box_downscale_averages_each_block() {
        // 2x2 image of four greys -> one pixel, their mean.
        let px: Vec<u8> = [10u8, 20, 30, 40]
            .iter()
            .flat_map(|&v| [v, v, v, 0])
            .collect();
        assert_eq!(box_downscale(&px, 2, 2, 8, 2), vec![25, 25, 25, 255]);
        // k = 1 is a copy with alpha set.
        assert_eq!(box_downscale(&px, 2, 2, 8, 1)[..4], [10, 10, 10, 255]);
    }

    #[test]
    fn frames_are_jpeg() {
        let bgra = vec![128u8; 16 * 9 * 4];
        let jpg = encode_jpeg(16, 9, &bgra).unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9], "EOI");
    }
}
