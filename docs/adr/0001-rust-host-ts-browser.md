# ADR 0001 — Rust host, vanilla TypeScript browser client

**Status:** accepted · **Report:** §1, §4.1, §17, §27

## Decision
The host application is Rust. GStreamer 1.28 owns the media path. The browser
client is vanilla TypeScript + CSS with a Vite build step and **no runtime
framework**. Compiled web assets are embedded in the host executable.

## Why
- Keeps the InPhase-owned layer small and memory-safe while GStreamer owns the
  complex capture/encode/WebRTC machinery (§1).
- A play surface is one `<video>` plus a small settings/diagnostics overlay — a
  framework buys little and costs startup time and state complexity (§17).

## Revisit trigger
A required Windows or media API is materially blocked by the Rust bindings
(missing surface in `gstreamer-rs` / `windows`) with no reasonable workaround.
