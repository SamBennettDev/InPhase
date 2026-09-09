//! Web client assets, embedded into the executable (§16 *"static assets embedded
//! in host executable"*, §17).
//!
//! `web/dist/` is produced by `npm run build` (Vite) before `cargo build`. In
//! debug builds `debug-embed` reads from disk so the dashboard hot-reloads.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
pub struct WebAssets;

/// Serve `path` from the embedded bundle, falling back to `index.html` for
/// client-side routes. Adds a long cache lifetime for fingerprinted assets.
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };

    match WebAssets::get(candidate) {
        Some(file) => {
            let mime = mime_guess::from_path(candidate).first_or_octet_stream();
            let cache = if candidate == "index.html" {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            (
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_str(mime.as_ref()).unwrap(),
                    ),
                    (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None if !candidate.contains('.') => serve("index.html"),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
