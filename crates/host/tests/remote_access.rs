// SPDX-License-Identifier: GPL-3.0-or-later
//! The remote-access boundary, exercised over a real TCP socket.
//!
//! These assertions are the difference between "reachable from the internet"
//! and "open to the internet", so they are tested end to end through the actual
//! router and middleware rather than by calling the predicate directly.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use inphase_host::config::Config;
use inphase_host::http::HttpState;

fn state(remote_enabled: bool) -> HttpState {
    let tmp = std::env::temp_dir().join(format!(
        "inphase-ra-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cfg = Config::default();
    cfg.remote_access.enabled = remote_enabled;
    cfg.pairing.persist_sessions = false;
    let cfg = Arc::new(cfg);
    let stats = Arc::new(inphase_host::stats::StatsCollector::default());
    HttpState {
        cfg: cfg.clone(),
        pairing: Arc::new(inphase_host::pairing::PairingManager::new(
            cfg.pairing.clone(),
        )),
        sessions: Arc::new(inphase_host::session::SessionManager::new(
            cfg.clone(),
            stats.clone(),
        )),
        stats,
        // Both write under the config dir; point that at a temp dir so the
        // test never touches real host state.
        identity: Arc::new(
            inphase_host::identity::HostIdentity::load_or_create_at(&tmp.join("id.key")).unwrap(),
        ),
        acl: Arc::new(inphase_host::identity::acl::ControllerAcl::load()),
        invites: Arc::new(inphase_host::identity::pairing_invite::InviteStore::new()),
        host_name: "test".into(),
        play_url: "https://test/".into(),
        https: true,
        remote_access: Arc::new(std::sync::atomic::AtomicBool::new(remote_enabled)),
        remote_mapping: inphase_host::portmap::shared(),
        art: inphase_host::gameart::ArtCache::new(false),
        started_at: Instant::now(),
        wt: None,
    }
}

/// Serve the LAN router on loopback and return its address.
async fn spawn(st: HttpState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app =
        inphase_host::http::lan_router(st).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Minimal HTTP/1.1 request; returns the status line.
async fn get_status(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn loopback_is_always_allowed_even_with_remote_access_off() {
    // The connection below comes from 127.0.0.1, which is a LAN peer, so the
    // gate must not touch it — otherwise the host locks its own operator out.
    let addr = spawn(state(false)).await;
    let line = get_status(addr, "/api/v1/status").await;
    assert!(
        line.contains("200"),
        "expected 200 for a local peer, got {line:?}"
    );
}

#[tokio::test]
async fn local_peers_are_allowed_when_remote_access_is_on_too() {
    let addr = spawn(state(true)).await;
    let line = get_status(addr, "/api/v1/status").await;
    assert!(line.contains("200"), "got {line:?}");
}

/// The predicate the gate is built on. Exercised directly for the off-network
/// cases, which cannot be produced from a loopback test client.
#[test]
fn off_network_peers_are_classified_as_remote() {
    use inphase_host::net::is_lan_peer;
    // The exact client that reached the host over cellular IPv6.
    assert!(!is_lan_peer("2607:fb90:1:2::abcd".parse().unwrap()));
    assert!(!is_lan_peer("8.8.8.8".parse().unwrap()));
    // ...while everything on the local network still counts as local.
    assert!(is_lan_peer("127.0.0.1".parse().unwrap()));
    assert!(is_lan_peer("192.168.1.100".parse().unwrap()));
    assert!(is_lan_peer("fe80::1".parse().unwrap()));
}

// ---------------------------------------------------------------------------
// The cases that matter most cannot be produced from a loopback client, so
// drive the real router with a synthesised peer address instead. This is still
// the production middleware stack — only the socket is simulated.
// ---------------------------------------------------------------------------

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn status_from_peer(st: HttpState, peer: &str) -> StatusCode {
    let app = inphase_host::http::lan_router(st);
    let mut req = Request::builder()
        .uri("/api/v1/status")
        .body(Body::empty())
        .unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_remote_peer_is_refused_while_remote_access_is_off() {
    // This is the whole point: the host is reachable from the internet (an
    // IPv6 firewall rule is open) but must still answer nothing.
    for peer in ["[2607:fb90:1:2::abcd]:44321", "8.8.8.8:44321"] {
        let code = status_from_peer(state(false), peer).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "peer {peer} should be refused");
    }
}

#[tokio::test]
async fn a_remote_peer_is_served_once_remote_access_is_enabled() {
    let code = status_from_peer(state(true), "[2607:fb90:1:2::abcd]:44321").await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn a_lan_peer_is_served_regardless() {
    for enabled in [false, true] {
        // IPv4 LAN, and an IPv6 client on the host's own /64 (a phone on Wi-Fi,
        // which holds a *global* address — the case a naive check breaks).
        for peer in ["192.168.1.50:44321", "[fe80::1]:44321"] {
            let code = status_from_peer(state(enabled), peer).await;
            assert_eq!(code, StatusCode::OK, "peer {peer} enabled={enabled}");
        }
    }
}

async fn pair_from(st: HttpState, peer: &str, body: &'static str) -> StatusCode {
    let app = inphase_host::http::lan_router(st);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    app.oneshot(req).await.unwrap().status()
}

/// Devices are enrolled on the LAN and authenticate remotely with the key they
/// were issued, so the internet-facing surface has no enrolment path at all.
#[tokio::test]
async fn no_form_of_pairing_is_accepted_from_off_network_by_default() {
    let remote = "[2607:fb90:1:2::abcd]:44321";
    // Remote access on — the host is deliberately serving the internet — and
    // pairing must *still* be refused, by PIN...
    assert_eq!(
        pair_from(state(true), remote, r#"{"pin":"000000"}"#).await,
        StatusCode::FORBIDDEN,
        "a 6-digit PIN must never be accepted from the internet"
    );
    // ...and by invite, because remote enrolment is off by default.
    assert_eq!(
        pair_from(state(true), remote, r#"{"invite":"anything"}"#).await,
        StatusCode::FORBIDDEN,
        "remote enrolment must be opt-in, not a default"
    );
}

/// The flow the design actually intends: pair on the LAN with the PIN.
#[tokio::test]
async fn pin_pairing_still_works_on_the_lan() {
    // A wrong PIN, so this asserts the request was *considered* (401) rather
    // than refused for being off-network (403).
    let code = pair_from(state(true), "192.168.1.50:44321", r#"{"pin":"000000"}"#).await;
    assert_eq!(
        code,
        StatusCode::UNAUTHORIZED,
        "LAN pairing must reach the PIN check, not the network gate"
    );
}

/// Pairing without a device key used to mint a session cookie anyway, leaving a
/// browser that looks paired and can never stream: every signalling connect
/// fails the Ed25519 challenge and the client had no way back but "Forget this
/// device". Reject the pair instead.
#[tokio::test]
async fn pairing_without_a_device_key_is_refused() {
    let st = state(true);
    let pin = st.pairing.current_pin().0;
    let body: &'static str = Box::leak(format!(r#"{{"pin":"{pin}"}}"#).into_boxed_str());
    let code = pair_from(st, "192.168.1.50:44321", body).await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "a correct PIN with no controller_pubkey must not produce a session"
    );
}

/// Opting in re-opens the invite path and, over HTTPS only, the PIN path.
#[tokio::test]
async fn opting_in_allows_remote_invites_and_https_remote_pins() {
    let mut st = state(true);
    let mut cfg = (*st.cfg).clone();
    cfg.remote_access.allow_remote_pairing = true;
    st.cfg = std::sync::Arc::new(cfg);
    let remote = "[2607:fb90:1:2::abcd]:44321";

    // A bogus invite is now *evaluated* (401 = rejected on its merits)...
    assert_eq!(
        pair_from(st.clone(), remote, r#"{"invite":"bogus"}"#).await,
        StatusCode::UNAUTHORIZED
    );
    // ...and a PIN reaches the PIN check too (401 = wrong code, not refused
    // for being off-network — the operator opted in).
    assert_eq!(
        pair_from(st.clone(), remote, r#"{"pin":"000000"}"#).await,
        StatusCode::UNAUTHORIZED
    );
    // But a PIN on plaintext HTTP never leaves the ground: it would cross the
    // internet readable by every hop.
    st.https = false;
    assert_eq!(
        pair_from(st, remote, r#"{"pin":"000000"}"#).await,
        StatusCode::FORBIDDEN,
        "remote PIN pairing must require HTTPS"
    );
}
