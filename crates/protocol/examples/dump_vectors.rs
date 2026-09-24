//! Writes the golden input-packet vectors to
//! `web/src/input/protocol.vectors.json` so the browser client's test suite can
//! assert byte-for-byte compatibility with the Rust implementation.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run -p inphase-protocol --example dump_vectors
//! ```

use std::io::Write;
use std::path::PathBuf;

use inphase_protocol::vectors::golden;

fn main() -> std::io::Result<()> {
    let mut out = String::from("[\n");
    let vectors = golden();
    for (i, v) in vectors.iter().enumerate() {
        let bytes = v.packet.encode();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        // Keep this hand-rolled so the example has zero deps beyond the crate.
        out.push_str(&format!(
            "  {{ \"name\": {name:?}, \"hex\": \"{hex}\", \"len\": {len} }}{comma}\n",
            name = v.name,
            len = bytes.len(),
            comma = if i + 1 == vectors.len() { "" } else { "," }
        ));
    }
    out.push_str("]\n");

    let target = repo_root().join("web/src/input/protocol.vectors.json");
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(&target)?;
    f.write_all(out.as_bytes())?;
    eprintln!("wrote {} vectors to {}", vectors.len(), target.display());
    Ok(())
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/protocol
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}
