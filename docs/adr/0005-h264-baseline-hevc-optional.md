# ADR 0005 — H.264 is the compatibility contract; HEVC is opportunistic

**Status:** accepted; **updated 2026-09** — the browser/GPU matrix improved
enough that the host now *prefers* HEVC when the client advertises an HEVC
receive codec and falls back to H.264 automatically (the ADR-0005 revisit
trigger). H.264 is still the interoperability floor. · **Report:** §7, §22, §27

## Decision
H.264 is a first-class, fully-tuned path and the interoperability floor together
with Opus. HEVC/H.265 is offered **only** when host encoder + browser RTP
receive capability + decode capability are all confirmed (§7 steps 1–4). AV1 is
out of v1 (§7).

## Why
H.264 + Opus is the conservative WebRTC interoperability floor. HEVC is
increasingly available but not universal and can "appear available yet decode
slowly or fail" on a specific OS/browser/GPU (§30).

## Revisit trigger
The browser/GPU matrix makes another codec broadly superior with measured
encode/decode latency benefit at every target mode.
