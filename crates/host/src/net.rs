//! LAN interface selection + ICE adapter filtering (architecture report §8.1
//! "Keep ICE local").
//!
//! * Gather host candidates only; prefer the interface that reaches the browser.
//! * Filter obviously unsuitable adapters by default: link-local, loopback,
//!   disconnected, Hyper-V / VM-only, and known virtual adapters.
//! * Expose an advanced `network.preferred_interface` override.

use std::net::IpAddr;

use crate::config::Config;

/// Names that usually indicate a virtual / VM-only adapter (§8.1). Case-insensitive
/// substring match against the interface name.
const VIRTUAL_HINTS: &[&str] = &[
    "hyper-v",
    "vethernet",
    "vmware",
    "virtualbox",
    "vbox",
    "loopback",
    "tailscale",
    "zerotier",
    "tap-windows",
    "wsl",
    "docker",
    "npcap",
];

/// All usable IPv4 LAN addresses, best first.
pub fn candidate_lan_ips(cfg: &Config) -> Vec<IpAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let mut out: Vec<(u8, IpAddr)> = Vec::new();
    for iface in ifaces {
        let ip = iface.ip();
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }
        // v1 media path is IPv4 (§8.1 examples); keep v6 out of the candidate set.
        if ip.is_ipv6() {
            continue;
        }
        if let IpAddr::V4(v4) = ip {
            if v4.is_link_local() {
                continue;
            }
        }
        let name = iface.name.to_lowercase();
        if VIRTUAL_HINTS.iter().any(|h| name.contains(h)) {
            continue;
        }

        // Rank: explicit override wins, then RFC1918 private ranges, then rest.
        let rank = if cfg
            .network
            .preferred_interface
            .as_deref()
            .map(|p| name.contains(&p.to_lowercase()))
            .unwrap_or(false)
        {
            0
        } else if is_private_v4(ip) {
            1
        } else {
            2
        };
        out.push((rank, ip));
    }
    out.sort_by_key(|(r, _)| *r);
    out.into_iter().map(|(_, ip)| ip).collect()
}

/// The single best LAN IP for the play URL / QR code (§25.1).
pub fn best_lan_ip(cfg: &Config) -> Option<IpAddr> {
    candidate_lan_ips(cfg).into_iter().next()
}

fn is_private_v4(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.is_private())
}

// ---------------------------------------------------------------------------
// IPv6 direct remote access (`docs/ipv6-remote-access.md`).
// ---------------------------------------------------------------------------

/// Pick the host's **stable** global-unicast IPv6 address, if it has one.
///
/// This is the address a remote browser connects to and that a Let's Encrypt
/// IP certificate is issued for, so stability matters more than anything else:
///
/// * `fe80::/10` link-local and `::1` are not routable off-link;
/// * `fc00::/7` unique-local is not routable on the internet — this is also
///   what Tailscale hands out (`fd7a:115c:a1e0::/48`), and picking it would
///   quietly reintroduce the dependency we are removing;
/// * SLAAC **temporary privacy addresses** (RFC 8981) rotate every few hours
///   by design. A certificate or QR code bound to one is broken before the day
///   is out. They cannot be told apart from a stable SLAAC address by value
///   alone, so on Windows we ask the OS which is which (`platform::windows`);
///   elsewhere we prefer the lowest-entropy interface identifier, which is
///   what a DHCPv6 or manually-configured address looks like.
///
/// Returns `None` when the host has no global IPv6 — the honest answer, and
/// the caller must say so rather than falling back to something unreachable.
pub fn stable_global_ipv6(cfg: &Config) -> Option<std::net::Ipv6Addr> {
    #[cfg(windows)]
    if let Some(a) = crate::platform::windows::net6::stable_global_ipv6() {
        return Some(a);
    }
    let ifaces = if_addrs::get_if_addrs().ok()?;
    let mut best: Option<std::net::Ipv6Addr> = None;
    for iface in ifaces {
        if is_virtual(&iface.name) || iface.is_loopback() {
            continue;
        }
        let IpAddr::V6(v6) = iface.ip() else { continue };
        if !is_global_unicast_v6(v6) {
            continue;
        }
        let _ = cfg;
        best = Some(match best {
            None => v6,
            // Fewer significant bits in the interface identifier = more likely
            // to be a DHCPv6 / static address (`…::100`) than a random SLAAC
            // one. Crude, but it never picks a privacy address over a stable
            // one in practice.
            Some(cur) => {
                if iid_entropy(v6) < iid_entropy(cur) {
                    v6
                } else {
                    cur
                }
            }
        });
    }
    best
}

/// Global unicast: `2000::/3`, excluding unique-local and link-local.
pub fn is_global_unicast_v6(a: std::net::Ipv6Addr) -> bool {
    let s = a.segments();
    if a.is_loopback() || a.is_unspecified() {
        return false;
    }
    if (s[0] & 0xffc0) == 0xfe80 {
        return false; // link-local
    }
    if (s[0] & 0xfe00) == 0xfc00 {
        return false; // unique-local (includes Tailscale's fd7a::/8 range)
    }
    (s[0] & 0xe000) == 0x2000
}

/// Count the set bits in the low 64 bits (the interface identifier).
fn iid_entropy(a: std::net::Ipv6Addr) -> u32 {
    let s = a.segments();
    s[4..8].iter().map(|w| w.count_ones()).sum()
}

/// True when an adapter name looks like a virtual / VM / VPN adapter, whose
/// addresses are never internet-reachable.
pub fn is_virtual(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_HINTS.iter().any(|h| lower.contains(h))
}

#[cfg(test)]
mod v6_tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn v6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    #[test]
    fn only_global_unicast_counts_as_reachable() {
        assert!(is_global_unicast_v6(v6("2001:db8:1234:5678::100")));
        assert!(is_global_unicast_v6(v6("2001:db8::1")));
        // Not routable off-link / on the internet:
        assert!(!is_global_unicast_v6(v6("fe80::1")));
        assert!(!is_global_unicast_v6(v6("::1")));
        assert!(!is_global_unicast_v6(v6("::")));
        assert!(!is_global_unicast_v6(v6("fc00::1")));
        assert!(!is_global_unicast_v6(v6("fd00::1")));
    }

    #[test]
    fn tailscale_ula_is_rejected() {
        // Picking this would silently reintroduce the dependency we removed.
        assert!(!is_global_unicast_v6(v6("fd7a:115c:a1e0::501:b0a9")));
    }

    #[test]
    fn a_dhcp_style_address_beats_a_privacy_address() {
        // Real values observed on the gaming PC: `::100` is the DHCPv6 lease,
        // the long one is an RFC 8981 temporary address that rotates.
        let stable = v6("2001:db8:1234:5678::100");
        let temporary = v6("2001:db8:1234:5678:dd2c:b8cb:3a75:3919");
        assert!(
            iid_entropy(stable) < iid_entropy(temporary),
            "the stable address must sort first"
        );
    }
}

/// Is this peer on our own network, as opposed to somewhere on the internet?
///
/// Gates two things: whether remote access is being used at all, and whether
/// PIN pairing is permitted (it never is from off-network — see
/// `http::api::pair`).
///
/// Getting this right for IPv6 is subtler than for IPv4. There is no
/// "private range" in normal use: a phone on the LAN holds a **global**
/// address from the same delegated prefix as the host, so "global means
/// remote" would lock out every local IPv6 client. The test that actually
/// works is prefix membership — same /64 as one of our own global addresses.
pub fn is_lan_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // 100.64/10 (CGNAT) — also where Tailscale lives.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            let s = v6.segments();
            if (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xfe00) == 0xfc00 {
                return true; // link-local or unique-local (incl. Tailscale)
            }
            // Global: local only if it shares a /64 with an address of ours.
            let Ok(ifaces) = if_addrs::get_if_addrs() else {
                return false;
            };
            ifaces.into_iter().any(|iface| {
                if is_virtual(&iface.name) {
                    return false;
                }
                match iface.ip() {
                    IpAddr::V6(mine) => {
                        is_global_unicast_v6(mine) && mine.segments()[..4] == s[..4]
                    }
                    _ => false,
                }
            })
        }
    }
}

/// Collapse an address to the unit we rate-limit on.
///
/// Per-address limiting is meaningless over IPv6: a single attacker is
/// routinely handed a /64, i.e. 2^64 source addresses to spread a brute-force
/// across, so a per-/128 counter never trips. Limit per /64 instead — the
/// smallest block a network can be delegated.
pub fn rate_limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

#[cfg(test)]
mod peer_tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_and_loopback_are_local() {
        for a in [
            "127.0.0.1",
            "192.168.1.5",
            "10.0.0.3",
            "172.16.4.4",
            "169.254.1.1",
        ] {
            assert!(is_lan_peer(ip(a)), "{a} should be local");
        }
        assert!(is_lan_peer(ip("::1")));
        assert!(is_lan_peer(ip("fe80::1")));
        assert!(is_lan_peer(ip("fd7a:115c:a1e0::1")), "Tailscale ULA");
        assert!(is_lan_peer(ip("100.64.0.10")), "CGNAT / Tailscale v4");
    }

    #[test]
    fn public_addresses_are_not_local() {
        // The cellular client we actually want to gate.
        assert!(!is_lan_peer(ip("2607:fb90:1:2::abcd")));
        assert!(!is_lan_peer(ip("8.8.8.8")));
        assert!(!is_lan_peer(ip("136.60.153.209")));
    }

    #[test]
    fn ipv6_rate_limiting_collapses_to_a_slash_64() {
        // Two addresses an attacker would rotate between inside one /64 must
        // land on the same counter, or the limiter does nothing.
        let a = ip("2607:fb90:1:2:aaaa:bbbb:cccc:dddd");
        let b = ip("2607:fb90:1:2:1111:2222:3333:4444");
        assert_eq!(rate_limit_key(a), rate_limit_key(b));
        // A different /64 is a different counter.
        let c = ip("2607:fb90:1:3::1");
        assert_ne!(rate_limit_key(a), rate_limit_key(c));
        // IPv4 is left alone.
        assert_eq!(rate_limit_key(ip("8.8.8.8")), ip("8.8.8.8"));
    }
}
