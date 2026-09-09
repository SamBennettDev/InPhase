# ADR 0002 — GStreamer 1.28.6 (MSVC x86-64), pinned

**Status:** accepted · **Report:** §4.1, §26, §27

## Decision
Pin the Windows runtime to GStreamer **1.28.6** MSVC x86-64 and the official Rust
bindings (`gstreamer` 0.25.x, `gst-plugin-webrtc` 0.15.x). Do not depend on
development-series 1.29 features. Ship a bundled runtime + the exact plugin set
InPhase tests; do not rely on a user-installed GStreamer (§26, §19 Phase 4).

## Why
1.28 is the current stable line with mature D3D11 capture, hardware encoders,
WebRTC (rswebrtc), WASAPI2 and Rust bindings.

## Revisit trigger
Upgrade the pin only after the compatibility regression suite (§23) passes on
NVIDIA, AMD and Intel reference hardware.
