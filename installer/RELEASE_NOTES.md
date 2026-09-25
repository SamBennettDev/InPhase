# InPhase 0.2.0

Stream your Windows gaming PC to a browser on another screen: hardware
encoding on the PC, WebTransport and WebCodecs in the browser. No InPhase
account or hosted relay. Download `InPhaseSetup.exe` from this release's assets,
run setup on the gaming PC, then pair another device from the tray dashboard.

## Highlights

- **PIN guessing capped.** After 10 wrong PINs in a row, from
  anywhere, PIN pairing turns off until you choose **New PIN** on the PC. A
  guesser gets at most 10 tries at a PIN: a 1 in 100,000 chance. Wrong attempts show on the
  dashboard and in the host log.
- **Tighter device security.** A session cookie copied off a device no longer
  works from outside your network on its own; revoking a device ends its
  stream and its sessions; web pages that try DNS rebinding are refused; your
  game library is visible only to paired devices.
- **Close to 120 fps on iPhone.** Video is drawn through WebGL in Safari and
  every iOS browser: at 1440p120 about 112 frames a second reach the screen
  (was ~68), at 4K120 about 89 (was ~52).
  Turn off Settings → Apps → Safari → Advanced → Feature Flags → *Prefer Page
  Rendering Updates near 60fps* to let Safari run pages at 120 Hz.
- **Lower latency, steadier streams.** Frames are paced to their own interval,
  the host's own send delay dropped from ~9 ms to under 0.1 ms, lost packets
  are repaired on the network's round-trip schedule, and the bitrate reaches
  and holds the rate you set.
- **Lip sync.** Audio plays on the video's clock.
- **Graceful slow devices.** A device whose decoder cannot keep up steps down
  to 60 fps first, then to lower resolutions, instead of freezing.
- **Redesigned interface** with the new InPhase brand: a dashboard that leads
  with what the PC is doing, a library with your live desktop as a tile, and
  clearer pairing and error screens.
- **No console windows flashing** when the host starts.

## Before publishing this draft

This build is **unsigned** unless a maintainer signs and verifies the final
artifacts. Complete docs/RELEASING.md: attach matching corresponding source and
record clean-install, GPU, audio, input and network results.

Automated checks verify build, protocol/policy behavior, simulated browser flows
and packaged DLL/plugin loading. They do not verify real GPU streaming. This
release was tested by hand on one NVIDIA RTX 3070 host with Chrome, Firefox and
Safari on macOS, and Safari on iPhone.

## Requirements and limits

Windows 10 build 19041 or newer, or Windows 11, x64; a supported hardware encoder,
current driver and active desktop/display. Start with 1080p/60 on a trusted LAN.
The browser needs WebTransport and WebCodecs: current Chrome, Edge, Firefox, or
Safari on macOS and iOS. Virtual gamepad support requires a separate compatible
driver and input opt-in. Remote access is off by default and depends on
router/ISP reachability.

## Verify

Compare the installer SHA-256 with `InPhaseSetup.exe.sha256`. Keep the binary
manifest and component notices with the release. InPhase-owned code is
GPL-3.0-or-later; dependencies retain their own terms.
