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
    /// info. A slot: the transport is replaced when its certificate rotates
    /// (`app::spawn_wt_rotation`), and every advertisement must use the current
    /// one. Empty = no WT video path.
    pub wt: crate::media::wt::WtSlot,
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            host_allowlist,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Answer only to names that are this host's.
///
/// DNS rebinding: a web page at `evil.example` re-points its own name at the
/// host's LAN address, and a browser on the LAN then talks to InPhase while
/// believing it is still `evil.example` - same-origin, so the Origin check is
/// satisfied, and from a LAN address, so "pairing is LAN-only" is too. Every
/// such request carries the attacker's name in `Host`. The host answers to IP
/// literals, `localhost`, `.local` (mDNS, never publicly registrable),
/// Tailscale `.ts.net` names and its own advertised names - nothing an outside page can make its own name
/// resolve to.
async fn host_allowlist(
    axum::extract::State(st): axum::extract::State<HttpState>,
    req: Request,
    next: Next,
) -> Response {
    let authority = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| {
            req.headers()
                .get(axum::http::header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string)
        });
    match authority {
        // No name at all is not a browser, so not a rebinding: every browser
        // request names its target, and a rebound one names the attacker's.
        None => next.run(req).await,
        Some(a) if host_is_ours(host_from_header(&a), &st) => next.run(req).await,
        other => {
            tracing::warn!(host = ?other, "refused a request for a name this host does not serve");
            (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response()
        }
    }
}

fn host_is_ours(host: &str, st: &HttpState) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = bare.trim_end_matches('.').to_ascii_lowercase();
    // `.ts.net` is Tailscale MagicDNS: resolvable only inside a tailnet, so no
    // outside page can point one at this host either.
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".ts.net") {
        return true;
    }
    let play = axum::http::Uri::try_from(st.play_url.as_str())
        .ok()
        .and_then(|u| u.host().map(|h| h.to_ascii_lowercase()));
    if play.as_deref() == Some(host.as_str())
        || host.eq_ignore_ascii_case(&st.host_name)
        || st
            .cfg
            .tls
            .domain
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(&host))
    {
        return true;
    }
    // The sslip.io name of one of this machine's own addresses: it moves with
    // the ISP prefix, so it is derived, not configured.
    host.ends_with(".sslip.io") && own_sslip_names().contains(&host)
}

/// sslip.io names of this machine's global IPv6 addresses, refreshed at most
/// every 30 s (the check runs on every request).
fn own_sslip_names() -> Vec<String> {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<(Option<Instant>, Vec<String>)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((None, Vec::new())));
    let mut g = cache.lock();
    if g.0
        .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(30))
    {
        let names = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| match i.ip() {
                std::net::IpAddr::V6(v6) if crate::net::is_global_unicast_v6(v6) => {
                    crate::acme::sslip_hostname(&v6.to_string())
                }
                _ => None,
            })
            .map(|n| n.to_ascii_lowercase())
            .collect();
        *g = (Some(Instant::now()), names);
    }
    g.1.clone()
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

/// Framing/referrer hardening for the play origin.
///
/// Do **not** send `Content-Security-Policy` here. Hash-pinned WebTransport
/// (`new WebTransport(url, { serverCertificateHashes })` on `:4433`) is a
/// distinct CSP identity. This Chrome:
///   1. treats `'unsafe-webtransport-hashes'` as an invalid source and ignores it,
///   2. then blocks the dial as `connect-src 'self'` whenever *any* CSP is set,
///      including a policy that omits `connect-src` / `default-src`.
///
/// The worker console names `connect-src 'self'` even when that directive was
/// never sent. `X-Frame-Options: DENY` covers clickjacking without a CSP.
async fn security_headers(
    axum::extract::State(_st): axum::extract::State<HttpState>,
    req: Request,
    next: Next,
) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    if is_api && !same_origin_request(req.uri(), req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
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
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
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
    if addr.ip().is_loopback() && local_host && same_origin_request(req.uri(), req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "admin API is loopback-only").into_response()
    }
}

/// Host header without a trailing HTTP port. Unbracketed IPv6 is left intact
/// (multiple colons); a last-hextet of all digits must not be treated as a port.
#[allow(dead_code)]
fn host_from_header(host: &str) -> &str {
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            return &host[..=end];
        }
        return host;
    }
    if host.bytes().filter(|&b| b == b':').count() > 1 {
        return host;
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
}

#[allow(dead_code)]
fn csp_safe_host(host: &str) -> Option<&str> {
    if host.is_empty() || host.contains('@') {
        return None;
    }
    host.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']'))
        .then_some(host)
}

/// Intended `connect-src` once Chrome parses `'unsafe-webtransport-hashes'`.
/// Not sent on the play origin today — unknown keyword is ignored, then
/// hash-pinned WT is blocked as `connect-src 'self'`.
#[allow(dead_code)]
fn connect_src_directive(host: Option<&str>, wt_port: Option<u16>) -> String {
    let mut connect_src = "'self' ws: wss:".to_string();
    if let Some(host) = host.and_then(csp_safe_host) {
        if let Some(port) = wt_port {
            let hostname = host_from_header(host);
            if hostname.contains(':') && !hostname.starts_with('[') {
                connect_src.push_str(&format!(" https://[{hostname}]:{port}"));
            } else {
                connect_src.push_str(&format!(" https://{hostname}:{port}"));
            }
        }
    }
    if let Some(port) = wt_port {
        connect_src.push_str(&format!(" https://*:{port} 'unsafe-webtransport-hashes'"));
    }
    connect_src
}

/// Check browser Origin and Fetch Metadata. Native clients may omit Origin;
/// they still pass the network, pairing and controller-key guards.
pub(super) fn same_origin_request(uri: &axum::http::Uri, headers: &axum::http::HeaderMap) -> bool {
    use axum::http::{header, uri::Authority, Uri};
    let fetch_site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if fetch_site == Some("cross-site") {
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
    if source.as_str().contains('@') {
        return false;
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let matches_target = |target: &Authority| {
        !target.as_str().contains('@')
            && source.host().eq_ignore_ascii_case(target.host())
            && source.port_u16().unwrap_or(default_port)
                == target.port_u16().unwrap_or(default_port)
    };
    // The target authority comes from the request URI first: HTTP/2 carries it
    // in `:authority` and does NOT send a `host` header, so reading only the
    // header rejected every browser POST as cross-origin. Live 2026-09-19:
    // pairing answered 403 "cross-origin request rejected" in Chromium, in
    // Safari and in Playwright, silently — no host log line, no PIN comparison —
    // while the identical request over HTTP/1.1, and every native client,
    // paired normally. Browsers speak HTTP/2 to this host, so pairing from a
    // browser was impossible.
    let same_authority = uri.authority().is_some_and(matches_target)
        || headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<Authority>().ok())
            .is_some_and(|t| matches_target(&t))
        // Last resort: the browser's own Fetch Metadata verdict. It is a
        // forbidden header - a page cannot set or forge it - and `same-origin`
        // is the browser saying this request targets the page's own origin.
        // Cross-site and same-site are NOT accepted; they fall through to the
        // authority comparison above.
        || fetch_site == Some("same-origin");
    same_authority && origin.path() == "/" && origin.query().is_none()
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
                        .advertised_origin(&cfg.tls.acme_hostname)
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
        rustls::crypto::ring::default_provider(),
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

#[cfg(test)]
mod connect_src_tests {
    use super::{connect_src_directive, host_from_header, same_origin_request};

    #[test]
    fn wildcard_when_wt_bound_even_without_host() {
        assert_eq!(
            connect_src_directive(None, Some(4433)),
            "'self' ws: wss: https://*:4433 'unsafe-webtransport-hashes'"
        );
    }

    #[test]
    fn no_wildcard_when_wt_unbound() {
        assert_eq!(
            connect_src_directive(Some("pc.local"), None),
            "'self' ws: wss:"
        );
    }

    #[test]
    fn sslip_host_and_wildcard() {
        let host = "2001-db8-1234-5678-0-0-0-100.sslip.io";
        let src = connect_src_directive(Some(host), Some(4433));
        assert!(src.contains(&format!("https://{host}:4433")), "{src}");
        assert!(src.contains("https://*:4433"), "{src}");
        assert!(src.contains("'unsafe-webtransport-hashes'"), "{src}");
    }

    #[test]
    fn hash_pinned_wt_needs_unsafe_keyword() {
        let src = connect_src_directive(Some("2001-db8-1234-5678-0-0-0-100.sslip.io"), Some(4433));
        assert!(
            src.contains("'unsafe-webtransport-hashes'"),
            "Chrome blocks serverCertificateHashes without this keyword: {src}"
        );
        assert!(
            !connect_src_directive(Some("pc.local"), None).contains("'unsafe-webtransport-hashes'")
        );
    }

    #[test]
    fn ipv6_literal_is_bracketed() {
        let src = connect_src_directive(Some("2001:db8:1234:5678:0:0:0:100"), Some(4433));
        assert!(
            src.contains("https://[2001:db8:1234:5678:0:0:0:100]:4433"),
            "{src}"
        );
        assert!(src.contains("https://*:4433"), "{src}");
    }

    #[test]
    fn http_port_stripped_for_wt_origin() {
        let src = connect_src_directive(Some("pc.local:8080"), Some(4433));
        assert!(src.contains("https://pc.local:4433"), "{src}");
        assert!(src.contains("ws:"), "{src}");
        assert!(src.contains("https://*:4433"), "{src}");
        assert!(src.contains("'unsafe-webtransport-hashes'"), "{src}");
    }

    /// The pairing outage of 2026-09-19. HTTP/2 puts the authority in
    /// `:authority` and sends no `host` header, so a check that read only the
    /// header refused every browser POST as cross-origin — silently, before the
    /// PIN was compared — and pairing from a browser was impossible.
    #[test]
    fn http2_authority_counts_as_the_request_target() {
        use axum::http::{HeaderMap, HeaderValue, Uri};

        let uri: Uri = "https://pc.example/".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("origin", HeaderValue::from_static("https://pc.example"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(
            same_origin_request(&uri, &headers),
            "an HTTP/2 request with no host header is same-origin"
        );

        // ...and the same request over HTTP/1.1, where the authority is the
        // header, still passes.
        let uri11: Uri = "/api/v1/pair".parse().unwrap();
        let mut h11 = headers.clone();
        h11.insert("host", HeaderValue::from_static("pc.example"));
        assert!(same_origin_request(&uri11, &h11));

        // A different origin on the same URI authority is still refused, with
        // or without the fetch-metadata header.
        let mut evil = headers.clone();
        evil.insert("origin", HeaderValue::from_static("https://evil.example"));
        evil.remove("sec-fetch-site");
        assert!(!same_origin_request(&uri, &evil));

        // Cross-site is refused even when it claims the right authority.
        let mut cross = headers.clone();
        cross.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin_request(&uri, &cross));

        // Same-site is not same-origin: another port or host under the same
        // site must fall through to the authority comparison, which fails.
        let mut same_site = headers.clone();
        same_site.insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        same_site.insert(
            "origin",
            HeaderValue::from_static("https://pc.example:8443"),
        );
        assert!(!same_origin_request(&uri, &same_site));

        // A native client omits Origin entirely and is not blocked here.
        let mut native = HeaderMap::new();
        native.insert("host", HeaderValue::from_static("pc.example"));
        assert!(same_origin_request(&uri11, &native));
    }

    #[test]
    fn rejects_host_that_would_break_csp() {
        let src = connect_src_directive(Some("evil; script-src *"), Some(4433));
        assert_eq!(
            src,
            "'self' ws: wss: https://*:4433 'unsafe-webtransport-hashes'"
        );
        let src = connect_src_directive(Some("user@pc.local"), None);
        assert_eq!(src, "'self' ws: wss:");
    }

    #[test]
    fn strips_http_port_not_ipv6_hextet() {
        assert_eq!(host_from_header("pc.local:8080"), "pc.local");
        assert_eq!(host_from_header("192.168.1.5:80"), "192.168.1.5");
        assert_eq!(
            host_from_header("2001:db8:1234:5678:0:0:0:100"),
            "2001:db8:1234:5678:0:0:0:100"
        );
        assert_eq!(
            host_from_header("[2001:db8:1234:5678:0:0:0:100]:8080"),
            "[2001:db8:1234:5678:0:0:0:100]"
        );
    }
}
