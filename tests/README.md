# InPhase test suites

Maps to the architecture report's acceptance gates (§20, §23) and validation
matrix. Three layers:

## `integration/` — protocol + host wiring (fast, CI)

| File | Covers |
|------|--------|
| `../crates/protocol/tests/vectors.rs` | golden binary-packet vectors, Rust side (§19 Phase 3) |
| `../web/src/input/protocol.test.ts` | golden vectors, TypeScript side — must match Rust byte-for-byte |
| `../crates/host/src/**` unit tests | pairing (PIN/TTL/rate-limit), session state machine, input state reconciliation, encoder-policy tables, config validation |

Run: `cargo test --workspace` · `npm --prefix web test`

## `compatibility/` — the §23 matrix (manual / semi-automated, on hardware)

Checklists, not code — each needs a real GPU + browser:

- `host-gpu.md` — NVIDIA / AMD / Intel: intended encoder element selected,
  B-frames + lookahead disabled, encode p50/p95 within frame budget (§6, §20).
- `codec-fallback.md` — HEVC-capable host → non-HEVC client falls back cleanly
  to H.264 (§22, §23 release gate).

## `latency/` — the §20/§21 instrumentation gates (on hardware)

- `queue-discipline.md` — throttle the link; the raw/video queue must not grow
  (0–1 frame), bitrate adapts before playout delay runs away (§8.2, §23).
- `idle-cost.md` — no capture/encoder pipeline when no player; near-zero GPU
  encode use (§23 release gate, §29).
- `reconnect.md` — 100 connect/disconnect cycles: no stuck input, no ghost
  controller, no resource leak (§23).
- `glass-to-glass.md` — high-speed-camera / optical fixture procedure; the
  **release** latency metric. Browser timestamps are pipeline diagnostics only
  (§20, §21).

`tests/latency/harness/` is a CDP + host-log harness (drives a headless Chrome
client, aggregates the host's per-frame trace). `scripts/smoke-gstreamer.ps1`
probes that the required GStreamer elements load.
