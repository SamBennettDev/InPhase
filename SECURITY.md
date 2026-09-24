# Security

InPhase controls a logged-in Windows desktop. Only pair devices you trust.
Keep remote access disabled unless you need and understand it. The current
early-access version is the focus of maintenance; older versions have no promised
support window.

## Report privately

Do not post exploitable vulnerabilities, private keys, PINs or cookies in a public
issue. Use **Report a vulnerability** on this repository's Security tab if
available. Otherwise contact the maintainer at the address in [NOTICE](NOTICE)
and request a private channel.

Include the affected revision, prerequisites, impact and minimal reproduction.
Use throwaway test devices and redact credentials. No response-time or
bug-bounty commitment is currently made.

Pairing, authentication, origin validation, input authorization, key storage and
release integrity are in scope. See the [security model](docs/lan-security-model.md)
and [audit](docs/reviews/AUDIT-2026-09-14.md).
