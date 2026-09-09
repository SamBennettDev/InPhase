# InPhase

LAN-first, browser-native gaming-PC remote play. Capture a Windows monitor,
hardware-encode it, send it over WebRTC to a browser, and send input back — with
low latency, high visual quality and near-zero idle cost.

**One open-source binary on the gaming PC.** No cloud service, no accounts. The
browser reaches the host directly over your LAN (or, opt-in, over direct global
IPv6); the host serves its own HTTPS — needed for the browser's secure-context
APIs — with a **bundled local CA**. The installer trusts it on the host PC;
other devices install it once from `GET /ca.crt`. See
[`docs/lan-security-model.md`](docs/lan-security-model.md).

## Architecture in one sentence

> A per-user Rust Windows application hosts a tiny same-origin web client and an
> authenticated WebRTC signaller; it lazily creates a GStreamer pipeline that
> captures a monitor as D3D11 textures, hardware-encodes into HEVC (or H.264
> when the browser can't receive HEVC), sends video + Opus audio over
> `webrtcbin` with an application-level AIMD bitrate controller driven by client
> telemetry, receives low-latency binary input over a data channel, translates
> it to Windows input, and tears the media/input pipeline back down when the
> player disconnects.

```
crates/
  protocol/   platform-neutral wire protocol (binary input packet v1 + JSON
              signaling). Golden Rust<->TS test vectors. Builds/tests anywhere.
  host/       the Windows host. GStreamer 1.28 + Win32.
              app · config · http · pairing · session · media · input · stats ·
              portmap · platform
web/          vanilla-TS player page + host dashboard (Vite). Embedded in the exe.
scripts/      provision-windows · build · package · build-installer · smoke-gstreamer
docs/         architecture overview, ADRs, the LAN security model, IPv6 remote access
```

The design report in `docs/research/` is the source of truth;
`docs/architecture/overview.md` is a map into the code. Section references in the
code (`§5.1`, `§12.2`, …) point back to the report.

## Remote access (opt-in)

`[remote_access]` in `config.toml` is **off by default**. When enabled, the host
also answers on its stable global IPv6 address, and the port-mapping task asks
the router (PCP / NAT-PMP) to open an inbound pinhole for TCP 443. Pairing stays
LAN-only unless `allow_remote_pairing` is also set — a device is enrolled once on
the LAN and afterwards authenticates remotely with a non-extractable per-browser
key. See [`docs/ipv6-remote-access.md`](docs/ipv6-remote-access.md).

## Build (on the Windows host)

```powershell
# one-time: Rust MSVC, VS BuildTools, Node LTS, GStreamer 1.28.x (complete)
pwsh scripts\provision-windows.ps1

# build web client + host
pwsh scripts\build.ps1 -Release

# self-contained folder with a license-clean bundled GStreamer runtime
pwsh scripts\package.ps1            # -> dist\InPhase\

# Inno Setup installer
pwsh scripts\build-installer.ps1    # -> dist\InPhaseSetup.exe
```

`protocol` and the host library build and test on any platform (the Windows
media stack compiles to inert stubs):

```
cargo test --workspace
cd web && npm ci && npm test && npm run build
```

## Run

```powershell
target\release\inphase-host.exe
# open the printed play URL in Chrome/Edge on another LAN machine and
# enter the PIN shown in the log / on the localhost dashboard.

inphase-host --doctor       # environment preflight
inphase-host --print-config # write a config template
inphase-host --net-probe    # gateway + PCP/NAT-PMP diagnostic
```

The host runs as a **windowless system-tray application**. Right-click the tray
icon for status, to toggle remote access or start-at-sign-in, to open the
dashboard, or to quit — no scripts, no config edits. The icon is electric blue
while a session is streaming and dim otherwise.

The play page shows a poster grid of your installed games (Steam, Epic, GOG,
Xbox, Battle.net, EA, Ubisoft, …). Covers come from each launcher's own cache
first; anything missing is filled in from Steam's public store API + CDN (no API
key) and cached under `%APPDATA%\InPhase\artcache\`. Set `[library] online_art =
false` for a fully offline host.

Config lives at `%APPDATA%\InPhase\config.toml`. Nothing secret is stored — the
PIN and the session cookie are memory-only; the per-browser controller key is
non-extractable and lives in the browser.

## Licensing

InPhase is **GPL-3.0-or-later** — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Same license as Sunshine and Moonlight. As sole copyright holder, Sam Bennett
retains the right to offer the code under other terms. Early versions were
briefly published under MPL-2.0; those grants stand for the versions released
under them.

Packaged builds dynamically link **GStreamer 1.28.x (LGPL-2.1-or-later)** and
link OpenSSL, libsrtp, libnice, libopus and other BSD-family media libraries,
plus ~390 Rust crates (mostly MIT / Apache-2.0). No GPL-incompatible component
is used or shipped; a per-file SPDX manifest ships with each packaged build.

**Codecs:** the host prefers **HEVC (H.265)** when the browser advertises an
HEVC receive codec and falls back to **H.264** automatically;
`media.allow_hevc = false` forces H.264 only. Both are covered by patent pools
(HEVC's more onerously). *Distributing* binaries that encode or decode them may
carry licensing obligations this software license cannot grant — get your own
advice before publishing release binaries. None of this is legal advice.
