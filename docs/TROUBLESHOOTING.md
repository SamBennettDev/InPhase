# Troubleshooting

## Start with the host

Keep the PC awake, signed in, and attached to an active display. Open InPhase
from Start or the tray. Its dashboard is normally
`http://127.0.0.1:47800/?dashboard`; use your configured HTTP port if different.
Use the dashboard's **play address** on other devices. `127.0.0.1` always means
the device where the browser is running.

Logs: `%LOCALAPPDATA%\InPhase\host.log`. Configuration and paired-device data:
`%APPDATA%\InPhase`. From a terminal:

```powershell
& "$env:ProgramFiles\InPhase\InPhaseHost.exe" --doctor
& "$env:ProgramFiles\InPhase\InPhaseHost.exe" --support-bundle "$env:TEMP\inphase-support.json"
```

Review support bundles before sharing: they can identify hardware and networks.
Never share private keys, browser storage, session cookies or pairing codes.

## Installer or startup problems

- Download from Releases and compare the SHA-256 with the accompanying checksum.
  Read the signing status in the release notes.
- A missing-DLL dialog is a packaging problem. Report the DLL name, installer
  version and Windows build. Users should not need the developer runtime.
- If the host exits, inspect the log and run `--doctor`. Check that another
  application is not using the configured HTTPS/HTTP ports.
- Windows ARM and old Windows builds are outside the current installer target.

## Certificate migration

Earlier builds stored TLS material in `%ProgramData%\InPhase\tls` with broad
local-user write access. New setup uses `%APPDATA%\InPhase\tls` and protects
the CA private key for the Windows user running the host. It removes the legacy
shared directory and legacy machine-store InPhase CA.

After upgrading, remove the old **InPhase Local CA** from player devices and
follow **Certificate setup** again. Do not copy another user's `ca.key`: DPAPI
binds it to that user's profile. Restore a matching certificate/key backup if a
CA becomes unreadable, or deliberately reset trust and enroll devices again.
InPhase does not silently replace an unreadable CA.

Run setup as your normal Windows user, even if installation required another
administrator's credentials:

```powershell
& "$env:ProgramFiles\InPhase\InPhaseHost.exe" --trust-ca
```

Do not bypass certificate errors as a permanent fix. Browser cryptography and
several input/audio APIs need trusted HTTPS. If `.local` does not resolve, use
the host LAN address after ensuring its certificate covers that address.

## Offline or pairing problems

Check reachability, guest-Wi-Fi isolation and Windows Firewall. Do not expose the
admin API to the LAN. Use the current PIN; it changes after pairing or rotation.
Invitations expire and work once. The first invited device can require host
approval. Wait before retrying after repeated failed attempts.

If the player answers **"Pair on the same local network using the secure play
address from your PC"** even though you are on that address, the player is
reporting a 403 from `/api/v1/pair` and the message names only one of its causes.
Two are worth knowing:

- **A browser was refused as cross-origin.** Builds before the unreleased fix
  compared the `Origin` header against a `host` header that HTTP/2 does not send,
  so every browser POST was rejected before the PIN was compared — with no host
  log line at all, because the refusal happens in middleware. Native clients and
  HTTP/1.1 were unaffected. If the host log shows no `refused an off-network
  pairing attempt` and no `refused an off-network request` for your attempt, this
  is what you are hitting: upgrade the host.
- **Remote pairing is off and you are off the LAN.** `pairing_is_lan_only` is
  logged by the host, and `remote_access.allow_remote_pairing` turns it off.
  Pairing is LAN-only by design; pair on your own network and the device then
  authenticates from anywhere with the key it was issued.

To confirm which one, run this in the page's own console — it prints the host's
error body instead of the player's summary (a deliberately wrong PIN is fine: a
`bad_pin` answer proves the PIN path is reachable, `403` shows the gate):

```js
await fetch("/api/v1/pair", { method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ pin: "000000", controller_pubkey: "ab".repeat(32), controller_name: "probe" }) })
  .then(async r => r.status + " " + await r.text())
```

## Missing games, picture, sound or input

Open the launcher on the PC and reload the library. Stream **Whole desktop** for
games that are not discovered. Title cards are normal when cover art is absent.

Run `--doctor` and record the encoder, GPU/driver, browser and transport. Start
with 1080p/60 and H.264 if HEVC decoding is unavailable. A supported hardware
encoder and active display are required; there is no silent software fallback.

Select the right host audio source in Stream settings. Check browser mute and
device volume. Click the stream to focus it. Browser shortcut restrictions,
Windows secure screens, elevated games and protected input paths can limit input.

Controllers appear on the PC as a virtual Xbox 360 controller through the
ViGEmBus driver. Setup installs it when the **Controller support** task is ticked
(the default), and the dashboard shows **Controllers ready** or that the driver is
missing, with a link to install it. The virtual controller is plugged in the first
time a controller is used in a stream. Keyboard and mouse input do not depend on it.
See [ADR-0008](adr/0008-input-injection.md) for the trade-offs.

## The stream connects but the picture is black

Pairing, the player UI and audio working while the picture never appears means no
video frames are arriving. Video travels over WebTransport only, so this is one
of three host-side conditions. `/api/v1/status` and the dashboard's stream health
distinguish them.

**The frame sender died.** One task owns the frame queue and writes every frame,
so if it ever exits — a panic included — the video path is over for the life of
the process. Nothing else notices: the transport keeps accepting dials, the QUIC
handshake completes, and the client's control channel works, so sessions still
come up and are black. The host log says so in as many words:

```
ERROR wt: VIDEO SENDER TASK EXITED - no frame can be sent on this transport again
```

and `/api/v1/status` reports `wt.sender_alive: false`. The host rebuilds the
transport within 15 s (log: `wt: frame sender is dead - rebuilding the video
transport`), so the next session has video without restarting. If you are running
a build older than this behaviour, ending the stream and reconnecting is not
enough — restart the host.

A panic in this task was the reported "works for days, then black until I quit
and restart": a forced keyframe that arrived while the client's video channel was
still being installed hit `expect("stream just ensured")` and killed the sender.
The log shows it as `ERROR PANIC at ...media/wt/transport.rs: ... stream just
ensured`, 15 µs after `wt: video channel installed`. Fixed in the unreleased
build; report it if you still see the panic line.

**The picture keeps going soft, then freezing, then recovering.** A route with a
bandwidth limit but a deep buffer. The host log of such a session is a sawtooth
of `encoder bitrate up` / `encoder bitrate DOWN` between the floor and a ceiling
— 600 kbps to ~6 Mbps and back, every 15-20 s, with `lost=0` on every line — and
client telemetry shows `lat_p95` climbing 25 → 155 ms while `decoded_fps` falls
to 0 at the top of each ramp. Nothing is being dropped: the path is *queuing*,
and the host's own congestion signal for that is the client's fragment tail
delay, not loss. The host now remembers the rate that queued the route and holds
below it instead of ramping back into it. If it still sawtooths, the route really
is that slow — the settled rate in the log (`encoder bitrate ... to_kbps=…`) is
what it carries, so pick a mode that fits it (1080p60 needs far less than
1440p60) rather than expecting 1440p60 through a 6 Mbps uplink.

Note that the client's decoder runs on the page's main thread while the wire
(datagram drain, reassembly, repair) runs in a Worker; `decoded_fps` comes from
the page's counters pushed to that worker. A tick where those disagree used to
report 0 fps and then double on a healthy stream — that is fixed, but if you see
`decoded_fps` alternating with `held=8`, read `lat_p95` and `in_kbps` beside it
before believing the stall is the client's fault.

**The picture freezes a few seconds in, then never returns.** The log names it:

```
WARN the stream is arriving but nothing is decoding - waiting for a keyframe (receiving 56693 kbps, loss 0%)
```

Bytes are arriving and nothing decodes, because the client lost its anchor (an
IDR) and cannot get another. Deltas ride the datagram carrier, and forced IDRs
ride the reliable stream — and a write into a stream the client has stopped
reading succeeds *silently*. So the host reports perfect delivery while the
client asks for a keyframe every second and stays black for minutes: telemetry
shows `held=8 decoded=0` with `recv` still climbing at 60 fps, `in_kbps` in the
tens of Mbps, `nacks` frozen, and `keys` (keyframe requests) rising by one a
second; the host log shows `wt: client requested a keyframe` once a second and
**no** `frame write timed out` or `frames dropped` line. Reported at 1440p60
over the internet, stalling ~30 s in: the bitrate controller ramps towards the
route's ceiling, one loss burst costs the anchor, and recovery depended on the
one carrier that can fail this way. A keyframe that answers a client request now
rides the datagram carrier as well (with a stream copy), so the anchor no longer
depends on the channel; the log line is `wt: repair keyframe sent as datagram
fragments too`. If you still see the stall, the log's `wt: encoder produced a
keyframe` lines (present or absent) separate an encoder that ignores
`ForceKeyUnit` from a carrier that loses the frame.

**The pinned certificate ran out.** The WebTransport dial is authenticated by a
certificate hash handed to the client over the already-authenticated signaling
socket. Browsers refuse a certificate that is not valid *at dial time*, and cap
its validity at two weeks, so the host replaces the video certificate on a timer
days before it expires — the log says `wt: video transport rotated - clients
dial the new pin`, and `wt.cert_expires_in_secs` in `/api/v1/status` reports the
time left. If that number keeps falling without a rotation line, the host is
serving an expiring pin: restart it, then report the log. The player console
shows `WebTransport handshake failed`, and the dashboard reports the host
capturing normally while the client decodes nothing — not `capture-stalled`.

**The desktop was not being captured.** Windows renders nothing, and desktop
duplication delivers nothing, while the display is powered off or the session is
locked — the normal state of a gaming PC after a few idle days, and the reason
the first stream afterwards is black until the host is restarted. The dashboard
reports `capture-stalled` ("a session is live but the capture source is producing
no frames"). InPhase does not wake a sleeping display, so on a PC that streams
unattended, keep the display on and the session unlocked:

```powershell
powercfg /change monitor-timeout-ac 0
powercfg /change standby-timeout-ac 0
```

Also set Settings → Accounts → Sign-in options → *If you've been away, when
should Windows require you to sign in again?* to **Never**, and remove the screen
saver. The Windows secure desktop is not capturable by design, so a locked PC
never streams.

## Uninstall

Quit the host and uninstall from Windows Settings. Application files and firewall
rules are removed. User data and trust stores may belong to another account than
an elevated uninstaller. Remove the InPhase startup entry and **InPhase Local CA**
from the relevant user profile when no longer needed, and remove the CA from
player devices too. Preserve data only if planning to reinstall. Do not delete
unrelated certificates.
