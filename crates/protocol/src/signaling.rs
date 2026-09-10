//! JSON signaling / control messages (architecture report §15, §16).
//!
//! These travel over the same authenticated session as pairing:
//!   * the `/api/v1/signal` WebSocket during negotiation, and
//!   * the reliable `control` data channel once the peer connection is up.
//!
//! The report's rule (§16): *"Keep signaling/control JSON-readable for
//! debugging. Keep only high-frequency input binary."* — hence serde/JSON here
//! and the hand-rolled binary format in [`crate::input`].

use serde::{Deserialize, Serialize};

/// Wire version for the JSON signaling protocol (§16 "version both protocols
/// from day one"). The client sends its version in [`SignalMessage::ClientHello`]
/// and the host rejects a mismatch with [`SignalErrorCode::ProtocolVersion`].
///
/// `2` — the signaling WebSocket is served over the host's own TLS (a real
/// Tailscale/Let's Encrypt cert) and carries an Ed25519 device
/// challenge/response ([`SignalMessage::AuthChallenge`] /
/// [`SignalMessage::AuthResponse`]). A v1 client is rejected at handshake.
pub const SIGNALING_PROTOCOL_VERSION: u32 = 2;

/// Video codec the product knows how to negotiate (§7). AV1 is deliberately
/// excluded from v1 (§7 "Do not add AV1 to v1").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    /// RTP MIME type as advertised by `RTCRtpReceiver.getCapabilities`.
    pub fn mime_type(self) -> &'static str {
        match self {
            VideoCodec::H264 => "video/H264",
            VideoCodec::H265 => "video/H265",
        }
    }
}

/// Latency/quality preset (§9). `Custom` exposes only resolution, FPS, codec
/// preference, bitrate cap and the network-protection toggle — never encoder
/// internals (§9, §25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    LowLatency,
    /// §9: "Balanced (default)".
    #[default]
    Balanced,
    Quality,
    Custom,
}

/// One RTP codec capability line, mirroring `RTCRtpCodecCapability`. The host
/// must not invent codec/fmtp values the browser did not advertise (§7 step 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtpCodecCapability {
    pub mime_type: String,
    pub clock_rate: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_fmtp_line: Option<String>,
}

/// `MediaCapabilities`-style decode hint for the requested codec/profile/size
/// (§7 step 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeHint {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub supported: bool,
    pub smooth: bool,
    pub power_efficient: bool,
}

/// What the client wants to stream: the whole desktop or a specific installed game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamTarget {
    #[default]
    Desktop,
    Game {
        id: String,
    },
}

/// Requested stream mode from the client's UI (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    #[serde(default)]
    pub preset: QualityPreset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_preference: Option<VideoCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub stream_target: StreamTarget,
}

/// Browser feature-detection results (§10, §12, §14). The host uses these to
/// decide what it can *promise*, not to gate the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClientFeatures {
    pub secure_context: bool,
    pub jitter_buffer_target: bool,
    pub keyboard_lock: bool,
    pub pointer_lock: bool,
    pub pointer_lock_unadjusted_movement: bool,
    pub request_video_frame_callback: bool,
    pub gamepad: bool,
}

/// What the host will actually inject, reported back so the play UI can show
/// "full keyboard capture unavailable" etc. (§24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCapabilities {
    pub keyboard: bool,
    pub mouse: bool,
    /// True only once a virtual-HID backend is active (Phase 5, §13).
    pub gamepad: bool,
    pub backend: String,
}

/// Host's chosen stream parameters, sent after capability intersection (§15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preset: QualityPreset,
    pub start_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    /// Suggested `RTCRtpReceiver.jitterBufferTarget` in milliseconds (§10).
    /// Always 0: the product carries no playout buffer — frames render as they
    /// arrive. Kept on the wire for protocol compatibility; current clients
    /// ignore it.
    pub jitter_buffer_target_ms: u32,
    pub input: InputCapabilities,
    pub encoder_backend: String,
    /// ICE servers the browser should use for candidate gathering.
    /// Empty in strict-LAN mode (host + VPN candidates only). Otherwise carries
    /// InPhase's own STUN URI(s) in browser `stun:host:port` form — never a
    /// third-party STUN, never TURN (InPhase runs no relay).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<String>,
    /// Desktop or a library game the client picked before connecting.
    #[serde(default)]
    pub stream_target: StreamTarget,
}

/// A single ICE candidate, matching the browser `RTCIceCandidateInit` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mline_index: Option<u32>,
}

/// Typed, actionable failure codes (§15 "Structured typed failure with
/// actionable code", §22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalErrorCode {
    /// Signaling protocol version mismatch — reload the play page.
    ProtocolVersion,
    /// Another player already owns the single [`crate::input`] session (§15, §22).
    Busy,
    /// No hardware encoder on the host for any acceptable codec (§6, §22).
    NoHardwareEncoder,
    /// Host and client share no usable video codec (§7).
    NoCommonCodec,
    /// DXGI Desktop Duplication could not be started or was lost (§22).
    CaptureUnavailable,
    /// Generic negotiation failure.
    NegotiationFailed,
    /// The authenticated session expired or was revoked (§14).
    Unauthorized,
    /// Catch-all; `message` carries detail.
    Internal,
}

/// Structured error payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalError {
    pub code: SignalErrorCode,
    pub message: String,
}

impl SignalError {
    pub fn new(code: SignalErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The full signaling message set (§15). Tagged by `"type"` for JSON
/// readability and forward-compatible parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// browser -> host: protocol version, browser build, requested mode.
    ClientHello {
        protocol_version: u32,
        browser: String,
        requested_mode: RequestedMode,
    },
    /// browser -> host: RTP codec capabilities, decode hints, feature flags.
    ClientCapabilities {
        rtp_video_codecs: Vec<RtpCodecCapability>,
        rtp_audio_codecs: Vec<RtpCodecCapability>,
        decode_hints: Vec<DecodeHint>,
        features: ClientFeatures,
    },
    /// host -> browser: chosen codec, resolution, FPS, preset, input caps.
    SessionConfig(SessionConfig),
    /// both: standard SDP offer.
    Offer {
        sdp: String,
    },
    /// both: standard SDP answer.
    Answer {
        sdp: String,
    },
    /// both: trickled ICE candidate. `end_of_candidates` when `candidate` empty.
    Ice(IceCandidate),
    /// host -> browser: first message on the signaling socket — a random nonce
    /// (base64) the browser must sign with its paired Ed25519 device key.
    AuthChallenge {
        nonce: String,
    },
    /// browser -> host: the controller's public key (hex) and its Ed25519
    /// signature (base64) over the challenge nonce. The host checks the key
    /// against its controller ACL; an unknown/revoked key is rejected before
    /// any media setup.
    AuthResponse {
        controller_id: String,
        signature: String,
    },
    /// host -> browser: peer + data channels ready; client may enter capture mode.
    SessionReady,
    /// host -> browser: structured typed failure.
    Error(SignalError),
    /// host -> browser: WebTransport video transport offer (ADR-0011). Sent
    /// right after the session is claimed when the host runs `media.wt_enabled`.
    /// The browser may dial QUIC on `port`, pin the self-signed certificate by
    /// `cert_sha256` (hex, for `serverCertificateHashes`), and present `token`
    /// as the first message on its control stream. The token is one-time and
    /// short-lived; it never grants anything but the video slot. Absent
    /// message = use the WebRTC path (older hosts, or WT disabled).
    WtVideoInfo {
        token: String,
        port: u16,
        cert_sha256: String,
    },
    /// client -> host: the one-time dial token from the previous
    /// `wt_video_info` was consumed (or expired) — mint a fresh one so the
    /// watchdog's redial can actually reconnect without a page reload.
    WtVideoInfoRequest,
    /// both: liveness ping on the reliable channel (§16).
    Ping {
        at_us: u64,
    },
    Pong {
        at_us: u64,
    },
    /// browser -> host: once-per-second telemetry summary (§20).
    ClientTelemetry(ClientTelemetry),
    /// browser -> host / host -> browser: clean shutdown of the current session.
    Bye,
}

/// Compact client telemetry pushed once per second on the reliable channel so
/// the host dashboard can attribute the bottleneck to capture / encode /
/// network / decode / playout (§20).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientTelemetry {
    pub at_us: u64,
    pub codec: Option<String>,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    /// Rate the decoder receives frames — the real stream fps (§20).
    pub decoded_fps: f32,
    /// Rate the browser actually paints — capped by the display refresh (§10).
    pub presented_fps: f32,
    pub decode_time_ms_p50: f32,
    pub decode_time_ms_p95: f32,
    /// Jitter-buffer delay per frame over the client's last window (delta of
    /// `jitterBufferTargetDelay` / delta of `jitterBufferEmittedCount`) — the
    /// current behaviour, not the lifetime cumulative average.
    pub jitter_buffer_target_ms: f32,
    /// Windowed video jitter-buffer delay per frame (see above).
    pub jitter_buffer_delay_ms: f32,
    /// Windowed audio jitter-buffer delay per frame. The browser syncs A/V
    /// playout by holding video back to audio's playout point, so while audio
    /// is on this is video's effective floor.
    #[serde(default)]
    pub audio_jitter_buffer_ms: f32,
    /// §13 evaluation: does THIS client's browser actually support WebCodecs
    /// Opus decoding (the prerequisite for Opus-over-WT)? `None` = probe not
    /// run / client predates it. The host logs the transition; removal of the
    /// WebRTC audio path waits for real support across target clients.
    #[serde(default)]
    pub audio_opus_supported: Option<bool>,
    pub packets_lost: u64,
    /// WT path only: frames that arrived in pieces but never assembled, this
    /// window. The decisive signal for a route that cannot carry the current
    /// resolution's keyframes - and one that shows up in no other counter,
    /// since decoded fps simply stays zero with no error reported anywhere.
    /// Absent from older clients, hence `default`.
    #[serde(default)]
    pub frames_dropped_incomplete: u64,
    pub rtt_ms: f32,
    pub inbound_bitrate_kbps: f32,
    pub freeze_count: u64,
    pub total_freeze_ms: f32,
    /// Client's read of the inbound audio track (§11):
    /// `"no-track" | "no-packets" | "no-samples" | "silent" | "signal"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_state: Option<String>,
    /// Latest inbound `audioLevel` (0–1).
    #[serde(default)]
    pub audio_level: f32,
    /// WT wire counters from the client's stream-reader loop (v3). They split
    /// "the route lost the frame" from "WebKit never yielded the stream" —
    /// failures with opposite fixes. Absent from older clients, hence `default`.
    #[serde(default)]
    pub frames_received: u64,
    #[serde(default)]
    pub streams_wedged: u64,
    #[serde(default)]
    pub datagrams_seen: u64,
    /// Datagrams the client actually read off `datagrams.readable` this
    /// telemetry window — its measured drain rate. The host paces v4 datagram
    /// injection below this (05:08 Chrome: unconstrained injection at
    /// ~5.5k datagrams/s overflowed the browser's incoming-datagram queue,
    /// which silently drops from the head; RFC 9221 datagrams have no flow
    /// control, so the loss is invisible server-side and every large frame
    /// became a permanent sequence hole → 1 fps re-key loop). Absent from
    /// older clients, hence `default`; 0 = unknown.
    #[serde(default)]
    pub drain_pps: u32,
    /// Capture → decode-complete latency percentiles (ms), client-side, via
    /// the pong clock anchor (research doc §measurement). Negative until the
    /// clock syncs. Percentiles rather than an EMA: the spikes define
    /// remote-play quality. Absent from older clients, hence `default`.
    #[serde(default)]
    pub lat_p50_ms: f32,
    #[serde(default)]
    pub lat_p95_ms: f32,
    /// Decode-side stall diagnosis (research doc §measurement): reorder
    /// buffer depth, frames the decoder queue skipped, codec backlog.
    #[serde(default)]
    pub decode_held: u64,
    #[serde(default)]
    pub decode_behind_events: u64,
    #[serde(default)]
    pub decode_queue_size: u64,
    /// v4 fragment repair: NACKs sent by the client and keyframes assembled.
    /// held>0 with keys=0 and nacks climbing = the repair path working;
    /// keys=0 with no nacks means the IDR never reassembles at all.
    #[serde(default)]
    pub nacks_sent: u64,
    #[serde(default)]
    pub keys_received: u64,
}

// PartialEq for ClientTelemetry uses f32 fields; Eq is intentionally not derived.
impl Eq for ClientTelemetry {}

impl SignalMessage {
    /// Convenience for the common host-side error path.
    pub fn error(code: SignalErrorCode, message: impl Into<String>) -> Self {
        SignalMessage::Error(SignalError::new(code, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips_as_tagged_json() {
        let msg = SignalMessage::ClientHello {
            protocol_version: SIGNALING_PROTOCOL_VERSION,
            browser: "Chrome/140".into(),
            requested_mode: RequestedMode {
                width: 1920,
                height: 1080,
                fps: 120,
                preset: QualityPreset::LowLatency,
                codec_preference: Some(VideoCodec::H264),
                max_bitrate_kbps: None,
                stream_target: StreamTarget::Desktop,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"client_hello\""));
        let back: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn error_code_is_snake_case() {
        let json = serde_json::to_string(&SignalMessage::error(
            SignalErrorCode::NoHardwareEncoder,
            "no NVENC",
        ))
        .unwrap();
        assert!(json.contains("\"no_hardware_encoder\""));
    }

    #[test]
    fn ice_end_of_candidates_shape() {
        let json = serde_json::to_string(&SignalMessage::Ice(IceCandidate {
            candidate: String::new(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        }))
        .unwrap();
        let back: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SignalMessage::Ice(c) if c.candidate.is_empty()));
    }

    #[test]
    fn wt_video_info_roundtrips() {
        let msg = SignalMessage::WtVideoInfo {
            token: "a1b2c3".into(),
            port: 4433,
            cert_sha256: "00ff".repeat(32),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"wt_video_info\""));
        let back: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }
}
