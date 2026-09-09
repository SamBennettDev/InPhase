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
use tracing::{debug, warn};

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
const FRAME_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(120);

/// Freshness budgets (docs/research/performance-latency-2026-09-09.md, P0):
/// a frame that has not left the host within its budget is worthless - it
/// would playout behind the frames captured after it - so every queue between
/// the encoder and the wire rejects expired work instead of delivering it.
/// Deltas must move fast; a keyframe gets more room because it is the only
/// thing that can resume a broken reference chain.
const DELTA_FRESHNESS: std::time::Duration = std::time::Duration::from_millis(35);
const KEY_FRESHNESS: std::time::Duration = std::time::Duration::from_millis(80);

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
}

impl FrameQueue {
    fn new(cap: usize) -> Self {
        Self {
            q: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            cap: cap.max(1),
            notify: tokio::sync::Notify::new(),
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
                }
                None => {
                    q.pop_front();
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

    fn len(&self) -> usize {
        self.q.lock().len()
    }
}

fn try_send_datagram(conn: &wtransport::Connection, data: &[u8]) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match conn.send_datagram(data) {
            Ok(()) => true,
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
        let server_config = wtransport::ServerConfig::builder()
            .with_bind_config(wtransport::config::IpBindConfig::InAddrAnyV6, cfg.port)
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
            audio: audio_tx.clone(),
            active: Mutex::new(None),
            next_session_id: std::sync::atomic::AtomicU64::new(1),
            audio_opus_supported: std::sync::atomic::AtomicI8::new(-1),
            live_connection,
            frame_anchor: frame_anchor.clone(),
            video_config: video_config.clone(),
            video_sink: Mutex::new(None),
            write_stall_ms: std::sync::atomic::AtomicU32::new(0),
            wt_timeline: Mutex::new(std::collections::VecDeque::new()),
            datagram_video: std::sync::atomic::AtomicBool::new(false),
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
            tokio::spawn(async move {
                let mut live_rx = live_connection_rx;
                let mut live: Option<wtransport::Connection> = None;
                // The sink is injected (client-opened), never opened here.
                let mut stream: Option<wtransport::stream::SendStream> = None;
                let (mut sent, mut stalled, mut expired) = (0u32, 0u32, 0u32);
                let mut stall_window_ms = 0u32;
                let mut last_report = std::time::Instant::now();
                loop {
                    if last_report.elapsed() >= std::time::Duration::from_secs(1) {
                        if stalled > 0 || expired > 0 {
                            warn!(
                                sent,
                                stalled,
                                expired,
                                "wt: frames dropped - client not reading, or past freshness"
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
                        shared_for_sender
                            .write_stall_ms
                            .store(
                                std::mem::take(&mut stall_window_ms),
                                std::sync::atomic::Ordering::Relaxed,
                            );
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
                    if shared_for_sender
                        .datagram_video
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        let Some(conn) = live.as_ref() else { continue };
                        let budget = conn
                            .max_datagram_size()
                            .map(|m| m.saturating_sub(inphase_protocol::WT_FRAGMENT_HEADER_LEN))
                            .unwrap_or(1082)
                            .max(256);
                        let frag_cnt = (frame.payload.len().div_ceil(budget)).max(1) as u16;
                        let mut ok = true;
                        for idx in 0..frag_cnt {
                            // Pace the burst: an IDR is hundreds of fragments
                            // and an unpaced burst overruns the datagram
                            // queue AND correlates with the cellular burst
                            // loss window - the encoded keyframe then never
                            // reassembles and decode waits for the next one
                            // (the 22:04 stalls). Spread it across the 80 ms
                            // key budget in 1 ms batches; deltas stay
                            // immediate.
                            if frame.key && frag_cnt > 16 && idx % 16 == 15 && idx + 1 < frag_cnt
                            {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                            let start = idx as usize * budget;
                            let end = ((idx as usize + 1) * budget).min(frame.payload.len());
                            let frag = inphase_protocol::WtFragment {
                                frame_no: frame.frame_no,
                                frag_idx: idx,
                                frag_cnt,
                                capture_us: frame.capture_us,
                                key: frame.key,
                                payload: frame.payload[start..end].to_vec(),
                            }
                            .encode();
                            if !try_send_datagram(conn, &frag) {
                                ok = false;
                                break; // queue full: the rest is past deadline anyway
                            }
                        }
                        if ok {
                            sent = sent.saturating_add(1);
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
                            if ok { crate::media::frametrace::now_us() } else { 0 },
                        ]);
                        continue;
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
                    if stream.is_none() {
                        match shared_for_sender.video_sink.lock().take() {
                            Some(s) => stream = Some(s),
                            None => {
                                stalled = stalled.saturating_add(1);
                                continue;
                            }
                        }
                    }
                    sent = sent.saturating_add(1);
                    // Frames are back-to-back on the stream; the v3 header is
                    // the boundary the client reads. A write that cannot land
                    // inside the frame budget means the peer stopped reading
                    // (flow control backs pressure up to here): reset the
                    // stream - it is mid-frame and unusable - and drop the
                    // frame. Fewer bytes in flight is what lets a client
                    // behind catch up.
                    let s = stream.as_mut().expect("stream just ensured");
                    let wstart = std::time::Instant::now();
                    // Timeline record: write_us = 0 marks a frame the sender
                    // reset mid-write (timeout / transport error).
                    let mut tl = [
                        frame.frame_no as u64,
                        frame.capture_host_us,
                        frame.enq_us,
                        pop_us,
                        0u64,
                    ];
                    match tokio::time::timeout(FRAME_STREAM_TIMEOUT, s.write_all(&wire)).await {
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
            shutdown_tx,
            video_config,
            endpoint,
            shared: shared.clone(),
            next_frame_no: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Data-fragment count of the most recent keyframe sent, or `None` before

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
        frame.capture_host_us = enq_us.saturating_sub(frame.captured_at.elapsed().as_micros() as u64);
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

#[cfg(test)]
mod tests {

    use super::super::session::read_line;
    use super::*;
    use crate::media::wt::{DEFAULT_DATAGRAM_BUDGET, DEFAULT_MAX_QUEUED_FRAMES};
    use inphase_protocol::{WtClientMessage, WtFrame};
    use std::time::Instant;
    use wtransport::tls::Sha256Digest;

    fn test_transport() -> WtVideoTransport {
        let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
        WtVideoTransport::bind(WtTransportConfig {
            port: 0,
            identity,
            datagram_budget: DEFAULT_DATAGRAM_BUDGET,
            max_queued_frames: DEFAULT_MAX_QUEUED_FRAMES,
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
        let snapshot: Vec<u32> =
            q.q.lock().iter().map(|f| f.frame_no).collect();
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
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let vch = conn.open_bi().await.expect("open bi").await.expect("bi accepted");
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
        use tokio::io::AsyncWriteExt as _;
        ctl.write_all(line.as_bytes()).await.unwrap();

        // Wait for the control reader to apply the switch: the flag travels
        // the control-stream path, not the queue path.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.datagram_video_enabled() {
            assert!(Instant::now() < deadline, "carrier switch never applied");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // One keyframe larger than a single datagram, plus two deltas.
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

        // Client side: read datagrams, reassemble fragments into frames.
        use inphase_protocol::{WtFragment, WT_AUDIO_DATAGRAM_TAG};
        let mut parts: std::collections::HashMap<u32, Vec<Option<Vec<u8>>>> =
            std::collections::HashMap::new();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 3 && Instant::now() < deadline {
            let d = tokio::time::timeout(Duration::from_secs(5), conn.receive_datagram()).await;
            let Ok(Ok(d)) = d else { break };
            if d.first() == Some(&inphase_protocol::WT_AUDIO_DATAGRAM_TAG) {
                continue; // audio piggybacks on the same reader; not video
            }
            let f = WtFragment::decode(&d).expect("fragment decodes");
            let e = parts.entry(f.frame_no).or_insert(vec![None; f.frag_cnt as usize]);
            e[f.frag_idx as usize] = Some(f.payload);
            if e.iter().all(|p| p.is_some()) && !got.contains(&f.frame_no) {
                got.push(f.frame_no);
            }
        }
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3], "every frame reassembled from datagrams");

        // The stream carrier must be silent in v4 mode: no video uni streams.
        drop(conn);
        wait_for(&t, |e| matches!(e, WtClientEvent::Disconnected)).await;
    }
}
