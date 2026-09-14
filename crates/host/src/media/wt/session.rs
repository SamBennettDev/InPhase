//! Per-connection session handling: auth, the control stream, and teardown.
//!
//! One accepted QUIC connection at a time becomes the live video session. A
//! dial carries no credentials, so everything here is refused until a valid
//! one-time token arrives as the first control-stream message.

use anyhow::Context as _;
use inphase_protocol::{WtClientMessage, WtHostMessage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wtransport::VarInt;

use super::{ControlOutSlot, Shared, WtClientEvent, AUTH_TIMEOUT};

pub(super) async fn handle_incoming(
    incoming: wtransport::endpoint::IncomingSession,
    shared: Arc<Shared>,
    control_out: ControlOutSlot,
) -> anyhow::Result<()> {
    let request = incoming.await.context("QUIC handshake")?;
    debug!(path = request.path(), "wt session request");
    if request.path() != "/wt-video" {
        anyhow::bail!("unexpected path {}", request.path());
    }
    let connection = request.accept().await.context("wt upgrade")?;
    info!("wt session established");

    // First control stream carries the token. Everything before a valid token
    // is refused: a QUIC dial is unauthenticated by construction.
    let (mut tx, mut rx) = tokio::time::timeout(AUTH_TIMEOUT, connection.accept_bi())
        .await
        .context("no control stream within auth timeout")?
        .context("control stream open failed")?;
    let auth_line = read_line(&mut rx, 4096)
        .await
        .context("reading auth line")?
        .context("control stream closed before auth")?;
    let token = match serde_json::from_str::<WtClientMessage>(&auth_line) {
        Ok(WtClientMessage::Auth { token }) => token,
        _ => {
            refuse(
                &mut tx,
                "unauthorized",
                "first control message must be Auth",
            )
            .await;
            connection.close(0u32.into(), b"unauthorized");
            anyhow::bail!("auth: first message was not Auth");
        }
    };
    if !shared.tokens.consume(&token) {
        refuse(
            &mut tx,
            "unauthorized",
            "token unknown, expired, or already used",
        )
        .await;
        connection.close(0u32.into(), b"unauthorized");
        anyhow::bail!("auth: token rejected");
    }

    // Every session starts on the reliable stream carrier. The v4 switch is
    // per-connection: the flag outliving a session (Shared outlives
    // connections) made the NEXT dial - whose client never asked - inherit
    // datagram video (21:42 enabled it; the 21:51 dial ran v4 unseen).
    shared
        .datagram_video
        .store(false, std::sync::atomic::Ordering::Relaxed);

    // One active video session (ADR-0007) - and the newest valid token wins.
    //
    // This used to refuse a second dial with "busy". That contradicted the very
    // next clause of its own comment: the signaling layer *is* the arbiter of
    // who plays, and it already decided by minting this one-time token over an
    // authenticated socket. The WT layer has no standing to veto it.
    //
    // It also broke reconnects in practice. A browser that dies without a clean
    // close (killed tab, crash, sleep) sends no CONNECTION_CLOSE, and the host
    // runs no server-side keep-alive on purpose, so `connection.closed()` does
    // not resolve until the 30s idle timeout. The slot stayed held by a client
    // that no longer existed, and every reconnect inside that window was refused
    // - a black screen for half a minute after any hard exit. SessionManager
    // already reclaims on "newest player wins" (34a55be); this now matches.
    //
    // The guard must die before any await (auto-Send), so resolve first, act after.
    let session_id = shared
        .next_session_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let displaced = {
        let mut active = shared.active.lock();
        let previous = active.replace(session_id);
        previous.map(|id| (id, shared.live_connection.borrow().clone()))
    };
    if let Some((old_id, old_conn)) = displaced {
        info!(
            old_id,
            new_id = session_id,
            "wt: newer session takes the video slot"
        );
        if let Some(c) = old_conn {
            c.close(VarInt::from_u32(0), b"replaced by a newer session");
        }
    }
    let _ = shared.events.send(WtClientEvent::Connected);

    // Control writer: serializes host messages onto the stream. The per-session
    // sender is installed so `push_control` from the media session reaches it.
    let (co_tx, mut co_rx) = mpsc::unbounded_channel::<WtHostMessage>();
    {
        let mut slot = control_out.lock();
        *slot = Some(co_tx.clone());
    }
    tokio::spawn(async move {
        while let Some(msg) = co_rx.recv().await {
            if !write_msg(&mut tx, &msg).await {
                break;
            }
        }
        let _ = tx.finish().await;
    });

    // The negotiated video config is the first host → client message: the
    // client's WebCodecs decoder configures from it and cannot decode until
    // it lands. `None` (no media session negotiated yet) sends nothing.
    if let Some(cfg) = shared.video_config.lock().clone() {
        let _ = co_tx.send(cfg);
    }

    // Dedicated client-opened streams (§12 input over WT): Safari never
    // delivers client -> host datagrams, and routing input through the control
    // stream serializes it behind NACK bursts - head-of-line blocking that
    // showed up as felt input lag under loss (2026-09-08 phone session:
    // hundreds of NACK events per second sharing the ordered control queue
    // with touches). Each stream is 2-byte BE length framed; the FIRST packet
    // decides the job. JSON with type "video_channel" marks the video sink -
    // a stream the client opened because iOS WebKit does not deliver
    // server-initiated streams to JavaScript (see `Shared::video_sink`); no
    // §12 input packet can start with '{'. Anything else is an input stream.
    {
        let input_shared = shared.clone();
        let input_conn = connection.clone();
        tokio::spawn(async move {
            loop {
                let stream = match input_conn.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => break, // connection gone
                };
                let events = input_shared.events.clone();
                let marker_shared = input_shared.clone();
                tokio::spawn(async move {
                    let (tx, mut rx) = stream;
                    let mut lbuf = [0u8; 2];
                    if rx.read_exact(&mut lbuf).await.is_err() {
                        return;
                    }
                    let len = u16::from_be_bytes(lbuf) as usize;
                    if len == 0 || len > 2048 {
                        return; // framing violation: drop the stream
                    }
                    let mut buf = vec![0u8; len];
                    if rx.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    if buf.first() == Some(&b'{') {
                        // JSON marker, never input: only "video_channel" exists.
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) {
                            if v.get("type").and_then(|t| t.as_str()) == Some("video_channel") {
                                info!("wt: video channel installed");
                                *marker_shared.video_sink.lock() = Some(tx);
                                drop(rx); // STOP_SENDING is fine; we never read more
                            }
                        }
                        return;
                    }
                    // §12 input stream: every packet - including the first -
                    // forwards as input.
                    loop {
                        let _ = events.send(WtClientEvent::Input(buf));
                        let mut lbuf = [0u8; 2];
                        if rx.read_exact(&mut lbuf).await.is_err() {
                            break;
                        }
                        let len = u16::from_be_bytes(lbuf) as usize;
                        if len == 0 || len > 2048 {
                            break; // framing violation: drop the stream
                        }
                        let mut pbuf = vec![0u8; len];
                        if rx.read_exact(&mut pbuf).await.is_err() {
                            break;
                        }
                        buf = pbuf;
                    }
                });
            }
        });
    }

    // Control reader: NACKs / keyframe requests / telemetry from the client.
    {
        let shared = shared.clone();
        let co_tx = co_tx.clone();
        let resend_conn = connection.clone();
        tokio::spawn(async move {
            // One reader for the whole control-stream lifetime: it retains
            // pipelined bytes across reads (see LineReader).
            let mut lines = LineReader::new();
            loop {
                let line = match lines.read_line(&mut rx, 8192).await {
                    Ok(Some(line)) => line,
                    _ => {
                        // Client gone (stream EOF or error). Force the QUIC
                        // connection closed so the session task's cleanup runs
                        // now, not at the idle timeout — the video slot must
                        // not be held by a zombie.
                        resend_conn.close(VarInt::from_u32(0), b"control stream ended");
                        break;
                    }
                };
                match serde_json::from_str::<WtClientMessage>(&line) {
                    Ok(WtClientMessage::Input { data_b64 }) => {
                        // Reliable input path: iOS Safari never delivers
                        // client -> host datagrams, so the same §12 packet
                        // also travels here. The app-level sequence gate
                        // dedupes against the (faster) datagram copy.
                        use base64::Engine as _;
                        match base64::engine::general_purpose::STANDARD_NO_PAD.decode(&data_b64) {
                            Ok(bytes) => {
                                let _ = shared.events.send(WtClientEvent::Input(bytes));
                            }
                            Err(e) => {
                                warn!("wt: input over control stream failed to decode: {e}");
                            }
                        }
                    }
                    Ok(WtClientMessage::KeyframeRequest) => {
                        if request_keyframe(&shared) {
                            info!("wt: client requested a keyframe");
                        }
                    }
                    Ok(WtClientMessage::ConfigAck { epoch }) => {
                        // The client applied a config change (review §5).
                        // For now this is observability; gating the next
                        // keyframe on it is the follow-up.
                        info!(epoch, "wt: client acknowledged config epoch");
                    }
                    Ok(WtClientMessage::Telemetry(t)) => {
                        // Generation guard: a displaced session's last
                        // telemetry must not overwrite its successor's
                        // stats (review §11 - every sample belongs to a
                        // session generation).
                        if *shared.active.lock() == Some(session_id) {
                            // §13 evidence: log the audio-probe verdict the
                            // first time (or whenever it flips) per session.
                            if let Some(ok) = t.audio_opus_supported {
                                let cur = shared
                                    .audio_opus_supported
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                let val = if ok { 1 } else { 0 };
                                if cur != val {
                                    shared
                                        .audio_opus_supported
                                        .store(val, std::sync::atomic::Ordering::Relaxed);
                                    info!(
                                        supported = ok,
                                        "client audio probe: opus via WebCodecs (audio-path                                          removal decision input, §13)"
                                    );
                                }
                            }
                            // Per-client pace: worker-drain clients own a
                            // dedicated read thread and tolerate ~6500 pps;
                            // in-page clients stay at the measured-safe 3800
                            // (main-thread drain 3.0-3.9k).
                            let pace = if t.worker {
                                crate::media::wt::WORKER_PACE_PPS as u32
                            } else {
                                crate::media::wt::WT_PACE_PPS as u32
                            };
                            shared
                                .pace_pps
                                .store(pace, std::sync::atomic::Ordering::Relaxed);
                            let _ = shared.events.send(WtClientEvent::Telemetry(t));
                        }
                    }
                    Ok(WtClientMessage::Ping { at_us }) => {
                        // Answer in capture-clock µs so the client can map
                        // frame capture_us onto its own clock (NTP-style).
                        // Answer on the capture clock as of the most
                        // recently SENT frame; stale anchors (no frames for
                        // >2 s) answer 0 and the client keeps its last sync.
                        let host_us = {
                            let anchor = shared.frame_anchor.lock();
                            match *anchor {
                                Some((t0, c0))
                                    if t0.elapsed() < std::time::Duration::from_secs(2) =>
                                {
                                    c0 + t0.elapsed().as_micros() as u64
                                }
                                _ => 0,
                            }
                        };
                        let _ = co_tx.send(WtHostMessage::Pong { at_us, host_us });
                    }
                    Ok(WtClientMessage::EnableDatagramVideo) => {
                        if !shared
                            .datagram_video
                            .swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            info!("wt: video carrier switched to v4 datagram fragments");
                        }
                    }
                    Ok(WtClientMessage::DisableDatagramVideo) => {
                        if shared
                            .datagram_video
                            .swap(false, std::sync::atomic::Ordering::Relaxed)
                        {
                            info!("wt: video carrier switched back to the reliable stream");
                        }
                    }
                    Ok(WtClientMessage::Nack { frame, idx }) => {
                        // Re-send the exact cached datagram. A miss (evicted,
                        // or the frame predates the cache) is silent: the
                        // client abandons the frame and the IDR path takes
                        // over. The control handler never touches freshness
                        // here - the client asked for this exact fragment
                        // because its decode chain is waiting on it, and a
                        // late-but-complete delta beats a frozen pipeline.
                        //
                        // Budgeted: a re-send may take a token, never more.
                        // 60/s (burst 60) is a fraction of the ~600-1000/s
                        // fresh stream, so NACK demand can never starve new
                        // frames the way it did at 23:59 (decode 0 for 20 s
                        // under the re-send storm). Beyond-budget NACKs are
                        // dropped: parity repairs the common case, the
                        // client repeats at most 4 rounds, and the next IDR
                        // recovers the rest.
                        if let Some(conn) = shared.live_connection.borrow().clone() {
                            let mut budget = shared.wt_resend_tokens.lock();
                            const RATE_PER_SEC: u32 = 60;
                            const BURST: u32 = 60;
                            let (refill_at, tokens) = &mut *budget;
                            let refill_ms = refill_at.elapsed().as_millis() as u32;
                            *tokens = RATE_PER_SEC.min(*tokens + refill_ms * RATE_PER_SEC / 1000);
                            *refill_at = std::time::Instant::now();
                            if *tokens == 0 {
                                continue;
                            }
                            *tokens -= 1;
                            let hit = shared
                                .wt_resend
                                .lock()
                                .iter()
                                .rev()
                                .find(|(no, idx0, _)| *no == frame && *idx0 == idx)
                                .map(|(_, _, enc)| enc.clone());
                            if let Some(enc) = hit {
                                let _ = crate::media::wt::transport::try_send_datagram(&conn, &enc);
                            }
                        }
                    }
                    Ok(WtClientMessage::Auth { .. }) => debug!("wt: duplicate Auth ignored"),
                    Err(e) => debug!(%e, "wt: bad control line"),
                }
            }
        });
    }

    // This connection becomes the frame sender's target. (Only one session can
    // hold the active slot, so watch writes never race.)
    let _ = shared.live_connection.send(Some(connection.clone()));
    {
        let peer = connection.remote_address().to_string();
        info!(%peer, "wt: live video connection set - datagrams target this client");
    }

    // Drain client → host datagrams: input rides them now (§13); the QUIC
    // receive buffer must never silently fill either way.
    {
        let conn = connection.clone();
        let drain_shared = shared.clone();
        tokio::spawn(async move {
            let shared = drain_shared;
            loop {
                match conn.receive_datagram().await {
                    Ok(bytes) => {
                        // Input rides WT datagrams (§13). A displaced
                        // session's datagrams must not touch its successor's
                        // input (§11) - guard on slot ownership.
                        if *shared.active.lock() == Some(session_id) {
                            let _ = shared.events.send(WtClientEvent::Input(bytes.to_vec()));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    connection.closed().await;
    // The 2026-09-09 18:43 phone session died here SILENTLY: the connection
    // went at +11 s, the frame sender went idle (sent=0 logs nothing), and the
    // only clue was a health warning nine seconds later. Never again - name
    // the moment the video carrier died.
    info!(session_id, "wt: video connection closed");

    // Only tear down the shared state if this session still owns the slot. A
    // displaced session reaches here *after* its successor is already live, and
    // clearing unconditionally would strip the new client's control sink and
    // frame target - handing it a silent, frameless session.
    let still_ours = {
        let mut active = shared.active.lock();
        if *active == Some(session_id) {
            *active = None;
            true
        } else {
            false
        }
    };
    if still_ours {
        let _ = shared.live_connection.send(None);
        *control_out.lock() = None;
        let _ = shared.events.send(WtClientEvent::Disconnected);
    } else {
        debug!(
            session_id,
            "wt: displaced session torn down; slot belongs to a newer one"
        );
    }
    Ok(())
}

/// Ask the encoder for an IDR, at most once a second.
///
/// Both the client's explicit request and the oversized-NACK path funnel
/// through here. They must: an unthrottled request on a lossy route becomes a
/// keyframe storm, and a keyframe is the largest thing this transport sends -
/// storming them on a path that is already dropping fragments is the worst
/// possible response to congestion.
pub(super) fn request_keyframe(shared: &Arc<Shared>) -> bool {
    let now = std::time::Instant::now();
    let due = {
        let mut last = shared.last_keyreq_log.lock();
        if now.duration_since(*last).as_millis() >= 1000 {
            *last = now;
            true
        } else {
            false
        }
    };
    if due {
        let _ = shared.events.send(WtClientEvent::KeyframeRequest);
    }
    due
}

async fn refuse(tx: &mut wtransport::SendStream, code: &str, message: &str) {
    if write_msg(
        tx,
        &WtHostMessage::Error {
            code: code.to_string(),
            message: message.to_string(),
        },
    )
    .await
    {
        // Flush the FIN so the refusal line reaches the client before the
        // connection-level close races it (QUIC discards unsent stream data).
        let _ = tx.finish().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Line reader that retains bytes after the last newline. Control messages
/// pipeline (telemetry, pings and NACKs can land in one read); discarding
/// the tail after `\n` silently dropped every message after the first.
pub(super) struct LineReader {
    buf: Vec<u8>,
}

impl LineReader {
    pub(super) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Extract one complete line from the retained buffer, if present.
    /// The newline (and a CRLF's `\r`) is not part of the line.
    fn take_line(&mut self) -> Option<String> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
        line.pop(); // the newline
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }

    pub(super) async fn read_line(
        &mut self,
        rx: &mut wtransport::RecvStream,
        cap: usize,
    ) -> anyhow::Result<Option<String>> {
        loop {
            if let Some(line) = self.take_line() {
                return Ok(Some(line));
            }
            if self.buf.len() > cap {
                anyhow::bail!("control line exceeds {cap} bytes");
            }
            let mut chunk = [0u8; 4096];
            match rx.read(&mut chunk).await? {
                Some(n) => self.buf.extend_from_slice(&chunk[..n]),
                None => {
                    return if self.buf.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(String::from_utf8_lossy(&self.buf).into_owned()))
                    };
                }
            }
        }
    }
}

/// One-shot convenience for callers that read a single line from a stream
/// (auth, tests). Nothing can legitimately pipeline past their first line.
pub(super) async fn read_line(
    rx: &mut wtransport::RecvStream,
    cap: usize,
) -> anyhow::Result<Option<String>> {
    LineReader::new().read_line(rx, cap).await
}

#[cfg(test)]
mod line_reader_tests {
    use super::LineReader;

    #[test]
    fn pipelined_messages_are_all_delivered() {
        let mut r = LineReader::new();
        r.buf = b"{\"type\":\"ping\"}\n{\"type\":\"keyframe_request\"}\n".to_vec();
        assert_eq!(r.take_line().unwrap(), "{\"type\":\"ping\"}");
        assert_eq!(r.take_line().unwrap(), "{\"type\":\"keyframe_request\"}");
        assert!(r.take_line().is_none());
    }

    #[test]
    fn partial_lines_wait_for_their_tail() {
        let mut r = LineReader::new();
        r.buf = b"{\"type\":\"na".to_vec();
        assert!(r.take_line().is_none(), "incomplete line is retained");
        r.buf.extend_from_slice(b"ck\"}\n{\"type\":\"ping\"}\n");
        assert_eq!(r.take_line().unwrap(), "{\"type\":\"nack\"}");
        assert_eq!(r.take_line().unwrap(), "{\"type\":\"ping\"}");
    }

    #[test]
    fn crlf_is_stripped() {
        let mut r = LineReader::new();
        r.buf = b"auth\r\nnext\n".to_vec();
        assert_eq!(r.take_line().unwrap(), "auth");
        assert_eq!(r.take_line().unwrap(), "next");
    }
}

/// Write one JSON line; `false` when the stream is gone.
async fn write_msg(tx: &mut wtransport::SendStream, msg: &WtHostMessage) -> bool {
    let mut line = match serde_json::to_string(msg) {
        Ok(l) => l,
        Err(_) => return false,
    };
    line.push('\n');
    tx.write_all(line.as_bytes()).await.is_ok()
}
