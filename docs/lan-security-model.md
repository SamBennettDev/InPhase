# InPhase LAN security model

InPhase is an open-source, all-in-one host: one binary on the gaming PC that
serves the player, the signaling WebSocket, and the WebRTC media, all reached
directly by a browser on the same LAN / Tailscale tailnet. There is no cloud
service in the path.

## Why HTTPS is mandatory

The browser player needs a **secure context**, and several of the APIs it
depends on are gated on it:

| API | Used for |
|---|---|
| `crypto.subtle` (Ed25519) | the per-connect device challenge (below) |
| Keyboard Lock (`navigator.keyboard.lock`) | Esc / Tab / Meta passthrough in fullscreen games |
| `navigator.mediaDevices`, `setSinkId` | the audio-output device picker |
| Gamepad API (`navigator.getGamepads`) | controller input (Chrome) |

`http://localhost` is a secure context, but only for a browser running *on the
host itself*. A remote LAN client needs real HTTPS on a name the browser trusts.

## How the host gets a cert

`crates/host/src/tls.rs`. `[tls].mode`:

### `local-ca` (default)

1. The host generates a **local CA** once — a self-signed root, key DPAPI-wrapped
   at `%ProgramData%\InPhase\tls\ca.key`, cert at `ca.crt`.
2. It issues a **leaf cert** signed by that CA covering `<machine>.local`, the
   bare machine name, `localhost`, and every non-loopback LAN IP (395-day, auto
   re-issued when it nears expiry or the IP set changes).
3. It serves HTTPS (rustls via `axum-server`) on `tls.port` (default 443) on all
   interfaces, and adds the CA to the OS trust store (`certutil -addstore Root`).
4. Other devices trust it once: **`GET /ca.crt`** (also served over plaintext
   HTTP so a phone can fetch it before it trusts anything), install, done.

The **installer** runs `inphase-host --trust-ca` elevated during install, so on
the host PC itself there is no warning and no manual step. On other devices the
dashboard's "Play from another device" card links the certificate + per-OS steps
(`web/src/main.ts` also shows this if you hit the host over plain HTTP).

### `off`

Plaintext HTTP only (dev / loopback). Secure-context APIs are unavailable, so
the player cannot pair.

`domain` overrides the hostname in the URL; `port` the HTTPS listener;
`extra_sans` adds names/IPs to the local-ca leaf. A plaintext HTTP listener on
`network.http_port` is always up for `/ca.crt` and the dashboard.

## Trust model

- **Transport.** TLS to the host's own origin (real cert) — the browser
  authenticates the host. WireGuard (Tailscale) additionally encrypts and
  authenticates the link between your devices. There is no third party to
  man-in-the-middle, so signaling frames are plain JSON.
- **Host identity.** A long-term Ed25519 key generated on first run,
  DPAPI-wrapped at rest (`%APPDATA%\InPhase\host-identity.key`). Its public half
  is the **Host ID**, shown in `/api/v1/status`.
- **Controller identity.** Each browser profile holds a
  non-extractable Ed25519 key in IndexedDB. Its public half is the controller id
  the host stores in `controllers.json` (the ACL). The list is written only by
  the host.
- **Pairing.** Either the Host-screen **PIN**, or a one-time **invitation**
  (256-bit secret shown as a QR fragment, or its 9-char short code) minted at
  `POST /api/v1/admin/pair-invite`. The browser presents it over the host's
  HTTPS along with its controller public key; the host adds the key to the ACL
  and sets a `Secure; HttpOnly` session cookie. The **first** controller on a
  fresh host also needs an explicit **Approve** click on the dashboard. No PAKE
  or HMAC proof — the TLS channel is already confidential and host-authenticated,
  so holding the secret within its 120 s TTL is the proof.
- **Per-connect device challenge.** On every `/api/v1/signal` connection the host
  sends a random 32-byte nonce; the browser returns
  `{ controller_id, signature }` where the signature is Ed25519 over the nonce.
  The host verifies it against an **active** ACL entry before any media setup.
  This binds the session to a specific paired device, not just cookie
  possession.
- **Input arming.** Input packets are dropped until the WebRTC media path is up
  (`session_ready`). Because the SDP + DTLS fingerprints were exchanged over the
  authenticated signaling channel, the DTLS peer is inherently the authenticated
  peer — no separate data-channel proof is needed.
- **Emergency stop.** `Ctrl+Alt+Shift+F12` on the host cuts any active
  session regardless of network / browser state.
- **Admin surface.** `/api/v1/admin/*` is loopback-only (served on
  `127.0.0.1:admin_port` and, guarded, on the main listener).

## What this does not defend against

- A compromised host OS, or malicious code served from the host's own origin
  (the player bundle is embedded in the host binary — trust the binary).
- Someone you've added to your tailnet who also has a paired browser. Sharing a
  tailnet is sharing trust; revoke devices in the dashboard.

## Out of scope (was in the SaaS design, now dropped)

The Noise `IK` end-to-end signaling channel, SPAKE2 short-code pairing, the
signaling-transcript hash, and the data-channel key confirmation all existed to
defend against an **InPhase-operated signaling relay**. With direct TLS to the
host there is no such relay, so they are not part of this design.
