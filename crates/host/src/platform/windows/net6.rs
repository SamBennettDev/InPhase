// SPDX-License-Identifier: GPL-3.0-or-later
//! Picking a **stable** global IPv6 address on Windows.
//!
//! Windows runs RFC 8981 temporary addresses by default, so an interface
//! typically carries several global addresses at once and only some are safe to
//! publish. On the machine this was developed against:
//!
//! ```text
//! 2605:…:2100:dd2c:b8cb:3a75:3919  SuffixOrigin=Random   <- rotates, unusable
//! 2605:…:2100:bdfc:6244:3032:261f  SuffixOrigin=Link     <- stable
//! 2605:…:2100::100                 SuffixOrigin=Dhcp     <- stable, and short
//! ```
//!
//! The value alone cannot tell them apart, so we ask the OS: `GetAdaptersAddresses`
//! reports a `SuffixOrigin` per address. Anything `Random` is rejected outright
//! — a certificate or QR code bound to one is dead within hours.
//!
//! Preference order is Dhcp > Manual > Link (EUI-64) — most to least likely to
//! survive a reboot and a router restart.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};
// The suffix-origin constants live in WinSock, not IpHelper.
use windows::Win32::Networking::WinSock::{
    IpSuffixOriginDhcp, IpSuffixOriginLinkLayerAddress, IpSuffixOriginManual, IpSuffixOriginRandom,
    AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6,
};

/// Rank a suffix origin; lower is better. `None` = never use this address.
fn rank(suffix_origin: i32) -> Option<u8> {
    match suffix_origin {
        x if x == IpSuffixOriginDhcp.0 => Some(0),
        x if x == IpSuffixOriginManual.0 => Some(1),
        x if x == IpSuffixOriginLinkLayerAddress.0 => Some(2),
        // RFC 8981 temporary address: rotates by design.
        x if x == IpSuffixOriginRandom.0 => None,
        _ => Some(3),
    }
}

/// The best stable global-unicast IPv6 address, or `None` if the host has no
/// usable one (no IPv6 service, or only privacy addresses).
pub fn stable_global_ipv6() -> Option<Ipv6Addr> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u8> = Vec::new();

    // The documented two-call pattern: grow until it stops overflowing.
    for _ in 0..4 {
        buf.resize(size as usize, 0);
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET6.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc == ERROR_SUCCESS.0 || rc == NO_ERROR.0 {
            return best_from(&buf);
        }
        if rc != ERROR_BUFFER_OVERFLOW.0 {
            tracing::debug!(rc, "GetAdaptersAddresses failed");
            return None;
        }
    }
    None
}

fn best_from(buf: &[u8]) -> Option<Ipv6Addr> {
    let mut best: Option<(u8, Ipv6Addr)> = None;
    let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;

    while !adapter.is_null() {
        let a = unsafe { &*adapter };
        // Skip adapters that are not up, and tunnels/virtual adapters — a
        // Tailscale or Hyper-V address is not internet-reachable.
        let name = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
        let skip = crate::net::is_virtual(&name);

        if !skip {
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u = unsafe { &*ua };
                if let Some(v6) = sockaddr_to_v6(u.Address.lpSockaddr as *const SOCKADDR_IN6) {
                    if crate::net::is_global_unicast_v6(v6) {
                        if let Some(r) = rank(u.SuffixOrigin.0) {
                            if best.as_ref().is_none_or(|(br, _)| r < *br) {
                                best = Some((r, v6));
                            }
                        }
                    }
                }
                ua = u.Next;
            }
        }
        adapter = a.Next;
    }
    best.map(|(_, a)| a)
}

fn sockaddr_to_v6(sa: *const SOCKADDR_IN6) -> Option<Ipv6Addr> {
    if sa.is_null() {
        return None;
    }
    let s = unsafe { &*sa };
    if s.sin6_family != AF_INET6 {
        return None;
    }
    Some(Ipv6Addr::from(unsafe { s.sin6_addr.u.Byte }))
}

/// A default-route next hop the host could send a PCP / NAT-PMP request to.
#[derive(Debug, Clone)]
pub struct Gateway {
    pub ip: IpAddr,
    /// Interface index — needed to give a link-local IPv6 gateway a usable
    /// scope (`fe80::…%<idx>`). 0 for global addresses.
    pub scope_id: u32,
    /// A same-interface unicast address of ours, used as the PCP "client
    /// address" (for an IPv6 pinhole this is the address the pinhole is for).
    pub local: IpAddr,
}

/// Every adapter's default gateway(s), v4 and v6, paired with one of our own
/// addresses on that adapter. Virtual / tunnel adapters are skipped — a
/// Tailscale or Hyper-V gateway is not the way out to the internet.
pub fn default_gateways() -> Vec<Gateway> {
    // `FirstGatewayAddress` stays null unless GAA_FLAG_INCLUDE_GATEWAYS is set.
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u8> = Vec::new();
    for _ in 0..4 {
        buf.resize(size as usize, 0);
        let rc = unsafe {
            GetAdaptersAddresses(
                0, // AF_UNSPEC — both families
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc == ERROR_SUCCESS.0 || rc == NO_ERROR.0 {
            return gateways_from(&buf);
        }
        if rc != ERROR_BUFFER_OVERFLOW.0 {
            tracing::debug!(rc, "GetAdaptersAddresses (gateways) failed");
            return Vec::new();
        }
    }
    Vec::new()
}

fn gateways_from(buf: &[u8]) -> Vec<Gateway> {
    let mut out = Vec::new();
    let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let a = unsafe { &*adapter };
        adapter = a.Next;

        let name = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
        tracing::trace!(adapter = %name, has_gateway = !a.FirstGatewayAddress.is_null(), "scanning adapter");
        if crate::net::is_virtual(&name) {
            continue;
        }

        // Our own addresses on this adapter, split by family.
        let (mut v4_local, mut v6_local) = (None, None);
        let mut ua = a.FirstUnicastAddress;
        while !ua.is_null() {
            let u = unsafe { &*ua };
            ua = u.Next;
            let sa = u.Address.lpSockaddr;
            if sa.is_null() {
                continue;
            }
            match unsafe { (*sa).sa_family } {
                f if f == AF_INET && v4_local.is_none() => {
                    let s = unsafe { &*(sa as *const SOCKADDR_IN) };
                    v4_local = Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(unsafe {
                        s.sin_addr.S_un.S_addr
                    }))));
                }
                f if f == AF_INET6 && v6_local.is_none() => {
                    if let Some(v6) = sockaddr_to_v6(sa as *const SOCKADDR_IN6) {
                        if crate::net::is_global_unicast_v6(v6) {
                            v6_local = Some(IpAddr::V6(v6));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut gw = a.FirstGatewayAddress;
        while !gw.is_null() {
            let g = unsafe { &*gw };
            gw = g.Next;
            let sa = g.Address.lpSockaddr;
            if sa.is_null() {
                continue;
            }
            match unsafe { (*sa).sa_family } {
                f if f == AF_INET => {
                    let s = unsafe { &*(sa as *const SOCKADDR_IN) };
                    let ip = Ipv4Addr::from(u32::from_be(unsafe { s.sin_addr.S_un.S_addr }));
                    if let Some(local) = v4_local {
                        out.push(Gateway {
                            ip: IpAddr::V4(ip),
                            scope_id: 0,
                            local,
                        });
                    }
                }
                f if f == AF_INET6 => {
                    let s = unsafe { &*(sa as *const SOCKADDR_IN6) };
                    let ip = Ipv6Addr::from(unsafe { s.sin6_addr.u.Byte });
                    let scope = unsafe { s.Anonymous.sin6_scope_id };
                    // Pinhole target: our stable global v6 on this adapter.
                    if let Some(local) = v6_local {
                        out.push(Gateway {
                            ip: IpAddr::V6(ip),
                            scope_id: scope.max(a.Ipv6IfIndex),
                            local,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out
}
