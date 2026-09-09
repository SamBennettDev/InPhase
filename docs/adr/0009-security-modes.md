# ADR 0009 — Trusted-LAN HTTP MVP, secure-origin production mode

**Status:** superseded 2026-09. The SaaS `secure-origin` control plane is gone;
InPhase now serves whole-origin HTTPS from a **bundled local CA** (installer
trusts the root; other devices install it from `/ca.crt`), and pairing adds a
non-extractable per-browser Ed25519 key on top of the PIN. Remote access is
opt-in and LAN-only for enrolment. See `docs/lan-security-model.md`. · **Report:** §14, §27

## Decision
- **MVP (`trusted-lan-http`):** 6-digit PIN (5-min TTL, rotate on pair) over
  plaintext LAN HTTP; HttpOnly/SameSite=Strict session cookie authenticates the
  same-origin `/api/v1/signal` WebSocket; global + per-IP rate limiting; strict
  CSP, no third-party assets; admin API loopback-only. This is an **explicit,
  labelled tradeoff**: a PIN deters casual access but is not MITM-safe on a
  hostile LAN, and secure-context APIs (Keyboard Lock) are unavailable.
- **Production (`secure-origin`):** per-host FQDN + ACME DNS-01 certificate;
  TLS private key stays on the host; whole play/signaling origin HTTPS/WSS so
  Keyboard Lock is available in Chromium (§14.2, Phase 7).

## Revisit trigger
Security or offline-operation requirements are explicitly reprioritised.
