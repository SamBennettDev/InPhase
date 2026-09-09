//! Locate cover art on disk for library grid posters.
//!
//! Everything here targets the library grid's 2:3 poster tile. `header.jpg`
//! (Steam's 460x215 store banner) and similar landscape/tiny assets are
//! deliberately excluded even when present — stretched into a portrait tile
//! they look blurry and badly cropped. Better to fall through to InPhase's
//! generated placeholder cover than show a smeared banner.

use std::path::{Path, PathBuf};

/// Steam client cache: `appcache/librarycache/<appid>/library_600x900.jpg`.
pub fn steam_library_poster(steam_root: &Path, appid: &str) -> Option<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache").join(appid);
    for name in [
        "library_600x900_2x.jpg",
        "library_600x900.jpg",
        "library_capsule.jpg",
    ] {
        let p = cache.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let Ok(entries) = std::fs::read_dir(&cache) else {
        return largest_image_in_dir(&cache);
    };
    for ent in entries.flatten() {
        let sub = ent.path();
        if !sub.is_dir() {
            continue;
        }
        for name in ["library_600x900_2x.jpg", "library_600x900.jpg"] {
            let p = sub.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    largest_image_in_dir(&cache)
}

/// Common install-folder and launcher-specific layouts (Xbox, GOG, etc.).
pub fn install_dir_poster(install_dir: &Path) -> Option<PathBuf> {
    if install_dir.as_os_str().is_empty() || !install_dir.is_dir() {
        return None;
    }
    for rel in [
        "library_600x900_2x.jpg",
        "library_600x900.jpg",
        "poster.jpg",
        "cover.jpg",
        "boxart.jpg",
        "SplashScreen.png",
        "LargeLogo.png",
        "Content/Resources/SplashScreen.png",
        "Content/Resources/Logo.png",
        "Content/Resources/Square480x480Logo.png",
        "Content/SplashScreen.png",
        "Content/LargeLogo.png",
        "Content/Logo.png",
        "media/logos/splash.png",
    ] {
        let p = install_dir.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    largest_image_in_dir(install_dir)
}

/// GOG Galaxy web cache for a product id.
pub fn gog_webcache_poster(game_id: &str) -> Option<PathBuf> {
    let base = PathBuf::from(r"C:\ProgramData\GOG.com\Galaxy\webcache").join(game_id);
    largest_image_in_dir(&base)
}

/// Ubisoft Connect asset cache keyed by space id.
pub fn ubisoft_asset_poster(space_id: &str) -> Option<PathBuf> {
    let base = PathBuf::from(r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\cache\assets");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return None;
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().to_lowercase();
        if name.contains(&space_id.to_ascii_lowercase()) {
            let p = ent.path();
            if p.is_file() && is_image(&p) {
                return Some(p);
            }
            if p.is_dir() {
                if let Some(found) = largest_image_in_dir(&p) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Attach a serveable poster URL when a local file was found.
pub fn attach_poster(id: &str, path: Option<PathBuf>) -> (Option<String>, Option<PathBuf>) {
    match path.filter(|p| p.is_file()) {
        Some(p) => (
            Some(format!("/api/v1/library/poster/{}", id.replace(':', "%3A"))),
            Some(p),
        ),
        None => (None, None),
    }
}

/// Minimum height, in pixels, worth showing at full-tile size — below this a
/// launcher's cached image is an icon/thumbnail, not real box art, and just
/// looks blurry when stretched to fill a poster tile.
const MIN_POSTER_HEIGHT: u32 = 200;
/// A poster tile is 2:3 (portrait). Accept anything from square down to that
/// ratio (with slack); reject landscape banners and store capsules outright.
const MAX_ASPECT_W_OVER_H: f32 = 1.05;

/// The largest *plausible cover art* image in a folder: portrait-or-square,
/// not a thumbnail-sized icon. Landscape banners and tiny icons are skipped
/// even if they are the biggest files present — showing nothing (so the
/// caller falls back to a generated placeholder) beats showing those
/// stretched or blown up.
fn largest_image_in_dir(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut best: Option<(u64, PathBuf)> = None;
    for ent in entries.flatten() {
        let p = ent.path();
        if !p.is_file() || !is_image(&p) {
            continue;
        }
        if let Some((w, h)) = image_dims(&p) {
            if h < MIN_POSTER_HEIGHT || (w as f32) > (h as f32) * MAX_ASPECT_W_OVER_H {
                continue;
            }
        }
        // Dimensions unreadable (e.g. .webp, which we don't parse) — fall
        // back to trusting the file is fine rather than discarding it.
        let len = ent.metadata().map(|m| m.len()).unwrap_or(0);
        if best.as_ref().map(|(s, _)| len > *s).unwrap_or(true) {
            best = Some((len, p));
        }
    }
    best.map(|(_, p)| p)
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()),
        Some(ext) if ext == "jpg" || ext == "jpeg" || ext == "png" || ext == "webp"
    )
}

/// Best-effort `(width, height)` from a JPEG or PNG's own header — no image
/// decoding, just enough parsing to reject the wrong shape/size of art.
/// Returns `None` for anything else (including .webp): callers treat that as
/// "can't tell, allow it" rather than rejecting.
fn image_dims(path: &Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        // PNG: 8-byte signature, then the IHDR chunk (length, "IHDR", w, h, ...).
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.len() >= 4 && bytes[0..2] == [0xFF, 0xD8] {
        // JPEG: walk markers looking for a start-of-frame segment.
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes(bytes[i + 2..i + 4].try_into().ok()?) as usize;
            let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof && i + 9 <= bytes.len() {
                let h = u16::from_be_bytes(bytes[i + 5..i + 7].try_into().ok()?) as u32;
                let w = u16::from_be_bytes(bytes[i + 7..i + 9].try_into().ok()?) as u32;
                return Some((w, h));
            }
            if marker == 0xDA || len < 2 {
                break; // start-of-scan: no SOF found before the entropy data
            }
            i += 2 + len;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A minimal (fake) PNG: real signature + IHDR with the given size, no
    /// further chunks. Enough for `image_dims`, not enough to actually decode.
    fn fake_png(w: u32, h: u32, padding: usize) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend(std::iter::repeat(0u8).take(padding));
        b
    }

    #[test]
    fn picks_largest_image_in_folder() {
        let dir = std::env::temp_dir().join("inphase-poster-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tiny.png"), [0u8; 8]).unwrap();
        fs::write(dir.join("big.png"), [0u8; 800]).unwrap();
        let found = largest_image_in_dir(&dir).unwrap();
        assert_eq!(found.file_name().unwrap(), "big.png");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_landscape_banner_even_when_larger() {
        let dir = std::env::temp_dir().join("inphase-poster-test-aspect");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // header.jpg-shaped banner: bigger file, but a landscape store banner.
        fs::write(dir.join("banner.png"), fake_png(460, 215, 2000)).unwrap();
        // Real vertical box art: smaller file, right shape.
        fs::write(dir.join("cover.png"), fake_png(600, 900, 10)).unwrap();
        let found = largest_image_in_dir(&dir).unwrap();
        assert_eq!(found.file_name().unwrap(), "cover.png");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_icon_sized_images() {
        let dir = std::env::temp_dir().join("inphase-poster-test-tiny");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A launcher's small cached thumbnail — right shape, too small to
        // stretch to a full poster tile without visible blur.
        fs::write(dir.join("icon.png"), fake_png(96, 96, 0)).unwrap();
        assert!(largest_image_in_dir(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_dims_reads_png_and_jpeg() {
        let dir = std::env::temp_dir().join("inphase-poster-test-dims");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let png = dir.join("p.png");
        fs::write(&png, fake_png(600, 900, 0)).unwrap();
        assert_eq!(image_dims(&png), Some((600, 900)));

        // Minimal JPEG: SOI, then a baseline SOF0 (0xC0) segment with a
        // 600x900 frame, nothing else needed for our parser.
        let mut jpg = vec![0xFF, 0xD8];
        jpg.extend_from_slice(&[0xFF, 0xC0]); // SOF0
        jpg.extend_from_slice(&(8u16).to_be_bytes()); // segment length (incl. these 2 bytes)
        jpg.push(8); // precision
        jpg.extend_from_slice(&(900u16).to_be_bytes()); // height
        jpg.extend_from_slice(&(600u16).to_be_bytes()); // width
        let jpg_path = dir.join("p.jpg");
        fs::write(&jpg_path, &jpg).unwrap();
        assert_eq!(image_dims(&jpg_path), Some((600, 900)));
        let _ = fs::remove_dir_all(&dir);
    }
}
