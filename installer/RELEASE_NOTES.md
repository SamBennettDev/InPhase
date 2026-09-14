# InPhase release candidate

A Windows host for browser-based desktop and game streaming. No InPhase account
or hosted relay is required. Download `InPhaseSetup.exe` from this release's assets,
run setup on the gaming PC, then use the tray dashboard to pair another device.

## Changes

- New host dashboard and searchable, responsive game library.
- Working QR invitations, explicit action failures and saved stream settings.
- Per-user certificate/key setup and stronger local administration checks.
- Self-contained DLL layout, broader encoder plugin bundle and strict packaging.
- Browser regression tests and an isolated Windows package smoke check.

## Before publishing this draft

This candidate is **unsigned** unless a maintainer signs and verifies the final
artifacts. Complete docs/RELEASING.md: attach matching corresponding source and
record clean-install, GPU, audio, input and network results. Do not publish broad
compatibility or performance claims without those measurements.

Automated checks verify build, protocol/policy behavior, simulated browser flows
and packaged DLL/plugin loading. They do not verify real GPU streaming.

## Requirements and limits

Windows 10 build 19041 or newer, or Windows 11, x64; a supported hardware encoder,
current driver and active desktop/display. Start with 1080p/60 on a trusted LAN.
Virtual gamepad support requires a separate compatible driver and input opt-in.
Remote access is off by default and depends on router/ISP reachability.

## Certificate migration

Setup retires the old shared `%ProgramData%\InPhase\tls` CA. TLS material now
belongs to your Windows user. Remove the old InPhase CA from player devices and
follow Certificate setup again. See docs/TROUBLESHOOTING.md.

## Verify

Compare the installer SHA-256 with `InPhaseSetup.exe.sha256`. Keep the binary
manifest and component notices with the release. InPhase-owned code is
GPL-3.0-or-later; dependencies retain their own terms.
