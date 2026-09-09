//! Telemetry aggregation (architecture report §20 "Performance instrumentation
//! and acceptance gates", §25.1).
//!
//! The report's rule (§2.1): *"Latency and queue depth are first-class metrics,
//! not something inferred from 'it feels fast.'"* This collector normalises
//! three sources into one snapshot the dashboard renders and the acceptance
//! gates check:
//!
//! * **host** — capture cadence, raw queue depth, encode p50/p95, encoder backend;
//! * **WebRTC** — candidate-pair RTT/loss, outbound bitrate (from the pipeline);
//! * **client** — decode time, jitter buffer, presented FPS, freezes
//!   ([`inphase_protocol::ClientTelemetry`], pushed once/sec on the control
//!   channel, §20).

use parking_lot::RwLock;
use serde::Serialize;

use inphase_protocol::ClientTelemetry;

/// Host-side pipeline metrics (§20).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HostStats {
    /// Frames pulled from the capture source in the last second.
    pub capture_fps: f32,
    /// Encoded access units out of the encoder in the last second. With
    /// videorate filling the rate, this flows even on a static desktop -
    /// the honest "is video being produced" signal.
    pub encoded_fps: f32,
    /// GStreamer leaky-queue level in frames. Gate: 0–1, never trending up (§20).
    pub raw_queue_frames: u32,
    /// Frames dropped by the single-frame leaky queue (stale-frame discipline).
    pub dropped_stale_frames: u64,
    pub encode_ms_p50: f32,
    pub encode_ms_p95: f32,
    /// e.g. `nvd3d11h264enc` (§6).
    pub encoder_backend: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    /// Current capture path, e.g. `dxgi-monitor` or `wgc-window:Game`.
    /// Current encoder target bitrate as driven by GCC (§8.2).
    pub encoder_bitrate_kbps: u32,
    /// Fragment loss the client is reporting, as a percentage. The input to
    /// the keyframe-completion model in [`crate::media::quality`].
    pub route_loss_pct: f32,
    /// Resolution that model says this route can actually deliver keyframes at.
    /// Equal to `width`/`height` on a healthy path; lower means the route is
    /// hostile enough that the current resolution's keyframes are unlikely to
    /// complete. `0` until a keyframe and client telemetry have both been seen.
    /// Frames the client received in pieces that never assembled, last tick.
    pub incomplete_frames: u64,
}

/// Liveness of the loopback-audio branch (§11). Video is never gated on this —
/// but InPhase must not *silently* lose audio either, so the state is explicit
/// and surfaced to the API / HUD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioHealth {
    /// Audio branch not built for this session (`enable_audio = false`).
    #[default]
    Disabled,
    /// Branch built and producing buffers.
    Healthy,
    /// `wasapi2src` posted a device open/IO/removal error; `continue-on-error`
    /// keeps it producing (silence) so the rest of the graph stays alive.
    Degraded,
    /// A downstream element (opus/pay) errored — audio is dead for this session,
    /// video unaffected. Recovers on reconnect.
    Failed,
    /// Reserved for in-session audio-branch reconstruction (not yet implemented).
    Restarting,
}

/// WebRTC transport metrics read from the sink / `webrtcbin` (§20).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TransportStats {
    pub rtt_ms: f32,
    pub packet_loss_pct: f32,
    pub outbound_bitrate_kbps: f32,
    pub nack_count: u64,
    pub pli_count: u64,
    pub fec_enabled: bool,
    pub retransmission_enabled: bool,
}

/// The full dashboard/diagnostics snapshot (§25.1 "Actual stream facts").
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsSnapshot {
    pub host: HostStats,
    pub transport: TransportStats,
    /// WebTransport video session currently live (ADR-0011).
    pub wt_active: bool,
    pub client: Option<ClientTelemetrySerde>,
    /// Rough attribution of the dominant latency contributor for the UI
    /// ("bottleneck: encode / network / decode / playout", §20).
    pub bottleneck: Option<&'static str>,
    pub audio_health: AudioHealth,
}

/// `ClientTelemetry` is defined in the protocol crate without `Serialize` for
/// the host's own JSON shape; re-expose the fields we surface.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientTelemetrySerde {
    pub codec: Option<String>,
    pub decoded_fps: f32,
    pub presented_fps: f32,
    pub decode_time_ms_p95: f32,
    pub jitter_buffer_target_ms: f32,
    pub jitter_buffer_delay_ms: f32,
    pub audio_jitter_buffer_ms: f32,
    pub packets_lost: u64,
    pub rtt_ms: f32,
    pub inbound_bitrate_kbps: f32,
    pub freeze_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_state: Option<String>,
    pub audio_level: f32,
}

impl From<&ClientTelemetry> for ClientTelemetrySerde {
    fn from(c: &ClientTelemetry) -> Self {
        Self {
            codec: c.codec.clone(),
            decoded_fps: c.decoded_fps,
            presented_fps: c.presented_fps,
            decode_time_ms_p95: c.decode_time_ms_p95,
            jitter_buffer_target_ms: c.jitter_buffer_target_ms,
            jitter_buffer_delay_ms: c.jitter_buffer_delay_ms,
            audio_jitter_buffer_ms: c.audio_jitter_buffer_ms,
            packets_lost: c.packets_lost,
            rtt_ms: c.rtt_ms,
            inbound_bitrate_kbps: c.inbound_bitrate_kbps,
            freeze_count: c.freeze_count,
            audio_state: c.audio_state.clone(),
            audio_level: c.audio_level,
        }
    }
}

/// Thread-safe, cheap to clone via `Arc`.
pub struct StatsCollector {
    host: RwLock<HostStats>,
    transport: RwLock<TransportStats>,
    client: RwLock<Option<ClientTelemetry>>,
    audio_health: RwLock<AudioHealth>,
    /// A WebTransport video session is live (ADR-0011). Drives the
    /// dashboard's carrier label; WebRTC remains the fallback whenever this
    /// is false.
    wt_active: std::sync::atomic::AtomicBool,
    /// When the current session started streaming, and when client telemetry
    /// last arrived. [`crate::health`] needs both as *ages*: "no telemetry" only
    /// means something relative to how long the session has been up.
    session_started: RwLock<Option<std::time::Instant>>,
    client_at: RwLock<Option<std::time::Instant>>,
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            host: RwLock::new(HostStats::default()),
            transport: RwLock::new(TransportStats::default()),
            client: RwLock::new(None),
            audio_health: RwLock::new(AudioHealth::Disabled),
            wt_active: std::sync::atomic::AtomicBool::new(false),
            session_started: RwLock::new(None),
            client_at: RwLock::new(None),
        }
    }

    /// Mark the moment the pipeline started streaming, so health rules can tell
    /// "has not started yet" from "started and is broken".
    pub fn mark_session_start(&self) {
        *self.session_started.write() = Some(std::time::Instant::now());
    }

    /// Seconds the current session has been streaming, if one is.
    pub fn session_age(&self) -> Option<std::time::Duration> {
        self.session_started.read().map(|t| t.elapsed())
    }

    /// Seconds since client telemetry last arrived, if it ever has.
    pub fn telemetry_age(&self) -> Option<std::time::Duration> {
        self.client_at.read().map(|t| t.elapsed())
    }

    pub fn audio_health(&self) -> AudioHealth {
        *self.audio_health.read()
    }

    /// Track whether the WT video transport currently has a live client
    /// session (drives the dashboard's video-path indicator).
    pub fn set_wt_active(&self, on: bool) {
        self.wt_active
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_audio_health(&self, h: AudioHealth) {
        let mut cur = self.audio_health.write();
        if *cur == AudioHealth::Failed && h != AudioHealth::Restarting {
            return;
        }
        if *cur != h {
            *cur = h;
        }
    }

    pub fn update_host(&self, f: impl FnOnce(&mut HostStats)) {
        f(&mut self.host.write());
    }

    /// Store the latest client telemetry frame (§20).
    pub fn ingest_client(&self, t: ClientTelemetry) {
        *self.client.write() = Some(t);
        *self.client_at.write() = Some(std::time::Instant::now());
    }

    /// Telemetry arriving on the **signaling socket** (the WebRTC path).
    ///
    /// Both transports send a `client_telemetry` message once a second and they
    /// used to land in the same slot, last-writer-wins. Since ADR-0011 removed
    /// WebRTC video entirely, this message's video counters are all zero, so it
    /// overwrote the WT path's real numbers every other second: the host's view
    /// of the client flapped between real and empty, `have_client` was false on
    /// most ticks, and `decoded_fps` alternated between 0 and a cumulative count
    /// divided by one second. Both the bitrate controller and adaptive
    /// resolution were being driven by that.
    ///
    /// Audio does still ride WebRTC, so while a WT video session is live this
    /// merges the audio fields and leaves the video ones alone. `client_at` is
    /// deliberately not refreshed here - the WT telemetry is what proves the
    /// *video* client is alive, and that is what the health rules ask about.
    pub fn ingest_client_signaling(&self, t: ClientTelemetry) {
        if self.wt_active.load(std::sync::atomic::Ordering::Relaxed) {
            let mut slot = self.client.write();
            if let Some(cur) = slot.as_mut() {
                cur.audio_state = t.audio_state;
                cur.audio_jitter_buffer_ms = t.audio_jitter_buffer_ms;
                cur.audio_level = t.audio_level;
                return;
            }
        }
        self.ingest_client(t);
    }

    /// Latest client telemetry, for the congestion controller.
    pub fn client_snapshot(&self) -> Option<ClientTelemetry> {
        self.client.read().clone()
    }

    /// Clear per-session state on disconnect so the dashboard shows a clean
    /// "Available" (§29 idle-cost gate: near-zero when no player).
    pub fn reset_session(&self) {
        *self.host.write() = HostStats::default();
        *self.transport.write() = TransportStats::default();
        *self.client.write() = None;
        *self.audio_health.write() = AudioHealth::Disabled;
        *self.session_started.write() = None;
        *self.client_at.write() = None;
    }

    /// Current [`crate::health`] verdict for this host.
    pub fn health(&self) -> crate::health::Health {
        let host = self.host.read().clone();
        let client = self.client.read().clone();
        let age = self.session_age();
        crate::health::assess(&crate::health::HealthInput {
            session_active: age.is_some(),
            session_age_secs: age.map(|d| d.as_secs_f32()).unwrap_or(0.0),
            capture_fps: host.capture_fps,
            encoded_fps: host.encoded_fps,
            telemetry_age_secs: self.telemetry_age().map(|d| d.as_secs_f32()),
            decoded_fps: client.as_ref().map(|c| c.decoded_fps).unwrap_or(0.0),
            presented_fps: client.as_ref().map(|c| c.presented_fps).unwrap_or(0.0),
            width: host.width,
            height: host.height,
            route_loss_pct: host.route_loss_pct,
            incomplete_frames: host.incomplete_frames,
            audio_health: *self.audio_health.read(),
        })
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let host = self.host.read().clone();
        let transport = self.transport.read().clone();
        let client = self.client.read().clone();
        let bottleneck = client
            .as_ref()
            .map(|c| classify_bottleneck(&host, &transport, c));
        StatsSnapshot {
            client: client.as_ref().map(ClientTelemetrySerde::from),
            bottleneck,
            host,
            transport,
            wt_active: self.wt_active.load(std::sync::atomic::Ordering::Relaxed),
            audio_health: *self.audio_health.read(),
        }
    }
}

/// Very rough single-label attribution for the UI. Real analysis is offline
/// (§20/§21); this just points a non-expert at the likely layer.
fn classify_bottleneck(
    host: &HostStats,
    transport: &TransportStats,
    client: &ClientTelemetry,
) -> &'static str {
    if host.raw_queue_frames > 1 || host.capture_fps + 5.0 < host.target_fps as f32 {
        "capture"
    } else if host.encode_ms_p95 > frame_budget_ms(host.target_fps) {
        "encode"
    } else if transport.packet_loss_pct > 2.0 || transport.rtt_ms > 40.0 {
        "network"
    } else if client.decode_time_ms_p95 > frame_budget_ms(host.target_fps) {
        "decode"
    } else if client.jitter_buffer_delay_ms > client.jitter_buffer_target_ms + 15.0 {
        "playout"
    } else {
        "none"
    }
}

fn frame_budget_ms(fps: u32) -> f32 {
    if fps == 0 {
        16.7
    } else {
        1000.0 / fps as f32
    }
}
