# IPv6 direct remote access

InPhase is **LAN-first**. Remote play over the internet is opt-in, layered, and
deliberately narrow: the host can be *reachable* from the internet without being
*open* to it.

## Two independent switches

| Setting | Default | What it does |
|---------|---------|--------------|
| `[remote_access].enabled` | `false` | Off-network clients get **403** on every HTTP route (player, pairing, signaling). LAN peers (including same-prefix IPv6) always pass. |
| `[remote_access].allow_remote_pairing` | `false` | Off-network clients cannot enroll a new browser — no PIN, no invite. When `true`, invites work from anywhere, and a PIN is accepted from off-network **over HTTPS only** (plaintext attempts get `pin_requires_https`). A device already paired on the LAN always authenticates remotely (Ed25519 device key in the controller ACL). |

**Pair on the LAN once; play from anywhere later** is the intended flow. A
six-digit PIN is fine against someone already on your network; on the open
internet it is a speed bump, so remote PIN pairing exists only behind the
explicit opt-in above — and never on plaintext HTTP.

Implementation: `crates/host/src/http/mod.rs::remote_access_gate`,
`crates/host/src/http/api.rs::pair`, and the integration tests in
`crates/host/tests/remote_access.rs`.

## How “LAN” is detected for IPv6

IPv4 is straightforward (RFC1918, loopback, link-local, CGNAT `100.64/10`).

IPv6 is not: phones on the same Wi‑Fi often hold **global** addresses from the
same delegated `/64` as the host. InPhase treats a peer as local when it shares
a `/64` with one of the host’s own global unicast addresses — see
`crates/host/src/net.rs::is_lan_peer`.

Rate limiting uses `/64` keys for IPv6 (not `/128`), because an attacker with a
delegated prefix can spread guesses across 2⁶⁴ source addresses.

## Stable global IPv6

For ACME and the play URL, the host must pick an address that will still be
valid tomorrow:

- Reject link-local, loopback, and unique-local (`fc00::/7`, including
  Tailscale’s `fd7a::/8`).
- Prefer a **stable** SLAAC/DHCPv6 address over RFC 8981 privacy/temporary
  addresses (on Windows the OS is queried; elsewhere lowest interface-ID entropy
  is used as a heuristic).

See `crates/host/src/net.rs::stable_global_ipv6`.

## Publicly trusted certs for a bare IP (`[acme]`)

Let's Encrypt issues short-lived certificates for IP addresses (v4 and v6). When
`[acme].enabled = true`:

1. The host selects its stable global address (or `[acme].address` override).
2. A background task obtains a cert via **tls-alpn-01** (`crates/host/src/acme/`).
   HTTP-01 is avoided: off-network HTTP is refused while remote access is off,
   but the CA must still reach the host for validation.
3. Browsers open `https://[2605:…::100]/` with a padlock — no per-device CA
   install.

Constraints:

- **160-hour lifetime** (`shortlived` profile) — renewal is load-bearing.
- **The address is published** to Certificate Transparency logs within minutes.
  “Security through obscurity” does not apply; rely on `remote_access_gate` and
  LAN-only pairing instead.
- **Staging is the default** (`[acme].staging = true`) until you deliberately
  switch to production.
- **You accept Let's Encrypt's terms.** Obtaining a certificate creates an ACME
  account on your behalf, which accepts the Let's Encrypt Subscriber Agreement
  (https://letsencrypt.org/repository/). Leave remote access off if you do not
  want that.

Pair with `[remote_access].enabled = true` only after proving issuance on
staging.

## Built-in WireGuard VPN (alternative path)

`[vpn]` (`crates/host/src/vpn/`) issues per-device WireGuard profiles on the
`*.inphase.internal` reserved TLD, with a pinned MTU sized for encapsulated RTP.
Use this when you want remote access without exposing the host’s HTTPS surface to
the public internet, or when your ISP does not provide a stable global IPv6
prefix.

## Operator checklist

1. Pair every browser **on the LAN** (PIN or QR invite).
2. Decide: **public IPv6 + ACME** or **WireGuard/Tailscale** — not both required.
3. If using public IPv6: enable `[remote_access].enabled`, prove ACME on staging,
   then production.
4. Leave `allow_remote_pairing = false` unless devices must be able to enroll
   themselves from outside the network: a one-time invite, or a PIN over HTTPS
   if you accept a six-digit code as an internet-facing credential.
5. On the phone: use the HTTPS play URL (or install the local CA if using
   `tls.mode = local-ca` on a private LAN).
