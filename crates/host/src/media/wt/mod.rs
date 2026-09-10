//! WebTransport video transport — host side (ADR-0011 P1).
//!
//! Runs *alongside* the WebRTC path, which stays authoritative for signaling,
//! audio, input, and the `control` channel; this module carries **video only**
//! as QUIC datagrams (unreliable, unordered — no browser jitter buffer in the
//! path, which is the entire point of ADR-0011).
//!
//! Session lifecycle (one active video session, ADR-0007):
//!
//! 1. The signaling WebSocket (already authenticated: PIN pairing + Ed25519
//!    device challenge, ADR-0009) issues a **one-time token** via
//!    [`TokenStore::issue`] when a player session starts.
//! 2. The client dials QUIC and presents the token as the **first** message on
//!    its control stream — a QUIC dial carries no credentials, so the host
//!    refuses everything until then.
//! 3. Valid token + no live video session → the connection becomes the frame
//!    sender's live target and control messages start flowing.
//!    Anything else → `Error` + immediate close.
//!
//! The pipeline thread never blocks: [`WtVideoTransport::send_frame`] is
//! `try_send` into a small bounded queue — a full queue drops the frame.
//! Skipping a frame is the right response to congestion; buffering it is how
//! the 145 ms WebRTC pathology happens (ADR-0011's latency budget).

//!
//! Layout (split out of a single 1,290-line file, which is where the quinn
//! send_datagram race and the telemetry tag drift both hid):
//!   [`tokens`]    - one-time dial tokens
//!   [`resend`]    - the NACK re-send cache, a pure struct
//!   [`session`]   - per-connection auth, control stream, teardown
//!   [`transport`] - the bound endpoint, public handle, and frame sender
//! This file keeps only the types those modules share.

pub(crate) mod session;
mod tokens;
mod transport;

pub use tokens::TokenStore;
pub use transport::WtVideoTransport;

use inphase_protocol::{ClientTelemetry, WtHostMessage};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Datagram size budget per fragment (PMTU-safe; see ADR-0011 framing).
/// Upper bound on the QUIC datagram size used for video fragments.
///
/// 1200 is QUIC's own conservative initial MTU and survives the tunnels people
/// actually reach a gaming PC through - notably WireGuard/Tailscale at 1280,
/// where the previous 1300 exceeded the tunnel MTU and every full-size fragment
/// was silently black-holed. This is only a ceiling: the live connection's
/// `max_datagram_size()` narrows it further per path (see the frame sender).
pub const DEFAULT_DATAGRAM_BUDGET: usize = 1200;
/// v4 injection pace, datagrams/s. The browser's incoming-datagram queue has
/// NO flow control (RFC 9221) and silently drops from the HEAD when the app
/// reads slower than the host injects — with the QUIC ACK already sent, the
/// host sees 0% loss while the oldest frame's fragments are destroyed
/// (05:08 Chrome: smooth at ~4.7k datagrams/s, permanent 1 fps re-key loop
/// from ~5.5k). 3800 paces every measured session below the collapse point;
/// the frame sender takes 64-token bursts. Deltas only — forced IDRs ride
/// the reliable stream (05:38 stutter fix), so the budget is never consumed
/// by a 700 KB burst. Raise only with a measured faster drain (worker).
pub const WT_PACE_PPS: f32 = 3800.0;
/// AIMD ceiling matching the pace, minus measured encoder overshoot. The
/// pacer injects 3800 datagrams/s; on the CIN-PC LAN the live budget is
/// ~1446 B (not the 1082 tunnel floor), so the wire carries ~39 Mbps of
/// data + parity. But the NVIDIA encoder overshoots its target ~16% on
/// bursts: at the old ceiling (32892) the 06:13 session measured 39-43 Mbps
/// inbound = 98% pace utilization, and transient whole-frame datagram loss
/// punched reorder holes every ~2.5 s. 28000 lands the real rate at ~83%
/// utilization - the band where every measured session ran 60 fps smooth.
pub const WT_PACED_CEILING_KBPS: u32 = 28_000;
/// How many frames may sit queued between the pipeline and the wire. Small by
/// design — this is a leaky bucket, not a buffer.
pub const DEFAULT_MAX_QUEUED_FRAMES: usize = 4;
/// How long a client has to present a valid token after dialing.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

// ---- transport ---------------------------------------------------------------

/// One encoded frame from the media pipeline.
#[derive(Debug)]
pub struct OutboundFrame {
    pub frame_no: u32,
    pub capture_us: u64,
    pub key: bool,
    pub payload: Vec<u8>,
    /// Host monotonic time the encoder produced this frame. Every queue
    /// downstream measures age against it and rejects work past the frame's
    /// freshness budget (`DELTA_FRESHNESS` / `KEY_FRESHNESS` in transport.rs):
    /// a frame older than its playout window is worthless, and delivering it
    /// only delays the frames behind it.
    pub captured_at: std::time::Instant,
    /// Host wall-clock µs when `send_frame` accepted the frame (timeline).
    pub enq_us: u64,
    /// Host wall-clock µs the encoder finished this frame, derived in
    /// `send_frame` from `captured_at` (monotonic) so the timeline's
    /// capture→handoff spans all sit in one clock. The wire carries the PTS
    /// `capture_us`; this is the host-clock twin.
    pub capture_host_us: u64,
}

/// Control-stream events surfaced to the media session (drained via
/// [`WtVideoTransport::try_next_event`]).
#[derive(Debug, Clone)]
pub enum WtClientEvent {
    /// Raw input packet bytes from a client WT datagram (§13). The consumer
    /// applies them to the current player's InputSession exactly like the
    /// WebRTC data-channel pump does; arming rules are identical.
    Input(Vec<u8>),
    Connected,
    Disconnected,
    KeyframeRequest,
    Telemetry(ClientTelemetry),
}

/// Bind arguments. The identity comes from the caller (the P2 wiring will pass
/// the bundled-CA leaf; `Identity::self_signed` is fine for development).
pub struct WtTransportConfig {
    pub port: u16,
    pub identity: wtransport::Identity,
    pub datagram_budget: usize,
    pub max_queued_frames: usize,
}

/// Where host→client control messages are delivered. `None` = no live
/// authenticated session (pushes are dropped silently).
type ControlOutSlot = Arc<Mutex<Option<mpsc::UnboundedSender<WtHostMessage>>>>;

struct Shared {
    last_keyreq_log: parking_lot::Mutex<std::time::Instant>,
    tokens: Arc<TokenStore>,
    events: mpsc::UnboundedSender<WtClientEvent>,
    /// Audio datagram funnel (opus packets, §11-on-WT). Drained by the
    /// single-owner sender task.
    audio: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Id of the session currently holding the single video slot (ADR-0007).
    ///
    /// An id rather than a flag because sessions displace each other: when a
    /// newer client takes over, the outgoing one still has teardown to run, and
    /// a bare flag would let it clear the slot its successor had just claimed.
    /// Teardown only releases the slot if it still owns it.
    active: Mutex<Option<u64>>,
    /// Hands out [`Shared::active`] ids. Monotonic; wrapping is not a concern.
    next_session_id: std::sync::atomic::AtomicU64,
    /// Last seen §13 audio probe from the client (-1 unknown / 0 no / 1 yes):
    /// logged on transition so the dashboard/log carries the evidence.
    audio_opus_supported: std::sync::atomic::AtomicI8,
    /// The frame-sender task follows the live connection through this watch.
    live_connection: watch::Sender<Option<wtransport::Connection>>,
    /// Maps wall time onto the capture (PTS) clock: `(anchor_instant,
    /// capture_us)` refreshed on every frame the sender puts on the wire.
    /// Anchoring continuously (rather than once at pipeline start) makes
    /// pongs immune to any constant epoch offset between GStreamer's clock
    /// and the host's monotonic clock.
    frame_anchor: Arc<Mutex<Option<(std::time::Instant, u64)>>>,
    /// The media session's negotiated video config (ADR-0011), stored by
    /// `MediaSession::configure` and pushed to the client right after auth:
    /// the WebCodecs decoder cannot decode a single frame until it has been
    /// configured, so this must be the first host → client control message.
    video_config: Arc<Mutex<Option<WtHostMessage>>>,
    /// Where video frames are written: the send half of a stream the CLIENT
    /// opened (the "video channel", see `session.rs`'s accept loop).
    ///
    /// Server-initiated streams are not reliably delivered to JavaScript by
    /// iOS WebKit — three independent failure shapes in one day (2026-09-09):
    /// never-delivered FINs exhausting flow credit (v2), every per-frame
    /// stream EOFing before its payload (v3 per-frame), and a persistent
    /// stream falling silent after one frame (v3 batched) — while every
    /// client-opened channel (control, input) and server→client datagrams
    /// (audio) carried data flawlessly throughout. Frames therefore ride a
    /// stream the client opened and marked. `None` = no channel yet; the
    /// sender drops frames until one appears.
    video_sink: Mutex<Option<wtransport::stream::SendStream>>,
    /// Worst frame `write_all` stall (ms) in the last sender second: how long
    /// the QUIC send buffer made a frame wait because the peer stopped
    /// consuming. This is the congestion signal WebKit cannot provide - it
    /// buffers happily (2026-09-09 19:53: 20 Mbps pushed into a cellular link
    /// with zero loss and 60 decoded fps until connection flow control froze
    /// the session) - so the transport measures its own queue and the bitrate
    /// controller reads it as `rtp_backlog_ms` in its feedback.
    write_stall_ms: std::sync::atomic::AtomicU32,
    /// Per-frame host timeline (research doc §measurement): frames of
    /// `[frame_no, capture_us, enq_us, pop_us, write_us]` in host wall-clock
    /// µs (`frametrace::now_us`). `write_us == 0` marks a frame the sender
    /// reset mid-write (timeout/error). Drained by the admin
    /// `frame-timeline` endpoint as JSONL for trace capture and replay.
    wt_timeline: Mutex<std::collections::VecDeque<[u64; 5]>>,
    /// v4 carrier switch: video frames go as deadline-aware datagram
    /// fragments instead of the reliable stream. Toggled by the client over
    /// the control channel (`enable_datagram_video` / `disable_datagram_video`)
    /// once it has proof that host→client datagrams arrive (audio datagrams
    /// flowing = the probe, on iOS Safari where server-initiated streams are
    /// broken but datagrams demonstrably work).
    datagram_video: std::sync::atomic::AtomicBool,
    /// Retransmit cache for the v4 carrier: recent datagram fragments
    /// `(frame_no, frag_idx, encoded_datagram)`. The client NACKs a missing
    /// fragment over the control stream (reliable, ordered, and it REPEATS
    /// the NACK while a hole persists - one re-send can hit the same burst
    /// loss window that ate the original); the control handler re-sends it
    /// straight from here. A reassembled chain beats an IDR wait: at
    /// cellular burst loss the IDR reassembly kept failing while deltas
    /// kept arriving (22:34: nacks +631, abandoned +29, held=8).
    wt_resend: Mutex<std::collections::VecDeque<(u32, u16, Vec<u8>)>>,
    /// NACK re-send budget: `tokens` datagrams, refilled at a fixed rate.
    /// Without a cap the re-send stream out-shouts fresh video (23:59: the
    /// storm consumed the datagram queue, decode starved at 0 for 20 s).
    wt_resend_tokens: Mutex<(std::time::Instant, u32)>,
}
