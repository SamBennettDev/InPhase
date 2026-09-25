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
   at `%APPDATA%\InPhase\tls\ca.key`, cert at `ca.crt`.
2. It issues a **leaf cert** signed by that CA covering `<machine>.local`, the
   bare machine name, `localhost`, and every non-loopback LAN IP (395-day, auto
   re-issued when it nears expiry or the IP set changes).
3. It serves HTTPS (rustls via `axum-server`) on `tls.port` (default 443) on all
   interfaces, and checks certificate trust without changing the trust store at normal startup.
4. Other devices trust it once: **`GET /ca.crt`** (also served over plaintext
   HTTP so a phone can fetch it before it trusts anything), install, done.

The **installer** runs `inphase-host --trust-ca` as the original Windows user.
This explicitly adds the CA to that user's trust store (`certutil -user -addstore Root`).
The host and certificate setup must use the same user profile, including when
installation uses another administrator's credentials. On other devices the
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
  authenticates the link between your devices. Trust the initial CA only after verifying it came from your PC. Signaling JSON
  travels inside TLS; a compromised initial certificate exchange is outside that protection.
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
- **PIN guessing is bounded, not just slowed.** Wrong PINs are rate-limited
  (`[pairing] max_attempts` per `rate_window_secs`, counted globally and per
  /64), and after `lockout_after` wrong PINs in a row (default 10), from any
  source, PIN pairing is refused until the owner chooses **New PIN** on the
  dashboard, which is loopback-only. A guesser therefore gets at most 10 tries
  at any PIN: a 1 in 100,000 chance, however long they keep trying. The count
  survives restarts; wrong PINs are logged with their source and shown on the
  dashboard. Off the LAN, PIN and invitation pairing are HTTPS-only and need
  `allow_remote_pairing`.
- **Per-connect device challenge.** On every `/api/v1/signal` connection the host
  sends a random 32-byte nonce; the browser returns
  `{ controller_id, signature }` where the signature is Ed25519 over the nonce.
  The host verifies it against an **active** ACL entry before any media setup.
  Sessions are bound to the device keys that used them: off the LAN a session
  cookie only works with a key it has already been used with, so a cookie
  copied off a device is not enough on its own. A key the host has never seen
  is enrolled from a paired session only on the LAN (the Safari Home Screen
  and cleared-storage cases); elsewhere the device must be re-paired.
  Revoking a device revokes the sessions it used and closes its stream.
- **Host names.** The LAN listener answers only to IP literals, `localhost`,
  `.local` and `.ts.net` names and its own advertised names, so a web page that
  re-points its own domain at the host (DNS rebinding) is refused before any
  route, pairing included.
- **Input arming.** Input is dropped until the media path is up. The
  WebTransport connection is authorized by a single-use 60 s token minted over
  the authenticated signaling socket; ending a stream from the dashboard, or
  revoking a device, closes that connection on the host rather than asking the
  client to leave.
- **Emergency stop.** `Ctrl+Alt+Shift+F12` on the host cuts any active
  session regardless of network / browser state.
- **Admin surface.** `/api/v1/admin/*` is loopback-only (served on
  `127.0.0.1:admin_port` and, guarded, on the main listener). Loopback authority
  and browser Origin checks reject DNS rebinding and cross-origin administration.
  API responses are not cached. Non-browser clients without Origin still need
  all normal authentication and network checks.

## What this does not defend against

- A compromised host OS, or malicious code served from the host's own origin
  (the player bundle is embedded in the host binary — trust the binary).
- Someone you've added to your tailnet who also has a paired browser. Sharing a
  tailnet is sharing trust; revoke devices in the dashboard. The host treats
  `100.64.0.0/10` (Tailscale, and carrier-grade NAT) as local.
- Another Windows account on the same PC: the admin API trusts loopback.
- A stolen local CA key (`tls\ca.key`, DPAPI-protected for your Windows user).
  The CA is not name-constrained, so a device that trusts it would trust any
  certificate signed with that key.
- A paired device you no longer trust, until you revoke it.

## Out of scope (was in the SaaS design, now dropped)

The Noise `IK` end-to-end signaling channel, SPAKE2 short-code pairing, the
signaling-transcript hash, and the data-channel key confirmation all existed to
defend against an **InPhase-operated signaling relay**. With direct TLS to the
host there is no such relay, so they are not part of this design.
