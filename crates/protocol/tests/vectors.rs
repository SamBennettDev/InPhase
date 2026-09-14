//! Golden vector assertions (architecture report §19 Phase 3).
//!
//! Every vector must:
//!   1. encode to exactly its committed `hex`, and
//!   2. decode from that `hex` back to an identical [`InputPacket`].
//!
//! The same vectors are dumped to `web/src/input/protocol.vectors.json` by
//! `cargo run -p inphase-protocol --example dump_vectors`, and the browser
//! client asserts against that file so both implementations stay byte-compatible.

use inphase_protocol::input::InputPacket;
use inphase_protocol::vectors::golden;

fn hex_to_bytes(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length in vector: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn every_vector_encodes_to_committed_hex() {
    for v in golden() {
        let got = bytes_to_hex(&v.packet.encode());
        assert_eq!(got, v.hex, "encode mismatch for vector `{}`", v.name);
    }
}

#[test]
fn every_vector_decodes_back_to_itself() {
    for v in golden() {
        let bytes = hex_to_bytes(v.hex);
        let decoded = InputPacket::decode(&bytes)
            .unwrap_or_else(|e| panic!("decode failed for `{}`: {e}", v.name));
        assert_eq!(decoded, v.packet, "decode mismatch for vector `{}`", v.name);
    }
}

#[test]
fn vector_names_are_unique() {
    let mut names: Vec<_> = golden().iter().map(|v| v.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate golden vector name");
}
