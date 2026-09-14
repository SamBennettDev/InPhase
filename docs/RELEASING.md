# Releasing InPhase

Green CI is necessary but does not prove installed streaming on gaming hardware.
Tags create a **draft** after automated checks pass. A maintainer completes the
review below before publishing.

## Candidate

1. Update workspace/web versions together, the lockfile, and release notes with
   tested hardware, known issues and migration requirements.
2. Pass Rust format/Clippy/tests, web unit/build/browser checks and Windows
   build/package/isolated-runtime checks.
3. Download `InPhase-windows-candidate` from that exact revision's Actions run.
   It includes the installer, checksum, binary manifest, notices and smoke report.
   Actions artifacts are candidates; end users download published release assets.

## Installed-build review

Record the exact revision and actual results. Do not mark untested hardware or
codecs as supported based on the presence of a plugin.

- Clean Windows x64 PC without developer tools; normal user, with both same-user
  elevation and separate administrator credentials.
- Fresh install, first launch, certificate trust, pairing, startup enabled and
  disabled, reboot, reinstall/upgrade, and uninstall cleanup.
- Upgrade from the shared CA and re-trust on a second device.
- At least one real host GPU. Broader vendor claims need NVIDIA, AMD and Intel
  runs with model/driver recorded. Exercise unsupported-encoder failure.
- Desktop/game launch, audio, keyboard/mouse, fullscreen exit, disconnect and
  reconnect, emergency stop and input release.
- Optional controller backend only with its driver and opt-in. State if untested.
- Separate LAN client, the browsers advertised, and both transports if claiming
  both. Include resolution, frame rate and codec.
- Remote access only if advertised: IPv6 reachability, mapping, pairing policy,
  loss and recovery. Loopback is not remote-network evidence.

See `tests/compatibility`, `tests/latency` and `tests/integration`. Performance
claims must include measurement conditions.

## Signing and source

`scripts/build-installer.ps1 -SignCmd '...'` can sign the host/installer with
maintainer Authenticode credentials. Keep credentials outside the repo. Verify
all signatures, including the uninstaller, then regenerate the final checksum.
The automatic candidate has no signing credentials and is unsigned; disclose it.

Before publishing binaries, review `MANIFEST.csv`, preserve exact notices, and
supply matching corresponding source for the app and bundled components whose
licenses require it. Include the `vendor/quinn-proto` patch, build scripts,
lockfiles, and exact GStreamer 1.28.6 source/build recipe and dependency revisions.
The repository's source ZIP does not contain the bundled runtime's source.
Do not claim a source archive/offer exists until the release actually includes it.

The license table is an inventory gate, not a compatibility proof. Review runtime
redistribution terms and codec rights. See NOTICE and the bundled license files.

## Publish

Tag the reviewed commit `vX.Y.Z`, matching Cargo.toml. The workflow reruns CI and
creates a draft. Inspect artifacts, signatures, checksum, matching source,
hardware evidence and known issues before publishing. Repository visibility is
a separate owner action.

Fix failures and rebuild; do not reuse an older EXE for a new revision or suppress
gates. Never replace a published binary silently; issue a new version.
