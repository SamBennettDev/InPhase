# ADR 0003 — DXGI Desktop Duplication as the primary capture path

**Status:** accepted · **Report:** §5.1, §27

## Decision
Capture the selected monitor with `d3d11screencapturesrc capture-api=dxgi`,
keeping frames as `video/x-raw(memory:D3D11Memory)` end-to-end until encoder
input. Windows Graphics Capture (WGC) stays as a fallback/experimental path and
the future window-capture route.

## Why
Desktop Duplication is designed for remote/collaboration scenarios and exposes
GPU-resident surfaces, giving the "no avoidable CPU copy in the normal path"
invariant (§5.1). The desired invariant is not "zero copies at any cost".

## Revisit trigger
A WGC benchmark demonstrates better correctness or latency for the target
selected-monitor cases.
