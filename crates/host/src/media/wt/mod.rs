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

pub mod congestion;
pub(crate) mod session;
mod tokens;
mod transport;

pub use congestion::WtCongestion;

/// Reads the pipeline's running time now, in µs; `None` once it is gone.
pub type CaptureClock = Arc<dyn Fn() -> Option<u64> + Send + Sync>;
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
/// by a 700 KB burst. This is the pace for IN-PAGE clients (measured main-
/// thread drain 3.0-3.9k); worker-drain clients get [`WORKER_PACE_PPS`].
pub const WT_PACE_PPS: f32 = 3800.0;
/// Injection pace for clients that drain datagrams in a Dedicated Worker
/// (telemetry `worker=true`): the read loop owns its thread, so the main-
/// thread contention that pinned the 3800 figure is gone.
///
/// 11000 pps is what an 80 Mbps encoder target actually needs:
/// `WT_PACE_TO_CEILING` caps the controller at `pace x 7.368` kbps, so the old
/// 6500 capped it at 47.9 Mbps no matter what the client asked for - the
/// negotiated `max_kbps=80000` session could not have reached even 60. At
/// 11000 the cap is 81 Mbps, and the pacer's own token bucket carries
/// 11000 x 1118 B = 98 Mbps of datagrams, which covers an 80 Mbps target
/// plus ~12.5% FEC parity and the encoder's overshoot.
///
/// Raising a pace is only safe because of the interleaved send order: if the
/// browser's queue does fill and drops from the head, the destroyed datagrams
/// are now spread one-per-FEC-group instead of landing three-deep in one
/// group, so row-1 parity rebuilds them instead of the frame dying. The
/// sent-vs-seen accounting reports the case anyway (see `wt_nack_dropped` /
/// the inferred-loss line), because the browser's queue has no flow control
/// and never tells the host.
///
/// 18000, not 11000 (2026-09-24). The "covers 80 Mbps + parity + overshoot"
/// sum above was optimistic: a Mac Chrome session at 1440p120 / 80 Mbps put
/// 95-114 Mbps of datagrams on the wire (NVENC overshoot at 120 fps is well
/// past 30 %), 9-10.4k datagrams/s against the 11k pace. A pace running at
/// its average has no room for a large frame: the send queue filled, the
/// host evicted frames, and each evicted frame was a hole no repair can fill:
/// a keyframe request every 0.5-2 s on a route with zero loss. 18000 is
/// ~160 Mbps of capacity; the loss-driven backoff still retreats for a client
/// that cannot drain it, and the burst (`pace_burst`) is 300 datagrams, well
/// inside the 500 both browsers took back to back without loss.
pub const WORKER_PACE_PPS: f32 = 18000.0;
/// AIMD encoder-target ceiling per pace pps (kbps of encoder target per
/// datagram/s): calibrated to the measured-good 28000 at 3800 pps. The
/// relationship absorbs NVIDIA overshoot (~16-30%) + FEC parity (~12.5%):
/// target × 1.25 must stay ≲83% of `pace × ~1377 B × 8`.
pub const WT_PACE_TO_CEILING: f32 = 7.368;
/// AIMD ceiling fallback when the client reports no pace (older telemetry):
/// matches the 3800-pps in-page pace.
pub const WT_PACED_CEILING_KBPS: u32 = 28_000;
/// How many frames may sit queued between the pipeline and the wire. 4 gave
/// a 5-frame encoder burst (scene change at 83% pace utilization) nowhere to
/// wait: the oldest frame was silently EVICTED, punching a frame_no hole the
/// client could only clear by re-keying — a freeze every ~12 s in the 06:28
/// session, with zero loss anywhere else (nacks=0, abandoned=1, drain==
/// inject). 8 frames ≈ 133 ms of runway absorbs the burst; DELTA_FRESHNESS
/// (150 ms) still rejects the stragglers. This is still a leaky bucket, not
/// a buffer — sustained overload still sheds at the oldest edge.
pub const DEFAULT_MAX_QUEUED_FRAMES: usize = 8;
/// How long a client has to present a valid token after dialing.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

// ---- certificate lifetime ----------------------------------------------------
//
// The WT dial is authenticated by a **pinned certificate hash**, not by a CA:
// the signaling socket hands the client the SHA-256 of the host's self-signed
// certificate, and the browser checks the dialed certificate against it. That
// path has a hard browser-side validity rule (W3C WebTransport,
// `serverCertificateHashes`): the certificate must be valid *at dial time* and
// its total validity period must not exceed two weeks. wtransport mirrors both
// rules (`ServerHashVerification::SELF_MAX_VALIDITY`, and `Identity::self_signed`
// mints exactly 14 days).
//
// So the certificate is not merely "rotated on restart" — it *expires*, and an
// expired pin is refused by every browser forever, however many times the client
// redials. `Identity::self_signed` used to be called once at boot with nothing
// behind it, which meant a host left running past that window served a dead pin:
// the page loaded, pairing and signaling succeeded, and the glass stayed black
// until someone restarted the host. Rotation is the fix; these constants keep it
// comfortably inside the browser's cap.

/// Validity period of the WT self-signed certificate. Must stay at or under two
/// weeks — that is the browser's cap, not a preference.
pub const WT_CERT_VALIDITY: Duration = Duration::from_secs(7 * 24 * 3600);
/// `not_before` backdate: a client whose clock trails the host's by minutes
/// would otherwise reject a freshly minted certificate as "not valid yet"
/// (the same reason the HTTPS leaf backdates an hour).
pub const WT_CERT_BACKDATE: Duration = Duration::from_secs(3600);
/// Rotate once the certificate has this much life left — half the validity, and
/// several days clear of the wall the browser enforces.
pub const WT_CERT_ROTATE_WHEN_LEFT: Duration = Duration::from_secs(3 * 24 * 3600);
/// How often the rotation task re-checks. Short, because it also watches for a
/// dead frame sender, which is a black screen no client can diagnose — waiting
/// half an hour to notice it would just be half an hour of black.
pub const WT_ROTATION_POLL: Duration = Duration::from_secs(15);

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

/// Bind arguments: where to listen, and the identity (and its validity window)
/// the client will pin by hash.
pub struct WtTransportConfig {
    pub port: u16,
    pub identity: wtransport::Identity,
    /// When `identity`'s certificate stops being valid. Carried alongside it so
    /// the transport can report its own remaining life (see the certificate
    /// lifetime constants above) and the rotation task can act before a client
    /// starts refusing the dial.
    pub cert_not_after: wtransport::tls::self_signed::time::OffsetDateTime,
    pub datagram_budget: usize,
    pub max_queued_frames: usize,
    /// QUIC congestion law for the connection (see [`congestion`]).
    pub congestion: WtCongestion,
}

/// A WT server identity and the validity window it was minted with.
///
/// The window is kept (rather than thrown away with the builder) because the
/// browser enforces it: [`WT_CERT_VALIDITY`] is a ceiling, and the host has to
/// replace the certificate before the client stops accepting the pin.
pub struct WtIdentity {
    pub identity: wtransport::Identity,
    pub not_before: wtransport::tls::self_signed::time::OffsetDateTime,
    pub not_after: wtransport::tls::self_signed::time::OffsetDateTime,
}

impl WtIdentity {
    /// Mint the short-lived self-signed identity the client pins by hash.
    pub fn self_signed(sans: &[String]) -> anyhow::Result<Self> {
        use wtransport::tls::self_signed::{time, SelfSignedIdentityBuilder};

        let not_before = time::OffsetDateTime::now_utc()
            - time::Duration::seconds(WT_CERT_BACKDATE.as_secs() as i64);
        let not_after = not_before + time::Duration::seconds(WT_CERT_VALIDITY.as_secs() as i64);
        let identity = SelfSignedIdentityBuilder::new()
            .subject_alt_names(sans.iter())
            .not_before(not_before)
            .not_after(not_after)
            .build()
            .map_err(|e| anyhow::anyhow!("WT self-signed certificate: {e}"))?;
        Ok(Self {
            identity,
            not_before,
            not_after,
        })
    }

    /// Total validity period — what the browser caps at two weeks.
    pub fn validity(&self) -> Duration {
        let secs = (self.not_after - self.not_before).whole_seconds().max(0) as u64;
        Duration::from_secs(secs)
    }
}

/// What a (re)bind needs beyond the certificate itself.
#[derive(Clone)]
pub struct WtBindParams {
    /// Names the client may dial: `localhost` plus the machine name.
    pub sans: Vec<String>,
    pub port: u16,
    pub datagram_budget: usize,
    pub max_queued_frames: usize,
    pub congestion: WtCongestion,
}

/// Bind a transport with a freshly minted certificate.
pub fn bind_with_fresh_cert(params: &WtBindParams) -> anyhow::Result<WtVideoTransport> {
    let id = WtIdentity::self_signed(&params.sans)?;
    WtVideoTransport::bind(WtTransportConfig {
        port: params.port,
        identity: id.identity,
        cert_not_after: id.not_after,
        datagram_budget: params.datagram_budget,
        max_queued_frames: params.max_queued_frames,
        congestion: params.congestion,
    })
}

/// Replace the transport in `slot` with a freshly certified one, on the same
/// port, and publish it.
///
/// The order is the whole point of having this in one place:
///
/// 1. Take the retired transport **out of the slot** and drop it. The slot is
///    what the signaling layer, the session manager and the client-event pump
///    read, so nothing can dial or advertise a transport that is mid-retirement;
/// 2. `shutdown()` closes the endpoint, which makes the accept loop observe the
///    signal and drop its own handle on the socket;
/// 3. only then can the port be bound again — and even then the release is
///    asynchronous, so the bind is *retried* rather than assumed.
///
/// A failure here is survivable and never silent: the caller still has days of
/// certificate left, and retries.
pub async fn rotate_transport(
    slot: &WtSlot,
    params: &WtBindParams,
    attempts: u32,
) -> anyhow::Result<Arc<WtVideoTransport>> {
    if let Some(retired) = slot.get() {
        slot.set(None);
        retired.shutdown();
        drop(retired);
    }
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=attempts.max(1) {
        if attempt > 1 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match bind_with_fresh_cert(params) {
            Ok(t) => {
                let t = Arc::new(t);
                slot.set(Some(t.clone()));
                return Ok(t);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no bind attempts made")))
}

/// The live WT transport, shared by the signaling layer, the session manager
/// and the client-event pump.
///
/// A slot rather than a plain `Arc` because the certificate behind the
/// transport has to be replaced on a timer: rotation means a new endpoint with
/// a new identity, and everything that advertises, attaches or drains the
/// transport must see the successor. `set` swaps it; `get` is the only read, so
/// no holder can pin a retired transport by accident.
#[derive(Clone, Default)]
pub struct WtSlot(Arc<parking_lot::RwLock<Option<Arc<WtVideoTransport>>>>);

impl WtSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<Arc<WtVideoTransport>> {
        self.0.read().clone()
    }

    pub fn set(&self, transport: Option<Arc<WtVideoTransport>>) {
        *self.0.write() = transport;
    }
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
    #[cfg(windows)]
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
    /// The capture clock itself, when a pipeline is running: current running
    /// time in µs - the clock `capture_us` (buffer PTS) is stamped on. Pongs
    /// answer from it; `frame_anchor` is only the fallback.
    capture_clock: Mutex<Option<CaptureClock>>,
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
    /// Set when the client begs for a keyframe, consumed by the next IDR: that
    /// IDR goes out over the datagram carrier AS WELL as the stream.
    ///
    /// A client with no anchor decodes nothing, and it only asks when it has
    /// none. In v4 mode deltas ride datagrams while IDRs ride the reliable
    /// stream — and a write into a stream the client stopped reading succeeds
    /// into the void, silently, so a starved client can stay starved for
    /// minutes while the host reports perfect delivery (2026-09-20: 5+ minutes
    /// at 1440p60, `held=8 decoded=0`, deltas arriving at 56 Mbps with 0 % loss,
    /// a keyframe request every second, zero stream errors at the sender). The
    /// response to "I have no anchor" cannot depend on the one carrier that
    /// fails this way: it is doubled, and the client takes whichever arrives
    /// intact. Self-limiting — a healthy client never asks.
    key_by_datagram: std::sync::atomic::AtomicBool,
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
    /// Current v4 injection pace (datagrams/s), per connection: 3800 for
    /// in-page drain, 6500 for worker-drain clients (telemetry `worker`).
    /// The sender reads this per frame; the telemetry handler sets it.
    pace_pps: std::sync::atomic::AtomicU32,
    /// NACK re-sends refused by the budget, and NACKs whose fragment was no
    /// longer in the cache. Both are silent today: the client counts every
    /// NACK it *sends*, so from telemetry a refused request, a re-send that
    /// was itself lost, and a repair that landed all look identical - the
    /// 80 Mbps session showed the client asking (up to 506/s) while this
    /// budget handed out 60/s and dropped the rest without a trace.
    wt_nack_dropped: std::sync::atomic::AtomicU64,
    wt_nack_missed: std::sync::atomic::AtomicU64,
    /// Latest inferred datagram loss in per-mille, over the session's
    /// `LossWindow` (session.rs).
    wt_inferred_loss_ppt: std::sync::atomic::AtomicU32,
    /// The pace this client is *allowed* (worker vs in-page), and how many
    /// consecutive lossy windows have been seen at the current pace. The
    /// effective pace backs off toward the in-page floor when the client
    /// demonstrably cannot drain what is being injected, and creeps back up
    /// while loss stays clean - the browser's incoming queue has no flow
    /// control and never reports anything else.
    pace_max_pps: std::sync::atomic::AtomicU32,
    pace_lossy_windows: std::sync::atomic::AtomicU32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The browser's `serverCertificateHashes` rule, pinned as a test.
    ///
    /// A certificate that is not valid *at dial time* is refused before any
    /// host code runs, and the period itself may not exceed two weeks. The host
    /// used to mint one at boot and never replace it, so a host left running
    /// eventually served a pin nothing would accept — the session connected and
    /// the glass stayed black until the process was restarted.
    #[test]
    fn wt_certificate_stays_inside_the_browser_hash_window() {
        let id = WtIdentity::self_signed(&["localhost".to_string()]).expect("self-signed identity");
        let now = wtransport::tls::self_signed::time::OffsetDateTime::now_utc();

        let two_weeks = wtransport::tls::self_signed::time::Duration::weeks(2);
        assert!(
            id.not_after - id.not_before <= two_weeks,
            "a validity period longer than two weeks is refused outright by browsers"
        );
        assert!(
            id.not_before <= now,
            "not_before must be backdated: a client whose clock trails the host's would \
             otherwise reject the certificate as not yet valid"
        );

        // A fresh certificate must outlive the rotation margin by a wide mark,
        // or the host would be rotating on the very first check.
        let usable = (id.not_after - now).whole_seconds().max(0) as u64;
        assert!(
            usable > 2 * WT_CERT_ROTATE_WHEN_LEFT.as_secs(),
            "usable lifetime {usable}s leaves no room to rotate before expiry"
        );
        assert!(
            WT_CERT_ROTATE_WHEN_LEFT < WT_CERT_VALIDITY,
            "rotation must happen before the certificate runs out"
        );
    }

    /// A rotated transport must be published to every reader of the slot —
    /// signaling advertises from it and the session manager attaches from it.
    #[test]
    fn wt_slot_publishes_the_current_transport() {
        let slot = WtSlot::new();
        assert!(
            slot.get().is_none(),
            "an unbound host advertises no video path"
        );
        // `None` → `None`: rotation failing must not conjure a transport.
        slot.set(None);
        assert!(slot.get().is_none());
    }

    /// The rotation sequence, end to end: the successor has to take the *same*
    /// UDP port the retired endpoint was serving.
    ///
    /// This is the step the whole fix rests on, and the one that is easy to get
    /// wrong: rebinding only works once the retired transport is out of the slot
    /// and its accept loop has dropped the socket, which happens
    /// asynchronously — hence the retry inside `rotate_transport`.
    #[tokio::test]
    async fn rotating_takes_the_same_port_with_a_new_certificate() {
        let params = WtBindParams {
            sans: vec!["localhost".to_string()],
            port: 0, // ephemeral; the successor must reuse whatever we got
            datagram_budget: DEFAULT_DATAGRAM_BUDGET,
            max_queued_frames: DEFAULT_MAX_QUEUED_FRAMES,
            congestion: WtCongestion::default(),
        };
        let slot = WtSlot::new();
        let first = Arc::new(bind_with_fresh_cert(&params).unwrap());
        let port = first.port();
        assert!(port != 0, "the OS assigned a port");
        assert!(
            first.cert_remaining() > WT_CERT_ROTATE_WHEN_LEFT,
            "a fresh transport must not be due for rotation immediately"
        );
        let first_pin = first.cert_sha256();
        slot.set(Some(first));

        let same_port = WtBindParams {
            port,
            ..params.clone()
        };
        let second = rotate_transport(&slot, &same_port, 40)
            .await
            .expect("the successor binds the port the retired endpoint released");

        assert_eq!(second.port(), port, "rotation must not move the dial port");
        assert_ne!(
            second.cert_sha256(),
            first_pin,
            "a rotated transport must present a new pin - reusing the old one is the bug"
        );
        assert!(
            second.cert_remaining() > WT_CERT_ROTATE_WHEN_LEFT,
            "the successor buys a full validity window"
        );
        // The slot, not the return value, is what signaling reads.
        let published = slot.get().expect("the successor is published");
        assert_eq!(published.cert_sha256_hex(), second.cert_sha256_hex());
    }

    /// A rotation that failed leaves the slot empty, and the next attempt has to
    /// come back from that: an empty slot must still bind, or one bad rotation
    /// would strand the host with no video path until it was restarted — the
    /// very outcome this task exists to prevent.
    #[tokio::test]
    async fn rotating_an_empty_slot_restores_the_video_path() {
        let params = WtBindParams {
            sans: vec!["localhost".to_string()],
            port: 0,
            datagram_budget: DEFAULT_DATAGRAM_BUDGET,
            max_queued_frames: DEFAULT_MAX_QUEUED_FRAMES,
            congestion: WtCongestion::default(),
        };
        let slot = WtSlot::new();
        assert!(slot.get().is_none());

        let restored = rotate_transport(&slot, &params, 3)
            .await
            .expect("binding an empty slot is how the host recovers");
        assert!(slot.get().is_some(), "the recovered transport is published");
        assert!(restored.cert_remaining() > WT_CERT_ROTATE_WHEN_LEFT);
    }
}
