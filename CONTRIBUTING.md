# Contributing

Small, focused pull requests are welcome. For a substantial feature, open an
issue describing the user problem before changing the media architecture.

Follow the build instructions in [README](README.md). Use npm and commit
`web/package-lock.json` when dependencies change. Keep `Cargo.lock` committed.
Build the web client before a release host build: it is embedded in the EXE.

Run Rust formatting, Clippy and tests, web unit tests and the production build.
UI changes should also run `npm run test:browser` in `web`; include desktop and
mobile screenshots and cover loading, empty, failure, busy and paired states.

Windows media work requires a real Windows host. Record the exact revision,
Windows version, GPU/driver, browser, resolution/FPS, codec, transport and network.
Do not generalize one successful machine into broad hardware support.

## Design boundaries

- Keep capture/encoding on the GPU, with bounded queues and explicit drops.
- Preserve the single-active-player and input-release invariants.
- Authenticate before accepting input or constructing media sessions.
- Keep secrets, cookies and private keys out of logs and screenshots.
- Prefer real browser/OS capability checks over user-agent guesses.
- Show failures clearly; only report success after the API succeeds.
- Preserve the player's chosen resolution and frame rate.

The ADRs document intentional choices. Update the relevant ADR and add a test
for the actual risk when changing behavior. Explain the problem, resulting
behavior and verification in your PR. Separate simulated tests from hardware runs.

Contributions use GPL-3.0-or-later. Preserve third-party license notices.
Be respectful and specific in reviews, welcome questions, and avoid personal
attacks. Maintainers may close abusive discussions.
