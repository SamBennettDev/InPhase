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

## Missing games, picture, sound or input

Open the launcher on the PC and reload the library. Stream **Whole desktop** for
games that are not discovered. Title cards are normal when cover art is absent.

Run `--doctor` and record the encoder, GPU/driver, browser and transport. Start
with 1080p/60 and H.264 if HEVC decoding is unavailable. A supported hardware
encoder and active display are required; there is no silent software fallback.

Select the right host audio source in Stream settings. Check browser mute and
device volume. Click the stream to focus it. Browser shortcut restrictions,
Windows secure screens, elevated games and protected input paths can limit input.

Gamepad emulation requires a compatible ViGEmBus driver and the opt-in in
[ADR-0008](adr/0008-input-injection.md). The installer does not install a kernel
driver. Keyboard/mouse input is independent of that optional backend.

## Uninstall

Quit the host and uninstall from Windows Settings. Application files and firewall
rules are removed. User data and trust stores may belong to another account than
an elevated uninstaller. Remove the InPhase startup entry and **InPhase Local CA**
from the relevant user profile when no longer needed, and remove the CA from
player devices too. Preserve data only if planning to reinstall. Do not delete
unrelated certificates.
