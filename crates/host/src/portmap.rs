// SPDX-License-Identifier: GPL-3.0-or-later
//! Opening an inbound path through the local router so a device off the LAN can
//! reach the host directly — the "hole punch" behind Remote Access.
//!
//! Two protocols, tried newest first, no dependencies:
//!
//! * **PCP** (RFC 6887) — the modern one. A single UDP request to the router's
//!   `:5351` maps a port *and*, for IPv6, opens a firewall pinhole. This is the
//!   one that matters here: remote access is IPv6-direct.
//! * **NAT-PMP** (RFC 6886) — PCP's predecessor, IPv4-only. Apple routers and
//!   older gear speak it.
//!
//! **This step is allowed to fail.** Plenty of consumer routers (and every ISP
//! gateway that hides its firewall settings) support neither. When that happens
//! the task says so in one clear line — the operator then has to add an inbound
//! rule by hand or the address is only reachable from the LAN. It keeps
//! re-probing, because a router reboot or a network change can turn support on.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::net::UdpSocket;

const PCP_NATPMP_PORT: u16 = 5351;
/// Lease we ask for. Renewed at half. RFC 6887 lets the router shorten it.
const LIFETIME_SECS: u32 = 3600;
const PROTO_TCP: u8 = 6;
/// How long to wait for a router that answers neither protocol before probing
/// again — long enough not to spam, short enough to pick up a reboot.
const REPROBE_AFTER: Duration = Duration::from_secs(15 * 60);
const RECV_TIMEOUT: Duration = Duration::from_millis(1500);

/// What the port-mapping task has managed to arrange, surfaced in
/// `/api/v1/status` so the dashboard can tell the operator whether a remote
/// device will actually be able to connect.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MappingStatus {
    /// `"pcp"`, `"nat-pmp"`, or `None` when the router opened nothing.
    pub method: Option<String>,
    /// The `addr:port` the internet would use, once the router reports it.
    pub external: Option<String>,
    /// One-line summary for the dashboard and logs.
    pub detail: String,
}

pub type Shared = Arc<RwLock<MappingStatus>>;

pub fn shared() -> Shared {
    Arc::new(RwLock::new(MappingStatus {
        detail: "not started".into(),
        ..Default::default()
    }))
}

/// Start maintaining an inbound mapping for `port`. Returns immediately; the
/// work runs in a background task. It idles while `enabled` is false, so it can
/// be spawned unconditionally and driven by the tray's remote-access toggle.
pub fn spawn(
    port: u16,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    status: Shared,
    wt_video_port: u16,
    preferred: Option<IpAddr>,
) {
    tokio::spawn(async move { run(port, wt_video_port, enabled, status, preferred).await });
}

async fn run(
    port: u16,
    wt_video_port: u16,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    status: Shared,
    preferred: Option<IpAddr>,
) {
    // The PCP nonce of the last successful mapping per protocol: reusing it
    // RENEWS the router-side mapping; a fresh nonce would create a second.
    let mut udp_nonce: Option<[u8; 12]> = None;
    let mut tcp_nonce: Option<[u8; 12]> = None;
    loop {
        if !enabled.load(std::sync::atomic::Ordering::Relaxed) {
            *status.write() = MappingStatus::default();
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let gateways = order_gateways(gateways(), preferred);
        if gateways.is_empty() {
            set(
                &status,
                MappingStatus {
                    detail: "no default gateway found — cannot open a router port".into(),
                    ..Default::default()
                },
            );
            tokio::time::sleep(REPROBE_AFTER).await;
            continue;
        }

        // The video transport needs its own pinhole (UDP). Best-effort: if
        // the router refuses it, the UI mapping still helps and the log says
        // what is missing.
        match try_map_udp(wt_video_port, &gateways, udp_nonce).await {
            Some(lease) => tracing::info!(
                port = wt_video_port,
                lifetime = lease.lifetime.as_secs(),
                "router opened the WT video pinhole (UDP)"
            ),
            None => tracing::warn!(
                "router did not open a UDP {wt_video_port} pinhole - remote WT video will not connect even if the UI maps"
            ),
        }

        match try_map(port, &gateways, tcp_nonce).await {
            Some(lease) => {
                let ext = lease
                    .external
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".into());
                tracing::info!(
                    method = lease.method,
                    external = %ext,
                    lease_secs = lease.lifetime.as_secs(),
                    "router opened an inbound path for remote access"
                );
                set(
                    &status,
                    MappingStatus {
                        method: Some(lease.method.into()),
                        external: lease.external.map(|a| a.to_string()),
                        detail: format!(
                            "mapped via {} (lease {}s); remote devices reach {}",
                            lease.method,
                            lease.lifetime.as_secs(),
                            ext
                        ),
                    },
                );
                // Retain the nonce: the next pass renews THIS mapping
                // instead of minting a duplicate on the router.
                udp_nonce = Some(lease.nonce);
                tcp_nonce = Some(lease.nonce);
                // Renew near the lease's actual expiry; wake every 30 s so a
                // remote-access toggle-off is noticed promptly.
                let renew_in = renew_after(lease.lifetime);
                let mut slept = Duration::ZERO;
                while slept < renew_in && enabled.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    slept += Duration::from_secs(30);
                }
            }
            None => {
                let hint = gateways
                    .iter()
                    .find_map(|g| match g.local {
                        IpAddr::V6(v6) => Some(format!("[{v6}]:{port}")),
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("this PC on TCP {port}"));
                tracing::warn!(
                    "router does not support automatic port opening (no PCP or NAT-PMP \
                     response). Remote access will only work once you add an inbound \
                     allow / pinhole for {hint} in the router, or it stays LAN-only."
                );
                set(
                    &status,
                    MappingStatus {
                        detail: format!(
                            "router supports neither PCP nor NAT-PMP — add an inbound rule \
                             for {hint} by hand, or remote access stays LAN-only"
                        ),
                        ..Default::default()
                    },
                );
                tokio::time::sleep(REPROBE_AFTER).await;
            }
        }
    }
}

fn set(status: &Shared, next: MappingStatus) {
    *status.write() = next;
}

/// `--net-probe`: print the discovered gateways and the result of one PCP /
/// NAT-PMP attempt on port 443. Diagnostic only.
pub async fn print_probe() -> anyhow::Result<()> {
    let gws = gateways();
    println!("default gateways: {}", gws.len());
    for g in &gws {
        println!("  {} (scope {})  local {}", g.ip, g.scope_id, g.local);
    }
    if gws.is_empty() {
        println!("no gateways — GetAdaptersAddresses reported no FirstGatewayAddress");
        return Ok(());
    }
    for g in &gws {
        let pcp = pcp_map(443, g, None).await;
        println!(
            "  PCP  -> {} : {}",
            g.ip,
            pcp.map(|l| format!("mapped, ext {:?}, {}s", l.external, l.lifetime.as_secs()))
                .unwrap_or_else(|| "no usable response".into())
        );
        if g.ip.is_ipv4() {
            let np = natpmp_map(443, g).await;
            println!(
                "  NATPMP -> {} : {}",
                g.ip,
                np.map(|l| format!("mapped, {}s", l.lifetime.as_secs()))
                    .unwrap_or_else(|| "no usable response".into())
            );
        }
    }
    Ok(())
}

struct Lease {
    method: &'static str,
    external: Option<SocketAddr>,
    lifetime: Duration,
    /// The PCP nonce this mapping was created with. Renewals MUST reuse it:
    /// a fresh nonce is a NEW mapping, and the old one lives out its lease —
    /// duplicates pile up on the router until it forgets them.
    nonce: [u8; 12],
}

/// When to renew: near the lease's ACTUAL expiry (30 s margin), never at a
/// fixed cadence pretending the lease is different from what the router said.
fn renew_after(lifetime: Duration) -> Duration {
    lifetime
        .saturating_sub(Duration::from_secs(30))
        .max(Duration::from_secs(30))
}

/// Try the gateway whose host address matches the EndpointPlan's selected
/// IPv6 first — the stable address is the one remote clients can actually
/// dial, and PCP mappings are per client address.
fn order_gateways(gateways: Vec<Gateway>, preferred: Option<IpAddr>) -> Vec<Gateway> {
    match preferred {
        None => gateways,
        Some(pref) => {
            let (mut first, mut rest): (Vec<Gateway>, Vec<Gateway>) =
                gateways.into_iter().partition(|g| g.local == pref);
            first.append(&mut rest);
            first
        }
    }
}

/// Try every gateway with PCP, then every IPv4 gateway with NAT-PMP.
const PROTO_UDP: u8 = 17;

async fn try_map_udp(port: u16, gateways: &[Gateway], retained: Option<[u8; 12]>) -> Option<Lease> {
    for g in gateways {
        if let Some(l) = pcp_map_udp(port, g, retained).await {
            return Some(l);
        }
    }
    None
}

async fn try_map(port: u16, gateways: &[Gateway], retained: Option<[u8; 12]>) -> Option<Lease> {
    for g in gateways {
        if let Some(l) = pcp_map(port, g, retained).await {
            return Some(l);
        }
    }
    for g in gateways {
        if g.ip.is_ipv4() {
            if let Some(l) = natpmp_map(port, g).await {
                return Some(l);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// PCP — RFC 6887. MAP opcode.
// ---------------------------------------------------------------------------

async fn pcp_map_udp(port: u16, g: &Gateway, nonce: Option<[u8; 12]>) -> Option<Lease> {
    pcp_map_proto(port, g, PROTO_UDP, nonce).await
}

async fn pcp_map(port: u16, g: &Gateway, nonce: Option<[u8; 12]>) -> Option<Lease> {
    pcp_map_proto(port, g, PROTO_TCP, nonce).await
}

async fn pcp_map_proto(
    port: u16,
    g: &Gateway,
    proto: u8,
    retained: Option<[u8; 12]>,
) -> Option<Lease> {
    let sock = bind_for(g).await?;
    let dst = gateway_sockaddr(g);

    // Common header (24 bytes) + MAP opcode payload (36 bytes) = 60.
    let mut req = Vec::with_capacity(60);
    req.push(2); // version
    req.push(1); // opcode 1 = MAP (request: top bit clear)
    req.extend_from_slice(&[0, 0]); // reserved
    req.extend_from_slice(&LIFETIME_SECS.to_be_bytes());
    req.extend_from_slice(&client_addr_bytes(g.local)); // PCP client address (16)

    let nonce: [u8; 12] = retained.unwrap_or_else(rand::random);
    req.extend_from_slice(&nonce);
    req.push(proto);
    req.extend_from_slice(&[0, 0, 0]); // reserved
    req.extend_from_slice(&port.to_be_bytes()); // internal port
    req.extend_from_slice(&port.to_be_bytes()); // suggested external port
    req.extend_from_slice(&client_addr_bytes(g.local)); // suggested external IP

    sock.send_to(&req, dst).await.ok()?;

    let mut buf = [0u8; 1100];
    let n = recv(&sock, &mut buf).await?;
    if n < 60 || buf[0] != 2 || buf[1] != 0x81 {
        return None; // not a MAP response
    }
    let result = buf[3];
    if result != 0 {
        tracing::debug!(result, gw = %g.ip, "PCP MAP refused");
        return None;
    }
    let lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    // MAP response: nonce(12) proto(1) rsv(3) internal(2) external(2) ext-ip(16)
    if buf[24..36] != nonce {
        return None;
    }
    let ext_port = u16::from_be_bytes([buf[42], buf[43]]);
    let ext_ip = parse_client_addr(&buf[44..60]);
    Some(Lease {
        method: "pcp",
        external: Some(SocketAddr::new(ext_ip, ext_port)),
        lifetime: Duration::from_secs(lifetime.max(1) as u64),
        nonce,
    })
}

// ---------------------------------------------------------------------------
// NAT-PMP — RFC 6886. IPv4 only.
// ---------------------------------------------------------------------------

async fn natpmp_map(port: u16, g: &Gateway) -> Option<Lease> {
    let sock = bind_for(g).await?;
    let dst = gateway_sockaddr(g);

    let mut req = Vec::with_capacity(12);
    req.push(0); // version
    req.push(2); // opcode 2 = map TCP
    req.extend_from_slice(&[0, 0]); // reserved
    req.extend_from_slice(&port.to_be_bytes()); // internal
    req.extend_from_slice(&port.to_be_bytes()); // suggested external
    req.extend_from_slice(&LIFETIME_SECS.to_be_bytes());
    sock.send_to(&req, dst).await.ok()?;

    let mut buf = [0u8; 64];
    let n = recv(&sock, &mut buf).await?;
    if n < 16 || buf[0] != 0 || buf[1] != 130 {
        return None;
    }
    let result = u16::from_be_bytes([buf[2], buf[3]]);
    if result != 0 {
        tracing::debug!(result, gw = %g.ip, "NAT-PMP map refused");
        return None;
    }
    let ext_port = u16::from_be_bytes([buf[10], buf[11]]);
    let lifetime = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let _ = g;
    // The map response does not carry the external IP (a separate NAT-PMP call
    // does); the dashboard's public-address line fills that in. Report the port.
    Some(Lease {
        method: "nat-pmp",
        external: Some(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            ext_port,
        )),
        lifetime: Duration::from_secs(lifetime.max(1) as u64),
        nonce: [0u8; 12],
    })
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

async fn bind_for(g: &Gateway) -> Option<UdpSocket> {
    // Bind our *specific* local address, not the wildcard: PCP result code 12
    // (ADDRESS_MISMATCH) is the router rejecting a request whose UDP source
    // address does not match the PCP Client Address field, and with a
    // link-local gateway the OS would otherwise pick a link-local source.
    let local: SocketAddr = match (g.ip, g.local) {
        // Bind the global address with scope 0; the destination sockaddr still
        // carries the scope, so the packet leaves the right interface with our
        // global address as its source.
        (IpAddr::V6(_), IpAddr::V6(v6)) => SocketAddr::from((v6, 0)),
        (_, IpAddr::V4(v4)) => SocketAddr::from((v4, 0)),
        (IpAddr::V4(_), IpAddr::V6(_)) => SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0)),
    };
    let sock = match UdpSocket::bind(local).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(%local, "portmap: bind failed ({e}); using wildcard");
            let any: SocketAddr = match g.ip {
                IpAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
                IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
            };
            UdpSocket::bind(any).await.ok()?
        }
    };
    tracing::trace!(bound = ?sock.local_addr().ok(), gw = %g.ip, "portmap: socket");
    Some(sock)
}

fn gateway_sockaddr(g: &Gateway) -> SocketAddr {
    match g.ip {
        IpAddr::V4(v4) => SocketAddr::from((v4, PCP_NATPMP_PORT)),
        IpAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(
            v6,
            PCP_NATPMP_PORT,
            0,
            g.scope_id,
        )),
    }
}

async fn recv(sock: &UdpSocket, buf: &mut [u8]) -> Option<usize> {
    match tokio::time::timeout(RECV_TIMEOUT, sock.recv_from(buf)).await {
        Ok(Ok((n, _))) => Some(n),
        _ => None,
    }
}

/// PCP carries every address as 16 bytes: native for v6, IPv4-mapped for v4.
fn client_addr_bytes(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V6(v6) => v6.octets(),
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
    }
}

fn parse_client_addr(b: &[u8]) -> IpAddr {
    let mut o = [0u8; 16];
    o.copy_from_slice(&b[..16]);
    let v6 = Ipv6Addr::from(o);
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

// ---------------------------------------------------------------------------
// Gateway discovery (platform-specific)
// ---------------------------------------------------------------------------

pub use gw::{gateways, Gateway};

#[cfg(windows)]
mod gw {
    pub use crate::platform::windows::net6::Gateway;
    pub fn gateways() -> Vec<Gateway> {
        crate::platform::windows::net6::default_gateways()
    }
}

#[cfg(not(windows))]
mod gw {
    use std::net::IpAddr;
    #[derive(Debug, Clone)]
    pub struct Gateway {
        pub ip: IpAddr,
        pub scope_id: u32,
        pub local: IpAddr,
    }
    pub fn gateways() -> Vec<Gateway> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcp_addr_round_trips_v4_and_v6() {
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        let v6: IpAddr = "2001:db8:1234:5678::100".parse().unwrap();
        assert_eq!(parse_client_addr(&client_addr_bytes(v4)), v4);
        assert_eq!(parse_client_addr(&client_addr_bytes(v6)), v6);
    }

    #[test]
    #[cfg(not(windows))]
    fn no_gateways_off_windows_is_a_clean_noop() {
        // The dev/CI target has no gateway discovery — the task must still
        // report a sane status rather than spin.
        assert!(gateways().is_empty());
    }
}
