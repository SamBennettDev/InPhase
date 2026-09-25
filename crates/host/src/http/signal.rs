//! `/api/v1/signal` WebSocket — authenticated WebRTC signaling + preflight
//! negotiation (architecture report §15, §16).
//!
//! Flow (§15 messages):
//! ```text
//! client_hello        -> version check, claim the single session (BUSY if taken)
//! client_capabilities -> codec/feature intersection -> session_config
//!                        + build & start the MediaSession
//! offer  (host->client), answer / ice (both)
//! session_ready       -> state = PLAYING
//! ping/pong, client_telemetry on the reliable path
//! bye / socket close  -> state = STOPPING (releases input, tears down media)
//! ```

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::{info, warn};

use inphase_protocol::{
    DecodeHint, QualityPreset, RtpCodecCapability, SessionConfig, SignalErrorCode, SignalMessage,
    VideoCodec, SIGNALING_PROTOCOL_VERSION,
};

use super::{require_session, HttpState};
use crate::identity::unhex;
use crate::media::encoder_policy;
use crate::media::MediaSignal;
use crate::session::{PeerInfo, SessionState};

fn b64d(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}
fn b64e(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// A signaling session runs over plain text frames. The transport (an inbound
/// LAN WebSocket) is bridged to these channels by the caller, so [`drive`] is
/// transport-agnostic
pub type LinkTx = mpsc::UnboundedSender<String>;
pub type LinkRx = mpsc::UnboundedReceiver<String>;

/// Who is on the other end, for logs + `PeerInfo`, and what the device
/// challenge may trust about it.
#[derive(Clone, Default)]
pub struct PeerDesc {
    pub browser: String,
    pub addr_label: String,
    /// The session cookie that authenticated the socket. Device keys bind to
    /// it; `None` (tests, internal links) skips the binding checks.
    pub session: Option<String>,
    /// The peer is on the local network. Only there may a paired session
    /// enrol a device key it has not used before.
    pub lan: bool,
    /// A view-only preview for the library tile (`?preview=1`).
    pub preview: bool,
}

fn send_msg(to_link: &LinkTx, msg: &SignalMessage) {
    if let Ok(s) = serde_json::to_string(msg) {
        let _ = to_link.send(s);
    }
}

pub async fn signal_ws(
    State(st): State<HttpState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // §14.1: cookie authenticates the socket; no bearer token in the URL.
    let Some(session) = require_session(&st, &headers) else {
        return (StatusCode::UNAUTHORIZED, "pair first").into_response();
    };
    // §14.1: validate Origin on WebSocket upgrades.
    if !origin_ok(&st, &headers) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    let browser = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let peer = PeerDesc {
        browser,
        addr_label: addr.ip().to_string(),
        session: Some(session),
        lan: crate::net::is_lan_peer(addr.ip()),
        preview: query
            .as_deref()
            .is_some_and(|q| q.split('&').any(|kv| kv == "preview=1")),
    };

    ws.on_upgrade(move |socket| bridge_inbound(socket, st, peer))
}

/// Bridge an inbound axum WebSocket to the [`drive`] text channels.
async fn bridge_inbound(socket: WebSocket, st: HttpState, peer: PeerDesc) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (from_tx, from_rx) = mpsc::unbounded_channel::<String>();
    let (to_tx, mut to_rx) = mpsc::unbounded_channel::<String>();

    let out = tokio::spawn(async move {
        while let Some(s) = to_rx.recv().await {
            if ws_tx.send(Message::Text(s)).await.is_err() {
                break;
            }
        }
    });
    let inb = tokio::spawn(async move {
        while let Some(Ok(frame)) = ws_rx.next().await {
            match frame {
                Message::Text(t) => {
                    if from_tx.send(t).is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    drive(from_rx, to_tx, st, peer).await;
    out.abort();
    inb.abort();
}

fn origin_ok(_st: &HttpState, headers: &HeaderMap) -> bool {
    // A WebSocket upgrade reaches this handler as HTTP/1.1 — extended CONNECT
    // is not advertised — so the authority is the `host` header and there is no
    // request URI to read. The empty Uri routes the check down that path.
    super::same_origin_request(&axum::http::Uri::default(), headers)
}

/// Run one browser↔Host signaling session over the text channels. Ends when
/// `from_link` closes (peer/relay gone) or on a fatal protocol error.
pub async fn drive(mut from_link: LinkRx, to_link: LinkTx, st: HttpState, peer: PeerDesc) {
    let addr = &peer.addr_label;

    // ---- device challenge: prove possession of a paired controller key ----
    // The socket already rode the host's TLS + a valid session cookie; this
    // additionally binds the session to a specific Ed25519 device in the ACL.
    let mut nonce = [0u8; 32];
    let _ = getrandom::getrandom(&mut nonce);
    send_msg(
        &to_link,
        &SignalMessage::AuthChallenge {
            nonce: b64e(&nonce),
        },
    );

    let Some(first) = from_link.recv().await else {
        return;
    };
    let controller = match serde_json::from_str::<SignalMessage>(&first) {
        Ok(SignalMessage::AuthResponse {
            controller_id,
            signature,
        }) => match verify_device(&st, &peer, &controller_id, &nonce, &signature) {
            Some(c) => {
                if let Some(sid) = &peer.session {
                    st.pairing.bind_controller(sid, &c.id);
                }
                c
            }
            None => {
                warn!(%addr, "signaling: device challenge failed — rejecting");
                send_msg(
                    &to_link,
                    &SignalMessage::error(
                        SignalErrorCode::Unauthorized,
                        "this Home Screen app is not paired yet — enter the PIN from your PC",
                    ),
                );
                return;
            }
        },
        _ => {
            send_msg(
                &to_link,
                &SignalMessage::error(
                    SignalErrorCode::ProtocolVersion,
                    "expected an auth_response — reload the player",
                ),
            );
            return;
        }
    };
    st.acl.touch(&controller.id);
    info!(
        %addr, browser = %peer.browser,
        controller = %&controller.id[..controller.id.len().min(12)],
        "signaling: controller authorized"
    );

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<MediaSignal>();

    // Claim the single session (§15). Second client -> BUSY, then close.
    let peer_info = PeerInfo {
        browser: peer.browser.clone(),
        client_ip: peer.addr_label.clone(),
        connected_since_unix: now_unix(),
    };
    // The ticket identifies the session THIS link created; its close path
    // may only tear down a session it still owns (review §11).
    let session_ticket = match st
        .sessions
        .claim(peer_info, out_tx.clone(), peer.preview)
        .await
    {
        Ok(t) => t,
        Err(_) => {
            send_msg(
                &to_link,
                &SignalMessage::error(SignalErrorCode::Busy, "another player is connected"),
            );
            return;
        }
    };
    st.sessions.set_controller(controller.id.clone());
    info!(%addr, browser = %peer.browser, "signaling session claimed");

    // Advertise the WebTransport video path (ADR-0011) over the now-trusted
    // socket: dial info + one-time short-lived token. The client may dial
    // QUIC and present it; the absence of this message = WebRTC only.
    if let Some(msg) = wt_advertisement(&st) {
        info!(port = msg_port(&msg), "wt video path advertised");
        let _ = out_tx.send(msg);
    }

    // Raw input-datachannel bytes -> InputSession. The media bridge pushes here
    // for every packet on the unreliable `input` channel (§12).
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Control-channel JSON (ping/pong, telemetry, bye) -> same handler as the WS.
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<String>();
    st.sessions.with_player(|p| {
        p.media.set_input_sink(input_tx);
        p.media.set_control_sink(ctrl_tx);
    });

    // Pump: outbound signal messages + media events -> the link (plain JSON;
    // the transport is the host's own TLS). Media `Ready` / `Failed` also drive
    // the session state machine (§15).
    let pump = {
        let sessions = st.sessions.clone();
        let ready_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                let msg: SignalMessage = tokio::select! {
                    m = out_rx.recv() => match m {
                        Some(m) => m,
                        None => break, // every SignalMessage sender dropped = teardown
                    },
                    m = media_rx.recv() => {
                        let Some(m) = m else { break };
                        match &m {
                            // ICE Connected *and* Completed both emit Ready;
                            // only the first counts. WT establishment also
                            // emits Ready (ADR-0011: WT is the video carrier)
                            // and can beat the WebRTC offer/answer - the
                            // 2026-09-08 Safari session established WT in
                            // 0.2 s while cellular ICE never reached
                            // Connected, leaving the client's "connecting"
                            // overlay up over a streaming video.
                            MediaSignal::Ready
                                if matches!(
                                    sessions.state(),
                                    SessionState::Paired
                                        | SessionState::Negotiating
                                        | SessionState::Reconnecting
                                ) =>
                            {
                                sessions.transition(SessionState::Playing);
                            }
                            MediaSignal::Failed(_) => {
                                sessions.transition(SessionState::Stopping);
                            }
                            _ => {}
                        }
                        // `Ready` arms input + releases `session_ready` (once);
                        // it carries no signalling frame of its own, so loop.
                        if matches!(m, MediaSignal::Ready) {
                            if sessions.mark_ready() {
                                let _ = ready_tx.send(SignalMessage::SessionReady);
                            }
                            continue;
                        }
                        match Option::<SignalMessage>::from(m) {
                            Some(m) => m,
                            None => continue,
                        }
                    }
                };
                let Ok(s) = serde_json::to_string(&msg) else {
                    continue;
                };
                if to_link.send(s).is_err() {
                    break;
                }
            }
        })
    };

    // Input pump: decode + apply every input packet — but only once input is
    // armed (media path up). Packets before that are dropped.
    let input_pump = {
        let sessions = st.sessions.clone();
        tokio::spawn(async move {
            let mut dropped_preauth = 0u64;
            while let Some(bytes) = input_rx.recv().await {
                sessions.with_player(|p| {
                    if !p.input_armed {
                        dropped_preauth += 1;
                        if dropped_preauth == 1 {
                            warn!("dropping input received before the session was ready");
                        }
                        return;
                    }
                    if let Err(e) = p.input.handle_packet(&bytes) {
                        tracing::debug!("bad input packet: {e}");
                    }
                });
            }
        })
    };

    // Control pump: route control-channel JSON. `ping` is answered *on the same
    // channel* so the client can measure true data-path RTT (§20); everything
    // else goes through the shared handler.
    let control_pump = {
        let st = st.clone();
        let out_tx = out_tx.clone();
        let media_tx = media_tx.clone();
        tokio::spawn(async move {
            while let Some(text) = ctrl_rx.recv().await {
                let Ok(msg) = serde_json::from_str::<SignalMessage>(&text) else {
                    continue;
                };
                match msg {
                    SignalMessage::Ping { at_us } => {
                        let pong = serde_json::to_string(&SignalMessage::Pong { at_us }).unwrap();
                        st.sessions.with_player(|p| p.media.send_control(&pong));
                    }
                    SignalMessage::WtVideoInfoRequest => {
                        if let Some(info) = wt_advertisement(&st) {
                            let _ = out_tx.send(info);
                        }
                    }
                    other => {
                        handle_message(&st, &out_tx, &media_tx, other).await;
                    }
                }
            }
        })
    };

    // Inbound loop: plain JSON `SignalMessage` frames.
    while let Some(text) = from_link.recv().await {
        let Ok(mut msg) = serde_json::from_str::<SignalMessage>(&text) else {
            warn!("unparseable signaling message");
            continue;
        };
        if peer.preview {
            if let SignalMessage::ClientHello { requested_mode, .. } = &mut msg {
                preview_mode(requested_mode);
            }
        }
        // The client's WT redial after its video connection closed. This
        // request used to be answered only by the control pump, which reads
        // the WebRTC data channel - gone since ADR-0011 - so over this socket
        // it fell through `handle_message`'s catch-all and vanished. Every WT
        // close was therefore permanent for the rest of the signaling
        // session: the host encoded into an empty slot (`sent=0 ...
        // evicted=121` each second), the client sat on "waiting for a
        // keyframe", and neither side said why (2026-09-20 21:32, 21:36,
        // 21:40, 21:41, 21:52 - 29 advertisements, zero redials, ever).
        if matches!(msg, SignalMessage::WtVideoInfoRequest) {
            if !st.sessions.owns(session_ticket) {
                tracing::debug!(%addr, "wt redial request from a displaced link - ignored");
            } else if let Some(info) = wt_advertisement(&st) {
                info!(%addr, port = msg_port(&info), "wt video path re-advertised for a redial");
                let _ = out_tx.send(info);
            } else {
                warn!(%addr, "wt redial requested but no WT transport is bound");
            }
            continue;
        }
        // A link displaced by a newer player speaks for nobody: its messages
        // (restart media, launch a game, change mode) would act on the new
        // player's session. It ends here; `end_link` below is a no-op for it.
        if !st.sessions.owns(session_ticket) {
            info!(%addr, "signaling link was displaced by a newer player - closing it");
            break;
        }
        if !handle_message(&st, &out_tx, &media_tx, msg).await {
            break;
        }
    }

    // Link closed: release input + tear down media - but only if this link
    // still owns the session. A displaced link's late FIN arrives here after
    // its successor claimed; tearing down unconditionally killed the new
    // player's session mid-stream (the "quit, re-enter, black" repro).
    info!(%addr, "signaling link closed");
    st.sessions.end_link(session_ticket);
    pump.abort();
    input_pump.abort();
    control_pump.abort();
}

/// Returns `false` to end the session.
async fn handle_message(
    st: &HttpState,
    out: &mpsc::UnboundedSender<SignalMessage>,
    media: &mpsc::UnboundedSender<MediaSignal>,
    msg: SignalMessage,
) -> bool {
    match msg {
        SignalMessage::ClientHello {
            protocol_version,
            requested_mode,
            ..
        } => {
            if protocol_version != SIGNALING_PROTOCOL_VERSION {
                let _ = out.send(SignalMessage::error(
                    SignalErrorCode::ProtocolVersion,
                    format!(
                        "host speaks signaling v{SIGNALING_PROTOCOL_VERSION}, client sent v{protocol_version} - reload the page"
                    ),
                ));
                return false;
            }
            // Stash requested mode for when capabilities arrive.
            st.sessions.with_player(|p| {
                p.config = Some(provisional_config(st, &requested_mode));
            });
            true
        }
        SignalMessage::ClientCapabilities {
            rtp_video_codecs,
            decode_hints,
            features,
            ..
        } => {
            let chosen = choose_config(st, &rtp_video_codecs, &decode_hints);
            let Some(cfg) = chosen else {
                let _ = out.send(SignalMessage::error(
                    SignalErrorCode::NoCommonCodec,
                    "host and browser share no usable video codec",
                ));
                return false;
            };
            let _ = features; // used later to gate keyboard-lock messaging (§24)
            info!(
                codec = ?cfg.codec,
                w = cfg.width,
                h = cfg.height,
                fps = cfg.fps,
                start_kbps = cfg.start_bitrate_kbps,
                max_kbps = cfg.max_bitrate_kbps,
                min_kbps = cfg.min_bitrate_kbps,
                target = ?cfg.stream_target,
                "negotiated session config"
            );

            // If the client picked a library game, start it on the host (unless
            // it is already running). Fire-and-forget: the stream comes up on
            // the desktop and the game appears in it a few seconds later, the
            // same as a console launch. A launch failure is not fatal — the
            // player still gets the desktop.
            if let inphase_protocol::StreamTarget::Game { id } = &cfg.stream_target {
                let id = id.clone();
                tokio::task::spawn_blocking(move || match crate::platform::launch_game(&id) {
                    Ok(true) => {}
                    Ok(false) => {}
                    Err(e) => warn!("could not launch `{id}`: {e:#}"),
                });
            }
            let media = media.clone();
            let cfg_for_build = cfg.clone();
            // GStreamer pipeline construction touches a lot of C; a bad element
            // property could still panic. Catch it so the client gets a typed
            // error instead of an eternal "Connecting..." (§15, §22).
            let started = st.sessions.with_player(move |p| {
                p.config = Some(cfg_for_build.clone());
                p.media.set_signal_sink(media);
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    p.media
                        .configure(&cfg_for_build)
                        .and_then(|_| p.media.start())
                }));
                match res {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(_) => Err("media pipeline panicked during construction".to_string()),
                }
            });
            match started {
                Some(Ok(())) => {
                    let _ = out.send(SignalMessage::SessionConfig(cfg));
                    st.sessions.transition(SessionState::Negotiating);
                    true
                }
                Some(Err(e)) => {
                    warn!("media pipeline setup failed: {e}");
                    let _ = out.send(SignalMessage::error(classify_media_error(&e), e));
                    // Reset the slot so the next attempt is not stuck on BUSY.
                    st.sessions.transition(SessionState::Stopping);
                    false
                }
                None => false,
            }
        }
        m @ (SignalMessage::Answer { .. } | SignalMessage::Ice(_)) => {
            st.sessions.with_player(|p| {
                if let Err(e) = p.media.on_client_signal(&m) {
                    warn!("client signal rejected: {e:#}");
                }
            });
            true
        }
        SignalMessage::Ping { at_us } => {
            let _ = out.send(SignalMessage::Pong { at_us });
            true
        }
        SignalMessage::ClientTelemetry(t) => {
            tracing::debug!(
                decoded_fps = t.decoded_fps as u32,
                inbound_kbps = t.inbound_bitrate_kbps as u32,
                lost = t.packets_lost,
                freezes = t.freeze_count,
                rtt_ms = t.rtt_ms as u32,
                jb_ms = t.jitter_buffer_delay_ms as u32,
                "client telemetry"
            );
            st.stats.ingest_client_signaling(t);
            true
        }
        SignalMessage::Bye => false,
        _ => true,
    }
}

/// Verify an `auth_response`: `signature` (base64) must be a valid Ed25519
/// signature by `controller_id` (hex) over the challenge `nonce`.
///
/// A valid session cookie already authenticated this socket; the device key
/// is the durable identity, and the two are tied together:
///
/// * off the local network a known key is accepted only on a session it is
///   bound to (or one with no bindings yet: paired before bindings existed,
///   which then binds to it). A cookie carried off the network cannot borrow
///   another device's standing;
/// * a key never seen before is enrolled only from the local network. iOS Home
///   Screen apps share Safari's cookie but keep their own IndexedDB, and
///   Safari may clear IndexedDB after a week of disuse while the cookie
///   survives; both legitimately arrive with a new key on a paired cookie. Off
///   the LAN the same request is exactly what a stolen cookie looks like, so
///   there the device has to be re-paired;
/// * revoked keys stay revoked, and revoking a key revokes its sessions.
fn verify_device(
    st: &HttpState,
    peer: &PeerDesc,
    controller_id: &str,
    nonce: &[u8; 32],
    signature: &str,
) -> Option<crate::identity::acl::Controller> {
    let bound = peer
        .session
        .as_deref()
        .map(|sid| st.pairing.session_controllers(sid))
        .unwrap_or_default();
    let policy = EnrolPolicy {
        bound: &bound,
        may_enrol: peer.lan || peer.session.is_none(),
        checked: peer.session.is_some(),
    };
    verify_and_maybe_enroll(&st.acl, &policy, controller_id, nonce, signature)
}

/// What a session may do with a verified key (see [`verify_device`]).
struct EnrolPolicy<'a> {
    /// Keys already bound to the session.
    bound: &'a [String],
    /// A key the host has never seen may be enrolled.
    may_enrol: bool,
    /// Enforce `bound` (false only for links with no session cookie).
    checked: bool,
}

fn verify_and_maybe_enroll(
    acl: &crate::identity::acl::ControllerAcl,
    policy: &EnrolPolicy<'_>,
    controller_id: &str,
    nonce: &[u8; 32],
    signature: &str,
) -> Option<crate::identity::acl::Controller> {
    // Truncate on char boundaries - a byte slice panics on multibyte
    // input (pre-auth, client-supplied JSON).
    let short: String = controller_id.chars().take(12).collect();
    let pk: [u8; 32] = unhex(controller_id).and_then(|v| v.try_into().ok())?;
    let vk = VerifyingKey::from_bytes(&pk).ok()?;
    let sig_bytes = b64d(signature)?;
    let Ok(sig): Result<[u8; 64], _> = sig_bytes.as_slice().try_into() else {
        warn!(
            id = short,
            sig_len = sig_bytes.len(),
            "auth: signature is not 64 bytes"
        );
        return None;
    };
    if vk.verify(nonce, &Signature::from_bytes(&sig)).is_err() {
        warn!(
            id = short,
            "auth: Ed25519 signature over the challenge did not verify"
        );
        return None;
    }
    let id = controller_id.to_ascii_lowercase();
    if let Some(controller) = acl.get(&id) {
        // On the LAN a session may pick up another enrolled key (Safari and
        // its Home Screen app, paired separately, sharing one cookie). Off it,
        // only a key this session has already used: a cookie carried off the
        // network cannot borrow another device's standing.
        if policy.checked
            && !policy.may_enrol
            && !policy.bound.is_empty()
            && !policy.bound.iter().any(|b| b.eq_ignore_ascii_case(&id))
        {
            warn!(
                id = short,
                "auth: off-network session presented a device key it has not used before - rejecting"
            );
            return None;
        }
        return Some(controller);
    }
    if acl
        .list()
        .iter()
        .any(|c| c.id.eq_ignore_ascii_case(&id) && c.revoked)
    {
        warn!(id = short, "auth: revoked controller — not re-enrolling");
        return None;
    }
    if !policy.may_enrol {
        warn!(
            id = short,
            "auth: new device key on a paired session from off the local network - \
             refusing to enrol it (re-pair this device on the LAN)"
        );
        return None;
    }
    // Cookie proved this origin is paired; the signature proved possession
    // of this new key. That is the Home Screen / split-storage case.
    info!(
        id = short,
        "auth: enrolled a new device key from a paired session (Home Screen / split storage)"
    );
    acl.add(&id, "Home Screen app");
    acl.get(&id)
}

/// What a preview may ask for: the tile's size at most, 30 fps, a few Mbps,
/// and only ever the desktop - a preview must not launch a game.
fn preview_mode(m: &mut inphase_protocol::RequestedMode) {
    const MAX_W: u32 = 960;
    const MAX_H: u32 = 540;
    if m.width > MAX_W || m.height > MAX_H {
        let scale = f64::min(
            MAX_W as f64 / m.width as f64,
            MAX_H as f64 / m.height as f64,
        );
        m.width = ((m.width as f64 * scale) as u32) & !1;
        m.height = ((m.height as f64 * scale) as u32) & !1;
    }
    m.fps = m.fps.min(30);
    m.max_bitrate_kbps = Some(m.max_bitrate_kbps.unwrap_or(2_500).min(2_500));
    m.stream_target = inphase_protocol::StreamTarget::Desktop;
}

/// Clamp a client-requested mode to what the host will actually serve.
fn clamp_mode(w: u32, h: u32, fps: u32) -> (u32, u32, u32) {
    // Honour the exact requested rate (NVENC/VA are fine with arbitrary fps),
    // just bound it. Width/height are bounded and forced even (H.264 4:2:0).
    let fps = fps.clamp(15, 240);
    let w = w.clamp(640, 3840) & !1;
    let h = h.clamp(360, 2160) & !1;
    (w, h, fps)
}

/// The `wt_video_info` to advertise for this host, or `None` when the host
/// runs WebRTC-only (the transport did not bind, or the client pre-dates WT).
/// Minting the one-time token is the signaling layer's job: the QUIC dial
/// itself authenticates nothing (ADR-0011).
pub fn wt_advertisement(st: &HttpState) -> Option<SignalMessage> {
    let wt = st.wt.get()?;
    Some(SignalMessage::WtVideoInfo {
        token: wt.issue_token(std::time::Duration::from_secs(60)),
        port: wt.port(),
        cert_sha256: wt.cert_sha256_hex(),
    })
}

fn msg_port(msg: &SignalMessage) -> u16 {
    match msg {
        SignalMessage::WtVideoInfo { port, .. } => *port,
        _ => 0,
    }
}

fn provisional_config(st: &HttpState, m: &inphase_protocol::RequestedMode) -> SessionConfig {
    let (w, h, fps) = clamp_mode(m.width, m.height, m.fps);
    build_config(
        st,
        w,
        h,
        fps,
        m.preset,
        st.cfg.preferred_offer_codec(),
        "pending",
        m.max_bitrate_kbps,
        m.stream_target.clone(),
    )
}

/// §7 codec negotiation: prefer HEVC only when host allows it, the browser
/// advertised an H265 receive codec, *and* `decode_hints` say it can actually
/// decode the requested mode. Else H.264. Resolution / FPS / bitrate come from
/// what the client requested in `client_hello` (stashed in `active_config`).
fn choose_config(
    st: &HttpState,
    client_codecs: &[RtpCodecCapability],
    decode_hints: &[DecodeHint],
) -> Option<SessionConfig> {
    let client_has = |mime: &str| {
        client_codecs
            .iter()
            .any(|c| c.mime_type.eq_ignore_ascii_case(mime))
    };
    let decodes = |codec: VideoCodec| -> bool {
        if decode_hints.is_empty() {
            return true;
        }
        decode_hints.iter().any(|h| h.codec == codec && h.supported)
    };
    // Prefer HEVC when the browser advertises it *and* reports it can decode
    // the mode; otherwise fall back to H.264, the interoperability floor
    // (ADR-0005). Offering HEVC alone stranded every browser without it —
    // Chrome on Linux has no HEVC decoder at all — while H.264 decoded those
    // same machines fine. Worse, the HEVC probe can answer `true` and then fail
    // at `configure()` (§30), so a client that passes the gate can still be
    // left with no picture and no fallback.
    let codec = if client_has("video/H265") && decodes(VideoCodec::H265) {
        VideoCodec::H265
    } else if client_has("video/H264") && decodes(VideoCodec::H264) {
        VideoCodec::H264
    } else {
        info!(
            client_has_h265 = client_has("video/H265"),
            client_has_h264 = client_has("video/H264"),
            decode_hints = decode_hints.len(),
            "client can decode neither H.265 nor H.264 - refusing the video session"
        );
        return None;
    };
    info!(?codec, "negotiated video codec");

    // Reuse the mode the client asked for; only settle codec + backend now.
    let base = st.sessions.active_config().unwrap_or_else(|| {
        build_config(
            st,
            st.cfg.media.default_width,
            st.cfg.media.default_height,
            st.cfg.media.default_fps,
            st.cfg.media.default_preset,
            codec,
            "hardware",
            None,
            inphase_protocol::StreamTarget::Desktop,
        )
    });
    let requested_bitrate = (base.encoder_backend == "pending").then_some(base.max_bitrate_kbps);
    Some(build_config(
        st,
        base.width,
        base.height,
        base.fps,
        base.preset,
        codec,
        "hardware",
        requested_bitrate,
        base.stream_target.clone(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_config(
    st: &HttpState,
    w: u32,
    h: u32,
    fps: u32,
    preset: QualityPreset,
    codec: VideoCodec,
    encoder_backend: &str,
    bitrate_override_kbps: Option<u32>,
    stream_target: inphase_protocol::StreamTarget,
) -> SessionConfig {
    let (policy_start, policy_max, _policy_min) = encoder_policy::bitrate_kbps(codec, w, h, fps);
    // `max` is the ceiling the adaptive controller (pipeline.rs) is allowed to
    // reach; the client's requested cap overrides the policy default.
    let max = bitrate_override_kbps
        .map(|b| b.clamp(2_000, 120_000))
        .unwrap_or(policy_max);
    // Always *start* modestly regardless of the ceiling: opening at 50 Mbps
    // floods a constrained path (cellular, or a Tailscale DERP relay ~1 Mbps)
    // before the first RTCP report and collapses the session. The controller
    // ramps to `max` within a few seconds on a clean path.
    let start = policy_start.min(max).min(6_000);
    // Floor low enough to survive a relay: ~800 kbps 1080p is a slideshow but
    // the input path stays alive and the picture still moves.
    let min = 600u32;
    let backend = crate::input::backends::new_default_backend(&st.cfg.input);
    SessionConfig {
        codec,
        width: w,
        height: h,
        fps,
        preset,
        start_bitrate_kbps: start,
        max_bitrate_kbps: max,
        min_bitrate_kbps: min,
        jitter_buffer_target_ms: encoder_policy::jitter_buffer_target_ms(preset),
        input: inphase_protocol::InputCapabilities {
            keyboard: true,
            mouse: true,
            gamepad: st.cfg.input.enable_virtual_hid,
            backend: backend.name().to_string(),
        },
        encoder_backend: encoder_backend.to_string(),
        ice_servers: browser_ice_servers(st),
        stream_target,
    }
}

/// what the browser advertises as `iceServers`. Strict-LAN → none.
/// Otherwise InPhase's configured STUN URI(s), rewritten from webrtcbin's
/// `stun://host:port` to the browser's `stun:host:port` form. No TURN.
fn browser_ice_servers(st: &HttpState) -> Vec<String> {
    if st.cfg.network.strict_lan {
        return Vec::new();
    }
    st.cfg
        .network
        .stun_servers
        .iter()
        .map(|u| u.replacen("stun://", "stun:", 1))
        .collect()
}

fn classify_media_error(e: &str) -> SignalErrorCode {
    let e = e.to_lowercase();
    if e.contains("encoder") || e.contains("nvenc") || e.contains("registry") {
        SignalErrorCode::NoHardwareEncoder
    } else if e.contains("capture") || e.contains("d3d11") || e.contains("dxgi") {
        SignalErrorCode::CaptureUnavailable
    } else {
        SignalErrorCode::Internal
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::acl::ControllerAcl;
    use ed25519_dalek::{Signer, SigningKey};

    fn hex32(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn temp_acl() -> ControllerAcl {
        let path = std::env::temp_dir().join(format!(
            "inphase-acl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        ControllerAcl::load_at(path)
    }

    /// A LAN session with no bindings yet: the most permissive real case.
    const LAN_NEW: EnrolPolicy<'static> = EnrolPolicy {
        bound: &[],
        may_enrol: true,
        checked: true,
    };

    #[test]
    fn a_new_key_is_not_enrolled_from_off_the_lan() {
        let acl = temp_acl();
        let (_sk, id, nonce, sig) = signed(11);
        let remote = EnrolPolicy {
            bound: &[],
            may_enrol: false,
            checked: true,
        };
        assert!(verify_and_maybe_enroll(&acl, &remote, &id, &nonce, &sig).is_none());
        assert!(acl.get(&id).is_none(), "nothing may be enrolled");
    }

    #[test]
    fn off_the_lan_a_known_key_is_refused_on_a_session_bound_to_other_keys() {
        let acl = temp_acl();
        let (_sk, id, nonce, sig) = signed(12);
        acl.add(&id, "phone");
        let other = vec!["ab".repeat(32)];
        let remote = EnrolPolicy {
            bound: &other,
            may_enrol: false,
            checked: true,
        };
        assert!(verify_and_maybe_enroll(&acl, &remote, &id, &nonce, &sig).is_none());
        let lan = EnrolPolicy {
            bound: &other,
            may_enrol: true,
            checked: true,
        };
        assert!(
            verify_and_maybe_enroll(&acl, &lan, &id, &nonce, &sig).is_some(),
            "Safari and its Home Screen app share a cookie on the LAN"
        );
        let mine = vec![id.clone()];
        let policy = EnrolPolicy {
            bound: &mine,
            may_enrol: false,
            checked: true,
        };
        assert!(
            verify_and_maybe_enroll(&acl, &policy, &id, &nonce, &sig).is_some(),
            "its own session, from anywhere"
        );
    }

    fn signed(seed: u8) -> (SigningKey, String, [u8; 32], String) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let id = hex32(&sk.verifying_key().to_bytes());
        let nonce = [9u8; 32];
        let sig = b64e(&sk.sign(&nonce).to_bytes());
        (sk, id, nonce, sig)
    }

    #[test]
    fn paired_session_enrolls_a_new_home_screen_key() {
        let acl = temp_acl();
        let (_sk, id, nonce, sig) = signed(7);
        assert!(acl.get(&id).is_none());
        let c = verify_and_maybe_enroll(&acl, &LAN_NEW, &id, &nonce, &sig).expect("enroll");
        assert_eq!(c.id, id);
        assert_eq!(c.name, "Home Screen app");
        assert!(acl.is_authorized(&id));
    }

    #[test]
    fn known_key_is_not_renamed_on_reconnect() {
        let acl = temp_acl();
        let (_sk, id, nonce, sig) = signed(8);
        acl.add(&id, "Safari on iOS");
        let c = verify_and_maybe_enroll(&acl, &LAN_NEW, &id, &nonce, &sig).unwrap();
        assert_eq!(c.name, "Safari on iOS");
    }

    #[test]
    fn revoked_key_is_not_silently_reenrolled() {
        let acl = temp_acl();
        let (_sk, id, nonce, sig) = signed(9);
        acl.add(&id, "old phone");
        acl.revoke(&id);
        assert!(verify_and_maybe_enroll(&acl, &LAN_NEW, &id, &nonce, &sig).is_none());
        assert!(!acl.is_authorized(&id));
    }

    #[test]
    fn bad_signature_is_not_enrolled() {
        let acl = temp_acl();
        let (_sk, id, nonce, _sig) = signed(3);
        let other = SigningKey::from_bytes(&[4u8; 32]);
        let sig = b64e(&other.sign(&nonce).to_bytes());
        assert!(verify_and_maybe_enroll(&acl, &LAN_NEW, &id, &nonce, &sig).is_none());
        assert!(acl.get(&id).is_none());
    }
}
