# Integration tests

The executable integration coverage lives with the code:

- `crates/protocol/tests/vectors.rs` — golden binary-packet vectors (Rust).
- `crates/host/src/*/mod.rs` `#[cfg(test)]` — pairing, session FSM, input state,
  encoder policy, config.
- `web/src/input/protocol.test.ts` — golden vectors (TypeScript), asserted
  against `protocol.vectors.json` generated from the Rust side.

`cargo test --workspace` + `npm --prefix web test` is the CI gate.

Planned (need a running host + headless Chrome, tracked for Phase 1):
- `pair_then_signal.rs` — POST /pair → WS /signal → client_hello/capabilities →
  receive `session_config` + `offer`.
- `busy_second_client.rs` — second WS gets `BUSY`, first session unaffected.
- `input_roundtrip.rs` — packet on the `input` channel moves a test window;
  watchdog releases held state on channel close.
