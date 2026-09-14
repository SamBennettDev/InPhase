# Changelog

## Unreleased

- Fix IPv4 LAN WebTransport connections by binding both IPv4 and IPv6.
- Redesigned dashboard: play address, PIN, working QR invitations, device access,
  certificate help and live stream health.
- Searchable player library, launcher filters, desktop selection, saved quality
  settings, accessible controls and responsive layouts.
- Clear busy, offline, empty and mutation-failure states; PIN-free diagnostics.
- Loopback Host and browser Origin checks, stricter CSP and no-store API responses.
- Safe Unicode identity parsing, fail-closed DPAPI writes and no plaintext PIN logs.
- Per-user CA storage and original-user setup. Upgrades retire the shared CA;
  other devices must trust the new certificate.
- Support DLLs beside the EXE; AMD, Intel and Media Foundation encoder plugins.
- Strict build/package checks, correct GPL metadata and distribution notices.
- Working web test discovery, browser regression tests and isolated Windows checks.
- Installation, troubleshooting, contribution and release documentation.

These changes do not certify hardware compatibility or a public release.
