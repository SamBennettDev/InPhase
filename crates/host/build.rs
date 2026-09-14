//! Ensure `web/dist/` exists so the `rust-embed` macro in `http::assets` always
//! has a folder to read. The real bundle is produced by `npm run build` (Vite)
//! and overwrites the placeholder. CI runs the web build before `cargo build`.
//!
//! Also stamps the **build id** into the binary. `tools/ship.sh` sets
//! `INPHASE_BUILD_ID` to the same value for the Vite build and this one, so the
//! embedded bundle and the host that serves it carry an identical marker. The
//! page compares its own compiled-in id against `/api/v1/status` and reloads on
//! a mismatch, which is what makes a stale tab self-heal after *any* wire change
//! — no hand-bumped protocol constant to forget (see `web/src/buildid.ts`).

use std::path::Path;

fn main() {
    // Unset (a plain `cargo build`) means "not shipped" — deliberately not a
    // git sha, so an ad-hoc local binary can never be mistaken for a release.
    let build_id = std::env::var("INPHASE_BUILD_ID").unwrap_or_else(|_| "dev".to_string());
    println!("cargo:rustc-env=INPHASE_BUILD_ID={build_id}");
    println!("cargo:rerun-if-env-changed=INPHASE_BUILD_ID");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dist = Path::new(&manifest).join("../../web/dist");
    let index = dist.join("index.html");

    if !index.exists() && std::env::var("PROFILE").as_deref() == Ok("release") {
        panic!("Release build requires npm --prefix web ci and npm --prefix web run build");
    }
    if !index.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(
            &index,
            "<!doctype html><meta charset=utf-8><title>InPhase</title>\
             <body style=\"font:16px system-ui;margin:3rem\">\
             <h1>InPhase</h1><p>Web client bundle not built. Run \
             <code>npm --prefix web ci &amp;&amp; npm --prefix web run build</code> \
             then rebuild the host.</p>",
        );
    }

    println!("cargo:rerun-if-changed=../../web/dist");
    println!("cargo:rerun-if-changed=build.rs");
}
