//! `inphase-host --doctor` — installer / support preflight. Runs a fixed set of
//! environment checks and reports them as a table or as JSON. Used by:
//!
//! * the installer, immediately after copying files ("did it actually work?");
//! * the dashboard's Advanced diagnostics panel;
//! * `--support-bundle`, which embeds a `--doctor --json` result.
//!
//! Exit code: 0 if every check is `ok` or `warn`, 2 if any check `fail`s.

use std::net::TcpListener;

use serde::Serialize;

use crate::config::Config;

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
        }
    }
}

#[derive(Serialize)]
pub struct Report {
    pub host_version: &'static str,
    pub checks: Vec<Check>,
    pub blocking: bool,
}

pub fn diagnose(cfg: &Config) -> Report {
    let mut checks = Vec::new();
    checks.push(check_os());
    checks.extend(check_gstreamer());
    checks.push(check_encoder());
    checks.push(check_monitors());
    checks.push(check_audio());
    checks.push(check_firewall());
    checks.push(check_https(cfg));
    checks.push(check_leaf_sans(cfg));
    checks.extend(check_ports(cfg));
    checks.push(check_wt_port(cfg));

    let blocking = checks.iter().any(|c| c.status == Status::Fail);
    Report {
        host_version: crate::HOST_VERSION,
        checks,
        blocking,
    }
}

/// Run the checks and print them. Returns the process exit code.
pub fn run(cfg: &Config, json: bool) -> i32 {
    let report = diagnose(cfg);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        println!("InPhase Host {} — environment check\n", report.host_version);
        for c in &report.checks {
            let mark = match c.status {
                Status::Ok => "[ ok ]",
                Status::Warn => "[warn]",
                Status::Fail => "[FAIL]",
            };
            println!("  {mark}  {:<22}  {}", c.name, c.detail);
        }
        println!(
            "\n{}",
            if report.blocking {
                "FAILED — InPhase cannot stream in this environment (see [FAIL] above)."
            } else if report.checks.iter().any(|c| c.status == Status::Warn) {
                "OK with warnings — InPhase can stream; some features may be limited."
            } else {
                "OK — ready to stream."
            }
        );
    }
    if report.blocking {
        2
    } else {
        0
    }
}

// --------------------------------------------------------------------------

fn check_os() -> Check {
    #[cfg(windows)]
    {
        // Read the build number from the registry (same approach as
        // platform::windows::startup — one reg.exe shell-out, no unsafe).
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
                "/v",
                "CurrentBuildNumber",
            ])
            .output();
        let build: u32 = out
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                s.split_whitespace()
                    .last()
                    .and_then(|t| t.trim().parse().ok())
            })
            .unwrap_or(0);
        match build {
            0 => Check::warn("windows", "could not read the Windows build number"),
            b if b >= 22000 => Check::ok("windows", format!("Windows 11 (build {b})")),
            b if b >= 19041 => Check::ok("windows", format!("Windows 10 (build {b})")),
            b => Check::fail(
                "windows",
                format!("build {b} is older than the supported minimum (Windows 10 20H1 / 19041)"),
            ),
        }
    }
    #[cfg(not(windows))]
    Check::warn(
        "windows",
        "not a Windows host — InPhase Host only runs on Windows",
    )
}

#[cfg(windows)]
fn check_gstreamer() -> Vec<Check> {
    use gstreamer as gst;
    let mut v = Vec::new();
    if let Err(e) = gst::init() {
        v.push(Check::fail(
            "gstreamer",
            format!("gst_init failed: {e} — the bundled GStreamer runtime is missing or broken"),
        ));
        return v;
    }
    v.push(Check::ok(
        "gstreamer",
        format!("runtime {}", gst::version_string()),
    ));

    // (element, hard-required?)
    let required: &[(&str, bool)] = &[
        ("d3d11screencapturesrc", true),
        ("d3d11convert", true),
        ("videorate", true),
        ("h264parse", true),
        ("rtph264pay", true),
        ("webrtcbin", true),
        ("dtlssrtpenc", true),
        ("nicesink", true),
        ("srtpenc", true),
        ("wasapi2src", false),
        ("opusenc", false),
        ("rtpopuspay", false),
    ];
    let missing_hard: Vec<&str> = required
        .iter()
        .filter(|(n, hard)| *hard && gst::ElementFactory::find(n).is_none())
        .map(|(n, _)| *n)
        .collect();
    let missing_soft: Vec<&str> = required
        .iter()
        .filter(|(n, hard)| !*hard && gst::ElementFactory::find(n).is_none())
        .map(|(n, _)| *n)
        .collect();
    if !missing_hard.is_empty() {
        v.push(Check::fail(
            "gst elements",
            format!("missing required elements: {}", missing_hard.join(", ")),
        ));
    } else if !missing_soft.is_empty() {
        v.push(Check::warn(
            "gst elements",
            format!(
                "audio elements missing ({}) — video will work, audio will not",
                missing_soft.join(", ")
            ),
        ));
    } else {
        v.push(Check::ok(
            "gst elements",
            "all capture / encode / WebRTC / audio elements present",
        ));
    }
    v
}

#[cfg(not(windows))]
fn check_gstreamer() -> Vec<Check> {
    vec![Check::warn(
        "gstreamer",
        "GStreamer checks only run on Windows",
    )]
}

fn check_encoder() -> Check {
    use crate::media::encoder_policy::element_for;
    use inphase_protocol::VideoCodec;

    let vendor = crate::platform::primary_gpu_vendor();
    let gpus = crate::platform::enumerate_gpus().unwrap_or_default();
    let names: Vec<String> = gpus.iter().map(|g| g.description.clone()).collect();
    match vendor {
        Some(v) => {
            let elem = element_for(v, VideoCodec::H264);
            #[cfg(windows)]
            let present = elem
                .map(|e| gstreamer::ElementFactory::find(e).is_some())
                .unwrap_or(false);
            #[cfg(not(windows))]
            let present = elem.is_some();
            match (elem, present) {
                (Some(e), true) => Check::ok(
                    "hw encoder",
                    format!(
                        "{:?} — {e} ({})",
                        v,
                        names.first().map(String::as_str).unwrap_or("?")
                    ),
                ),
                (Some(e), false) => Check::fail(
                    "hw encoder",
                    format!(
                        "{:?} GPU found but the `{e}` element is not registered — GStreamer plugin \
                         `gstnvcodec`/`gstamfcodec`/`gstqsv` missing, or driver too old",
                        v
                    ),
                ),
                (None, _) => Check::fail(
                    "hw encoder",
                    format!("{:?} GPU has no H.264 hardware encoder path in InPhase", v),
                ),
            }
        }
        None if gpus.is_empty() => Check::fail(
            "hw encoder",
            "no GPU detected — InPhase needs a hardware H.264 encoder (no CPU fallback)",
        ),
        None => Check::fail(
            "hw encoder",
            format!(
                "GPU(s) [{}] — none is a supported NVIDIA/AMD/Intel encoder",
                names.join(", ")
            ),
        ),
    }
}

fn check_monitors() -> Check {
    match crate::platform::enumerate_monitors() {
        Ok(m) if m.is_empty() => Check::warn(
            "displays",
            "no display attached — capture needs an active desktop (RDP/headless sessions cannot capture)",
        ),
        Ok(m) => {
            let p = m.iter().find(|x| x.primary).or_else(|| m.first());
            Check::ok(
                "displays",
                match p {
                    Some(d) => format!("{} displays; primary {}x{}@{}Hz", m.len(), d.width, d.height, d.refresh_hz),
                    None => format!("{} displays", m.len()),
                },
            )
        }
        Err(e) => Check::warn("displays", format!("enumeration failed: {e:#}")),
    }
}

fn check_audio() -> Check {
    match crate::platform::enumerate_audio_endpoints() {
        Ok(e) if e.is_empty() => Check::warn(
            "audio",
            "no playback endpoints — game audio capture will be silent",
        ),
        Ok(e) => {
            let def = e
                .iter()
                .find(|x| x.is_default)
                .map(|x| x.name.as_str())
                .unwrap_or("?");
            Check::ok(
                "audio",
                format!("{} playback endpoints; default \"{def}\"", e.len()),
            )
        }
        Err(e) => Check::warn("audio", format!("enumeration failed: {e:#}")),
    }
}

fn check_firewall() -> Check {
    #[cfg(windows)]
    {
        let rule_exists = |name: &str| {
            std::process::Command::new("netsh")
                .args([
                    "advfirewall",
                    "firewall",
                    "show",
                    "rule",
                    &format!("name={name}"),
                ])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("Rule Name"))
                .unwrap_or(false)
        };
        if rule_exists("InPhase LAN") || rule_exists("InPhase App") {
            Check::ok("firewall", "InPhase inbound rule is present")
        } else {
            Check::warn(
                "firewall",
                "no InPhase firewall rule yet — the Host adds one on first launch; \
                 approve any Windows prompt, or the installer adds it",
            )
        }
    }
    #[cfg(not(windows))]
    Check::warn("firewall", "Windows Firewall check only runs on Windows")
}

/// HTTPS readiness: the browser needs a secure context (WebCrypto, Keyboard
/// Lock, gamepad), i.e. a cert it trusts.
fn check_https(cfg: &Config) -> Check {
    use crate::config::TlsMode;
    match cfg.tls.mode {
        TlsMode::Off => Check::warn(
            "https",
            "tls.mode = off — the dashboard works on localhost but the player cannot pair",
        ),
        TlsMode::LocalCa => {
            let ca = cfg.tls.cert_dir_path().join("ca.crt");
            if ca.is_file() {
                Check::ok(
                    "https",
                    format!(
                        "local CA present ({}); other devices install it from /ca.crt",
                        ca.display()
                    ),
                )
            } else {
                Check::warn(
                    "https",
                    "local CA not created yet — the host generates it on first run; \
                     run `inphase-host --trust-ca` elevated to trust it machine-wide",
                )
            }
        }
    }
}

/// §10: the leaf must cover the identity the host advertises NOW. A machine
/// rename, domain change, or stale regeneration leaves a cert whose SANs no
/// longer match the URL - clients fail with confusing TLS errors.
fn check_leaf_sans(cfg: &Config) -> Check {
    let dir = cfg.tls.cert_dir_path();
    let sans_file = dir.join("host.sans");
    let want = crate::tls::local_ca::collect_sans(cfg);
    let want_set: Vec<String> = {
        // `want.0` (the primary) is already in `want.1` - dedup, don't
        // duplicate, or every leaf reads as a mismatch.
        let mut v = want.1.clone();
        if !want.0.is_empty() && !v.contains(&want.0) {
            v.push(want.0);
        }
        v.sort();
        v
    };
    match std::fs::read_to_string(&sans_file) {
        Ok(on_disk) => {
            let mut got: Vec<String> = on_disk
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            got.sort();
            if got == want_set {
                Check::ok(
                    "cert san",
                    format!(
                        "leaf covers the advertised identity ({})",
                        want_set.join(", ")
                    ),
                )
            } else {
                Check::warn(
                    "cert san",
                    format!(
                        "leaf SANs {:?} do not match the advertised identity {:?} - \
                         the leaf regenerates on next host start; a device that still \
                         fails should re-pair",
                        got, want_set
                    ),
                )
            }
        }
        Err(_) => Check::warn(
            "cert san",
            "no leaf SAN record yet - the host creates it with the first leaf cert",
        ),
    }
}

/// WebTransport video transport (ADR-0011): the QUIC listener needs its UDP
/// port (the port is the only switch left - §15 removed the wt_enabled flag). A bind test here catches "port already taken by
/// another process / leftover instance" before a player ever dials.
fn check_wt_port(cfg: &Config) -> Check {
    if cfg.media.wt_port == 0 {
        return Check::ok(
            "wt video",
            "port 0 — WebTransport video disabled by port config",
        );
    }
    match std::net::UdpSocket::bind(("0.0.0.0", cfg.media.wt_port)) {
        Ok(s) => {
            drop(s);
            Check::ok(
                "wt video",
                format!("UDP {} is free — WebTransport video will be offered", cfg.media.wt_port),
            )
        }
        Err(e) => Check::warn(
            "wt video",
            format!(
                "UDP {} cannot be bound ({e}) — WebTransport video is enabled in config but will not come up, and the player has no other video path",
                cfg.media.wt_port
            ),
        ),
    }
}

fn check_ports(cfg: &Config) -> Vec<Check> {
    let mut v = Vec::new();
    for (label, addr) in [
        ("http port", cfg.http_socket_addr()),
        ("admin port", cfg.admin_socket_addr()),
    ] {
        match TcpListener::bind(addr) {
            Ok(l) => {
                drop(l);
                v.push(Check::ok(label, format!("{addr} is free")));
            }
            Err(_) => v.push(Check::warn(
                label,
                format!("{addr} is already in use — another InPhase Host may be running, or change the port in config.toml"),
            )),
        }
    }
    v
}

// --------------------------------------------------------------------------

/// Write a redacted support bundle (JSON) to `path` — an exportable support
/// bundle with secrets removed.
pub fn write_support_bundle(cfg: &Config, path: &std::path::Path) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct NetSummary<'a> {
        http_port: u16,
        admin_port: u16,
        allowed_remote_cidrs: &'a [String],
    }
    #[derive(Serialize)]
    struct CfgSummary<'a> {
        media: &'a crate::config::MediaConfig,
        capture: &'a crate::config::CaptureConfig,
        network: NetSummary<'a>,
    }
    #[derive(Serialize)]
    struct Platform {
        monitors: Vec<crate::platform::MonitorInfo>,
        gpus: Vec<crate::platform::GpuInfo>,
        audio_endpoints: Vec<crate::platform::AudioEndpointInfo>,
    }
    #[derive(Serialize)]
    struct Bundle<'a> {
        generated: String,
        host_version: &'static str,
        doctor: Report,
        config: CfgSummary<'a>,
        platform: Platform,
        log_tail: String,
    }

    let log_tail = std::fs::read_to_string(log_path())
        .ok()
        .map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(2000);
            lines[start..].join("\n")
        })
        .unwrap_or_default();

    // config.toml holds no secrets (PINs and cookies are memory-only); surface
    // only the non-sensitive fields anyway.
    let bundle = Bundle {
        generated: now_iso(),
        host_version: crate::HOST_VERSION,
        doctor: diagnose(cfg),
        config: CfgSummary {
            media: &cfg.media,
            capture: &cfg.capture,
            network: NetSummary {
                http_port: cfg.network.http_port,
                admin_port: cfg.network.admin_port,
                allowed_remote_cidrs: &cfg.network.allowed_remote_cidrs,
            },
        },
        platform: Platform {
            monitors: crate::platform::enumerate_monitors().unwrap_or_default(),
            gpus: crate::platform::enumerate_gpus().unwrap_or_default(),
            audio_endpoints: crate::platform::enumerate_audio_endpoints().unwrap_or_default(),
        },
        log_tail,
    };
    std::fs::write(path, serde_json::to_string_pretty(&bundle)?)?;
    Ok(())
}

fn log_path() -> std::path::PathBuf {
    // Where the installed launcher writes host.log (see installer/inphase.iss).
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local)
            .join("InPhase")
            .join("host.log");
    }
    std::path::PathBuf::from("host.log")
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn packaged_runtime_ships_videorate() {
        let ps1 = include_str!("../../../scripts/package.ps1");
        assert!(
            ps1.contains("\"gstvideorate\""),
            "scripts/package.ps1 must ship gstvideorate (gst-plugins-base)"
        );
    }
}
