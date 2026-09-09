# ADR 0007 — Exactly one active player

**Status:** accepted · **Report:** §15, §22, §27, §29

## Decision
One `PlayerSession` owns the one `MediaSession`. A second play request receives
`BUSY` with current-session metadata safe to expose — never a second encoder by
accident. Entering `STOPPING` releases all input immediately; the media pipeline
is destroyed before returning to `IDLE`.

## Revisit trigger
Multi-viewer becomes a real product requirement (spectators, co-play).
