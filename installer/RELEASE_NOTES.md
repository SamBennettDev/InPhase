LAN-first, browser-native gaming-PC remote play. One windowless tray binary on
the gaming PC — no cloud service, no accounts.

### Install

Download **`InPhaseSetup.exe`** and run it. It installs to `Program Files\InPhase`,
adds a "start at sign-in" entry, and trusts the bundled local CA on this PC so
HTTPS works with no warning. The host lives in the system tray — right-click for
status, the current pairing PIN, the remote-access / start-at-sign-in toggles,
the dashboard, and quit.

On another device: open the play URL shown on the dashboard, install the CA once
from `GET /ca.crt`, and enter the PIN.

### Notes

- **Unsigned.** Windows SmartScreen will warn on first run — "More info" →
  "Run anyway". A code-signing certificate is the next release blocker.
- Windows 10 20H1 (build 19041) or newer, x64, with an NVIDIA / AMD / Intel
  hardware H.264 / HEVC encoder.
- Remote access (direct global IPv6 + a PCP / NAT-PMP router pinhole) is **off
  by default**; enable it from the tray. Pairing stays LAN-only.

GPL-3.0-or-later. Bundles a license-clean GStreamer 1.28.x runtime — only the
plugins InPhase loads (`OPEN-SOURCE-COMPONENTS.txt` / `MANIFEST.csv` in the
install folder).
