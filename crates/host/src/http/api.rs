//! REST handlers (architecture report §16 "Host HTTP API").

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use inphase_protocol::{QualityPreset, VideoCodec, SIGNALING_PROTOCOL_VERSION};

use super::{require_session, HttpState, SESSION_COOKIE};
use crate::input::backends::new_default_backend;
use crate::platform;
use crate::session::SessionState;

pub async fn index() -> Response {
    super::assets::serve("index.html")
}

pub async fn static_asset(uri: axum::http::Uri) -> Response {
    super::assets::serve(uri.path())
}

/// `GET /ca.crt` — the local CA certificate, for installing on other devices so
/// their browsers trust this host's HTTPS. Unauthenticated; also served over
/// plaintext HTTP so a device can fetch it before it trusts the HTTPS origin.
pub async fn ca_cert(State(st): State<HttpState>) -> Response {
    let path = st.cfg.tls.cert_dir_path().join("ca.crt");
    match std::fs::read(&path) {
        Ok(pem) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"InPhase-Local-CA.crt\"",
                ),
            ],
            pem,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "no local CA (this host uses a different TLS mode)",
        )
            .into_response(),
    }
}

/// `GET /api/v1/status` — safe host status, no auth (§16).
#[derive(Serialize)]
pub struct HostStatus {
    pub pc_name: String,
    pub version: String,
    /// The build this host binary came from (see [`crate::BUILD_ID`]). The page
    /// compares it against its own compiled-in id and reloads once if they
    /// differ, so a tab left open across a deploy cannot keep talking an old
    /// wire format to a new host.
    pub build_id: &'static str,
    pub signaling_protocol: u32,
    pub state: &'static str,
    pub busy: bool,
    pub available: bool,
    pub supported_modes: Vec<Mode>,
    /// True when served over HTTPS with a real cert (secure context available).
    pub https: bool,
    /// `local-ca` | `off` — how the HTTPS cert is obtained.
    pub tls_mode: &'static str,
    /// Router port-mapping verdict for remote access (PCP / NAT-PMP). Omitted
    /// unless remote access is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_mapping: Option<crate::portmap::MappingStatus>,
    /// The URL to open in a browser (`https://<machine>.local/` etc.).
    pub play_url: String,
    /// WebTransport video transport (ADR-0011): dial port + pinned cert hash,
    /// present whenever the transport bound. `None` = transport did not start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wt: Option<WtStatus>,
    /// Stable Ed25519 Host ID (hex). The trust anchor a controller pins.
    pub host_id: String,
    /// What the host is currently streaming, when a session is live. The play
    /// page shows this so a second device knows what is already on screen and
    /// can highlight the matching library card. `null` when idle. The client
    /// resolves a game id to a name from its own `/api/v1/library` copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_stream: Option<inphase_protocol::StreamTarget>,
}

/// Public shape of the WT video offer (no secrets — the cert hash is designed
/// to be public; trust comes from the authenticated signaling socket).
#[derive(Serialize, Clone)]
pub struct WtStatus {
    pub port: u16,
    pub cert_sha256: String,
}

#[derive(Serialize, Clone)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub async fn status(State(st): State<HttpState>) -> Json<HostStatus> {
    let state = st.sessions.state();
    let active_stream = matches!(
        state,
        SessionState::Playing | SessionState::Negotiating | SessionState::Reconnecting
    )
    .then(|| st.sessions.active_config().map(|c| c.stream_target))
    .flatten();
    Json(HostStatus {
        active_stream,
        pc_name: st.host_name.clone(),
        version: crate::HOST_VERSION.to_string(),
        build_id: crate::BUILD_ID,
        signaling_protocol: SIGNALING_PROTOCOL_VERSION,
        state: state.label(),
        busy: st.sessions.is_busy(),
        available: matches!(state, SessionState::Idle),
        supported_modes: default_modes(),
        host_id: st.identity.host_id(),
        wt: st.wt.as_ref().map(|t| WtStatus {
            port: t.port(),
            cert_sha256: t.cert_sha256_hex(),
        }),
        https: st.https,
        tls_mode: match st.cfg.tls.mode {
            crate::config::TlsMode::LocalCa => "local-ca",
            crate::config::TlsMode::Off => "off",
        },
        remote_mapping: st
            .remote_access
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| st.remote_mapping.read().clone()),
        play_url: st.play_url.clone(),
    })
}

/// Is the stream actually working? A verdict with named causes, not raw metrics
/// (see [`crate::health`]). Unauthenticated on purpose: it exposes no session
/// content, and a health check that needs a cookie is one nobody runs.
pub async fn health(State(st): State<HttpState>) -> Json<crate::health::Health> {
    Json(st.stats.health())
}

fn default_modes() -> Vec<Mode> {
    [
        (1920, 1080, 60),
        (1920, 1080, 120),
        (2560, 1440, 60),
        (2560, 1440, 120),
    ]
    .into_iter()
    .map(|(width, height, fps)| Mode { width, height, fps })
    .collect()
}

/// `POST /api/v1/pair` — authenticate a new browser and receive an HttpOnly
/// session cookie (§14.1). Two mutually-exclusive auth modes:
///
/// * **PIN** (`pin`) — the Host-screen PIN (§14.1).
/// * **Invitation** (`invite`) — the base64url secret from a QR fragment, or the
///   typed short code, minted at `POST /api/v1/admin/pair-invite`. Presented over
///   the host's own HTTPS, so the secret itself is the proof (no PAKE / HMAC).
#[derive(Deserialize)]
pub struct PairRequest {
    #[serde(default)]
    pub pin: String,
    /// Ed25519 controller public key, hex — the durable per-browser device
    /// identity. The browser signs a challenge with the matching
    /// private key on every signaling connect.
    #[serde(default)]
    pub controller_pubkey: Option<String>,
    #[serde(default)]
    pub controller_name: Option<String>,
    /// Invitation secret (base64url) or typed short code.
    #[serde(default)]
    pub invite: Option<String>,
}

pub async fn pair(
    State(st): State<HttpState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> Response {
    // Enrolment happens on the LAN. Full stop, unless the operator has
    // explicitly opted into remote invites.
    //
    // This is the hinge the whole remote-access design turns on: pairing here
    // writes a non-extractable Ed25519 key into the browser and records it in
    // the controller ACL, and *that key* is what authenticates the device later
    // from anywhere. So there is no need for an internet-reachable way to
    // enrol, and not having one means the only remote credential is a key that
    // was issued face-to-face on your own network — unguessable, and nothing a
    // stranger can start the process for.
    //
    // It is also what keeps the PIN honest. Six digits is 10^6: a speed bump,
    // not a lock. Facing it at the internet is the operator's call — the first
    // gate above only lets it through with `allow_remote_pairing`, the PIN path
    // below additionally demands HTTPS, and the limiter counts attempts per /64,
    // so spraying guesses across one delegated prefix buys an attacker nothing.
    let on_lan = crate::net::is_lan_peer(addr.ip());
    if !on_lan && !st.cfg.remote_access.allow_remote_pairing {
        tracing::warn!(
            "refused an off-network pairing attempt - devices are paired on the \
             LAN, then authenticate remotely with the key they were issued"
        );
        return pair_err(StatusCode::FORBIDDEN, "pairing_is_lan_only");
    }

    // ---- invitation path ----------------------------------------------
    if let Some(invite) = body.invite.as_deref().filter(|s| !s.trim().is_empty()) {
        return match st.invites.consume(invite) {
            Ok(()) => pair_success(&st, &body).await,
            Err(e) => pair_err(StatusCode::UNAUTHORIZED, e.as_str()),
        };
    }

    // ---- PIN path ------------------------------------------------------
    // Remote PIN pairing is opt-in (`allow_remote_pairing` above) and
    // HTTPS-only: a six-digit code on plaintext HTTP is readable on every hop
    // between phone and host.
    if !on_lan && !st.https {
        return pair_err(StatusCode::FORBIDDEN, "pin_requires_https");
    }

    match st.pairing.try_pair(addr.ip(), body.pin.trim()) {
        Ok(_sid) => pair_success(&st, &body).await,
        Err(crate::pairing::PairError::BadPin) => pair_err(StatusCode::UNAUTHORIZED, "bad_pin"),
        Err(crate::pairing::PairError::RateLimited { retry_after }) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.as_secs().to_string())],
            Json(serde_json::json!({ "error": "rate_limited" })),
        )
            .into_response(),
    }
}

fn pair_err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Common success path: register the controller key in the ACL and mint the
/// session cookie.
async fn pair_success(st: &HttpState, body: &PairRequest) -> Response {
    // A device key is mandatory. Minting a session cookie without enrolling one
    // produces a browser that looks paired and can never stream: every
    // signalling connect fails the Ed25519 device challenge, and the only way
    // out is "Forget this device". Fail the pair instead.
    let Some(pk) = body.controller_pubkey.as_deref().filter(|p| is_hex_key(p)) else {
        tracing::warn!("pair: rejected - missing or malformed controller_pubkey");
        return pair_err(StatusCode::BAD_REQUEST, "controller_key_required");
    };
    let name = body
        .controller_name
        .as_deref()
        .map(sanitize_name)
        .unwrap_or_else(|| "browser".into());
    st.acl.add(pk, &name);

    let sid = st.pairing.mint_session();
    let secure = st.https;
    let max_age = match st.cfg.pairing.session_ttl_secs {
        0 => 10 * 365 * 24 * 3600,
        n => n,
    };
    let cookie = format!(
        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{}",
        sid.as_str(),
        if secure { "; Secure" } else { "" }
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true, "host_id": st.identity.host_id() })),
    )
        .into_response()
}

/// `GET /api/v1/session` — is this browser still paired? Lets the play page skip
/// the PIN screen when the cookie is still valid.
pub async fn session_check(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    match require_session(&st, &headers) {
        Some(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "authenticated": true })),
        )
            .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "authenticated": false })),
        )
            .into_response(),
    }
}

/// `GET /api/v1/capabilities` — authenticated (§16). Host codecs, resolutions,
/// FPS, input backends, plus monitor/GPU enumeration for the dashboard.
#[derive(Serialize)]
pub struct Capabilities {
    pub codecs: Vec<&'static str>,
    pub modes: Vec<Mode>,
    pub presets: Vec<&'static str>,
    pub input_backends: Vec<String>,
    pub monitors: Vec<platform::MonitorInfo>,
    pub gpus: Vec<platform::GpuInfo>,
    pub require_hardware_encoder: bool,
    pub audio: AudioCaps,
}

/// Loopback-audio state + endpoint list (§11).
#[derive(Serialize)]
pub struct AudioCaps {
    pub enabled: bool,
    pub capture: AudioCapture,
    /// All playback endpoints; `is_default` marks the Windows default.
    pub endpoints: Vec<platform::AudioEndpointInfo>,
}

#[derive(Serialize)]
pub struct AudioCapture {
    /// Chosen endpoint id, or "" for "follow the system default".
    pub device_id: String,
    pub name: String,
    pub is_default: bool,
    pub active: bool,
    pub health: crate::stats::AudioHealth,
    /// From the active player's telemetry: `true` = real signal seen,
    /// `false` = samples flowing but silent, `null` = unknown / no session.
    pub signal_detected: Option<bool>,
}

pub async fn capabilities(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    if require_session(&st, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "pair first").into_response();
    }
    // H.265 only (user directive): H.264 is not a capability any more.
    let codecs = vec![VideoCodec::H265.mime_type()];
    let backend = new_default_backend(&st.cfg.input);
    let endpoints = if st.cfg.media.enable_audio {
        platform::enumerate_audio_endpoints().unwrap_or_default()
    } else {
        Vec::new()
    };
    // Live value the next session will use — the runtime override wins.
    let device_id = st
        .sessions
        .audio_device()
        .or_else(|| st.cfg.media.audio_capture_device.clone())
        .unwrap_or_default();
    let matched = endpoints.iter().find(|e| {
        if device_id.is_empty() {
            e.is_default
        } else {
            e.id == device_id
        }
    });
    let signal_detected = st
        .stats
        .snapshot()
        .client
        .and_then(|c| c.audio_state)
        .and_then(|s| match s.as_str() {
            "signal" => Some(true),
            "silent" => Some(false),
            _ => None,
        });

    Json(Capabilities {
        codecs,
        modes: default_modes(),
        presets: preset_names(),
        input_backends: vec![backend.name().to_string()],
        monitors: platform::enumerate_monitors().unwrap_or_default(),
        gpus: platform::enumerate_gpus().unwrap_or_default(),
        require_hardware_encoder: st.cfg.media.require_hardware_encoder,
        audio: AudioCaps {
            enabled: st.cfg.media.enable_audio,
            capture: AudioCapture {
                name: matched.map(|e| e.name.clone()).unwrap_or_else(|| {
                    if device_id.is_empty() {
                        "System default".into()
                    } else {
                        device_id.clone()
                    }
                }),
                is_default: device_id.is_empty() || matched.map(|e| e.is_default).unwrap_or(false),
                active: matched.map(|e| e.active).unwrap_or(true),
                health: st.stats.audio_health(),
                signal_detected,
                device_id,
            },
            endpoints,
        },
    })
    .into_response()
}

fn preset_names() -> Vec<&'static str> {
    vec!["low_latency", "balanced", "quality", "custom"]
}

/// `POST /api/v1/session/stop` — the player ends its own session (§16).
pub async fn session_stop(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    match require_session(&st, &headers) {
        Some(_) => {
            st.sessions.transition(SessionState::Stopping);
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        None => (StatusCode::UNAUTHORIZED, "not the active player").into_response(),
    }
}

/// `POST /api/v1/audio/capture-device` — pick which playback endpoint InPhase
/// loopback-captures (§11). Authenticated; takes effect on the next session.
#[derive(Deserialize)]
pub struct AudioDeviceRequest {
    /// `IMMDevice::GetId` string, or null/"" for the Windows default.
    #[serde(default)]
    pub device: Option<String>,
}

pub async fn set_audio_device(
    State(st): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<AudioDeviceRequest>,
) -> Response {
    if require_session(&st, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "pair first").into_response();
    }
    let id = body.device.filter(|s| !s.is_empty());
    st.sessions.set_audio_device(id.clone());
    if let Err(e) = crate::config::persist_audio_capture_device(id.as_deref()) {
        tracing::warn!("could not persist audio_capture_device: {e:#}");
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "device": id, "reconnect_required": true })),
    )
        .into_response()
}

/// `POST /api/v1/logout[?controller=<hex>]` — un-pair *this* browser: revoke the
/// session, clear the cookie, and (if the controller key is given) revoke it
/// from the ACL so the device is fully forgotten (§14).
pub async fn logout(
    State(st): State<HttpState>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Some(sid) = require_session(&st, &headers) {
        st.pairing.revoke(&sid);
        st.sessions.transition(SessionState::Stopping);
    }
    if let Some(pk) = q
        .as_deref()
        .and_then(|s| s.split('&').find_map(|kv| kv.strip_prefix("controller=")))
        .filter(|s| is_hex_key(s))
    {
        st.acl.revoke(pk);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
        )],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

// ---- admin (loopback only) ----------------------------------------------

pub async fn admin_status(State(st): State<HttpState>) -> Json<serde_json::Value> {
    // Loopback-only, so exposing the current PIN here is fine — it is exactly
    // what the host dashboard needs (§25.1 "Current short-lived PIN with expiry
    // countdown").
    let (pin, ttl) = st.pairing.current_pin();
    Json(serde_json::json!({
        "state": st.sessions.state().label(),
        "pin": pin,
        "pin_ttl_secs": ttl.map(|d| d.as_secs()),
        "paired_devices": st.pairing.session_count(),
        "play_url": st.play_url,
        "peer": st.sessions.peer_info().map(|p| serde_json::json!({
            "browser": p.browser, "ip": p.client_ip,
        })),
        "uptime_secs": st.started_at.elapsed().as_secs(),
        "stats": st.stats.snapshot(),
    }))
}

pub async fn admin_disconnect(State(st): State<HttpState>) -> Response {
    st.sessions.force_disconnect();
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

pub async fn admin_rotate_pin(State(st): State<HttpState>) -> Response {
    let pin = st.pairing.rotate_pin();
    (StatusCode::OK, Json(serde_json::json!({ "pin": pin }))).into_response()
}

/// Un-pair every device (dashboard). Drops the active session, invalidates every
/// browser cookie, and revokes every registered controller key.
pub async fn admin_revoke_all(State(st): State<HttpState>) -> Response {
    st.pairing.revoke_all();
    st.acl.revoke_all();
    st.sessions.force_disconnect();
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// `POST /api/v1/admin/pair-invite` — mint a QR + short-code pairing
/// invitation. Loopback admin only. `needs_approval` is set automatically for
/// the first controller on a fresh Host.
pub async fn admin_pair_invite(State(st): State<HttpState>) -> Json<serde_json::Value> {
    let needs_approval = st.acl.is_first();
    let v = st.invites.create(needs_approval);
    // `<play_url>pair#<secret>` — the fragment stays in the browser until the
    // explicit pair POST.
    let base = st.play_url.trim_end_matches('/');
    let url = format!("{base}/pair#{}", v.secret_b64url);
    let qr_svg = qr_svg(&url);
    Json(serde_json::json!({
        "id": v.id,
        "url": url,
        "short_code": v.short_code,
        "qr_svg": qr_svg,
        "not_after_unix": v.not_after_unix,
        "needs_approval": v.needs_approval,
    }))
}

/// `GET /api/v1/admin/pair-invites` — invitations currently on screen.
pub async fn admin_pair_invites(State(st): State<HttpState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "invites": st.invites.pending() }))
}

#[derive(Deserialize)]
pub struct ApproveRequest {
    pub id: String,
}

/// `POST /api/v1/admin/pair-invite/approve {id}` — the operator authorises the
/// first controller (local Host approval is required for the first controller).
pub async fn admin_pair_approve(
    State(st): State<HttpState>,
    Json(body): Json<ApproveRequest>,
) -> Response {
    if st.invites.approve(body.id.trim()) {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        pair_err(StatusCode::NOT_FOUND, "invite not found or expired")
    }
}

fn qr_svg(data: &str) -> String {
    use qrcode::render::svg;
    match qrcode::QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(180, 180)
            .quiet_zone(true)
            .dark_color(svg::Color("#0b0f17"))
            .light_color(svg::Color("#ffffff"))
            .build(),
        Err(_) => String::new(),
    }
}

/// `GET /api/v1/admin/controllers` — the local controller ACL.
pub async fn admin_controllers(State(st): State<HttpState>) -> Json<serde_json::Value> {
    let host_id = st.identity.host_id();
    let controllers: Vec<_> = st
        .acl
        .list()
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "short": &c.id[..c.id.len().min(12)],
                "name": c.name,
                "added_unix": c.added_unix,
                "last_seen_unix": c.last_seen_unix,
                "revoked": c.revoked,
            })
        })
        .collect();
    Json(serde_json::json!({ "host_id": host_id, "controllers": controllers }))
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    /// Full controller id or a unique short prefix.
    pub id: String,
}

/// `POST /api/v1/admin/controllers/revoke {id}` — revoke one controller.
pub async fn admin_revoke_controller(
    State(st): State<HttpState>,
    Json(body): Json<RevokeRequest>,
) -> Response {
    match st.acl.revoke(body.id.trim()) {
        Some(id) => {
            // If the active player is that controller, cut it now.
            st.sessions.force_disconnect();
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "revoked": id })),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no unique match for that id" })),
        )
            .into_response(),
    }
}

// ---- helpers -----------------------------------------------------------------

/// A 64-char (Ed25519) or 130-char (P-256 raw point) lowercase hex string.
fn is_hex_key(s: &str) -> bool {
    matches!(s.len(), 64 | 130) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).take(80).collect();
    if cleaned.trim().is_empty() {
        "browser".into()
    } else {
        cleaned.trim().to_string()
    }
}

/// `GET /api/v1/library` — installed games for the play-page grid (desktop is
/// injected client-side as the first card).
#[derive(Serialize)]
pub struct LibraryItem {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
}

#[derive(Serialize)]
pub struct LibraryResponse {
    pub items: Vec<LibraryItem>,
    /// True while the host is still fetching Steam cover art in the background;
    /// the client polls again so posters upgrade without a reload.
    pub art_pending: bool,
}

pub async fn library(State(st): State<HttpState>) -> Json<LibraryResponse> {
    let games = platform::enumerate_installed_games().unwrap_or_default();

    // Prefer Steam box art everywhere, except for Steam games that already show
    // Steam's own art from the local client cache (same image — no point
    // re-fetching it).
    let wants: Vec<crate::gameart::Want> = games
        .iter()
        .filter(|g| !(g.source == "steam" && g.poster_url.is_some()))
        .map(|g| crate::gameart::Want {
            id: g.id.clone(),
            name: g.name.clone(),
            steam_appid: g.steam_app_id,
        })
        .collect();
    st.art.warm(wants);

    let items = games
        .into_iter()
        .map(|g| {
            // Steam art first, launcher poster as fallback. The `?s=` tag makes
            // the URL change when a launcher poster is later replaced by Steam
            // art, so the browser re-fetches instead of showing its cache.
            let poster_url = if st.art.cached(&g.id).is_some() {
                Some(format!(
                    "/api/v1/library/poster/{}?s=steam",
                    g.id.replace(':', "%3A")
                ))
            } else {
                g.poster_url
            };
            LibraryItem {
                id: g.id,
                name: g.name,
                kind: "game",
                poster_url,
                source: Some(g.source),
                last_played: g.last_played,
            }
        })
        .collect();
    Json(LibraryResponse {
        items,
        art_pending: st.art.busy(),
    })
}

/// `GET /api/v1/library/poster/:id` — a cover image: the Steam box art we
/// fetched keylessly, or the file a launcher had on disk.
pub async fn library_poster(State(st): State<HttpState>, Path(id): Path<String>) -> Response {
    let Some(path) = st
        .art
        .cached(&id)
        .or_else(|| platform::poster_path_for_game_id(&id))
    else {
        return (StatusCode::NOT_FOUND, "no poster").into_response();
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return (StatusCode::NOT_FOUND, "poster unreadable").into_response();
    };
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response()
}
