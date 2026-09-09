//! Fallback-matrix tests for the WebTransport video path (ADR-0011 P4).
//!
//! The invariants the ADR promises:
//! * host runs `media.wt_enabled` → a claimed signaling session is offered
//!   `wt_video_info` (token + port + pinned cert hash);
//! * the transport is off (or failed to bind) → **no** `wt_video_info` is ever
//!   sent, and nothing else about the session changes — WebRTC carries video;
//! * the advertised port and cert hash match the bound transport.

use std::sync::Arc;
use std::time::Duration;

use inphase_host::config::Config;
use inphase_host::http::HttpState;
use inphase_host::media::wt::{WtTransportConfig, WtVideoTransport};
use inphase_protocol::SignalMessage;

fn state_with(wt: Option<Arc<WtVideoTransport>>) -> HttpState {
    let cfg = Arc::new(Config::default());
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
        identity: Arc::new(inphase_host::identity::HostIdentity::load_or_create().unwrap()),
        acl: Arc::new(inphase_host::identity::acl::ControllerAcl::load()),
        invites: Arc::new(inphase_host::identity::pairing_invite::InviteStore::new()),
        host_name: "test-host".into(),
        play_url: "http://127.0.0.1:8765/".into(),
        https: false,
        remote_access: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        remote_mapping: inphase_host::portmap::shared(),
        art: inphase_host::gameart::ArtCache::new(false),
        wt,
        started_at: std::time::Instant::now(),
    }
}

#[tokio::test]
async fn wt_disabled_advertises_nothing() {
    let st = state_with(None);
    // The helper is what the signaling connect handler calls; with the
    // transport absent it must produce no message at all.
    assert!(
        inphase_host::http::wt_advertisement(&st).is_none(),
        "WebRTC-only host must not advertise wt_video_info"
    );
}

#[tokio::test]
async fn wt_enabled_advertises_dial_info_matching_the_transport() {
    let identity = wtransport_identity();
    let transport = Arc::new(
        WtVideoTransport::bind(WtTransportConfig {
            port: 0,
            identity,
            datagram_budget: 1300,
            max_queued_frames: 4,
        })
        .unwrap(),
    );
    let st = state_with(Some(transport.clone()));

    let msg = inphase_host::http::wt_advertisement(&st).expect("wt host advertises");
    let SignalMessage::WtVideoInfo {
        token,
        port,
        cert_sha256,
    } = msg
    else {
        panic!("expected WtVideoInfo, got {msg:?}");
    };
    assert_eq!(port, transport.port());
    let expect: String = transport
        .cert_sha256()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(cert_sha256, expect);
    assert_eq!(cert_sha256.len(), 64);
    assert_eq!(token.len(), 32, "128-bit token as hex");

    // The token is one-time: presenting it once consumes it. A second dial
    // with the same token must be rejected by the transport (its own tests
    // cover the rejection; here we assert the advertised token actually works
    // exactly once against the real store).
    assert!(
        consume_via_client(&transport, &token).await,
        "advertised token must authenticate a real dial"
    );

    transport.shutdown();
}

#[tokio::test]
async fn each_claimed_session_gets_a_fresh_token() {
    let transport = Arc::new(
        WtVideoTransport::bind(WtTransportConfig {
            port: 0,
            identity: wtransport_identity(),
            datagram_budget: 1300,
            max_queued_frames: 4,
        })
        .unwrap(),
    );
    let st = state_with(Some(transport.clone()));

    let first = inphase_host::http::wt_advertisement(&st).unwrap();
    let second = inphase_host::http::wt_advertisement(&st).unwrap();
    let (SignalMessage::WtVideoInfo { token: a, .. }, SignalMessage::WtVideoInfo { token: b, .. }) =
        (&first, &second)
    else {
        panic!("expected WtVideoInfo");
    };
    assert_ne!(a, b, "tokens are per-session, never reused");

    // Both are independently valid (each session dials with its own).
    assert!(consume_via_client(&transport, a).await);
    assert!(consume_via_client(&transport, b).await);

    transport.shutdown();
}

/// Dial the transport like a browser would and present `token` as the first
/// control-stream message. Returns whether the connection survived auth.
async fn consume_via_client(transport: &WtVideoTransport, token: &str) -> bool {
    let cfg = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([wtransport::tls::Sha256Digest::new(
            transport.cert_sha256(),
        )])
        .build();
    let endpoint = wtransport::Endpoint::client(cfg).unwrap();
    let conn = endpoint
        .connect(
            wtransport::endpoint::ConnectOptions::builder(format!(
                "https://127.0.0.1:{}/wt-video",
                transport.port()
            ))
            .build(),
        )
        .await
        .unwrap();
    let (mut tx, mut rx) = conn.open_bi().await.unwrap().await.unwrap();
    let auth = inphase_protocol::WtClientMessage::Auth {
        token: token.to_string(),
    };
    let mut wire = serde_json::to_string(&auth).unwrap();
    wire.push('\n');
    tx.write_all(wire.as_bytes()).await.unwrap();

    // The host answers a valid auth with silence (VideoConfig comes later)
    // and an invalid one with an error line. So: a short quiet read that ends
    // without an error message = authenticated.
    let mut buf = [0u8; 512];
    let read = tokio::time::timeout(Duration::from_millis(300), rx.read(&mut buf)).await;
    match read {
        Err(_) => true, // no refusal within 300 ms → accepted
        Ok(Ok(Some(n))) => !String::from_utf8_lossy(&buf[..n]).contains("unauthorized"),
        Ok(_) => false, // stream closed by the host = refused
    }
}

fn wtransport_identity() -> wtransport::Identity {
    wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap()
}
