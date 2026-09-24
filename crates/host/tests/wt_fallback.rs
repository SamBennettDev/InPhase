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
use inphase_host::media::wt::{WtIdentity, WtSlot, WtTransportConfig, WtVideoTransport};
use inphase_protocol::SignalMessage;

fn state_with(wt: Option<Arc<WtVideoTransport>>) -> HttpState {
    let tmp = std::env::temp_dir().join(format!(
        "inphase-wt-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut config = Config::default();
    config.pairing.persist_sessions = false;
    let cfg = Arc::new(config);
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
        identity: Arc::new(
            inphase_host::identity::HostIdentity::load_or_create_at(&tmp.join("id.key")).unwrap(),
        ),
        acl: Arc::new(inphase_host::identity::acl::ControllerAcl::load_at(
            tmp.join("controllers.json"),
        )),
        invites: Arc::new(inphase_host::identity::pairing_invite::InviteStore::new()),
        host_name: "test-host".into(),
        play_url: "http://127.0.0.1:8765/".into(),
        https: false,
        remote_access: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        remote_mapping: inphase_host::portmap::shared(),
        art: inphase_host::gameart::ArtCache::new(false),
        wt: {
            // The slot is what the HTTP state and the session manager read; a
            // rotation would replace its contents, so tests publish through it
            // exactly as boot does.
            let slot = WtSlot::new();
            slot.set(wt);
            slot
        },
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
    let transport = Arc::new(wt_transport(0));
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
    let transport = Arc::new(wt_transport(0));
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

/// A transport with a fresh, in-window certificate — the state the host
/// advertises from.
fn wt_transport(port: u16) -> WtVideoTransport {
    let id = WtIdentity::self_signed(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    WtVideoTransport::bind(WtTransportConfig {
        port,
        identity: id.identity,
        cert_not_after: id.not_after,
        datagram_budget: 1300,
        max_queued_frames: 4,
        congestion: Default::default(),
    })
    .unwrap()
}

/// Receive the next message of type `want` from the host side of a signaling
/// link, skipping the others.
async fn next_of(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    want: impl Fn(&SignalMessage) -> bool,
) -> SignalMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = rx.recv().await.expect("signaling link closed");
            if let Ok(msg) = serde_json::from_str::<SignalMessage>(&text) {
                if want(&msg) {
                    return msg;
                }
            }
        }
    })
    .await
    .expect("host answered in time")
}

/// A client whose WT connection closed asks for a fresh dial over the
/// signaling socket. That request used to be answered only on the retired
/// WebRTC data channel, so over the socket it vanished - and a single WT close
/// left the session frameless until the player reloaded (host: `sent=0 ...
/// evicted=121` every second; client: "waiting for a keyframe").
#[tokio::test]
async fn a_redial_request_over_signaling_gets_a_fresh_dial() {
    use base64::Engine as _;
    use ed25519_dalek::Signer as _;

    let transport = Arc::new(wt_transport(0));
    let st = state_with(Some(transport.clone()));
    let (to_host, from_link) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (to_link, mut from_host) = tokio::sync::mpsc::unbounded_channel::<String>();
    let link = tokio::spawn(inphase_host::http::signal::drive(
        from_link,
        to_link,
        st,
        inphase_host::http::signal::PeerDesc {
            browser: "test".into(),
            addr_label: "test".into(),
        },
    ));

    let SignalMessage::AuthChallenge { nonce } = next_of(&mut from_host, |m| {
        matches!(m, SignalMessage::AuthChallenge { .. })
    })
    .await
    else {
        unreachable!()
    };
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(nonce)
        .unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let controller_id: String = key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(&nonce).to_bytes());
    to_host
        .send(
            serde_json::to_string(&SignalMessage::AuthResponse {
                controller_id,
                signature,
            })
            .unwrap(),
        )
        .unwrap();

    let is_info = |m: &SignalMessage| matches!(m, SignalMessage::WtVideoInfo { .. });
    let SignalMessage::WtVideoInfo { token: first, .. } = next_of(&mut from_host, is_info).await
    else {
        unreachable!()
    };

    to_host
        .send(serde_json::to_string(&SignalMessage::WtVideoInfoRequest).unwrap())
        .unwrap();
    let SignalMessage::WtVideoInfo {
        token: second,
        port,
        ..
    } = next_of(&mut from_host, is_info).await
    else {
        unreachable!()
    };
    assert_ne!(first, second, "a redial gets its own one-time token");
    assert_eq!(port, transport.port());
    assert!(
        consume_via_client(&transport, &second).await,
        "the re-advertised token authenticates a real dial"
    );

    drop(to_host);
    let _ = tokio::time::timeout(Duration::from_secs(5), link).await;
    transport.shutdown();
}
