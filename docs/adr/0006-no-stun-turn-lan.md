# ADR 0006 — No STUN/TURN in strict LAN mode

**Status:** accepted · **Report:** §8.1, §27

## Decision
In strict LAN mode: no TURN, no STUN. Explicitly override any library default
public STUN URI. Gather host ICE candidates only and prefer the interface that
reaches the browser. Filter link-local / disconnected / Hyper-V / VM / known
virtual adapters by default; expose an advanced interface override. Bind the
web/signaling server to LAN interfaces only and add a Private-profile /
LocalSubnet firewall rule.

## Why
Keeps media and signaling local, with no intermediate server — the
architecture's strongest simplicity property.

## Revisit trigger
Product scope expands beyond direct LAN / VPN reachability (Internet mode as a
separate product mode, not by contaminating the LAN path).
