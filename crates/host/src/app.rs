//! `HostRuntime` — boot, configuration, lifecycle, shutdown (architecture
//! report §3.1 `HostRuntime`: *"Boot, configuration, lifecycle, shutdown"* —
//! must **not** own codec logic or capture details).

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::http::{self, HttpState};
use crate::pairing::PairingManager;
use crate::platform;
use crate::session::SessionManager;
use crate::stats::StatsCollector;

pub struct HostRuntime {
    cfg: Arc<Config>,
    pairing: Arc<PairingManager>,
    sessions: Arc<SessionManager>,
    stats: Arc<StatsCollector>,
    identity: Arc<crate::identity::HostIdentity>,
    acl: Arc<crate::identity::acl::ControllerAcl>,
    invites: Arc<crate::identity::pairing_invite::InviteStore>,
    host_name: String,
}

impl HostRuntime {
    pub fn boot(cfg: Config) -> anyhow::Result<Self> {
        let cfg = Arc::new(cfg);
        let stats = Arc::new(StatsCollector::new());
        let pairing = Arc::new(PairingManager::new(cfg.pairing.clone()));
        let sessions = Arc::new(SessionManager::new(cfg.clone(), stats.clone()));
        let identity =
            Arc::new(crate::identity::HostIdentity::load_or_create().context("host identity")?);
        let acl = Arc::new(crate::identity::acl::ControllerAcl::load());
        let invites = Arc::new(crate::identity::pairing_invite::InviteStore::new());
        info!(host_id = %identity.host_id(), "host identity ready");
        let host_name = hostname();

        // Startup checks (§29 step 1: *"Host launches and can inspect required
        // elements at startup"*).
        report_startup_environment(&cfg);

        if cfg.network.manage_firewall_rule {
            if let Err(e) = apply_firewall_rules(&cfg, false) {
                warn!("firewall rule setup failed: {e:#}");
            }
        }
        // Autostart is managed by the installer + the tray's "Start at sign-in"
        // toggle — the Run key is the single source of truth, not a config flag.

        Ok(Self {
            cfg,
            pairing,
            sessions,
            stats,
            identity,
            acl,
            invites,
            host_name,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        // Resolve HTTPS: the bundled local CA issues a leaf for `<machine>.local`
        // + the LAN IPs, which gives the browser a secure context (WebCrypto,
        // Keyboard Lock, gamepad). Without it the player refuses to pair.
        let tls = http::resolve_tls(&self.cfg).await;

        // Remote access — a runtime flag the tray toggles, seeded from config.
        // The port-mapping task (PCP / NAT-PMP) runs always but idles while the
        // flag is off; its verdict is published into `/api/v1/status`.
        let remote_access = Arc::new(std::sync::atomic::AtomicBool::new(
            self.cfg.remote_access.enabled,
        ));
        let remote_mapping = crate::portmap::shared();
        // One endpoint description (review §8): the mapping task prefers the
        // plan's selected stable IPv6 as its PCP client address.
        let endpoint = crate::endpoint::EndpointPlan::detect(
            &self.cfg,
            self.host_name.clone(),
            crate::net::stable_global_ipv6(&self.cfg),
        );
        crate::portmap::spawn(
            self.cfg.tls.port,
            remote_access.clone(),
            remote_mapping.clone(),
            self.cfg.media.wt_port,
            endpoint.ipv6.map(std::net::IpAddr::V6),
        );

        // WebTransport video transport for internet mode (ADR-0011). This is
        // the video path - there is no WebRTC-video fallback to degrade to
        // (§1), so a bind failure is a hard startup error with a real message.
        let wt_transport =
            crate::media::wt::WtVideoTransport::bind(crate::media::wt::WtTransportConfig {
                port: self.cfg.media.wt_port,
                identity: wtransport::Identity::self_signed([
                    "localhost".to_string(),
                    self.host_name.clone(),
                ])
                .context("WT self-signed identity")?,
                datagram_budget: crate::media::wt::DEFAULT_DATAGRAM_BUDGET,
                max_queued_frames: crate::media::wt::DEFAULT_MAX_QUEUED_FRAMES,
            })
            .with_context(|| {
                format!(
                    "WebTransport video transport failed to bind UDP {} - the video path is \
                     unavailable (no WebRTC fallback exists)",
                    self.cfg.media.wt_port
                )
            })?;
        let wt = {
            let t = Arc::new(wt_transport);
            info!(port = t.port(), cert_sha256 = %t.cert_sha256_hex(), "WebTransport video transport bound");
            self.sessions.set_wt_transport(Some(t.clone()));

            // Client events: keyframe requests drive the encoder; WT
            // telemetry feeds the same congestion controller the
            // WebRTC path uses (stats.client_snapshot -> AIMD step).
            let sessions = self.sessions.clone();
            let stats = self.stats.clone();
            let wt_events = t.clone();
            let input_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let input_logged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn(async move {
                loop {
                    match wt_events.try_next_event() {
                        Some(crate::media::wt::WtClientEvent::KeyframeRequest) => {
                            sessions.with_player(|p| p.media.request_keyframe());
                        }
                        Some(crate::media::wt::WtClientEvent::Telemetry(tel)) => {
                            // The client's wire counters split "route lost the
                            // frame" (abandoned rises, wedged flat) from
                            // "WebKit never yielded the stream" (wedged rises)
                            // from "transport dead" (everything freezes). One
                            // line a second at DEBUG is the whole story of a
                            // cellular freeze; without it both look identical
                            // from here.
                            debug!(
                                recv = tel.frames_received,
                                abandoned = tel.frames_dropped_incomplete,
                                wedged = tel.streams_wedged,
                                dgrams = tel.datagrams_seen,
                                decoded_fps = tel.decoded_fps,
                                in_kbps = tel.inbound_bitrate_kbps as u32,
                                lat_p50 = tel.lat_p50_ms,
                                lat_p95 = tel.lat_p95_ms,
                                held = tel.decode_held,
                                behind = tel.decode_behind_events,
                                queue = tel.decode_queue_size,
                                "wt client wire"
                            );
                            stats.ingest_client(tel);
                        }
                        Some(crate::media::wt::WtClientEvent::Input(bytes)) => {
                            // Input over WT datagrams (§13): identical
                            // semantics to the WebRTC data-channel
                            // pump - arming rules, apply, no bypass.
                            let n = input_pkts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if !input_logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                info!("wt: first input packet received ({} bytes) - the client's touch/input path works", bytes.len());
                            } else if n > 0 && n % 500 == 0 {
                                info!("wt: {} input packets received so far", n + 1);
                            }
                            sessions.with_player(|p| {
                                if !p.input_armed {
                                    return;
                                }
                                if let Err(e) = p.input.handle_packet(&bytes) {
                                    tracing::debug!("bad wt input packet: {e}");
                                }
                            });
                        }
                        Some(crate::media::wt::WtClientEvent::Connected) => {
                            stats.set_wt_active(true);
                            input_logged.store(false, std::sync::atomic::Ordering::Relaxed);
                            // The video carrier is up - release the client's
                            // "connecting" screen without waiting for WebRTC
                            // ICE (audio only in this mode; cellular ICE can
                            // take arbitrarily long or never complete).
                            sessions.with_player(|p| p.media.signal_ready());
                            // A fresh WT session must start on a clean IDR:
                            // the client's decoder has no reference chain, and
                            // waiting for the natural GOP boundary showed up
                            // as a black glass on mid-session redials
                            // (2026-09-08 Safari: redials every 14-19 s on a
                            // degraded cellular route, black until refresh).
                            sessions.with_player(|p| p.media.request_keyframe());
                        }
                        Some(crate::media::wt::WtClientEvent::Disconnected) => {
                            stats.set_wt_active(false);
                        }
                        None => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                    }
                }
            });
            t
        };

        let play_url = match &tls {
            Some(t) => t.play_url.clone(),
            None => {
                let ip = crate::net::best_lan_ip(&self.cfg)
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "localhost".into());
                format!("http://{ip}:{}/", self.cfg.network.http_port)
            }
        };

        let art = crate::gameart::ArtCache::new(self.cfg.library.online_art);

        let state = HttpState {
            cfg: self.cfg.clone(),
            pairing: self.pairing.clone(),
            sessions: self.sessions.clone(),
            stats: self.stats.clone(),
            identity: self.identity.clone(),
            acl: self.acl.clone(),
            invites: self.invites.clone(),
            host_name: self.host_name.clone(),
            play_url: play_url.clone(),
            https: tls.is_some(),
            remote_access: remote_access.clone(),
            remote_mapping,
            art,
            wt: Some(wt),
            started_at: Instant::now(),
        };
        if tls.is_some() {
            info!(%play_url, "InPhase host ready — open this URL in a browser on your LAN");
        } else {
            warn!(
                %play_url,
                "InPhase host running WITHOUT https — the dashboard works on localhost but the \
                 player cannot pair. Check `[tls]` in config.toml."
            );
        }
        let (pin, ttl) = self.pairing.current_pin();
        info!(
            pin = %pin,
            ttl_secs = ttl.map(|d| d.as_secs() as i64).unwrap_or(-1),
            sessions = self.pairing.session_count(),
            "current pairing PIN (ttl -1 = no expiry)"
        );

        // ---- tray icon + control menu ------------------------------------
        let tray_model = std::sync::Arc::new(platform::TrayModel::default());
        {
            use std::sync::atomic::Ordering::Relaxed;
            tray_model
                .remote_access
                .store(remote_access.load(Relaxed), Relaxed);
            tray_model
                .start_at_login
                .store(platform::start_at_login_enabled(), Relaxed);
            tray_model.set_status("Available");
            tray_model.set_pin_line(tray_pin_line(&self.pairing));
        }
        let tray_actions = platform::TrayActions {
            open_dashboard: {
                let url = play_url.clone();
                Box::new(move || open_url(&url))
            },
            set_remote_access: {
                use std::sync::atomic::Ordering::Relaxed;
                let flag = remote_access.clone();
                let model = tray_model.clone();
                let http_port = self.cfg.network.http_port;
                let tls_port = self.cfg.tls.port;
                let wt_port = self.cfg.media.wt_port;
                let lan_cidrs = self.cfg.network.allowed_remote_cidrs.clone();
                let manage_fw = self.cfg.network.manage_firewall_rule;
                Box::new(move |on: bool| {
                    flag.store(on, Relaxed);
                    model.remote_access.store(on, Relaxed);
                    if let Err(e) = crate::config::persist_bool("remote_access", "enabled", on) {
                        warn!("persisting remote_access failed: {e:#}");
                    }
                    if manage_fw {
                        let mut cidrs = lan_cidrs.clone();
                        if on {
                            cidrs.push("2000::/3".to_string());
                        }
                        let mut ports = vec![http_port];
                        if tls_port != 0 {
                            ports.push(tls_port);
                        }
                        if wt_port != 0 {
                            ports.push(wt_port);
                        }
                        if let Err(e) = platform::ensure_firewall_rule(&ports, &cidrs, true) {
                            warn!("re-applying the firewall rule failed: {e:#}");
                        }
                    }
                    info!(on, "remote access toggled from the tray");
                })
            },
            set_start_at_login: {
                use std::sync::atomic::Ordering::Relaxed;
                let model = tray_model.clone();
                Box::new(move |on: bool| match platform::set_start_at_login(on) {
                    Ok(()) => model.start_at_login.store(on, Relaxed),
                    Err(e) => warn!("changing start-at-sign-in failed: {e:#}"),
                })
            },
            quit: {
                let sessions = self.sessions.clone();
                Box::new(move || {
                    sessions.force_disconnect();
                    std::process::exit(0);
                })
            },
        };
        let tray = platform::Tray::spawn(tray_model.clone(), tray_actions);
        if let Some(t) = &tray {
            t.refresh();
            let handle = t.handle();
            let sessions = self.sessions.clone();
            let pairing = self.pairing.clone();
            let model = tray_model.clone();
            tokio::spawn(async move {
                use std::sync::atomic::Ordering::Relaxed;
                let mut iv = tokio::time::interval(std::time::Duration::from_millis(750));
                let mut last: Option<(bool, String, String)> = None;
                loop {
                    iv.tick().await;
                    let st = sessions.state();
                    let streaming = matches!(
                        st,
                        crate::session::SessionState::Playing
                            | crate::session::SessionState::Reconnecting
                    );
                    let line = tray_status_line(streaming, st, model.remote_access.load(Relaxed));
                    let pin_line = tray_pin_line(&pairing);
                    if last.as_ref() != Some(&(streaming, line.clone(), pin_line.clone())) {
                        model.streaming.store(streaming, Relaxed);
                        model.set_status(line.clone());
                        model.set_pin_line(pin_line.clone());
                        handle.refresh();
                        last = Some((streaming, line, pin_line));
                    }
                }
            });
        }
        let _tray = tray;

        // Emergency local disconnect — Ctrl+Alt+Shift+F12 on the host cuts any
        // active session regardless of network / browser state.
        let _hotkey = platform::EmergencyHotkey::spawn({
            let sessions = self.sessions.clone();
            move || sessions.force_disconnect()
        });

        // Input watchdog tick for the active session (§12.2).
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_millis(50));
            loop {
                iv.tick().await;
                sessions.with_player(|p| p.input.tick_watchdog());
            }
        });

        // Health watchdog. Nothing on 2026-09-07 announced itself: the host had
        // the facts to say "a client is connected and no frames are reaching the
        // screen" and never said it, so every failure was found by reading logs
        // afterwards. This says it at the moment it becomes true, and says it
        // once per transition rather than every tick.
        let stats = self.stats.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut last = String::from("ok");
            loop {
                iv.tick().await;
                let h = stats.health();
                let now = h.summary();
                if now == last {
                    continue;
                }
                if h.ok {
                    info!(was = %last, "health: recovered");
                } else {
                    for s in &h.symptoms {
                        match s.severity {
                            crate::health::Severity::Critical => {
                                error!(code = s.code, "health: {}", s.detail)
                            }
                            crate::health::Severity::Warn => {
                                warn!(code = s.code, "health: {}", s.detail)
                            }
                        }
                    }
                }
                last = now;
            }
        });

        http::serve(state, tls).await.context("http server")
    }
}

/// Add the inbound Windows Firewall rules for the host's ports. The host calls
/// this on every start; the installer also calls it once (elevated, via
/// `--setup-firewall`) so a background sign-in launch does not depend on the
/// user approving a Windows prompt.
pub fn apply_firewall_rules(cfg: &Config, recreate: bool) -> anyhow::Result<()> {
    let mut ports = vec![cfg.network.http_port];
    if cfg.tls.wants_tls() {
        ports.push(cfg.tls.port);
    }
    if cfg.media.wt_port != 0 {
        // QUIC needs its own UDP listener rule (ADR-0011).
        ports.push(cfg.media.wt_port);
    }
    let remote_cidrs = cfg
        .network
        .firewall_allowed_remote_cidrs(&cfg.remote_access);
    platform::ensure_firewall_rule(&ports, &remote_cidrs, recreate)
}

fn report_startup_environment(cfg: &Config) {
    match platform::enumerate_monitors() {
        Ok(mons) => {
            for m in &mons {
                info!(
                    index = m.index, name = %m.name, w = m.width, h = m.height,
                    hz = m.refresh_hz, primary = m.primary, "monitor"
                );
            }
        }
        Err(e) => warn!("monitor enumeration failed: {e:#}"),
    }
    match platform::enumerate_gpus() {
        Ok(gpus) => {
            for g in &gpus {
                info!(
                    index = g.index, gpu = %g.description, vram_mb = g.dedicated_vram_mb,
                    encoders = ?g.encoder_elements, "gpu"
                );
            }
            if cfg.media.require_hardware_encoder
                && gpus.iter().all(|g| g.encoder_elements.is_empty())
            {
                warn!(
                    "no hardware video encoder elements found - sessions will fail with a \
                     clear error rather than falling back to CPU (sec 6)"
                );
            }
        }
        Err(e) => warn!("GPU enumeration failed: {e:#}"),
    }
}

/// The greyed status line at the top of the tray menu (and the tooltip).
fn tray_status_line(
    streaming: bool,
    state: crate::session::SessionState,
    remote_access: bool,
) -> String {
    use crate::session::SessionState::*;
    let base = if streaming {
        "Streaming".to_string()
    } else {
        match state {
            Paired | Negotiating => "Connecting…".to_string(),
            _ => "Available".to_string(),
        }
    };
    if remote_access {
        format!("{base} · remote access on")
    } else {
        base
    }
}

/// Pairing PIN for the tray right-click menu (and tooltip while idle).
fn tray_pin_line(pairing: &PairingManager) -> String {
    let (pin, ttl) = pairing.current_pin();
    match ttl {
        Some(d) => {
            let secs = d.as_secs();
            format!(
                "Pairing PIN: {pin} ({mins}:{rem:02} left)",
                mins = secs / 60,
                rem = secs % 60
            )
        }
        None => format!("Pairing PIN: {pin}"),
    }
}

/// Open a URL in the default browser (tray "Open dashboard").
fn open_url(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "InPhase-Host".into())
}
