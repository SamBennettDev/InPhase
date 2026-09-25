//! Typed host configuration (architecture report §26: *"Keep config/logs in
//! per-user app-data; never log PINs, cookies, SDP secrets or raw input
//! events."*).
//!
//! Precedence: built-in defaults < `%APPDATA%\InPhase\config.toml` <
//! environment overrides (`INPHASE_*`). Nothing secret is stored here — the PIN
//! and session cookie live only in memory ([`crate::pairing`]).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use inphase_protocol::{QualityPreset, VideoCodec};

/// Which desktop-capture API to use (§5.1, ADR-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureApi {
    /// DXGI Desktop Duplication of the monitor. The default. Lowest latency and
    /// full frame rate for the desktop and for **borderless-windowed** games
    /// (which is how a game should run for streaming). A game in *exclusive*
    /// fullscreen throttles every screen-capture API — set it to borderless.
    #[default]
    Dxgi,
    /// Windows.Graphics.Capture of the monitor. Alternative capture path; try
    /// it if DXGI gives a black or frozen picture on a particular game.
    Wgc,
}

/// How the host obtains the TLS cert it serves HTTPS with. A cert the browser
/// trusts is what gives it a **secure context** (WebCrypto, Keyboard Lock,
/// gamepad, audio-output picker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// Bundled local CA: the host generates its own CA (once) and a leaf cert
    /// for `<machine>.local` + the LAN IPs, and adds the CA to the machine's
    /// trust store (the installer does this with admin rights; first run
    /// otherwise). Other devices install the CA once from `/ca.crt`.
    #[default]
    #[serde(alias = "auto")]
    LocalCa,
    /// Plaintext HTTP only. Dev / loopback use — secure-context APIs are off.
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Port for the LAN-facing web/signaling server.
    pub http_port: u16,
    /// Bind address for the LAN server. `0.0.0.0` = all LAN interfaces; the
    /// server still filters/reports candidates per §8.1.
    pub bind: IpAddr,
    /// Loopback-only admin API port (§16 `/api/v1/admin/*`).
    pub admin_port: u16,
    /// ICE: gather host candidates only, no STUN/TURN in strict LAN mode (§8.1).
    pub strict_lan: bool,
    /// Optional explicit STUN URIs. Ignored while `strict_lan` is true.
    pub stun_servers: Vec<String>,
    /// Advanced override: force a specific local interface for ICE/media (§8.1).
    pub preferred_interface: Option<String>,
    /// Add a Private-profile Windows Firewall rule on startup (§8.1, Phase 4)
    /// plus a program-scoped allow for the exe so Windows never shows the
    /// "allow network access?" prompt.
    pub manage_firewall_rule: bool,
    /// Remote address scopes the firewall rule allows in. `netsh remoteip`
    /// syntax — `localsubnet` for the LAN only (§8.1 default). `[remote_access]`
    /// widens this to global IPv6 when enabled.
    pub allowed_remote_cidrs: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            http_port: 47_800,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            admin_port: 47_801,
            strict_lan: true,
            stun_servers: Vec::new(),
            preferred_interface: None,
            manage_firewall_rule: true,
            allowed_remote_cidrs: vec![
                "localsubnet".to_string(),
                "100.64.0.0/10".to_string(), // Tailscale / CGNAT
            ],
        }
    }
}

impl NetworkConfig {
    /// Remote scopes for the Windows Firewall rules.
    ///
    /// The default is LAN + tailnet only. With direct remote access enabled the
    /// host also has to accept the internet — but scoped to **global-unicast
    /// IPv6** (`2000::/3`), not `any`: the design is "IPv6 remote access", the
    /// address that gets a Let's Encrypt certificate is v6, and opening IPv4
    /// as well would expose the host on a second path the operator never asked
    /// for. `2000::/3` is every routable v6 address and nothing else.
    pub fn firewall_allowed_remote_cidrs(&self, remote_access: &RemoteAccessConfig) -> Vec<String> {
        let mut cidrs = self.allowed_remote_cidrs.clone();
        if remote_access.enabled {
            for extra in ["2000::/3"] {
                if !cidrs.iter().any(|c| c.eq_ignore_ascii_case(extra)) {
                    cidrs.push(extra.to_string());
                }
            }
        }
        cidrs
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub api: CaptureApi,
    /// Monitor to capture. `None` = primary. Set data-driven once enumeration
    /// works (§29 step 2/3).
    pub monitor_index: Option<u32>,
    /// Composite the OS cursor into the stream. On by default — you cannot aim a
    /// mouse you cannot see. The report keeps it off "for gaming" (§5.1); the
    /// Low-Latency / fullscreen-game preset can turn it off.
    pub show_cursor: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            api: CaptureApi::Dxgi,
            monitor_index: None,
            show_cursor: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    /// Default stream mode when the client does not request one (§29: start at
    /// 1080p60 H.264, then make data-driven).
    pub default_width: u32,
    pub default_height: u32,
    pub default_fps: u32,
    pub default_preset: QualityPreset,
    /// Never fall back to a CPU encoder. If no hardware encoder exists, fail
    /// with a clear error (§6, §22).
    pub require_hardware_encoder: bool,
    /// Opus loopback audio branch (Phase 2, §11). Off until Phase 2 wiring.
    pub enable_audio: bool,
    pub audio_bitrate_bps: u32,
    /// Opus frame size in milliseconds (§11: start at 10 ms).
    pub audio_frame_ms: u32,
    /// WASAPI render-endpoint id to loopback-capture (from the host log's
    /// "audio loopback endpoint" lines). Empty / unset = Windows default
    /// playback device.
    #[serde(default)]
    pub audio_capture_device: Option<String>,
    /// WebTransport video transport for internet mode (ADR-0011). Off by
    /// default: the WebRTC path remains the default everywhere; this adds the
    /// second transport behind an explicit host opt-in (P1).
    /// UDP port for the WT endpoint (QUIC needs its own UDP
    /// listener; the TCP listeners are untouched). Firewalls need a rule for it.
    pub wt_port: u16,
    /// QUIC congestion law on the WT connection: `realtime` (default) leaves
    /// rate to the application's bitrate controller; `cubic` is quinn's
    /// loss-based default, kept for A/B comparison. See
    /// `media::wt::congestion`.
    pub wt_congestion: crate::media::wt::WtCongestion,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            default_width: 1920,
            default_height: 1080,
            default_fps: 60,
            default_preset: QualityPreset::Balanced,
            // HEVC by default when the browser can receive it; H.264 is the
            // fallback for clients that only advertise AVC (e.g. some mobile Safari).
            require_hardware_encoder: true,
            enable_audio: false,
            audio_bitrate_bps: 160_000,
            audio_frame_ms: 10,
            audio_capture_device: None,
            wt_port: 4433,
            wt_congestion: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    /// Release all held keys/buttons if no input packet arrives within this many
    /// milliseconds (§12.2: *"start with ~250 ms and tune"*).
    pub watchdog_ms: u64,
    /// Game controllers through a virtual Xbox 360 pad (ViGEmBus, which setup
    /// offers to install). On by default: with no driver the host logs it and
    /// streams without controllers, and the pad is only plugged in once a
    /// controller is actually used.
    pub enable_virtual_hid: bool,
    /// Enable the optional elevated input broker (§13). Off until a reproducible
    /// elevated-game limitation is demonstrated (§19 Phase 3).
    /// Sustained input packets/second accepted per session (token-bucket rate).
    /// Every data-channel input message is rate-limited and validated. ~4× a
    /// 240 Hz mouse leaves plenty of headroom.
    pub max_packets_per_sec: u32,
    /// Token-bucket burst allowance.
    pub packet_burst: u32,
    /// Reject a single mouse-move packet whose |dx| or |dy| exceeds this (raw
    /// device units). A real mouse never jumps this far in one 4–8 ms tick;
    /// a larger value is a malformed / hostile client.
    pub max_mouse_delta: i32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            watchdog_ms: 250,
            enable_virtual_hid: true,
            max_packets_per_sec: 2_000,
            packet_burst: 400,
            max_mouse_delta: 4_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PairingConfig {
    /// PIN lifetime before it auto-rotates (§14.1: 5 minutes). `0` = never
    /// auto-rotate (single-user trusted setup; rotate only via the dashboard).
    pub pin_ttl_secs: u64,
    /// Mint a fresh PIN after each successful pair (§14.1). Turn off so an
    /// already-paired browser is the credential and the PIN is stable.
    pub rotate_pin_on_pair: bool,
    /// Failed-pair attempts allowed inside `rate_window_secs` (§14.1: 5 / 10 min).
    pub max_attempts: u32,
    pub rate_window_secs: u64,
    /// Wrong PINs in a row, from anywhere, after which PIN pairing stops until
    /// the owner picks a new PIN on the PC's dashboard. The rate limit only
    /// slows guessing; this bounds it: an attacker gets this many tries at a
    /// given PIN in total, a 10 in 1,000,000 chance at the default. `0`
    /// disables the lockout.
    pub lockout_after: u32,
    /// Authenticated-session lifetime. `0` = never expires ("paired forever on
    /// this device"). The cookie's `Max-Age` follows this.
    pub session_ttl_secs: u64,
    /// Persist the session store to `<config_dir>/sessions.json` so a host
    /// restart does not un-pair every browser. The file holds 256-bit random
    /// tokens in the per-user app-data dir — same trust boundary as the rest of
    /// trusted-LAN mode (§14.1).
    pub persist_sessions: bool,
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            pin_ttl_secs: 300,
            rotate_pin_on_pair: true,
            max_attempts: 5,
            rate_window_secs: 600,
            lockout_after: 10,
            session_ttl_secs: 0,
            persist_sessions: true,
        }
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub tls: TlsConfig,
    pub remote_access: RemoteAccessConfig,
    pub network: NetworkConfig,
    pub capture: CaptureConfig,
    pub media: MediaConfig,
    pub input: InputConfig,
    pub pairing: PairingConfig,
    pub library: LibraryConfig,
}

/// Play-page game library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    /// Fill in missing cover art from Steam's public store API + CDN (no API
    /// key; the same endpoints the Steam website uses). Games the local
    /// launchers already had art for are untouched. Set `false` for a fully
    /// offline host.
    pub online_art: bool,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self { online_art: true }
    }
}

/// Direct remote access over the internet (`docs/ipv6-remote-access.md`).
///
/// **Off by default, deliberately.** Enabling it makes the host's HTTPS and
/// WebRTC surface reachable from the internet, which is a decision the operator
/// has to take knowingly — not something a default flips on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteAccessConfig {
    pub enabled: bool,
    /// Allow *enrolling a new device* from off-network, using a one-time
    /// invite. **Off by default**, which is what makes the whole design simple:
    ///
    /// A device is enrolled once, on the LAN — where the only credential it
    /// needs is the PIN, and an attacker would have to already be inside the
    /// network to use it. Enrolment writes a non-extractable Ed25519 key into
    /// that browser and records its public half in the controller ACL. From
    /// then on the device authenticates *remotely* by signing a per-connection
    /// challenge with that key, so going remote needs no new secret and no new
    /// enrolment path.
    ///
    /// With this off, the internet-facing surface has no way to enrol anything.
    /// Possession of an already-enrolled device key is the only remote
    /// credential that exists, and it cannot be guessed, phished from a
    /// six-digit code, or brute-forced.
    ///
    /// Turn it on only if you need to add a device you cannot bring onto the
    /// LAN; it is safe (a 256-bit one-time invite), but it is strictly more
    /// attack surface than not having it.
    ///
    /// When on, the six-digit **PIN** is also accepted from off-network, but
    /// only over HTTPS (plaintext attempts are refused with
    /// `pin_requires_https`). The per-/64 attempt limiter bounds guessing, yet
    /// ten million combinations is a speed bump, not a lock — leave this off
    /// unless devices must be able to pair from outside the network.
    pub allow_remote_pairing: bool,
}

/// HTTPS listener config. See [`TlsMode`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub mode: TlsMode,
    /// Preferred hostname in the play URL. Empty = `<machine>.local` (local-ca)
    /// Empty = `<machine>.local`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// HTTPS port. 443 is standard (Windows can bind it without privilege).
    pub port: u16,
    /// Directory the cert + key (and the local CA) are cached in.
    /// Empty = `<config_dir>/tls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_dir: Option<String>,
    /// Extra SAN entries (hostnames / IPs) to add to the local-ca leaf cert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_sans: Vec<String>,
    /// Let's Encrypt hostname for the public listener (sslip.io form, e.g.
    /// `2001-db8-1234-5678-0-0-0-100.sslip.io`). Empty = auto-derive from the
    /// PCP-reported public IPv6 once it is known. Issuance + renewal are
    /// automatic; browsers trust the result with zero prompts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acme_hostname: String,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            mode: TlsMode::LocalCa,
            domain: None,
            port: 443,
            cert_dir: None,
            extra_sans: Vec::new(),
            acme_hostname: String::new(),
        }
    }
}

impl TlsConfig {
    pub fn wants_tls(&self) -> bool {
        self.mode != TlsMode::Off
    }
    pub fn cert_dir_path(&self) -> PathBuf {
        if let Some(d) = self.cert_dir.as_deref().filter(|d| !d.is_empty()) {
            return PathBuf::from(d);
        }
        // Keys belong to the Windows user, never a shared writable ProgramData
        // directory. Existing installations need a one-time certificate re-trust.
        Config::config_dir().join("tls")
    }
}

impl Config {
    /// Default config directory: `%APPDATA%\InPhase` on Windows,
    /// `$XDG_CONFIG_HOME/inphase` elsewhere (dev/CI).
    pub fn config_dir() -> PathBuf {
        #[cfg(windows)]
        {
            if let Ok(appdata) = std::env::var("APPDATA") {
                return PathBuf::from(appdata).join("InPhase");
            }
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("inphase");
        }
        std::env::temp_dir().join("inphase")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// Load defaults, overlay the on-disk file if present, then apply
    /// `INPHASE_*` env overrides. Missing file is not an error.
    pub fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::default();
        let path = Self::config_path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            // Every field is `#[serde(default)]`, so a partial file overlays
            // cleanly on the defaults.
            cfg = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
            // Unknown keys are reported, never fatal. A settings file written by
            // a newer or older build must not stop the host from starting — that
            // turns every upgrade that renames or drops a knob into an outage.
            for key in Self::unknown_keys(&text, &cfg) {
                tracing::warn!(%key, "ignoring unrecognised setting in config.toml");
            }
        }
        cfg.apply_env_overrides();
        cfg.validate()?;
        Ok(cfg)
    }

    /// `section.key` paths present in `text` that this build does not know.
    ///
    /// Compared against the **parsed** config re-serialised, not a
    /// hand-maintained list and not `Config::default()`: every key serde
    /// actually consumed survives the round-trip, and any key serde ignored (a
    /// typo, or a knob written by a newer build) vanishes — exactly the set
    /// worth reporting. Comparing against the *default* config false-positives
    /// `Option` fields (e.g. `media.audio_capture_device`): they serialise as
    /// absent when unset, so a file that sets them looks "unknown".
    fn unknown_keys(text: &str, parsed: &Config) -> Vec<String> {
        let (Ok(found), Ok(known)) = (text.parse::<toml::Value>(), toml::Value::try_from(parsed))
        else {
            return Vec::new();
        };
        let (Some(found), Some(known)) = (found.as_table(), known.as_table()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (section, value) in found {
            match (value.as_table(), known.get(section)) {
                // A whole section this build no longer has.
                (_, None) => out.push(section.clone()),
                // A known section: check its keys.
                (Some(sub), Some(known_section)) => {
                    if let Some(known_sub) = known_section.as_table() {
                        for key in sub.keys() {
                            if !known_sub.contains_key(key) {
                                out.push(format!("{section}.{key}"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.sort();
        out
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(p) = std::env::var("INPHASE_HTTP_PORT") {
            if let Ok(p) = p.parse() {
                self.network.http_port = p;
            }
        }
        if let Ok(v) = std::env::var("INPHASE_MONITOR_INDEX") {
            self.capture.monitor_index = v.parse().ok();
        }
        if let Ok(v) = std::env::var("INPHASE_CAPTURE_API") {
            self.capture.api = match v.to_ascii_lowercase().as_str() {
                "wgc" => CaptureApi::Wgc,
                _ => CaptureApi::Dxgi,
            };
        }
        if std::env::var("INPHASE_ENABLE_AUDIO").is_ok() {
            self.media.enable_audio = true;
        }
        if let Ok(v) = std::env::var("INPHASE_AUDIO_DEVICE") {
            self.media.audio_capture_device = Some(v).filter(|s| !s.is_empty());
        }
        if let Ok(v) = std::env::var("INPHASE_TLS_MODE") {
            self.tls.mode = match v.as_str() {
                "off" => TlsMode::Off,
                _ => TlsMode::LocalCa,
            };
        }
        if let Ok(v) = std::env::var("INPHASE_TLS_DOMAIN") {
            self.tls.domain = Some(v).filter(|s| !s.is_empty());
        }
        if let Ok(v) = std::env::var("INPHASE_TLS_PORT") {
            if let Ok(p) = v.parse() {
                self.tls.port = p;
            }
        }
        // point at STUN for direct paths off the tailnet.
        // `INPHASE_STUN_SERVERS` is a comma-separated list of `stun://host:port`
        // URIs; setting it also clears `strict_lan` unless `INPHASE_STRICT_LAN`
        // says otherwise.
        if let Ok(v) = std::env::var("INPHASE_STUN_SERVERS") {
            self.network.stun_servers = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            self.network.strict_lan = self.network.stun_servers.is_empty();
        }
        if let Ok(v) = std::env::var("INPHASE_STRICT_LAN") {
            self.network.strict_lan = matches!(v.as_str(), "1" | "true" | "yes");
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.network.http_port != 0, "network.http_port must be set");
        anyhow::ensure!(
            self.network.http_port != self.network.admin_port,
            "http_port and admin_port must differ"
        );
        anyhow::ensure!(
            matches!(self.media.default_fps, 30 | 60 | 120),
            "media.default_fps must be 30, 60 or 120 (got {})",
            self.media.default_fps
        );
        anyhow::ensure!(self.tls.port != 0, "tls.port must be set");
        Ok(())
    }

    pub fn http_socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.network.bind, self.network.http_port)
    }

    pub fn admin_socket_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.network.admin_port)
    }

    /// The codec the host will offer first: H.265, always (H.264 removed).
    pub fn preferred_offer_codec(&self) -> VideoCodec {
        VideoCodec::H265
    }
}

impl Config {
    /// Serialise the current config to TOML for writing the template file.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

/// Set one `[section] key = <bool>` in `config.toml`, leaving the rest of the
/// (possibly hand-edited) file untouched. Used by the tray toggles so a feature
/// enabled from the menu survives a restart.
pub fn persist_bool(section: &str, key: &str, value: bool) -> anyhow::Result<()> {
    let path = Config::config_path();
    let mut doc: toml::Table = if path.exists() {
        std::fs::read_to_string(&path)?.parse()?
    } else {
        toml::Table::new()
    };
    let tbl = doc
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(t) = tbl {
        t.insert(key.to_string(), toml::Value::Boolean(value));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&doc)?)?;
    Ok(())
}

/// Persist just `media.audio_capture_device` into `config.toml`, leaving the
/// rest of the (possibly hand-edited) file untouched. `None`/empty clears it.
pub fn persist_audio_capture_device(id: Option<&str>) -> anyhow::Result<()> {
    let path = Config::config_path();
    let mut doc: toml::Table = if path.exists() {
        std::fs::read_to_string(&path)?.parse()?
    } else {
        toml::Table::new()
    };
    let media = doc
        .entry("media".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(m) = media {
        match id.filter(|s| !s.is_empty()) {
            Some(v) => {
                m.insert(
                    "audio_capture_device".into(),
                    toml::Value::String(v.to_string()),
                );
            }
            None => {
                m.remove("audio_capture_device");
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_setting_is_reported_but_never_fatal() {
        // The exact shape that used to brick startup: a key this build removed.
        let text = r#"
[media]
webrtc_backend = "webrtcbin"
allow_hevc = false

[vpn]
enabled = true
"#;
        let cfg: Config = toml::from_str(text).expect("stale keys must not fail the parse");
        let unknown = Config::unknown_keys(text, &cfg);
        assert!(
            unknown.contains(&"media.webrtc_backend".to_string()),
            "{unknown:?}"
        );
        assert!(unknown.contains(&"vpn".to_string()), "{unknown:?}");
    }

    #[test]
    fn a_current_config_reports_nothing_unknown() {
        let text = toml::to_string(&Config::default()).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(Config::unknown_keys(&text, &cfg).is_empty());
    }

    #[test]
    fn an_option_field_set_in_the_file_is_not_reported_unknown() {
        // `media.audio_capture_device` is `Option<String>`: unset it serialises
        // as absent, which made the default-config comparison flag every file
        // that set it (the host even logged "unrecognised setting" while the
        // value was in fact applied).
        let text = r#"
[media]
audio_capture_device = "{0.0.1.00000000}.{037288aa-d79f-45dc-97d6-4ceb1db47f87}"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(
            cfg.media.audio_capture_device.as_deref(),
            Some("{0.0.1.00000000}.{037288aa-d79f-45dc-97d6-4ceb1db47f87}")
        );
        assert!(Config::unknown_keys(text, &cfg).is_empty());
    }

    #[test]
    fn a_typo_inside_a_known_section_is_still_reported() {
        let text = "[media]\nno_such_knob = 1\n";
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(
            Config::unknown_keys(text, &cfg),
            vec!["media.no_such_knob".to_string()]
        );
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn rejects_equal_ports() {
        let mut c = Config::default();
        c.network.admin_port = c.network.http_port;
        assert!(c.validate().is_err());
    }

    #[test]
    fn firewall_scope_widens_to_ipv6_only_when_remote_access_is_on() {
        let net = NetworkConfig::default();

        let off = RemoteAccessConfig {
            enabled: false,
            ..Default::default()
        };
        let cidrs = net.firewall_allowed_remote_cidrs(&off);
        assert!(
            !cidrs.iter().any(|c| c.contains("::")),
            "LAN-only by default: {cidrs:?}"
        );

        let on = RemoteAccessConfig {
            enabled: true,
            ..Default::default()
        };
        let cidrs = net.firewall_allowed_remote_cidrs(&on);
        assert!(
            cidrs.contains(&"2000::/3".to_string()),
            "global v6 must be allowed: {cidrs:?}"
        );
        assert!(
            !cidrs.iter().any(|c| c.eq_ignore_ascii_case("any")),
            "must NOT open IPv4 to the internet: {cidrs:?}"
        );
        // Still keeps the LAN + tailnet scopes.
        assert!(cidrs.iter().any(|c| c.eq_ignore_ascii_case("localsubnet")));
    }

    #[test]
    fn rejects_odd_fps() {
        let mut c = Config::default();
        c.media.default_fps = 75;
        assert!(c.validate().is_err());
    }

    #[test]
    fn strict_lan_is_default() {
        assert!(Config::default().network.strict_lan);
        assert!(Config::default().media.require_hardware_encoder);
    }
}
