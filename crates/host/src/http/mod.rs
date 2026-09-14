//! HTTP + WebSocket server (architecture report §14, §16).
//!
//! Two listeners:
//! * **LAN** (`network.bind:http_port`) — `/`, `/api/v1/status`,
//!   `/api/v1/pair`, `/api/v1/capabilities`, `/api/v1/signal`,
//!   `/api/v1/session/stop`. Bound to LAN interfaces; a strict CSP is applied
//!   and no third-party JS/fonts are served (§14.1).
//! * **admin** (`127.0.0.1:admin_port`) — `/api/v1/admin/*`, loopback only
//!   (§16 *"Admin controls should be loopback-only by default"*).

mod api;
pub mod assets;
pub mod signal;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::config::{Config, TlsMode};
use crate::pairing::PairingManager;
use crate::session::SessionManager;
use crate::stats::StatsCollector;

/// Shared state for all handlers.
#[derive(Clone)]
pub struct HttpState {
    pub cfg: Arc<Config>,
    pub pairing: Arc<PairingManager>,
    pub sessions: Arc<SessionManager>,
    pub stats: Arc<StatsCollector>,
    pub identity: Arc<crate::identity::HostIdentity>,
    pub acl: Arc<crate::identity::acl::ControllerAcl>,
    pub invites: Arc<crate::identity::pairing_invite::InviteStore>,
    pub host_name: String,
    pub play_url: String,
    /// True once the host is actually serving HTTPS (a real cert). Gates the
    /// `Secure` cookie flag and is surfaced in `/api/v1/status`.
    pub https: bool,
    /// Whether remote (off-LAN) access is currently allowed. Toggled at runtime
    /// from the tray; read by [`remote_access_gate`] and `/api/v1/status`.
    pub remote_access: Arc<AtomicBool>,
    /// Keyless cover-art cache for library games missing local art.
    pub art: Arc<crate::gameart::ArtCache>,
    /// Live result of the router port-mapping task (empty unless remote access
    /// is on). Surfaced in `/api/v1/status` as `remote_mapping`.
    pub remote_mapping: crate::portmap::Shared,
    /// WebTransport video transport (ADR-0011), when the host enabled it. The
    /// signaling handler mints per-session tokens and advertises the dial
    /// info; `None` = WebRTC only.
    pub wt: Option<Arc<crate::media::wt::WtVideoTransport>>,
    pub started_at: Instant,
}

/// Cookie name for the authenticated browser session (§14.1).
pub const SESSION_COOKIE: &str = "inphase_session";

/// The `/api/v1/admin/*` surface. Mounted on the dedicated loopback listener
/// **and** (loopback-guarded) on the LAN listener so the dashboard, which is
/// served from the LAN origin, can reach it.
fn admin_routes() -> Router<HttpState> {
    Router::new()
        .route("/api/v1/admin/status", get(api::admin_status))
        .route(
            "/api/v1/admin/frame-timeline",
            get(api::admin_frame_timeline),
        )
        .route("/api/v1/admin/disconnect", post(api::admin_disconnect))
        .route("/api/v1/admin/rotate-pin", post(api::admin_rotate_pin))
        .route("/api/v1/admin/revoke-all", post(api::admin_revoke_all))
        .route("/api/v1/admin/controllers", get(api::admin_controllers))
        .route(
            "/api/v1/admin/controllers/revoke",
            post(api::admin_revoke_controller),
        )
        .route("/api/v1/admin/pair-invite", post(api::admin_pair_invite))
        .route("/api/v1/admin/pair-invites", get(api::admin_pair_invites))
        .route(
            "/api/v1/admin/pair-invite/approve",
            post(api::admin_pair_approve),
        )
}

pub fn lan_router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(api::index))
        .route("/pair", get(api::index))
        .route("/ca.crt", get(api::ca_cert))
        .route("/api/v1/status", get(api::status))
        .route("/api/v1/health", get(api::health))
        .route("/api/v1/pair", post(api::pair))
        .route("/api/v1/session", get(api::session_check))
        .route("/api/v1/capabilities", get(api::capabilities))
        .route("/api/v1/library", get(api::library))
        .route("/api/v1/library/poster/:id", get(api::library_poster))
        .route("/api/v1/session/stop", post(api::session_stop))
        .route("/api/v1/audio/capture-device", post(api::set_audio_device))
        .route("/api/v1/logout", post(api::logout))
        .route("/api/v1/signal", get(signal::signal_ws))
        .merge(admin_routes().layer(middleware::from_fn(loopback_only)))
        .fallback(get(api::static_asset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            remote_access_gate,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub fn admin_router(state: HttpState) -> Router {
    admin_routes()
        .layer(middleware::from_fn(loopback_only))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state)
}

/// Strict CSP + framing/referrer hardening for the play origin (§14.1: *"use a
/// strict Content-Security-Policy; serve no third-party JS or fonts"*).
///
/// The CSP is built per-request for exactly one reason: the WebTransport video
/// endpoint (ADR-0011) listens on its own port, and the browser must be allowed
/// to dial it. The client reaches the host under whichever hostname it loaded
/// the page from (IPv6 literal, mDNS name, LAN name), so the grant is scoped by
/// *port*, not host — `https://*:<wt_port>` — and only appears once a transport
/// is actually bound. Without one, the policy is byte-identical to the old
/// static string.
async fn security_headers(
    axum::extract::State(st): axum::extract::State<HttpState>,
    req: Request,
    next: Next,
) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    if is_api && !same_origin_request(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
    }
    let authority = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
        .filter(|h| !h.as_str().contains('@'));
    let mut connect_src = "'self'".to_string();
    if let Some(authority) = authority {
        connect_src.push_str(&format!(" ws://{authority} wss://{authority}"));
        if let Some(wt) = st.wt.as_ref() {
            connect_src.push_str(&format!(" https://{}:{}", authority.host(), wt.port()));
        }
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    if is_api {
        h.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
    }
    h.insert(
        "content-security-policy",
        HeaderValue::from_str(&format!(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src {connect_src}; media-src 'self' blob:; \
             font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ))
        .expect("CSP header content is controlled ASCII"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("gamepad=(self), fullscreen=(self)"),
    );
    res
}

/// Refuse every off-network request while remote access is disabled.
///
/// This is the single switch behind "Remote Access is off by default". It sits
/// in front of the whole LAN router — player page, pairing, signalling — so a
/// host that is reachable from the internet (an IPv6 firewall rule, a port
/// forward) still answers nothing until the operator opts in. Being reachable
/// and being open are deliberately two different things.
async fn remote_access_gate(
    axum::extract::State(st): axum::extract::State<HttpState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if st.remote_access.load(Ordering::Relaxed) || crate::net::is_lan_peer(addr.ip()) {
        return next.run(req).await;
    }
    // Logged because on an internet-facing host this is the only record that
    // anything off-network reached the listener at all — which is both a
    // security signal and, when remote access is being brought up, the
    // difference between "the router is not forwarding" and "it forwarded and
    // we refused".
    tracing::info!(peer = %addr.ip(), "refused an off-network request");
    // Deliberately terse: do not describe the service to an unauthenticated
    // stranger, and do not hint that enabling a setting would let them in.
    (StatusCode::FORBIDDEN, "not available").into_response()
}

/// Reject anything that is not from loopback on the admin listener.
async fn loopback_only(
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // Reject DNS rebinding: a loopback connection must name a loopback host.
    let local_host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
        .map(|h| {
            matches!(
                h.host().to_ascii_lowercase().as_str(),
                "localhost" | "127.0.0.1" | "[::1]"
            )
        })
        .unwrap_or(false);
    if addr.ip().is_loopback() && local_host && same_origin_request(req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "admin API is loopback-only").into_response()
    }
}

/// Check browser Origin and Fetch Metadata. Native clients may omit Origin;
/// they still pass the network, pairing and controller-key guards.
pub(super) fn same_origin_request(headers: &axum::http::HeaderMap) -> bool {
    use axum::http::{header, uri::Authority, Uri};
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin) = origin.to_str().ok().and_then(|v| v.parse::<Uri>().ok()) else {
        return false;
    };
    let Some(scheme @ ("http" | "https")) = origin.scheme_str() else {
        return false;
    };
    let Some(source) = origin.authority() else {
        return false;
    };
    let Some(target) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Authority>().ok())
    else {
        return false;
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    !source.as_str().contains('@')
        && !target.as_str().contains('@')
        && source.host().eq_ignore_ascii_case(target.host())
        && source.port_u16().unwrap_or(default_port) == target.port_u16().unwrap_or(default_port)
        && origin.path() == "/"
        && origin.query().is_none()
}

/// Authenticated-session guard used by the protected handlers.
pub fn require_session(state: &HttpState, headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    let sid = cookie
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())?;
    state
        .pairing
        .validate_session(&sid)
        .map(|s| s.as_str().to_string())
}

/// Everything `serve` needs to bring up the HTTPS listener.
pub struct ResolvedTls {
    pub domain: String,
    pub certs: crate::tls::CertPaths,
    pub https_addr: std::net::SocketAddr,
    /// `https://<domain>[:port]/`
    pub play_url: String,
    pub mode: TlsMode,
}

/// Work out whether the host can serve HTTPS and, if so, obtain the cert.
/// `None` means fall back to plaintext HTTP (dashboard-on-localhost only).
pub async fn resolve_tls(cfg: &Config) -> Option<ResolvedTls> {
    let dir = cfg.tls.cert_dir_path();
    let port = cfg.tls.port;
    let addr = std::net::SocketAddr::new(cfg.network.bind, port);

    match cfg.tls.mode {
        TlsMode::Off => None,

        TlsMode::LocalCa => {
            let (ca_pem, ca_key) = match crate::tls::local_ca::load_or_create_ca(&dir) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("local CA setup failed: {e:#}");
                    return None;
                }
            };
            crate::tls::local_ca::trust_ca(&dir, false).await;
            let (primary, sans) = crate::tls::local_ca::collect_sans(cfg);
            match crate::tls::local_ca::ensure_leaf(&ca_pem, &ca_key, &primary, &sans, &dir) {
                Ok(certs) => Some(ResolvedTls {
                    // One endpoint description: the advertised URL is the
                    // plan's canonical origin, not a locally-built string.
                    play_url: format!(
                        "{}/",
                        crate::endpoint::EndpointPlan::detect(
                            cfg,
                            crate::tls::machine_hostname(),
                            crate::net::stable_global_ipv6(cfg)
                        )
                        .canonical_origin()
                    ),
                    domain: primary,
                    certs,
                    https_addr: addr,
                    mode: TlsMode::LocalCa,
                }),
                Err(e) => {
                    tracing::error!("issuing the local leaf cert failed: {e:#}");
                    None
                }
            }
        }
    }
}

/// Spawn the listeners. Returns when one exits.
///
/// * **HTTPS** (`tls` present) on `tls.port` — the player + api + signaling.
/// * **plaintext HTTP** on `http_port` — always up: it serves `/ca.crt` and the
///   dashboard so a new device can grab the CA before it trusts HTTPS. The
///   player itself needs the HTTPS origin (secure context).
/// * **admin HTTP** on `127.0.0.1:admin_port` — loopback dashboard/admin.
///
/// Bind a listener, or fail with a message that names the cause.
///
/// "Address already in use" almost always means a second host was launched -
/// InPhase has two autostart paths (the installer's HKCU `Run` value, and a
/// scheduled task on dev machines), and when both fire the loser used to die
/// with a bare io error and, launched windowless, no console to print it to.
/// That is the "the exe died with no console" failure: the information existed
/// and had nowhere to go. Log it at error level so it reaches `host.log`
/// regardless of how the process was started.
async fn bind_or_explain(
    addr: std::net::SocketAddr,
    what: &str,
) -> anyhow::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::error!(
                %addr,
                listener = what,
                "port already in use - another InPhase host is almost certainly \
                 running already (look for InPhaseHost.exe in Task Manager). This \
                 instance is exiting; the one holding the port keeps serving."
            );
            anyhow::bail!("{what} port {addr} is already in use (another InPhase host is running)")
        }
        Err(e) => Err(anyhow::Error::new(e)).context(format!("binding {what} listener on {addr}")),
    }
}

pub async fn serve(state: HttpState, tls: Option<ResolvedTls>) -> anyhow::Result<()> {
    let admin_addr = state.cfg.admin_socket_addr();
    let admin = bind_or_explain(admin_addr, "admin").await?;
    info!(%admin_addr, "admin http listener up (loopback)");
    let admin_srv = axum::serve(
        admin,
        admin_router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    );

    let http_addr = state.cfg.http_socket_addr();
    let http = bind_or_explain(http_addr, "plaintext http").await?;
    info!(%http_addr, "plaintext http listener up (/ca.crt + dashboard)");
    let http_srv = axum::serve(
        http,
        lan_router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    );

    // A second plaintext listener on `[::]`, for the same reason the HTTPS
    // listener below gets one: Windows defaults `IPV6_V6ONLY` on, so binding
    // `http_addr` (`0.0.0.0`) alone is not reachable over IPv6 there. A new
    // device fetching `/ca.crt` to trust HTTPS — the whole reason this
    // plaintext port exists — needs it over whichever family it has.
    let http_v6_addr =
        std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, http_addr.port()));
    let http6_srv = match tokio::net::TcpListener::bind(http_v6_addr).await {
        Ok(l) => {
            info!(http_addr = %http_v6_addr, "plaintext http listener up (IPv6)");
            Some(axum::serve(
                l,
                lan_router(state.clone())
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            ))
        }
        Err(e) => {
            info!("no IPv6 http listener ({e}) - IPv4 only");
            None
        }
    };

    let Some(t) = tls else {
        match http6_srv {
            Some(h6) => tokio::select! {
                r = http_srv => r?,
                r = h6 => r?,
                r = admin_srv => r?,
            },
            None => tokio::select! {
                r = http_srv => r?,
                r = admin_srv => r?,
            },
        }
        return Ok(());
    };

    let _ = rustls::crypto::ring::default_provider().install_default();

    let rustls_cfg =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&t.certs.cert, &t.certs.key)
            .await
            .context("loading the TLS cert")?;

    // ACME (Let's Encrypt) on the public v6 listener: a dual cert resolver
    // serves the ACME certificate once issued and the local-CA leaf before
    // that, so the page stays reachable through a failed or pending issuance.
    // The v4 listener keeps the plain local-ca path (LAN traffic, nothing to
    // trust publicly).
    let acme_slot = Arc::new(std::sync::OnceLock::new());
    let local_key = crate::acme::load_local_certified_key(&t.certs.cert, &t.certs.key)
        .unwrap_or_else(|e| {
            tracing::warn!("acme disabled - local leaf unavailable for fallback: {e:#}");
            // Unreachable in practice: from_pem_file just loaded these files.
            panic!("acme fallback cert unavailable: {e:#}")
        });
    let dual = Arc::new(crate::acme::DualResolver {
        acme: acme_slot.clone(),
        local: local_key,
        logged: std::sync::atomic::AtomicBool::new(false),
    });
    // One config answers everything: `acme-tls/1` ALPN → the challenge cert
    // (served by the ACME resolver once a validation is pending), browser
    // traffic → issued ACME cert, or the local-CA leaf before issuance.
    let mut public_cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider().into(),
    ))
    .with_safe_default_protocol_versions()
    .expect("tls versions")
    .with_no_client_auth()
    .with_cert_resolver(dual);
    public_cfg.alpn_protocols = vec![b"acme-tls/1".to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];
    let public_cfg = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(public_cfg));
    crate::acme::spawn_autostart(
        state.cfg.tls.acme_hostname.clone(),
        crate::config::Config::config_dir().join("tls").join("acme"),
        acme_slot.clone(),
        state.remote_mapping.clone(),
    );

    // Renew + hot-reload on a timer (local-ca: instant, no network).
    {
        let rc = rustls_cfg.clone();
        let cfg = state.cfg.clone();
        let mode = t.mode;
        tokio::spawn(async move {
            let dir = cfg.tls.cert_dir_path();
            let mut iv = tokio::time::interval(crate::tls::RENEW_INTERVAL);
            iv.tick().await;
            loop {
                iv.tick().await;
                let fresh = match mode {
                    TlsMode::LocalCa => (|| {
                        let (ca_pem, ca_key) = crate::tls::local_ca::load_or_create_ca(&dir)?;
                        let (primary, sans) = crate::tls::local_ca::collect_sans(&cfg);
                        crate::tls::local_ca::ensure_leaf(&ca_pem, &ca_key, &primary, &sans, &dir)
                    })(),
                    TlsMode::Off => break,
                };
                match fresh {
                    Ok(c) => {
                        if let Err(e) = rc.reload_from_pem_file(&c.cert, &c.key).await {
                            tracing::warn!("cert reload failed: {e:#}");
                        }
                    }
                    Err(e) => tracing::warn!("cert renewal check failed: {e:#}"),
                }
            }
        });
    }

    info!(https_addr = %t.https_addr, domain = %t.domain, mode = ?t.mode, "https listener up");
    let app =
        lan_router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let https_srv = axum_server::bind_rustls(t.https_addr, rustls_cfg.clone()).serve(app);

    // A second listener on `[::]`, so the host is reachable over IPv6.
    //
    // Two separate sockets rather than one dual-stack socket on purpose:
    // Windows defaults `IPV6_V6ONLY` to *on*, Linux usually to off, so a single
    // `[::]` bind is reachable over IPv4 on one platform and not the other.
    // Binding both families explicitly behaves identically everywhere. If the
    // v6 bind fails (no IPv6 stack at all) that is not fatal — IPv4 still works.
    let v6_addr =
        std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, t.https_addr.port()));
    let https6 = match std::net::TcpListener::bind(v6_addr) {
        Ok(l) => {
            let _ = l.set_nonblocking(true);
            info!(https_addr = %v6_addr, "https listener up (IPv6)");
            let app6 =
                lan_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
            // The public listener: same stack, but the TLS config carries the
            // dual ACME/local resolver. Behaviour until issuance = local-ca.
            Some(axum_server::from_tcp_rustls(l, public_cfg).serve(app6))
        }
        Err(e) => {
            info!("no IPv6 https listener ({e}) - IPv4 only");
            None
        }
    };

    match (https6, http6_srv) {
        (Some(s6), Some(h6)) => tokio::select! {
            r = https_srv => r?,
            r = s6 => r?,
            r = http_srv => r?,
            r = h6 => r?,
            r = admin_srv => r?,
        },
        (Some(s6), None) => tokio::select! {
            r = https_srv => r?,
            r = s6 => r?,
            r = http_srv => r?,
            r = admin_srv => r?,
        },
        (None, Some(h6)) => tokio::select! {
            r = https_srv => r?,
            r = http_srv => r?,
            r = h6 => r?,
            r = admin_srv => r?,
        },
        (None, None) => tokio::select! {
            r = https_srv => r?,
            r = http_srv => r?,
            r = admin_srv => r?,
        },
    }
    Ok(())
}

/// Helper for handlers that need the caller IP.
pub fn peer_ip(ConnectInfo(addr): &ConnectInfo<std::net::SocketAddr>) -> std::net::IpAddr {
    addr.ip()
}

// Re-export for `app.rs`.
pub use api::{HostStatus, WtStatus};
pub use signal::wt_advertisement;
