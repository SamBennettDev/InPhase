# InPhase 0.3.1

**Fixes input in 0.3.0.** In 0.3.0 a stream could show the PC but ignore your
keyboard, mouse, touch and controller for the whole session, because the host
missed the moment the video connection came up. Update if you installed 0.3.0;
installing over it keeps your settings and paired devices.

Everything below is from 0.3.0.

---


Stream your Windows gaming PC to a browser on another screen: hardware
encoding on the PC, WebTransport and WebCodecs in the browser. No InPhase
account or hosted relay. Download `InPhaseSetup.exe` from this release's assets,
run setup on the gaming PC, then pair another device from the tray dashboard.

Website: https://inphase.sam-bennett.dev

## Highlights

- **Controllers out of the box.** Setup offers to install the free ViGEmBus
  driver (ticked by default, skipped if you already have it), and a controller
  on your phone or laptop shows up on the PC as an Xbox 360 controller. The
  dashboard says whether controllers are ready.
- **A live desktop in the library.** The desktop tile is the real stream at
  tile size, about 30 fps, and hands over instantly when you press Stream.
- **Close to 120 fps on iPhone.** Video is drawn through WebGL in Safari and
  every iOS browser: at 1440p120 about 112 frames a second reach the screen
  (was ~68). Turn off Settings → Apps → Safari → Advanced → Feature Flags →
  *Prefer Page Rendering Updates near 60fps* to let Safari run pages at 120 Hz.
- **Lower latency, steadier streams.** Frames are paced to their own interval,
  the host's own send delay dropped from ~9 ms to under 0.1 ms, lost packets
  are repaired on the network's round-trip schedule, and the bitrate reaches
  and holds the rate you set. Audio plays on the video's clock.
- **Graceful slow devices.** A device whose decoder cannot keep up steps down
  to 60 fps first, then to lower resolutions, instead of freezing.
- **Security.** After 10 wrong PINs in a row PIN pairing locks until you choose
  New PIN on the PC; sessions are bound to device keys; revoking a device ends
  its stream; DNS-rebinding pages are refused; your library is visible only to
  paired devices.
- **Fixed:** the host no longer crashes when it cannot capture the screen (a
  locked PC or a UAC prompt); the player is told why. No console windows flash
  at startup.
- **New look:** a redesigned dashboard, library and player with the new InPhase
  brand.

## Before you install

This build is **unsigned**, so Windows SmartScreen shows "Windows protected your
PC": choose **More info → Run anyway**. Compare the installer's SHA-256 with
`InPhaseSetup.exe.sha256` from this release.

Tested by hand on one NVIDIA RTX 3070 host with Chrome, Firefox and Safari on
macOS, and Safari on iPhone. Automated checks cover the build, protocol and
policy logic, simulated browser flows and the packaged runtime; they do not
cover real GPU streaming on other hardware.

## Requirements and limits

Windows 10 build 19041 or newer, or Windows 11, x64, with a hardware video
encoder (NVIDIA, AMD or Intel), a current driver and the PC awake and signed in.
The player needs a browser with WebTransport and WebCodecs: current Chrome,
Edge, Firefox, or Safari on macOS and iOS. Start with 1080p/60 on your home
network. Remote access is off by default and depends on router/ISP
reachability.

InPhase is provided as is, without warranty (GPL-3.0). Some games' anti-cheat
or terms of service don't allow remote input or virtual controllers; see the
README's Disclaimer.

## Verify

Keep the binary manifest and component notices with the release. InPhase-owned
code is GPL-3.0-or-later; dependencies retain their own terms.
