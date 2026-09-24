//! The bound transport: its public handle, and the frame-sender task.
//!
//! `WtVideoTransport::bind` owns the QUIC endpoint, the accept loop, and the
//! single task that turns encoded frames into datagrams. That task is the only
//! caller of `send_datagram` on purpose - concurrent calls from two tasks race
//! quinn's datagram budget accounting and trip its invariant (seen as
//! "payload_bytes desynced" -> poisoned mutex -> process death), so NACK
//! re-sends are funnelled through it rather than sent directly.

use anyhow::Context as _;
use inphase_protocol::WtHostMessage;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use super::session::handle_incoming;
use super::{ControlOutSlot, OutboundFrame, Shared, TokenStore, WtClientEvent, WtTransportConfig};

/// First byte of an audio datagram (client-side stream discriminator; video
/// fragments begin with the 25-byte WT video header whose first byte is the
/// high byte of a little-endian frame number - zero for any sane session).
const WT_AUDIO_TAG: u8 = 0x41;

/// Send one datagram, catching quinn's own panic. The overflow accounting in
/// quinn-proto's datagram queue panics on its invariant under sustained loss
/// (live: "datagrams.outgoing.payload_bytes desynchronized"); a poisoned
/// connection then panics on every touch. Returning `false` in that case feeds
/// the existing failure-streak teardown, which closes the session so the
/// client's recovery redials into a healthy endpoint.
/// How long a single frame's WRITE may take before the stream is abandoned.
/// QUIC retries a stream indefinitely; a frame past its playout deadline is
/// worthless, so the application decides when to stop caring (§5.2).
///
/// This bounds throughput directly, and it used to be a second. Frames went
/// out one-per-stream then, each holding a permit until the peer ACKed, so
/// eight permits against a one-second deadline was a hard ceiling of 8 frames
/// per second - exactly what a stalled session measured: `sent=8 stalled=52`,
/// every second, until it died (2026-09-09, reproduced with
/// `wt_probe --read-delay-ms 400`).
///
/// A frame is worthless once the next few have been captured, so the deadline
/// belongs near the frame interval, not a thousand times past it. On a single
/// persistent stream a timeout means the peer stopped reading (connection
/// flow control backs pressure up to the write); the stream is mid-frame and
/// unusable, so it is reset and the frame dropped.
/// The injection pace for one frame: fast enough that its datagrams (data
/// and parity) are all out within `FRAME_SPREAD` of the frame interval -
/// sooner when frames are already queued - and never slower than the
/// connection's pace.
///
/// A fixed pace cannot be right for every mode. 18000/s with a 32-datagram
/// burst needs ~4 ms for an average 1440p120 frame (~110 datagrams with two
/// parity rows) and 12-30 ms for a busy one, against an 8.3 ms interval: the
/// 8-frame send queue filled and evicted frames every few seconds, and each
/// eviction is a hole the client can only re-key out of (a Mac at 1440p120,
/// 2026-09-24: 32 evictions and 34 keyframe requests in 5.5 min, with the
/// network clean). Spreading each frame over most of its interval keeps the
/// arrival rate as smooth as the mode allows - the reason for pacing at all -
/// without ever letting the sender fall behind the encoder.
pub(crate) fn frame_pace_pps(
    base_pps: f32,
    datagrams: u32,
    interval_s: f32,
    backlog: usize,
) -> f32 {
    let spread_s = (interval_s * FRAME_SPREAD) / (1 + backlog) as f32;
    (datagrams as f32 / spread_s.max(1e-4)).clamp(base_pps, MAX_FRAME_PACE_PPS.max(base_pps))
}

/// Share of a frame interval a frame's datagrams are spread over.
const FRAME_SPREAD: f32 = 0.9;
/// Ceiling on the per-frame pace: bursts above this are no longer "spread".
const MAX_FRAME_PACE_PPS: f32 = 100_000.0;

/// Datagrams the pacer may inject back to back.
///
/// The burst is bounded from both sides. Too small with a coarse timer, and
/// frames waited on token refills one "1 ms" sleep at a time - a 15.6 ms tick
/// before the host raised the timer resolution (6.7 ms p50 / 18.9 ms p95 to
/// send a frame). Too large, and the frame lands faster than the browser's
/// incoming-datagram queue drains: a whole-frame-interval burst (300 at the
/// worker pace) cost a Mac Chrome session 1-4 % of its datagrams on some
/// seconds with zero network loss (2026-09-24). With a 1 ms timer, 5 ms of
/// pace (90 datagrams at 18000/s) spreads a large frame over a few ms and
/// costs a typical one nothing.
///
/// 32 for worker clients (2026-09-24): 90 still overran Chrome's internal
/// datagram queue on a Mac at 1440p120 - 3-10 % of datagrams gone on some
/// seconds with zero network loss. 32 at 18000/s spreads an ~80-fragment
/// frame over ~3 ms. In-page clients keep the 64 they were measured safe at;
/// their pace is a fifth of this and their frames arrive spread anyway.
pub(crate) fn pace_burst(pace_pps: f32) -> f32 {
    if pace_pps > crate::media::wt::WT_PACE_PPS {
        32.0
    } else {
        64.0
    }
}

const FRAME_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(120);
/// Forced IDRs ride the reliable stream in v4 mode; a 1440p IDR is ~700 KB
/// and lands on a client channel that may still be doing its handshake RTT.
const KEY_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Freshness budgets (docs/research/performance-latency-2026-09-09.md, P0):
/// a frame that has not left the host within its budget is worthless - it
/// would playout behind the frames captured after it - so every queue between
/// the encoder and the wire rejects expired work instead of delivering it.
/// 150 ms on the paced v4 carrier (was 35 ms): at ~98% pace utilization a
/// recovery burst backs the frame queue up and deltas blow the budget by
/// tens of ms — expiring them punched permanent holes in the frame_no
/// sequence, the client cleared its reorder window and re-keyed, and the
/// re-key IDR restarted the whole loop (05:38 session: expired=7 sent=53
/// between re-keys). A late-but-complete delta decodes one frame late; a
/// hole costs a 2.5 s freeze. IDRs no longer share the datagram budget
/// (they ride the reliable stream), so the burst source is gone — this is
/// the safety net for the queue behind a scene-change burst.
const DELTA_FRESHNESS: std::time::Duration = std::time::Duration::from_millis(150);
/// 500 ms: on the paced v4 carrier a large IDR drains at the client's
/// measured datagram rate (~850 fragments at ~4k/s ≈ 210 ms), and the IDR is
/// the anchor — late is fine, unassemblable is not. (Keyframes now ride the
/// reliable stream in v4 mode; the budget still bounds queue-side staleness.)
const KEY_FRESHNESS: std::time::Duration = std::time::Duration::from_millis(500);

/// Biggest keyframe worth copying onto the unreliable datagram carrier.
///
/// ~12 fragments. The copy exists so a client with no anchor is not dependent
/// on the one carrier that can fail invisibly, but a burst past this size loses
/// fragments on a Wi-Fi hop far more often than it delivers (measured: 15 %
/// recovery for 30-100 KB IDRs against 100 % under 8 KB), and the burst itself
/// crowds out the deltas the client can still decode.
const KEY_DATAGRAM_MAX_BYTES: usize = 12_000;

/// Encoded-frame handoff with latest-value semantics.
///
/// The old bounded mpsc rejected the NEWEST frame when full - exactly the
/// wrong victim: live video wants the freshest frame, not the four oldest.
/// A full queue now evicts the oldest non-key frame to admit the new one (a
/// keyframe evicts the plain oldest; it is the one frame that can restart a
/// broken reference chain, so it always gets a slot). Popping also enforces
/// the freshness budget: expired frames are counted, never sent.
struct FrameQueue {
    q: parking_lot::Mutex<std::collections::VecDeque<OutboundFrame>>,
    cap: usize,
    notify: tokio::sync::Notify,
    /// Frames silently discarded to admit newer ones. Every eviction is a
    /// frame_no hole the client must re-key out of (06:28 session: gaps
    /// every ~12 s with no other loss signature - the queue was the only
    /// thing that ever dropped a frame).
    evicted: std::sync::atomic::AtomicU64,
}

impl FrameQueue {
    fn new(cap: usize) -> Self {
        Self {
            q: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            cap: cap.max(1),
            notify: tokio::sync::Notify::new(),
            evicted: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn push(&self, frame: OutboundFrame) {
        let mut q = self.q.lock();
        if q.len() >= self.cap {
            // Evict the oldest non-key frame; if only keyframes remain, the
            // plain oldest goes. Never evict a keyframe in favor of a delta.
            match q.iter().position(|f| !f.key) {
                Some(pos) => {
                    q.remove(pos);
                    self.evicted
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                None => {
                    q.pop_front();
                    self.evicted
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        q.push_back(frame);
        drop(q);
        self.notify.notify_one();
    }

    async fn pop(&self) -> OutboundFrame {
        loop {
            if let Some(f) = self.q.lock().pop_front() {
                return f;
            }
            self.notify.notified().await;
        }
    }

    /// Frames waiting to be sent.
    fn len(&self) -> usize {
        self.q.lock().len()
    }

    /// Cumulative frames discarded to admit newer ones (never sent).
    fn evicted_total(&self) -> u64 {
        self.evicted.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Datagrams this process handed to the socket, cumulative.
///
/// Compared against the client's `datagrams_seen` once a second this is the
/// ONLY sender-side view of what the transport destroyed. QUIC datagrams are
/// unreliable and unacknowledged, and the browser's incoming queue has no flow
/// control - it drops from the HEAD when the app reads slower than the host
/// injects, with the QUIC ACK already sent, so the host sees 0% loss while the
/// oldest frame's fragments are gone. Nothing else on the sender can see it:
/// `send_datagram` returns Ok, the stream carrier is fine, and the client's
/// own loss counter is structurally zero.
static DATAGRAMS_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn datagrams_sent() -> u64 {
    DATAGRAMS_SENT.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn try_send_datagram(conn: &wtransport::Connection, data: &[u8]) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match conn.send_datagram(data) {
            Ok(()) => {
                DATAGRAMS_SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Err(wtransport::error::SendDatagramError::TooLarge) => {
                warn!("wt: datagram exceeds peer limit - fragmenter budget mis-set");
                true
            }
            Err(_) => false,
        }
    }))
    .unwrap_or(false)
}

pub struct WtVideoTransport {
    frame_queue: Arc<FrameQueue>,
    events_rx: Mutex<mpsc::UnboundedReceiver<WtClientEvent>>,
    audio_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    audio_seq: parking_lot::Mutex<u32>,
    audio_seen: std::sync::atomic::AtomicBool,
    tokens: Arc<TokenStore>,
    control_out: ControlOutSlot,
    port: u16,
    cert_sha256: [u8; 32],
    /// When the pinned certificate stops being accepted by browsers. Surfaced by
    /// [`Self::cert_remaining`]; the rotation task in `app.rs` acts on it.
    cert_expires_at: std::time::Instant,
    /// Cleared by the frame sender when it exits. See [`Self::sender_alive`].
    sender_alive: Arc<std::sync::atomic::AtomicBool>,
    shutdown_tx: watch::Sender<bool>,
    endpoint: Arc<wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>>,
    video_config: Arc<Mutex<Option<WtHostMessage>>>,
    /// Shared session state.
    shared: Arc<Shared>,
    /// Frame numbering, monotonic for the life of the transport.
    ///
    /// It used to live in the GStreamer tap, so it restarted at 0 with every
    /// pipeline - and the previous pipeline's in-flight frames then arrived on
    /// the new session numbered in the hundreds, ahead of the new keyframe.
    /// The client cannot tell those apart from out-of-order delivery by number
    /// alone. Numbering per transport makes a stale frame simply an old frame,
    /// which every layer already handles.
    next_frame_no: std::sync::atomic::AtomicU32,
}

impl WtVideoTransport {
    /// Bind the UDP endpoint and start the accept loop. Must be called from a
    /// tokio runtime context (the host's main runtime).
    pub fn bind(cfg: WtTransportConfig) -> anyhow::Result<Self> {
        let frame_queue = Arc::new(FrameQueue::new(cfg.max_queued_frames.max(1)));
        // Audio datagrams (§11 on WT, ADR-0011 WT-only): Opus packets from
        // the GStreamer streaming thread funnel through the SAME single-owner
        // sender task as video — never send_datagram from the audio thread.
        let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut audio_rx = audio_rx;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (live_connection, live_connection_rx) = watch::channel(None::<wtransport::Connection>);
        // False once the frame sender exits for any reason (panic included).
        // The rotation task in `app.rs` treats it as a reason to rebuild the
        // transport, because a dead sender otherwise looks exactly like a
        // healthy transport that nobody is watching.
        let sender_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let identity = cfg.identity;
        let leaf = identity
            .certificate_chain()
            .as_slice()
            .first()
            .context("identity has no leaf certificate")?
            .der()
            .to_vec();
        use sha2::Digest as _;
        let cert_sha256: [u8; 32] = sha2::Sha256::digest(&leaf).into();

        // When the browser will stop accepting this certificate. The dial is
        // authenticated by this pin, so an expired one is refused at the QUIC
        // handshake no matter how often the client redials - see the lifetime
        // constants in `media/wt/mod.rs` and the rotation task in `app.rs`.
        let cert_expires_at = {
            let now = wtransport::tls::self_signed::time::OffsetDateTime::now_utc();
            let left = (cfg.cert_not_after - now).whole_seconds().max(0) as u64;
            std::time::Instant::now() + Duration::from_secs(left)
        };

        let mut quic_cfg = wtransport::config::QuicTransportConfig::default();
        // Headroom against the resend storm: quinn's datagram-queue overflow
        // path has an accounting bug that panics the connection on its own
        // invariant ("datagrams.outgoing.payload_bytes desynchronized" - nine
        // occurrences live under heavy loss, each wedging the endpoint until
        // a manual restart). 4 MB keeps the paced sender plus capped resends
        // inside the buffer even at 50 % loss; the capped resends below keep
        // us out of the overflow path entirely.
        // 256 KB ~= 350 ms of video at 6 Mbps. This buffer is a playout trap
        // at any larger size: when the route dips, the queue fills with
        // seconds of video that are already past the client's playout
        // deadline by the time they drain. The client then receives plenty
        // of bytes and decodes NOTHING (2026-09-08 phone sessions: "works
        // for a second, then dies forever", recv 6 Mbps / decoded 0). A
        // small buffer makes the SENDER drop the fragments it cannot send
        // in time - parity, NACK and the next IDR recover those - while
        // fresh data keeps flowing. The quinn accounting panic this used to
        // paper over is fixed in vendor/quinn-proto; the big buffer is no
        // longer doing any work here.
        quic_cfg.datagram_send_buffer_size(256 * 1024);
        super::congestion::apply(&mut quic_cfg, cfg.congestion);
        info!(congestion = ?cfg.congestion, "wt: QUIC congestion control");
        // The play URL can resolve to IPv4 on a home LAN. V6 explicitly disables
        // IPv4 in wtransport; dual-stack keeps both LAN and IPv6 clients reachable.
        let server_config = wtransport::ServerConfig::builder()
            .with_bind_config(wtransport::config::IpBindConfig::InAddrAnyDual, cfg.port)
            .with_custom_transport(identity, quic_cfg)
            // No server-side keep-alive on purpose: keep-alives would hold a
            // vanished client's connection open forever (the video slot then
            // stays busy — seen live). The client proves liveness itself: an
            // app-level ping every 2 s plus ~60 datagrams/s. Dead clients are
            // reaped by the idle timeout.
            .max_idle_timeout(Some(Duration::from_secs(30)))
            .expect("30s is a valid idle timeout")
            .build();
        let endpoint = Arc::new(
            wtransport::Endpoint::server(server_config).context("binding WebTransport endpoint")?,
        );
        let port = endpoint.local_addr()?.port();

        let tokens = Arc::new(TokenStore::new());
        let control_out: ControlOutSlot = Arc::new(Mutex::new(None));
        let video_config = Arc::new(Mutex::new(None));
        let frame_anchor: Arc<Mutex<Option<(std::time::Instant, u64)>>> =
            Arc::new(Mutex::new(None));
        let shared = Arc::new(Shared {
            last_keyreq_log: parking_lot::Mutex::new(
                std::time::Instant::now() - std::time::Duration::from_secs(10),
            ),
            tokens: tokens.clone(),
            events: events_tx,
            #[cfg(windows)]
            audio: audio_tx.clone(),
            active: Mutex::new(None),
            next_session_id: std::sync::atomic::AtomicU64::new(1),
            audio_opus_supported: std::sync::atomic::AtomicI8::new(-1),
            live_connection,
            frame_anchor: frame_anchor.clone(),
            capture_clock: Mutex::new(None),
            video_config: video_config.clone(),
            video_sink: Mutex::new(None),
            write_stall_ms: std::sync::atomic::AtomicU32::new(0),
            wt_timeline: Mutex::new(std::collections::VecDeque::new()),
            datagram_video: std::sync::atomic::AtomicBool::new(false),
            key_by_datagram: std::sync::atomic::AtomicBool::new(false),
            wt_resend: Mutex::new(std::collections::VecDeque::new()),
            wt_resend_tokens: Mutex::new((std::time::Instant::now(), 0)),
            pace_pps: std::sync::atomic::AtomicU32::new(crate::media::wt::WT_PACE_PPS as u32),
            wt_nack_dropped: std::sync::atomic::AtomicU64::new(0),
            wt_nack_missed: std::sync::atomic::AtomicU64::new(0),
            // u32::MAX = "no window measured yet", so a measured zero is
            // distinguishable from no evidence at all.
            wt_inferred_loss_ppt: std::sync::atomic::AtomicU32::new(u32::MAX),
            pace_max_pps: std::sync::atomic::AtomicU32::new(
                crate::media::wt::WORKER_PACE_PPS as u32,
            ),
            pace_lossy_windows: std::sync::atomic::AtomicU32::new(0),
        });

        // Accept loop: one task per incoming session; auth gates everything.
        {
            let accept_endpoint = endpoint.clone();
            let accept_shared = shared.clone();
            let accept_control_out = control_out.clone();
            let loop_shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                loop {
                    let mut shutdown = loop_shutdown.clone();
                    let incoming = tokio::select! {
                        _ = shutdown.changed() => break,
                        incoming = accept_endpoint.accept() => incoming,
                    };
                    let shared = accept_shared.clone();
                    let control_out = accept_control_out.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_incoming(incoming, shared, control_out).await {
                            debug!("wt session ended: {e:#}");
                        }
                    });
                }
            });
        }

        // Video sender: the single consumer of the pipeline's frame queue. It
        // follows whichever connection is currently live via the watch -
        // tokio receivers are not Clone, so the queue must have exactly one.
        //
        // Frames go back-to-back onto ONE stream with v3 self-delimiting
        // headers - but the stream is NOT opened here. iOS WebKit does not
        // reliably deliver server-initiated streams to JavaScript (three
        // failure shapes on 2026-09-09: v2 never-FIN credit exhaustion, v3
        // per-frame early EOFs, v3 persistent stream silent after one frame)
        // while every client-opened channel carried data flawlessly. So the
        // client opens and marks the "video channel" (session.rs accept loop
        // installs its send half into `Shared::video_sink`), and this loop
        // writes frames there. No sink yet, or it died: drop frames until the
        // client opens another one.
        //
        // A write that cannot land inside the frame budget resets the stream
        // and drops the frame: the leaky bucket, now covering stream health
        // too. The client's read side sees the reset and opens a fresh
        // channel.
        {
            let mut shutdown = shutdown_rx.clone();
            let shared_for_sender = shared.clone();
            let frame_queue_sender = frame_queue.clone();
            let sender_alive_task = sender_alive.clone();
            tokio::spawn(async move {
                // Cleared on ANY exit from this task, including a panic: the
                // guard's Drop runs while the task unwinds. See `sender_alive`.
                struct SenderGuard(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for SenderGuard {
                    fn drop(&mut self) {
                        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
                        error!("wt: VIDEO SENDER TASK EXITED - no frame can be sent on this transport again");
                    }
                }
                let _alive = SenderGuard(sender_alive_task);
                let mut live_rx = live_connection_rx;
                let mut live: Option<wtransport::Connection> = None;
                // The sink is injected (client-opened), never opened here.
                let mut stream: Option<wtransport::stream::SendStream> = None;
                let (mut sent, mut stalled, mut expired) = (0u32, 0u32, 0u32);
                // Queue evictions are cumulative; diff them per window.
                let mut evicted_last = frame_queue_sender.evicted_total();
                // Paced-injection bucket (05:08 Chrome session): datagram
                // fragments go out at no more than ~0.8× the client's measured
                // drain rate. The browser's incoming-datagram queue has NO flow
                // control (RFC 9221) and silently drops from the HEAD when the
                // app reads slower than the host injects — with the QUIC ACK
                // already sent, the host sees 0% loss while every fragment of
                // the oldest frame is destroyed. 64 tokens of burst lets small
                // frames through unthrottled; big IDRs drain over a fraction of
                // KEY_FRESHNESS.
                let mut pace_tokens: f32 = 64.0;
                // Frame interval from capture timestamps (EMA), for the
                // per-frame pace; starts at 60 fps.
                let mut frame_interval_s: f32 = 1.0 / 60.0;
                let mut last_capture_us: Option<u64> = None;
                let mut pace_at = std::time::Instant::now();
                let mut stall_window_ms = 0u32;
                let mut key_fresh = 0u32;
                let mut key_cached = 0u32;
                let mut last_report = std::time::Instant::now();
                let mut path_last = (0u64, 0u64, 0u64);
                loop {
                    if last_report.elapsed() >= std::time::Duration::from_secs(1) {
                        // Queue evictions are cumulative; diff them per window.
                        let evicted_total = frame_queue_sender.evicted_total();
                        let evicted = evicted_total.saturating_sub(evicted_last) as u32;
                        evicted_last = evicted_total;
                        // Which carrier each forced IDR took. The IDR is the
                        // only thing that ends a client's stall, and a
                        // "cached stream" delivery is the one that cannot be
                        // told apart from success - the write goes into the
                        // void without error when the client stopped reading
                        // that channel. 306 of 337 IDRs went that way before
                        // the client learned to open a spare channel first, so
                        // this pair is the number to watch.
                        if key_cached > 0 || key_fresh > 0 {
                            info!(key_fresh, key_cached, "wt: forced IDR carrier");
                            key_cached = 0;
                            key_fresh = 0;
                        }
                        // Repair accounting. `swap(0)` makes these per-window.
                        // A refused request or an aged-out fragment used to be
                        // indistinguishable from a repair that landed, which is
                        // why a session could report nacks climbing while the
                        // picture stayed frozen and nothing on either side said
                        // why.
                        let nack_dropped = shared_for_sender
                            .wt_nack_dropped
                            .swap(0, std::sync::atomic::Ordering::Relaxed);
                        let nack_missed = shared_for_sender
                            .wt_nack_missed
                            .swap(0, std::sync::atomic::Ordering::Relaxed);
                        if nack_dropped > 0 || nack_missed > 0 {
                            warn!(
                                nack_dropped,
                                nack_missed,
                                "wt: repair requests refused by the budget or aged out of the cache - these frames recover only by IDR"
                            );
                        }
                        if stalled > 0 || expired > 0 || evicted > 0 {
                            warn!(
                                sent,
                                stalled,
                                expired,
                                evicted,
                                "wt: frames dropped - client not reading, past freshness, or queue eviction"
                            );
                        } else if sent > 0 {
                            debug!(sent, "wt: frame send rate");
                        } else if live.is_some() {
                            // A live connection that is sent nothing while the
                            // pipeline is supposed to be encoding is the
                            // silent-frameless-session signature; name it.
                            debug!("wt: sender idle - no frames queued this second");
                        }
                        sent = 0;
                        stalled = 0;
                        expired = 0;
                        shared_for_sender.write_stall_ms.store(
                            std::mem::take(&mut stall_window_ms),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        // QUIC's own view of the path: the window it allows,
                        // what it declared lost from ACKs, and how often it
                        // reacted. Without this a transport-side throttle
                        // (a loss-based window collapsing on Wi-Fi erasure)
                        // is indistinguishable from the network dropping
                        // datagrams - both only show up as the client's
                        // missing receive count.
                        if let Some(conn) = live.as_ref() {
                            let p = conn.quic_connection().stats().path;
                            let (lost0, sent0, cong0) = path_last;
                            debug!(
                                cwnd = p.cwnd,
                                rtt_ms = p.rtt.as_secs_f32() * 1000.0,
                                sent_pkts = p.sent_packets.saturating_sub(sent0),
                                lost_pkts = p.lost_packets.saturating_sub(lost0),
                                congestion_events = p.congestion_events.saturating_sub(cong0),
                                mtu = p.current_mtu,
                                "wt: quic path"
                            );
                            path_last = (p.lost_packets, p.sent_packets, p.congestion_events);
                        }
                        last_report = std::time::Instant::now();
                    }
                    let frame = tokio::select! {
                        _ = shutdown.changed() => break,
                        // Audio first: 10 ms Opus frames are latency-bound, and
                        // they stay on datagrams - each one fits, and a late
                        // audio packet is worth less than a fresh one.
                        a = audio_rx.recv() => {
                            match a {
                                Some(d) => {
                                    if let Some(conn) = live.clone() {
                                        try_send_datagram(&conn, &d);
                                    }
                                }
                                None => break, // audio channel dropped
                            }
                            continue;
                        }
                        _ = live_rx.changed() => {
                            live = live_rx.borrow().clone();
                            // A new connection gets a new stream; the old
                            // one belonged to a connection that is going away.
                            stream = None;
                            path_last = live
                                .as_ref()
                                .map(|c| {
                                    let p = c.quic_connection().stats().path;
                                    (p.lost_packets, p.sent_packets, p.congestion_events)
                                })
                                .unwrap_or_default();
                            continue;
                        }
                        frame = frame_queue_sender.pop(), if live.is_some() => frame,
                    };
                    // Freshness budget: a frame the encoder finished more than
                    // its budget ago would playout behind newer frames that
                    // are already ahead of it - drop it, don't deliver it.
                    let budget = if frame.key {
                        KEY_FRESHNESS
                    } else {
                        DELTA_FRESHNESS
                    };
                    if frame.captured_at.elapsed() > budget {
                        expired = expired.saturating_add(1);
                        continue;
                    }
                    let pop_us = crate::media::frametrace::now_us();
                    if let Some(prev) = last_capture_us {
                        let d = frame.capture_us.saturating_sub(prev) as f32 / 1e6;
                        if d > 0.002 && d < 0.1 {
                            frame_interval_s = 0.9 * frame_interval_s + 0.1 * d;
                        }
                    }
                    last_capture_us = Some(frame.capture_us);
                    // Refresh the send-time anchor: Pong replies project the
                    // capture clock from the most recently sent frame, so no
                    // constant PTS-epoch offset can leak into client ages.
                    *shared_for_sender.frame_anchor.lock() =
                        Some((std::time::Instant::now(), frame.capture_us));
                    // ---- v4 carrier: deadline-aware datagram fragments ----
                    // Every fragment is one datagram: loss costs one fragment,
                    // never the frames behind it. A fragment the QUIC
                    // implementation cannot queue (congestion) is dropped,
                    // never buffered - the frame misses its deadline whole or
                    // not at all. The reliable stream stays installed as the
                    // fallback carrier; the client toggles this switch off the
                    // same way it toggled it on (research doc Safari matrix).
                    // Deltas only: forced IDRs ride the client-opened
                    // reliable stream below (flow-controlled, cannot drop -
                    // a 700 KB IDR at pace would occupy the whole 3800 pps
                    // budget for ~210 ms, blow DELTA_FRESHNESS on every delta
                    // queued behind it, and expire them into permanent
                    // frame_no holes → re-key → repeat; 05:38 session).
                    // A keyframe the client explicitly asked for (it has no
                    // anchor) ALSO rides the datagram carrier, while the stream
                    // copy below still goes out. The client is decoding nothing
                    // at that moment, so a paced IDR spending ~200 ms of the
                    // datagram budget costs it nothing - and a starved client
                    // must not depend on the one carrier that can fail
                    // invisibly. Set once per client request (see
                    // `Shared::key_by_datagram`), so a healthy session never
                    // pays for it.
                    // ...but only while the IDR is small enough to survive as a
                    // burst. Measured over 373 repair IDRs in one log: those
                    // under 8 KB recovered the client 100 % of the time, 8-30 KB
                    // 39 %, 30-100 KB 15 %. Beyond a dozen fragments the copy is
                    // not a fallback, it is a burst that lands on a lossy Wi-Fi
                    // hop and is dropped whole - so a big IDR goes on the
                    // reliable stream alone (the client now opens a fresh
                    // channel for exactly this) and the datagram budget is left
                    // to the deltas that are still arriving at 60 fps.
                    let key_repair = frame.key
                        && frame.payload.len() <= KEY_DATAGRAM_MAX_BYTES
                        && shared_for_sender
                            .key_by_datagram
                            .swap(false, std::sync::atomic::Ordering::Relaxed);
                    if frame.key && !key_repair {
                        // Either nobody asked for a datagram copy, or this one is
                        // too big to be worth sending. Consume the flag either
                        // way so it cannot leak into a later keyframe.
                        shared_for_sender
                            .key_by_datagram
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        if frame.payload.len() > KEY_DATAGRAM_MAX_BYTES {
                            debug!(
                                frame_no = frame.frame_no,
                                bytes = frame.payload.len(),
                                "wt: repair keyframe too large for the datagram carrier - stream only"
                            );
                        }
                    }
                    if shared_for_sender
                        .datagram_video
                        .load(std::sync::atomic::Ordering::Relaxed)
                        && (!frame.key || key_repair)
                    {
                        let Some(conn) = live.as_ref() else { continue };
                        // `max_datagram_size` is the limit for what we hand to
                        // `send_datagram`, and quinn recomputes it as the path
                        // MTU estimate moves - so a fragment sized against the
                        // old value can come back `TooLarge` mid-frame, and the
                        // NACK re-send replays those same oversized bytes from
                        // the cache and fails identically. A margin costs
                        // nothing (one extra fragment per 1200 bytes) and keeps
                        // a shrinking estimate from stranding a frame. The 373
                        // `datagram exceeds peer limit` warnings in one log are
                        // frames that went to the client with a hole in them.
                        let budget = conn
                            .max_datagram_size()
                            .map(|m| {
                                m.saturating_sub(inphase_protocol::WT_FRAGMENT_HEADER_LEN + 64)
                            })
                            .unwrap_or(1082)
                            .max(256);
                        let frag_cnt = (frame.payload.len().div_ceil(budget)).max(1) as u16;
                        // Per-connection pace: the telemetry handler raises
                        // this for worker-drain clients (see WORKER_PACE_PPS).
                        let pace_pps = shared_for_sender
                            .pace_pps
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as f32;
                        let parity_rows =
                            if inphase_protocol::wants_two_parity_rows(frame.key, frag_cnt) {
                                2
                            } else {
                                1
                            };
                        let pace_pps = frame_pace_pps(
                            pace_pps,
                            frag_cnt as u32 + (frag_cnt as u32).div_ceil(8) * parity_rows,
                            frame_interval_s,
                            frame_queue_sender.len(),
                        );
                        // ---- send order: interleave across FEC groups -----
                        // Walking a frame front to back puts a whole FEC group
                        // in consecutive datagrams, and Wi-Fi does not lose
                        // datagrams independently - it loses A-MPDU
                        // aggregates, so a run of 8 consecutive datagrams
                        // takes 3+ fragments out of ONE group. Row-1 parity
                        // repairs exactly one hole per group, so the frame is
                        // FEC-dead and only a NACK re-send can finish it. That
                        // is the shape of the 80 Mbps session: the client
                        // received 18-27 Mbps with the host dropping nothing
                        // (0 stalled, 0 expired, 1 eviction), yet decoded
                        // 1-17 fps while its NACK count climbed by up to 506/s
                        // against a 60/s re-send budget, so most of its repair
                        // requests were discarded and the frames never
                        // assembled.
                        //
                        // Transposing the order (0, G, 2G, ..., 1, G+1, ...)
                        // means any burst of B <= G consecutive datagrams hits
                        // at most ceil(B/G) fragments per group, so row-1
                        // parity rebuilds all of them locally with no round
                        // trip and no re-send traffic. The client reassembles
                        // by frag_idx, and the parity rows are computed over
                        // group membership rather than arrival, so this is a
                        // pure scheduling change on the wire.
                        let mut ok = true;
                        for idx in fragment_send_order(frag_cnt) {
                            let start = idx as usize * budget;
                            let end = ((idx as usize + 1) * budget).min(frame.payload.len());
                            let frag = inphase_protocol::WtFragment {
                                frame_no: frame.frame_no,
                                frag_idx: idx,
                                frag_cnt,
                                capture_us: frame.capture_us,
                                key: frame.key,
                                parity: 0,
                                payload: frame.payload[start..end].to_vec(),
                            };
                            let enc = frag.encode();
                            // Cache the encoded datagram for client NACK
                            // re-sends: the last few hundred fragments cover
                            // the newest frames, so a single lost delta
                            // fragment is repaired in one RTT instead of
                            // triggering the IDR spiral (22:15: held=8,
                            // decoded=0 for 7 s). Re-sending the encoded
                            // form keeps frag_cnt/capture_us/key identical.
                            {
                                let mut cache = shared_for_sender.wt_resend.lock();
                                cache.push_back((frame.frame_no, idx, enc.clone()));
                                // Keep the cache sized in *frames*, not
                                // fragments. 1024 fragments is 0.28 s of video
                                // at 40 Mbps but 2.4 s at 4 Mbps, and the client
                                // cannot ask for a re-send until 150 ms after a
                                // frame starts assembling, plus a round trip - so
                                // at high bitrate the fragment was always gone
                                // before the request arrived. Live 80 Mbps
                                // session: **11,252 NACKs hit an empty cache
                                // against 1,431 refused by the rate budget**,
                                // which is why damaged frames expired at 900 ms,
                                // got abandoned, and pulled 140 recovery IDRs in eleven
                                // minutes. A frame window scales with both the
                                // bitrate and the repair latency.
                                // 150: a second at 120 fps. 60 was 0.5 s
                                // there, shorter than the client's repair
                                // schedule, so re-sends asked for late in
                                // it had already aged out (nack_missed 7-48/s
                                // on a 1440p120 Mac session).
                                const RESEND_FRAMES: u32 = 150;
                                const RESEND_MAX_FRAGMENTS: usize = 16_384;
                                while cache.len() > RESEND_MAX_FRAGMENTS
                                    || cache.front().is_some_and(|(no, _, _)| {
                                        frame.frame_no.saturating_sub(*no) > RESEND_FRAMES
                                    })
                                {
                                    cache.pop_front();
                                }
                            }
                            // Paced injection (see pace_tokens): take a token,
                            // draining audio while we wait so 10 ms Opus
                            // frames never queue behind a paced IDR.
                            while {
                                let elapsed = pace_at.elapsed().as_secs_f32();
                                pace_at = std::time::Instant::now();
                                pace_tokens =
                                    (pace_tokens + elapsed * pace_pps).min(pace_burst(pace_pps));
                                pace_tokens < 1.0
                            } {
                                tokio::select! {
                                    a = audio_rx.recv() => {
                                        if let Some(d) = a {
                                            try_send_datagram(conn, &d);
                                        }
                                    }
                                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                                }
                            }
                            pace_tokens -= 1.0;
                            if !try_send_datagram(conn, &enc) {
                                ok = false;
                                break; // queue full: the rest is past deadline anyway
                            }
                        }
                        if ok {
                            sent = sent.saturating_add(1);
                            // ---- FEC parity (research doc §target transport):
                            // 8+1 XOR groups for every frame, 8+2 for
                            // keyframes. One lost fragment per group is
                            // rebuilt client-side with no NACK and no RTT —
                            // the NACK-only repair amplified congestion at
                            // 9% loss (23:21: nacks 65 -> 10789 in 8 s, the
                            // re-send storm crowded out fresh video).
                            let data_frags: Vec<Vec<u8>> = (0..frag_cnt as usize)
                                .map(|i| {
                                    let s = i * budget;
                                    let e = ((i + 1) * budget).min(frame.payload.len());
                                    frame.payload[s..e].to_vec()
                                })
                                .collect();
                            let two_rows =
                                inphase_protocol::wants_two_parity_rows(frame.key, frag_cnt);
                            let parity = inphase_protocol::fragment_parity(&data_frags, two_rows);
                            for (round, (row, acc)) in parity.into_iter().enumerate() {
                                let group = (round / if two_rows { 2 } else { 1 }) as u16;
                                let pf = inphase_protocol::WtFragment {
                                    frame_no: frame.frame_no,
                                    frag_idx: group,
                                    frag_cnt,
                                    capture_us: frame.capture_us,
                                    key: frame.key,
                                    parity: row,
                                    payload: acc,
                                };
                                let enc = pf.encode();
                                while {
                                    let elapsed = pace_at.elapsed().as_secs_f32();
                                    pace_at = std::time::Instant::now();
                                    pace_tokens = (pace_tokens + elapsed * pace_pps)
                                        .min(pace_burst(pace_pps));
                                    pace_tokens < 1.0
                                } {
                                    tokio::select! {
                                        a = audio_rx.recv() => {
                                            if let Some(d) = a {
                                                try_send_datagram(conn, &d);
                                            }
                                        }
                                        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                                    }
                                }
                                pace_tokens -= 1.0;
                                if !try_send_datagram(conn, &enc) {
                                    break; // parity is best-effort; NACK remains
                                }
                            }
                        } else {
                            stalled = stalled.saturating_add(1);
                            // Datagram queue-full IS the congestion signal on
                            // the v4 carrier - there is no writer stall to
                            // measure, and without this the controller sees a
                            // zero backlog all the way up until fragment loss
                            // makes frames unassemblable (the 21:51 no-decode
                            // episode: in_kbps ramped 6→13.5 Mbps before the
                            // first cut). Publish a synthetic stall so the
                            // 1 s AIMD sees real backpressure.
                            stall_window_ms = stall_window_ms.max(300);
                        }
                        shared_for_sender.wt_timeline.lock().push_back([
                            frame.frame_no as u64,
                            frame.capture_host_us,
                            frame.enq_us,
                            pop_us,
                            if ok {
                                crate::media::frametrace::now_us()
                            } else {
                                0
                            },
                        ]);
                        // A delta is datagram-only. A repair keyframe falls
                        // through so the stream carries it too: whichever copy
                        // arrives intact anchors the client.
                        if !frame.key {
                            continue;
                        }
                        info!(
                            frame_no = frame.frame_no,
                            bytes = frame.payload.len(),
                            "wt: repair keyframe sent as datagram fragments too (client has no anchor)"
                        );
                    }
                    let wire = inphase_protocol::WtFrame {
                        frame_no: frame.frame_no,
                        capture_us: frame.capture_us,
                        key: frame.key,
                        payload: frame.payload,
                    }
                    .encode();
                    // Take the newest client-opened video channel, if any.
                    // Missing sink = drop the frame; the client (re)opens one
                    // on its own schedule and the next frame picks it up.
                    // KEYFRAMES always prefer a FRESH sink: a cached stream
                    // can be one the client already stopped reading (it
                    // cancels its reader on wedges/carrier changes), and a
                    // write into a cancelled stream can succeed silently into
                    // the void - the 06:13 session lost every forced IDR into
                    // a dead channel and decoded 0 fps for 30 s. Deltas on
                    // the v3 carrier keep the cached stream: they need
                    // write-ordering continuity, and an empty slot means the
                    // cached stream was the newest one installed.
                    if frame.key {
                        match shared_for_sender.video_sink.lock().take() {
                            Some(s) => {
                                stream = Some(s);
                                key_fresh = key_fresh.saturating_add(1);
                                debug!(
                                    frame_no = frame.frame_no,
                                    bytes = wire.len(),
                                    "wt: keyframe sent on a freshly installed channel"
                                );
                            }
                            None => {
                                key_cached = key_cached.saturating_add(1);
                                debug!(
                                    frame_no = frame.frame_no,
                                    bytes = wire.len(),
                                    "wt: keyframe has no fresh channel - using the cached stream"
                                );
                            }
                        }
                    } else if stream.is_none() {
                        match shared_for_sender.video_sink.lock().take() {
                            Some(s) => stream = Some(s),
                            None => {
                                stalled = stalled.saturating_add(1);
                                continue;
                            }
                        }
                    }
                    // No sink AND no cached stream: the client has not opened
                    // its video channel yet, or has just cancelled it. Drop the
                    // frame; it asks for another keyframe once a channel is up
                    // (on connect, and again from its never-decoded ladder).
                    //
                    // This was an `expect("stream just ensured")`, on the
                    // assumption that one of the two branches above must have
                    // produced a stream. It cannot hold: the accept loop logs
                    // "video channel installed" and only THEN stores the sink
                    // (session.rs), so an IDR already in the queue - the client
                    // requests one the moment it connects - lands in that window
                    // with both empty. Live 2026-09-16 and 2026-09-19, at
                    // session start, 15 us after the install line: the panic
                    // killed this task. A dead sender is silent and permanent -
                    // the endpoint keeps accepting dials and completes the
                    // handshake, so every session after it is a black glass
                    // until the process is restarted, which is the reported
                    // "works for days, then black until I quit and restart".
                    let Some(s) = stream.as_mut() else {
                        stalled = stalled.saturating_add(1);
                        // A keyframe with nowhere to go is the worst case in
                        // this file: the client decodes nothing until an IDR
                        // arrives, it asks for one once a second, and this is
                        // the only record that the answer never left the host.
                        if frame.key {
                            warn!(
                                frame_no = frame.frame_no,
                                "wt: KEYFRAME DROPPED - no client video channel and no cached stream; \
                                 the client is waiting on this frame"
                            );
                        }
                        continue;
                    };
                    sent = sent.saturating_add(1);
                    // Frames are back-to-back on the stream; the v3 header is
                    // the boundary the client reads. A write that cannot land
                    // inside the frame budget means the peer stopped reading
                    // (flow control backs pressure up to here): reset the
                    // stream - it is mid-frame and unusable - and drop the
                    // frame. Fewer bytes in flight is what lets a client
                    // behind catch up.
                    let wstart = std::time::Instant::now();
                    // Keyframes get 500 ms (KEY_STREAM_TIMEOUT): a 1440p IDR
                    // is ~700 KB and the client may just have opened the
                    // channel it rides. Deltas keep the 120 ms budget.
                    let wtimeout = if frame.key {
                        KEY_STREAM_TIMEOUT
                    } else {
                        FRAME_STREAM_TIMEOUT
                    };
                    // Timeline record: write_us = 0 marks a frame the sender
                    // reset mid-write (timeout / transport error).
                    let mut tl = [
                        frame.frame_no as u64,
                        frame.capture_host_us,
                        frame.enq_us,
                        pop_us,
                        0u64,
                    ];
                    match tokio::time::timeout(wtimeout, s.write_all(&wire)).await {
                        Ok(Ok(())) => {
                            let wms = wstart.elapsed().as_millis() as u32;
                            stall_window_ms = stall_window_ms.max(wms);
                            tl[4] = crate::media::frametrace::now_us();
                        }
                        Ok(Err(e)) => {
                            debug!("wt: frame stream failed: {e}");
                            stream = None;
                        }
                        Err(_) => {
                            // Reset, never drop: a stream abandoned mid-frame
                            // keeps its buffered bytes queued against the
                            // connection, so the backlog we are shedding
                            // persists; and a half-written frame would
                            // desynchronize every frame after it.
                            let _ = s.reset(0u32.into());
                            stream = None;
                            stalled = stalled.saturating_add(1);
                            debug!("wt: frame write timed out - stream reset");
                        }
                    }
                    shared_for_sender.wt_timeline.lock().push_back(tl);
                }
            });
        }

        Ok(Self {
            frame_queue,
            events_rx: Mutex::new(events_rx),
            audio_tx,
            audio_seq: parking_lot::Mutex::new(0),
            audio_seen: std::sync::atomic::AtomicBool::new(false),
            tokens,
            control_out,
            port,
            cert_sha256,
            cert_expires_at,
            sender_alive,
            shutdown_tx,
            video_config,
            endpoint,
            shared: shared.clone(),
            next_frame_no: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// UDP port actually bound (useful when the config said 0).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// SHA-256 of the WT certificate — the client pins this via
    /// `serverCertificateHashes`, delivered over the authenticated signaling
    /// WebSocket.
    pub fn cert_sha256(&self) -> [u8; 32] {
        self.cert_sha256
    }

    /// [`Self::cert_sha256`] as lowercase hex (the signaling wire format).
    pub fn cert_sha256_hex(&self) -> String {
        self.cert_sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// How long the pinned certificate stays valid.
    ///
    /// This is not a hint. Past zero the browser refuses every dial to this
    /// transport — no client error, no fallback, just a black glass until the
    /// host process is restarted. Rotation must keep it well above zero.
    pub fn cert_remaining(&self) -> Duration {
        self.cert_expires_at
            .saturating_duration_since(std::time::Instant::now())
    }

    /// Whether the frame-sender task is still running.
    ///
    /// A panicked sender takes the entire video path with it and leaves no
    /// other trace: the endpoint keeps accepting dials and completes the
    /// handshake, the client's control channel works (so it looks connected),
    /// and no frame is ever written again. That is indistinguishable from a
    /// working transport from outside, so it has to be asked.
    pub fn sender_alive(&self) -> bool {
        self.sender_alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Worst frame-write stall (ms) in the last sender second - the WT path's
    /// send-queue depth, for the congestion controller's `rtp_backlog_ms`
    /// input. See `Shared::write_stall_ms`.
    pub fn send_stall_ms(&self) -> u32 {
        self.shared
            .write_stall_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn issue_token(&self, ttl: Duration) -> String {
        self.tokens.issue(ttl)
    }

    /// Hand one encoded frame to the wire. Returns `false` when the queue is
    /// full or the transport is shutting down — the frame is dropped, which is
    /// the intended backpressure semantics (never block the encoder).
    /// The next frame number for this transport. Callers must use this rather
    /// than their own counter - see `next_frame_no`.
    pub fn next_frame_no(&self) -> u32 {
        self.next_frame_no
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Install (or clear) the running pipeline's clock; see
    /// `Shared::capture_clock`.
    pub fn set_capture_clock(&self, clock: Option<crate::media::wt::CaptureClock>) {
        *self.shared.capture_clock.lock() = clock;
    }

    pub fn send_frame(&self, frame: OutboundFrame) -> bool {
        // Latest-value semantics (docs/research/performance-latency-2026-09-09.md):
        // a full queue evicts the oldest non-key frame to admit the newest -
        // live video wants the freshest frame, never the oldest. It
        // deliberately does NOT force a keyframe. That repair looked correct
        // - an infinite GOP means a dropped P-frame breaks the reference chain
        // until the next IDR - but drops are not isolated events. When the
        // client stops reading, *every* frame drops, so every second produced a
        // forced IDR, each one the largest thing on the wire, which filled the
        // queue harder and guaranteed the next drop. A self-sustaining keyframe
        // storm that froze the stream within seconds (2026-09-08).
        //
        // The client already asks for a keyframe when it is actually stuck, and
        // that request is throttled and reflects the decoder's real state
        // rather than the sender's. One repair signal, from the end that knows.
        let mut frame = frame;
        let enq_us = crate::media::frametrace::now_us();
        frame.enq_us = enq_us;
        frame.capture_host_us =
            enq_us.saturating_sub(frame.captured_at.elapsed().as_micros() as u64);
        self.frame_queue.push(frame);
        true
    }

    /// Snapshot of the per-frame host timeline, oldest first, for the admin
    /// `frame-timeline` endpoint. Capped at 4096 records (≈1 min at 60 fps).
    pub fn timeline_snapshot(&self) -> Vec<[u64; 5]> {
        let mut tl = self.shared.wt_timeline.lock();
        while tl.len() > 4096 {
            tl.pop_front();
        }
        tl.iter().copied().collect()
    }

    /// Whether the v4 datagram carrier is currently selected (the client
    /// toggles it; surfaced for the dashboard and tests).
    pub fn datagram_video_enabled(&self) -> bool {
        self.shared
            .datagram_video
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the client-opened video channel sink is installed (tests).
    pub fn video_sink_is_empty(&self) -> bool {
        self.shared.video_sink.lock().is_none()
    }

    /// Current v4 injection pace for this client (the AIMD ceiling derives
    /// from it — worker-drain clients pace and climb higher).
    pub fn pace_pps(&self) -> u32 {
        self.shared
            .pace_pps
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Fraction of the datagrams the host injected that the client never saw,
    /// or `None` before a window has been measured.
    ///
    /// The controller's own loss input is structurally zero on this carrier:
    /// QUIC datagrams are unacknowledged, so a datagram the browser's queue
    /// dropped on the floor leaves no trace anywhere - the send returns Ok.
    /// This is the difference between the host's cumulative send count and the
    /// client's cumulative receive count over one telemetry interval, which is
    /// the only loss signal that exists on the sender side.
    pub fn inferred_loss_frac(&self) -> Option<f32> {
        match self
            .shared
            .wt_inferred_loss_ppt
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            u32::MAX => None,
            ppt => Some(ppt as f32 / 1000.0),
        }
    }

    /// Queue one Opus packet as a WT audio datagram (§11-on-WT). Header:
    /// tag `0x41` + seq u32 + capture pts u64 — the client's first byte
    /// distinguishes audio from a video fragment (video frame numbers keep
    /// the high byte zero for any session under 2^56 frames).
    pub fn send_audio_datagram(&self, payload: &[u8], pts_us: u64) {
        let first = !self
            .audio_seen
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if first {
            tracing::info!(
                bytes = payload.len(),
                "wt: first audio datagram sent - the Opus-over-WT audio path is live"
            );
        }
        let mut seq = self.audio_seq.lock();
        let mut buf = Vec::with_capacity(13 + payload.len());
        buf.push(WT_AUDIO_TAG);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&pts_us.to_be_bytes());
        buf.extend_from_slice(payload);
        *seq = seq.wrapping_add(1);
        let _ = self.audio_tx.send(buf);
    }

    /// Push a host → client control message (e.g. `VideoConfig`). Dropped when
    /// no authenticated session has opened its control stream yet.
    pub fn push_control(&self, msg: WtHostMessage) {
        // The per-session sender is installed by the accept path on auth; a
        // send through the live slot is what MediaSession means here. With no
        // live session this is a silent no-op.
        if let Some(sender) = self.control_out.lock().clone() {
            let _ = sender.send(msg);
        }
    }

    /// Store the media session's negotiated video config (ADR-0011). Sent to
    /// the client as the *first* control message after auth — the WebCodecs
    /// decoder must be configured before any frame can decode. Replaced on
    /// every media renegotiation; `description_b64` stays `None` because the
    /// tap carries GStreamer's Annex-B byte stream.
    fn next_config_epoch(&self) -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static EPOCH: AtomicU32 = AtomicU32::new(0);
        EPOCH.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn set_video_config(
        &self,
        codec: &str,
        width: u32,
        height: u32,
        fps: u32,
        start_bitrate_kbps: u32,
    ) {
        *self.video_config.lock() = Some(WtHostMessage::VideoConfig {
            codec: codec.to_string(),
            width,
            height,
            fps,
            start_bitrate_kbps,
            description_b64: None,
            epoch: self.next_config_epoch(),
        });
    }

    /// Push a route warning to the live client's control stream (§"respect
    /// user inputs": the resolution stays; the user is told the route is
    /// struggling). Dropped when no control stream is attached, like every
    /// other push.
    pub fn push_route_warning(&self, detail: String) {
        self.push_control(WtHostMessage::RouteWarning { detail });
    }

    /// Swap the stored video config's codec string for the description-bearing
    /// variant (HVCC for HEVC, AVCC for H.264) once the encoder's caps are
    /// known, and push it to the live session: the client reconfigures and the
    /// next keyframe decodes. No-op while no config has been negotiated.
    pub fn update_video_config_description(&self, codec: &str, description_b64: String) {
        let mut slot = self.video_config.lock();
        let replaced = match slot.as_ref() {
            Some(WtHostMessage::VideoConfig {
                width,
                height,
                fps,
                start_bitrate_kbps,
                ..
            }) => WtHostMessage::VideoConfig {
                codec: codec.to_string(),
                width: *width,
                height: *height,
                fps: *fps,
                start_bitrate_kbps: *start_bitrate_kbps,
                description_b64: Some(description_b64),
                epoch: self.next_config_epoch(),
            },
            _ => return,
        };
        self.push_control(replaced.clone());
        *slot = Some(replaced);
    }

    /// Non-blocking drain of client events, for the media session's tick loop.
    pub fn try_next_event(&self) -> Option<WtClientEvent> {
        self.events_rx.lock().try_recv().ok()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        self.endpoint.close(0u32.into(), b"inphase wt shutdown");
    }
}

/// Order in which a frame's data fragments hit the wire.
///
/// Front to back would put an entire FEC group in consecutive datagrams, and
/// Wi-Fi loses A-MPDU aggregates rather than individual datagrams - a run of
/// eight consecutive fragments takes three or more out of ONE group, which
/// row-1 parity (one hole per group) cannot repair, leaving the frame
/// FEC-dead and dependent on a NACK re-send. Measured on the 80 Mbps session:
/// the client received 18-27 Mbps with the host dropping nothing at all
/// (0 stalled, 0 expired, 1 eviction) yet decoded 1-17 fps while its NACK
/// count climbed by up to 506/s against a 60/s re-send budget - most repair
/// requests were discarded and the frames never assembled.
///
/// Transposing (0, G, 2G, ..., 1, G+1, ...) puts any burst of B <= G
/// consecutive datagrams in at most ceil(B/G) fragments of each group, so
/// row-1 parity rebuilds them all locally with no round trip and no re-send
/// traffic. The client reassembles by `frag_idx` and the parity rows are
/// computed over group membership, not arrival, so this is a pure scheduling
/// change on the wire.
fn fragment_send_order(frag_cnt: u16) -> Vec<u16> {
    let g = inphase_protocol::FRAGMENT_GROUP as u16;
    if frag_cnt == 0 {
        return Vec::new();
    }
    let groups = frag_cnt.div_ceil(g);
    let mut order = Vec::with_capacity(frag_cnt as usize);
    // One fragment from every group in turn, then move to the next offset
    // inside each group: consecutive datagrams therefore belong to different
    // groups.
    for offset in 0..g {
        for group in 0..groups {
            let idx = group * g + offset;
            if idx < frag_cnt {
                order.push(idx);
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_frame_is_paced_to_finish_well_inside_its_interval() {
        let i120 = 1.0 / 120.0;
        // An average 1440p120 frame: ~110 datagrams in 90 % of 8.3 ms.
        let pps = super::frame_pace_pps(18_000.0, 110, i120, 0);
        assert!(110.0 / pps <= i120 * 0.91, "{pps}");
        // A small frame never goes slower than the connection's pace.
        assert_eq!(super::frame_pace_pps(18_000.0, 10, i120, 0), 18_000.0);
        // Frames waiting: catch up faster.
        assert!(super::frame_pace_pps(18_000.0, 110, i120, 3) > pps);
        // Bounded.
        assert!(super::frame_pace_pps(18_000.0, 5_000, i120, 7) <= 100_000.0);
    }

    #[test]
    fn the_pace_burst_is_a_few_milliseconds_of_pace() {
        let burst = super::pace_burst(crate::media::wt::WORKER_PACE_PPS);
        assert_eq!(burst, 32.0, "worker burst");
        // In-page clients keep the measured-safe burst.
        assert_eq!(super::pace_burst(crate::media::wt::WT_PACE_PPS), 64.0);
    }

    use super::super::session::read_line;
    use super::*;
    use crate::media::wt::{DEFAULT_DATAGRAM_BUDGET, DEFAULT_MAX_QUEUED_FRAMES};
    use inphase_protocol::{WtClientMessage, WtFrame};
    use std::time::Instant;
    use wtransport::tls::Sha256Digest;

    fn test_transport() -> WtVideoTransport {
        let id =
            crate::media::wt::WtIdentity::self_signed(&["localhost".into(), "127.0.0.1".into()])
                .unwrap();
        WtVideoTransport::bind(WtTransportConfig {
            port: 0,
            identity: id.identity,
            cert_not_after: id.not_after,
            datagram_budget: DEFAULT_DATAGRAM_BUDGET,
            max_queued_frames: DEFAULT_MAX_QUEUED_FRAMES,
            congestion: crate::media::wt::WtCongestion::default(),
        })
        .unwrap()
    }

    async fn connect_client(t: &WtVideoTransport) -> wtransport::Connection {
        let cfg = wtransport::ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([Sha256Digest::new(t.cert_sha256())])
            .build();
        let endpoint = wtransport::Endpoint::client(cfg).unwrap();
        endpoint
            .connect(
                wtransport::endpoint::ConnectOptions::builder(format!(
                    "https://[::1]:{}/wt-video",
                    t.port()
                ))
                .build(),
            )
            .await
            .unwrap()
    }

    /// Open the control stream and present the token as the first message.
    async fn open_control_and_auth(
        conn: &wtransport::Connection,
        token: &str,
    ) -> (wtransport::SendStream, wtransport::RecvStream) {
        let (mut tx, rx) = conn.open_bi().await.unwrap().await.unwrap();
        let mut line = serde_json::to_string(&WtClientMessage::Auth {
            token: token.to_string(),
        })
        .unwrap();
        line.push('\n');
        tx.write_all(line.as_bytes()).await.unwrap();
        (tx, rx)
    }

    async fn wait_for(
        t: &WtVideoTransport,
        pred: impl Fn(&WtClientEvent) -> bool,
    ) -> WtClientEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(e) = t.try_next_event() {
                if pred(&e) {
                    return e;
                }
                continue;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for client event");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn cert_hash_hex_matches_raw() {
        let t = test_transport();
        let hex = t.cert_sha256_hex();
        assert_eq!(hex.len(), 64);
        let raw = t.cert_sha256();
        let expect: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expect);
    }

    #[test]
    fn tokens_are_one_time_and_expire() {
        let store = TokenStore::new();
        let t = store.issue(Duration::from_secs(60));
        assert!(store.consume(&t));
        assert!(!store.consume(&t), "one-time");
        let short = store.issue(Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!store.consume(&short), "expired");
        assert!(!store.consume("never-issued"));
    }

    #[tokio::test]
    async fn queue_evicts_oldest_delta_never_a_keyframe() {
        let t = test_transport();
        // No client connects. Fill the queue with deltas, then verify
        // latest-value semantics: the newest frame is admitted, the oldest
        // delta is the victim, and a keyframe in the queue is never evicted
        // by a delta (it is the only frame that can restart the reference
        // chain).
        let make = |i: u32, key: bool| OutboundFrame {
            frame_no: i,
            capture_us: 0,
            key,
            payload: vec![i as u8; 100],
            captured_at: std::time::Instant::now(),
            enq_us: 0,
            capture_host_us: 0,
        };
        for i in 0..DEFAULT_MAX_QUEUED_FRAMES as u32 {
            assert!(t.send_frame(make(i, false)), "frame {i} accepted");
        }
        assert!(
            t.send_frame(make(DEFAULT_MAX_QUEUED_FRAMES as u32, false)),
            "queue full → newest still admitted (evicts oldest delta)"
        );
        assert!(t.send_frame(make(200, true)), "keyframe admitted");
        // A delta arriving behind a full queue of [1 delta + 1 keyframe …]
        // must evict the oldest DELTA, never the keyframe.
        for _ in 0..DEFAULT_MAX_QUEUED_FRAMES {
            assert!(t.send_frame(make(201, false)), "deltas keep flowing");
        }
        let q: &FrameQueue = &t.frame_queue;
        let snapshot: Vec<u32> = q.q.lock().iter().map(|f| f.frame_no).collect();
        assert!(
            snapshot.contains(&200),
            "keyframe survives eviction pressure: {snapshot:?}"
        );
        assert!(
            !snapshot.contains(&0),
            "oldest delta evicted first: {snapshot:?}"
        );
    }

    /// The whole video path, v3: authenticate, send frames, read each one off
    /// the wire, hang up.
    ///
    /// Frames are self-delimited (18-byte header with the payload length) on
    /// unidirectional streams. The sender batches them onto ONE persistent
    /// stream - the 2026-09-09 18:57 phone session showed WebKit ending every
    /// per-frame stream before its payload, while a single long-lived stream
    /// (the control channel) worked fine - but a reader that handles "one or
    /// more frames per stream, in order" is the contract on both ends, so the
    /// test reads length-prefixed frames rather than streams.
    #[tokio::test]
    async fn frames_arrive_self_delimited() {
        let t = test_transport();
        let token = t.issue_token(Duration::from_secs(30));

        let conn = connect_client(&t).await;
        let (_tx, _rx) = open_control_and_auth(&conn, &token).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        // Open the video channel the way the web client does: a bidi stream,
        // first packet a 2-byte BE framed JSON marker. Frames then arrive on
        // this stream's receive half - the path iOS WebKit can actually
        // deliver (server-initiated streams are not). Opened BEFORE frames
        // are queued: a frame with no sink is dropped.
        let vch = conn
            .open_bi()
            .await
            .expect("open bi")
            .await
            .expect("bi accepted");
        let (mut vtx, mut vrx) = vch;
        let marker = br#"{"type":"video_channel"}"#;
        let mut framed = Vec::with_capacity(2 + marker.len());
        framed.extend_from_slice(&(marker.len() as u16).to_be_bytes());
        framed.extend_from_slice(marker);
        vtx.write_all(&framed).await.expect("marker");
        vtx.finish().await.ok(); // client never writes more

        // A keyframe far larger than any datagram would carry, plus two deltas.
        for n in 1..=3u32 {
            assert!(
                t.send_frame(OutboundFrame {
                    frame_no: n,
                    capture_us: n as u64 * 16_667,
                    key: n == 1,
                    payload: vec![n as u8; if n == 1 { 400_000 } else { 900 }],
                    captured_at: std::time::Instant::now(),
                    enq_us: 0,
                    capture_host_us: 0,
                }),
                "frame {n} accepted"
            );
        }

        // Read frames off the channel's receive half.
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        'outer: while got.len() < 3 && Instant::now() < deadline {
            // Never read to EOF: a persistent frame stream has no FIN until
            // the session ends, and waiting for one is the v2 Safari bug. The
            // 18-byte header is the boundary - read header, then payload.
            let mut head = [0u8; inphase_protocol::WT_VIDEO_HEADER_LEN];
            if vrx.read_exact(&mut head).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue; // stream spent (reset or FIN); wait for a fresh channel
            }
            let len = u32::from_le_bytes([head[14], head[15], head[16], head[17]]) as usize;
            let mut payload = vec![0u8; len];
            if vrx.read_exact(&mut payload).await.is_err() {
                warn!("stream ended mid-payload - frame lost");
                continue 'outer;
            }
            let mut buf = head.to_vec();
            buf.extend_from_slice(&payload);
            let frame = WtFrame::decode(&buf).expect("frame decodes");
            assert_eq!(frame.key, frame.frame_no == 1, "keyframe flag survives");
            let expected = if frame.frame_no == 1 { 400_000 } else { 900 };
            assert_eq!(frame.payload.len(), expected, "payload arrives whole");
            assert!(
                frame.payload.iter().all(|b| *b == frame.frame_no as u8),
                "payload bytes are intact"
            );
            got.push(frame.frame_no);
        }
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3], "every frame arrived");

        // Client hangs up -> server notices.
        drop(conn);
        wait_for(&t, |e| matches!(e, WtClientEvent::Disconnected)).await;
    }

    /// The reported black screens, reproduced.
    ///
    /// The accept loop logs "video channel installed" and only then stores the
    /// sink, so there is a window where the sender has neither a sink nor a
    /// cached stream - and an IDR already in the queue lands in it, because the
    /// client asks for one the moment it connects. The sender used to
    /// `expect("stream just ensured")` through that window (live 2026-09-16 and
    /// 2026-09-19, both at session start). Losing that task kills the video
    /// path for the life of the process: dials keep succeeding, the handshake
    /// completes, and every session from then on is black until the host is
    /// restarted.
    #[tokio::test]
    async fn a_keyframe_with_no_channel_yet_does_not_kill_the_sender() {
        let t = test_transport();
        let token = t.issue_token(Duration::from_secs(30));

        let conn = connect_client(&t).await;
        let (_tx, _rx) = open_control_and_auth(&conn, &token).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        // A keyframe with no video channel open - the racing case.
        assert!(
            t.send_frame(OutboundFrame {
                frame_no: 1,
                capture_us: 16_667,
                key: true,
                payload: vec![7u8; 400_000],
                captured_at: std::time::Instant::now(),
                enq_us: 0,
                capture_host_us: 0,
            }),
            "frame accepted"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            t.sender_alive(),
            "the sender must drop a frame with no sink, not panic on it"
        );

        // The channel arrives late, as it does in the race. The video path must
        // still work afterwards - a sender that died above never sends again.
        let vch = conn
            .open_bi()
            .await
            .expect("open bi")
            .await
            .expect("bi accepted");
        let (mut vtx, mut vrx) = vch;
        let marker = br#"{"type":"video_channel"}"#;
        let mut framed = Vec::with_capacity(2 + marker.len());
        framed.extend_from_slice(&(marker.len() as u16).to_be_bytes());
        framed.extend_from_slice(marker);
        vtx.write_all(&framed).await.expect("marker");
        vtx.finish().await.ok();

        // The client re-asks for an IDR when it sees nothing (so this is also
        // what the client's ladder produces in production).
        for n in 2..=3u32 {
            assert!(t.send_frame(OutboundFrame {
                frame_no: n,
                capture_us: n as u64 * 16_667,
                key: false,
                payload: vec![n as u8; 900],
                captured_at: std::time::Instant::now(),
                enq_us: 0,
                capture_host_us: 0,
            }));
        }

        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 2 && Instant::now() < deadline {
            let mut head = [0u8; inphase_protocol::WT_VIDEO_HEADER_LEN];
            if vrx.read_exact(&mut head).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            let len = u32::from_le_bytes([head[14], head[15], head[16], head[17]]) as usize;
            let mut payload = vec![0u8; len];
            if vrx.read_exact(&mut payload).await.is_err() {
                continue;
            }
            let mut buf = head.to_vec();
            buf.extend_from_slice(&payload);
            got.push(WtFrame::decode(&buf).expect("frame decodes").frame_no);
        }
        got.sort_unstable();
        assert_eq!(
            got,
            vec![2, 3],
            "frames queued after the channel opened must still arrive"
        );
    }

    /// The deadlock of 2026-09-20, as a test.
    ///
    /// A starved client asks for a keyframe — it only asks when it has no
    /// anchor. In v4 mode IDRs ride the client-opened reliable stream, and a
    /// write into a stream the client stopped reading succeeds into the void
    /// silently. With no channel open at all there is nowhere for the answer to
    /// go, and the client stays starved while the host reports perfect delivery:
    /// live, `held=8 decoded=0` for over five minutes at 1440p60, deltas
    /// arriving at 56 Mbps with 0 % loss, a keyframe request every second, and
    /// zero stream errors at the sender. A keyframe that answers a request now
    /// rides the datagram carrier as well, so the answer does not depend on the
    /// one carrier that can fail this way.
    #[tokio::test]
    async fn a_requested_keyframe_reaches_a_client_with_no_video_channel() {
        use inphase_protocol::WtFragment;

        let t = test_transport();
        let token = t.issue_token(Duration::from_secs(30));
        let conn = connect_client(&t).await;
        let (mut ctl, _rx) = open_control_and_auth(&conn, &token).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        let mut line = serde_json::to_string(&WtClientMessage::EnableDatagramVideo).unwrap();
        line.push('\n');
        ctl.write_all(line.as_bytes()).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.datagram_video_enabled() {
            assert!(Instant::now() < deadline, "carrier switch never applied");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Starved, and NO video channel: the client's channel went silent
        // without ever resetting, so its read side never opened another.
        let mut req = serde_json::to_string(&WtClientMessage::KeyframeRequest).unwrap();
        req.push('\n');
        ctl.write_all(req.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            t.video_sink_is_empty(),
            "the premise is a client with no channel installed"
        );

        let payload_len = 6_000;
        assert!(t.send_frame(OutboundFrame {
            frame_no: 7,
            capture_us: 16_667,
            key: true,
            payload: vec![9u8; payload_len],
            captured_at: std::time::Instant::now(),
            enq_us: 0,
            capture_host_us: 0,
        }));

        // The keyframe's own fragments must arrive on the datagram carrier.
        let mut data_parts: std::collections::HashMap<u16, Vec<u8>> =
            std::collections::HashMap::new();
        let mut frag_cnt = 0u16;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let d = tokio::time::timeout(Duration::from_secs(2), conn.receive_datagram()).await;
            let Ok(Ok(d)) = d else { break };
            if d.first() == Some(&inphase_protocol::WT_AUDIO_DATAGRAM_TAG) {
                continue;
            }
            let f = WtFragment::decode(&d).expect("fragment decodes");
            if f.frame_no != 7 {
                continue;
            }
            assert!(f.key, "the keyframe flag survives the datagram carrier");
            frag_cnt = f.frag_cnt;
            if f.parity == 0 {
                data_parts.insert(f.frag_idx, f.payload);
            }
        }

        assert!(
            frag_cnt > 0,
            "the requested keyframe never reached the client: it has no channel, \
             so the answer to its request must ride the datagrams"
        );
        assert_eq!(
            data_parts.len(),
            frag_cnt as usize,
            "every fragment of the anchor arrives"
        );
        let mut assembled = Vec::new();
        for idx in 0..frag_cnt {
            assembled.extend_from_slice(&data_parts[&idx]);
        }
        assert_eq!(assembled.len(), payload_len, "the anchor reassembles whole");
        assert!(assembled.iter().all(|b| *b == 9), "payload intact");
    }

    #[tokio::test]
    async fn bad_token_is_refused() {
        let t = test_transport();
        let conn = connect_client(&t).await;
        let (_tx, mut rx) = open_control_and_auth(&conn, "not-a-real-token").await;
        let reply = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx, 4096))
            .await
            .unwrap()
            .unwrap()
            .expect("error line before close");
        assert!(reply.contains("unauthorized"), "got: {reply}");
        // The connection is closed by the host right after.
        let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        assert!(closed.is_ok(), "connection should close after refusal");
    }

    /// The newest holder of a valid token takes the video slot, and the
    /// incumbent is closed.
    ///
    /// This replaces `second_session_gets_busy`, which asserted the opposite.
    /// Refusing was wrong on two counts: the one-time token was already minted
    /// by the authenticated signaling layer, which is the arbiter of who plays,
    /// so the transport has no standing to veto it; and a client that dies
    /// without a clean close (killed tab, crash, sleep) holds the slot until the
    /// 30s QUIC idle timeout, so every reconnect inside that window was refused.
    /// Measured live: a second session 5s after the first got
    /// "host refused: busy" and sat black.
    #[tokio::test]
    async fn a_newer_session_takes_the_slot() {
        let t = test_transport();
        // A session with no negotiated config gets no host messages at all, so
        // give it one - the video config is what a served client sees first.
        t.set_video_config("avc1.42E01E", 1920, 1080, 60, 12_000);
        let first = connect_client(&t).await;
        let (_tx1, _rx1) =
            open_control_and_auth(&first, &t.issue_token(Duration::from_secs(30))).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        let second = connect_client(&t).await;
        let (_tx2, mut rx2) =
            open_control_and_auth(&second, &t.issue_token(Duration::from_secs(30))).await;

        // The newcomer is served, not refused: its first host message is the
        // video config, exactly as for a first session.
        let reply = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx2, 4096))
            .await
            .unwrap()
            .unwrap()
            .expect("a host line for the new session");
        assert!(
            !reply.contains("busy"),
            "the newer session must not be refused, got: {reply}"
        );

        // And the incumbent is dropped rather than left half-live.
        let closed = tokio::time::timeout(Duration::from_secs(5), first.closed()).await;
        assert!(closed.is_ok(), "the displaced session should be closed");
    }

    /// The displaced session tears down *after* its successor is already live.
    /// If it cleared the shared slot unconditionally it would strip the new
    /// client's control sink and frame target, leaving it silently frameless.
    #[tokio::test]
    async fn a_displaced_session_does_not_tear_down_its_successor() {
        let t = test_transport();
        t.set_video_config("avc1.42E01E", 1920, 1080, 60, 12_000);
        let first = connect_client(&t).await;
        let (_tx1, _rx1) =
            open_control_and_auth(&first, &t.issue_token(Duration::from_secs(30))).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        let second = connect_client(&t).await;
        let (_tx2, mut rx2) =
            open_control_and_auth(&second, &t.issue_token(Duration::from_secs(30))).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx2, 4096)).await;

        // Let the displaced session finish its teardown.
        let _ = tokio::time::timeout(Duration::from_secs(5), first.closed()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The survivor still receives pushes, so the control sink was not cleared.
        t.push_control(WtHostMessage::Error {
            code: "probe".into(),
            message: "still wired".into(),
        });
        let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx2, 4096))
            .await
            .unwrap()
            .unwrap()
            .expect("the surviving session should still receive control messages");
        assert!(line.contains("still wired"), "got: {line}");
    }

    #[tokio::test]
    async fn video_config_is_the_first_host_message() {
        // `MediaSession::configure` stores the negotiated config; the client
        // must receive it before anything else or its WebCodecs decoder never
        // configures (the P1 gap that left real sessions on WebRTC).
        let t = test_transport();
        t.set_video_config("hev1.1.6.L153.B0", 2560, 1440, 60, 50_000);
        let conn = connect_client(&t).await;
        let (_tx, mut rx) =
            open_control_and_auth(&conn, &t.issue_token(Duration::from_secs(30))).await;
        let first = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx, 4096))
            .await
            .unwrap()
            .unwrap()
            .expect("video config line");
        assert!(first.contains("video_config"), "got: {first}");
        assert!(first.contains("hev1.1.6.L153.B0"), "got: {first}");
        assert!(first.contains("2560"), "got: {first}");
    }

    #[tokio::test]
    async fn push_control_reaches_the_live_session() {
        let t = test_transport();
        let conn = connect_client(&t).await;
        let (_tx, mut rx) =
            open_control_and_auth(&conn, &t.issue_token(Duration::from_secs(30))).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        t.push_control(WtHostMessage::VideoConfig {
            codec: "avc1.42E01E".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            start_bitrate_kbps: 12_000,
            description_b64: None,
            epoch: 1,
        });
        let reply = tokio::time::timeout(Duration::from_secs(5), read_line(&mut rx, 4096))
            .await
            .unwrap()
            .unwrap()
            .expect("video config line");
        assert!(reply.contains("video_config"), "got: {reply}");

        // No live session after disconnect: push is a silent no-op.
        drop(conn);
        wait_for(&t, |e| matches!(e, WtClientEvent::Disconnected)).await;
        t.push_control(WtHostMessage::Pong {
            at_us: 1,
            host_us: 0,
        });
    }

    /// The v4 carrier end to end: the client asks for datagram video over the
    /// control stream, and every frame arrives reassembled from datagram
    /// fragments - no reliable stream involved. The stream carrier itself is
    /// proven by `frames_arrive_self_delimited`; the fallback design means
    /// both stay true (research doc P0).
    /// A burst of consecutive datagrams must not concentrate in one FEC
    /// group: that is what makes a Wi-Fi A-MPDU loss unrepairable.
    #[test]
    fn send_order_interleaves_fec_groups() {
        const G: usize = inphase_protocol::FRAGMENT_GROUP;
        for cnt in [1u16, 2, 7, 8, 9, 16, 17, 37, 100, 370] {
            let order = fragment_send_order(cnt);
            assert_eq!(
                order.len(),
                cnt as usize,
                "every fragment sent once ({cnt})"
            );
            let mut seen = order.clone();
            seen.sort_unstable();
            assert_eq!(
                seen,
                (0..cnt).collect::<Vec<_>>(),
                "the order is a permutation of 0..{cnt}"
            );
            let groups = (cnt as usize).div_ceil(G);
            // The frame opens by taking one fragment from every group, so a
            // burst at the head of a frame can never concentrate.
            let head = &order[..groups.min(order.len())];
            let touched: std::collections::HashSet<usize> =
                head.iter().map(|i| *i as usize / G).collect();
            assert_eq!(
                touched.len(),
                head.len(),
                "the frame's first sends repeat a FEC group (cnt={cnt}, groups={groups})"
            );
            // When every group is full - the case that matters at high
            // bitrate, where a 1440p60 frame is 30-40 fragments - every window
            // of `groups` consecutive sends is all-distinct, so a burst of
            // that length leaves exactly one hole per group and row-1 parity
            // repairs every one of them without a round trip.
            if (cnt as usize).is_multiple_of(G) && order.len() >= groups {
                for w in order.windows(groups) {
                    let touched: std::collections::HashSet<usize> =
                        w.iter().map(|i| *i as usize / G).collect();
                    assert_eq!(
                        touched.len(),
                        w.len(),
                        "window {w:?} repeats a group (cnt={cnt})"
                    );
                }
            }
        }
    }

    /// The frame's final fragment is the short one, and the client derives
    /// every other length from the budget - the transposed order must still
    /// place it last-of-nothing and send it exactly once.
    #[test]
    fn send_order_handles_a_short_tail_fragment() {
        let order = fragment_send_order(9);
        assert_eq!(order.len(), 9);
        assert_eq!(order.iter().filter(|i| **i == 8).count(), 1);
    }

    #[tokio::test]
    async fn frames_arrive_as_datagrams_when_enabled() {
        let t = test_transport();
        let token = t.issue_token(Duration::from_secs(30));

        let conn = connect_client(&t).await;
        let (mut ctl, _rx) = open_control_and_auth(&conn, &token).await;
        wait_for(&t, |e| matches!(e, WtClientEvent::Connected)).await;

        // Ask for the v4 carrier, exactly as the browser does once the
        // audio-datagram probe has proven the path.
        let mut line = serde_json::to_string(&WtClientMessage::EnableDatagramVideo).unwrap();
        line.push('\n');
        ctl.write_all(line.as_bytes()).await.unwrap();

        // Wait for the control reader to apply the switch: the flag travels
        // the control-stream path, not the queue path.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.datagram_video_enabled() {
            assert!(Instant::now() < deadline, "carrier switch never applied");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // One keyframe larger than a single datagram, plus two deltas. In v4
        // mode the keyframe rides the client-opened reliable stream (it is
        // the anchor; a 700 KB burst would blow the delta freshness budget
        // for every frame queued behind it) - so the client opens its video
        // channel first, exactly as the web client does.
        let vch = conn
            .open_bi()
            .await
            .expect("open bi")
            .await
            .expect("bi accepted");
        let (mut vtx, mut vrx) = vch;
        let marker = br#"{"type":"video_channel"}"#;
        let mut framed = Vec::with_capacity(2 + marker.len());
        framed.extend_from_slice(&(marker.len() as u16).to_be_bytes());
        framed.extend_from_slice(marker);
        vtx.write_all(&framed).await.expect("marker");
        vtx.finish().await.ok(); // client never writes more
                                 // Wait for the host to install the sink before queueing frames.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !t.video_sink_is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        for n in 1..=3u32 {
            assert!(t.send_frame(OutboundFrame {
                frame_no: n,
                capture_us: n as u64 * 16_667,
                key: n == 1,
                payload: vec![n as u8; if n == 1 { 6_000 } else { 900 }],
                captured_at: std::time::Instant::now(),
                enq_us: 0,
                capture_host_us: 0,
            }));
        }

        // Client side: read datagrams, reassemble fragments into frames. The
        // keyframe must NOT be among them - it rides the reliable stream.
        use inphase_protocol::WtFragment;
        let mut parts: std::collections::HashMap<u32, Vec<Option<Vec<u8>>>> =
            std::collections::HashMap::new();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 2 && Instant::now() < deadline {
            let d = tokio::time::timeout(Duration::from_secs(5), conn.receive_datagram()).await;
            let Ok(Ok(d)) = d else { break };
            if d.first() == Some(&inphase_protocol::WT_AUDIO_DATAGRAM_TAG) {
                continue; // audio piggybacks on the same reader; not video
            }
            let f = WtFragment::decode(&d).expect("fragment decodes");
            let e = parts
                .entry(f.frame_no)
                .or_insert(vec![None; f.frag_cnt as usize]);
            e[f.frag_idx as usize] = Some(f.payload);
            if e.iter().all(|p| p.is_some()) && !got.contains(&f.frame_no) {
                got.push(f.frame_no);
            }
        }
        got.sort_unstable();
        assert_eq!(
            got,
            vec![2, 3],
            "deltas reassemble from datagrams; keyframe does not ride them"
        );

        // The keyframe arrives self-delimited on the client-opened channel.
        let mut head = [0u8; inphase_protocol::WT_VIDEO_HEADER_LEN];
        tokio::time::timeout(Duration::from_secs(5), vrx.read_exact(&mut head))
            .await
            .expect("keyframe header timed out")
            .expect("keyframe header read");
        let len = u32::from_le_bytes([head[14], head[15], head[16], head[17]]) as usize;
        let mut payload = vec![0u8; len];
        tokio::time::timeout(Duration::from_secs(5), vrx.read_exact(&mut payload))
            .await
            .expect("keyframe payload timed out")
            .expect("keyframe payload read");
        assert!(payload.iter().all(|b| *b == 1), "keyframe payload intact");

        // NACK repair: the client asks for a specific fragment and the exact
        // cached datagram comes back (frag_cnt/capture_us/key intact). Deltas
        // only - the keyframe never entered the fragment cache.
        let mut nack = serde_json::to_string(&WtClientMessage::Nack { frame: 2, idx: 0 }).unwrap();
        nack.push('\n');
        ctl.write_all(nack.as_bytes()).await.unwrap();
        let mut repaired = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !repaired && Instant::now() < deadline {
            let d = tokio::time::timeout(Duration::from_secs(5), conn.receive_datagram()).await;
            let Ok(Ok(d)) = d else { break };
            let Ok(f) = WtFragment::decode(&d) else {
                continue;
            };
            if f.frame_no == 2 && f.frag_idx == 0 {
                repaired = true;
            }
        }
        assert!(repaired, "NACK re-sent the requested fragment");

        // Server-initiated uni streams stay silent in v4 mode (the deltas are
        // datagrams; the keyframe rode the CLIENT-opened channel above).
        drop(conn);
        wait_for(&t, |e| matches!(e, WtClientEvent::Disconnected)).await;
    }
}
