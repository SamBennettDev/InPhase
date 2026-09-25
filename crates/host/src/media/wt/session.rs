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
    // The whole pre-auth exchange shares one deadline: timing only the stream
    // open let an unauthenticated peer hold a connection open indefinitely by
    // never finishing its auth line.
    let deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
    let (mut tx, mut rx) = tokio::time::timeout_at(deadline, connection.accept_bi())
        .await
        .context("no control stream within auth timeout")?
        .context("control stream open failed")?;
    let auth_line = tokio::time::timeout_at(deadline, read_line(&mut rx, 4096))
        .await
        .context("no auth line within auth timeout")?
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
                                // Keep the read half open and drain it; never
                                // drop it. Dropping sends STOP_SENDING, and
                                // Firefox answers that by erroring the WHOLE
                                // bidirectional stream ("Error in input
                                // stream") - so every channel the client opened
                                // for a keyframe died on arrival, no IDR ever
                                // reached it, and Firefox decoded nothing
                                // (2026-09-23: 47 deltas held, 0 decoded, four
                                // redials in 60 s). Chromium ignores it, which
                                // is why only one engine ever saw this.
                                tokio::spawn(async move {
                                    let mut sink = [0u8; 256];
                                    while let Ok(Some(_)) = rx.read(&mut sink).await {}
                                });
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
            // Per session: the client's receive counter starts at zero with
            // each connection, so a window carried over from the previous one
            // differences against the wrong baseline.
            let mut loss_window = LossWindow::default();
            // The pace is transport-wide state, but what it measures (can THIS
            // client drain the injection rate) belongs to one connection. A
            // pace one session backed off to the floor used to carry into the
            // next, where the encoder ceiling (pace x WT_PACE_TO_CEILING) then
            // held a worker client at 28 Mbps against an 80 Mbps setting.
            let mut pace_fresh = true;
            loop {
                let line = match lines.read_line(&mut rx, 8192).await {
                    Ok(Some(line)) => line,
                    other => {
                        debug!(
                            session_id,
                            error = other.err().map(|e| e.to_string()),
                            "wt: control stream ended"
                        );
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
                            // Per-client pace ceiling: worker-drain clients
                            // own a dedicated read thread (11000 pps, the
                            // figure an 80 Mbps target needs); in-page clients
                            // stay at the measured-safe 3800 (main-thread
                            // drain 3.0-3.9k).
                            //
                            // The *effective* pace moves between the in-page
                            // floor and this ceiling on evidence: the browser's
                            // incoming-datagram queue has no flow control and
                            // silently drops from the head when the app reads
                            // slower than the host injects, so over-pacing is
                            // invisible from here except as loss the client
                            // never received. Back off when that happens, creep
                            // back while it does not.
                            let max_pace = if t.worker {
                                crate::media::wt::WORKER_PACE_PPS as u32
                            } else {
                                crate::media::wt::WT_PACE_PPS as u32
                            };
                            let floor_pace = crate::media::wt::WT_PACE_PPS as u32;
                            shared
                                .pace_max_pps
                                .store(max_pace, std::sync::atomic::Ordering::Relaxed);
                            let current =
                                shared.pace_pps.load(std::sync::atomic::Ordering::Relaxed);
                            if std::mem::take(&mut pace_fresh) || current == 0 || current > max_pace
                            {
                                shared
                                    .pace_pps
                                    .store(max_pace, std::sync::atomic::Ordering::Relaxed);
                                info!(
                                    pace_pps = max_pace,
                                    worker = t.worker,
                                    ceiling_kbps =
                                        (max_pace as f32 * crate::media::wt::WT_PACE_TO_CEILING) as u32,
                                    "wt: client datagram pace set - this is the hard ceiling on the encoder target"
                                );
                            }
                            // ---- inferred datagram loss ------------------
                            // The only sender-side evidence that datagrams did
                            // not arrive. Both counters cover the same classes
                            // (video data, FEC parity, audio), and both windows
                            // are ~1 s wide, so the difference is real loss -
                            // the browser's unusable head-drop, Wi-Fi, or a
                            // queue the socket could not drain. One RTT of skew
                            // remains (the client samples before its telemetry
                            // is sent), so a window needs real volume before
                            // the ratio means anything.
                            {
                                let sent_now = crate::media::wt::transport::datagrams_sent();
                                let seen_now = t.datagrams_seen;
                                let sample = loss_window.push(sent_now, seen_now);
                                if let Some(w) = sample.window_ppt {
                                    shared
                                        .wt_inferred_loss_ppt
                                        .store(w, std::sync::atomic::Ordering::Relaxed);
                                }
                                if let Some((ppt, sent_delta, seen_delta)) = sample.tick {
                                    let window_pct = sample.window_ppt.map(|w| w as f32 / 10.0);
                                    if ppt >= 30 {
                                        warn!(
                                            loss_pct = ppt as f32 / 10.0,
                                            ?window_pct,
                                            sent = sent_delta,
                                            seen = seen_delta,
                                            pace_pps = shared.pace_pps.load(
                                                std::sync::atomic::Ordering::Relaxed
                                            ),
                                            "wt: inferred datagram loss - the client did not receive what the host sent"
                                        );
                                    } else {
                                        debug!(
                                            loss_pct = ppt as f32 / 10.0,
                                            ?window_pct,
                                            sent = sent_delta,
                                            seen = seen_delta,
                                            "wt: inferred datagram loss"
                                        );
                                    }
                                    // Over-pacing looks exactly like loss and
                                    // nothing else, so the pace is what gives
                                    // way: 25% per two consecutive lossy
                                    // seconds, down to the measured-safe
                                    // in-page floor, and 5% of the ceiling per
                                    // clean second back up. This is what makes
                                    // raising WORKER_PACE_PPS safe - if a
                                    // client cannot drain it, the pace retreats
                                    // instead of the picture breaking.
                                    let lossy = shared
                                        .pace_lossy_windows
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let now_pace =
                                        shared.pace_pps.load(std::sync::atomic::Ordering::Relaxed);
                                    if ppt >= 90 {
                                        let lossy = lossy.saturating_add(1);
                                        shared
                                            .pace_lossy_windows
                                            .store(lossy, std::sync::atomic::Ordering::Relaxed);
                                        if lossy >= 2 && now_pace > floor_pace {
                                            let next = (now_pace * 3 / 4).max(floor_pace);
                                            shared
                                                .pace_pps
                                                .store(next, std::sync::atomic::Ordering::Relaxed);
                                            shared
                                                .pace_lossy_windows
                                                .store(0, std::sync::atomic::Ordering::Relaxed);
                                            warn!(
                                                from_pps = now_pace,
                                                to_pps = next,
                                                loss_pct = ppt as f32 / 10.0,
                                                "wt: client cannot drain this pace - backing the datagram injection off"
                                            );
                                        }
                                    } else if ppt <= 10 {
                                        shared
                                            .pace_lossy_windows
                                            .store(0, std::sync::atomic::Ordering::Relaxed);
                                        if now_pace < max_pace {
                                            let next = (now_pace + max_pace / 20).min(max_pace);
                                            shared
                                                .pace_pps
                                                .store(next, std::sync::atomic::Ordering::Relaxed);
                                        }
                                    }
                                } else {
                                    // Too little traffic to measure (< 500
                                    // datagrams this tick) is also far below
                                    // any pace, so the pace is not what is
                                    // being tested: recover it. Holding it
                                    // instead froze a backed-off pace for as
                                    // long as the screen stayed quiet - and
                                    // with it the encoder ceiling.
                                    let now_pace =
                                        shared.pace_pps.load(std::sync::atomic::Ordering::Relaxed);
                                    if now_pace < max_pace {
                                        shared.pace_pps.store(
                                            (now_pace + max_pace / 20).min(max_pace),
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                    }
                                }
                            }
                            let _ = shared.events.send(WtClientEvent::Telemetry(t));
                        }
                    }
                    Ok(WtClientMessage::Ping { at_us }) => {
                        // Answer in capture-clock µs so the client can map
                        // frame capture_us onto its own clock (NTP-style).
                        //
                        // From the pipeline clock itself when there is one.
                        // The fallback below projects the clock from the most
                        // recently SENT frame, which equates a frame's
                        // capture with its send: every age the client
                        // computed (HUD latency, the harness's e2e) silently
                        // left out capture, encode, queueing and pacing on
                        // this host - measured at 8.6 ms p50 / 28 ms p95.
                        let clock = shared.capture_clock.lock().clone();
                        let host_us = if let Some(now) = clock.and_then(|c| c()) {
                            now
                        } else {
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
                            // 300/s, not 60/s. The client's demand is bounded
                            // by its own round policy (<= 8 holes per frame
                            // per round, 4 rounds, >= 180 ms apart), but at
                            // high bitrate a frame is 30-40 fragments and the
                            // 80 Mbps session showed it asking up to 506/s
                            // while this handed out 60/s - so ~88 % of its
                            // repair requests were discarded, silently, and
                            // the frames never assembled (decoded 1-17 fps
                            // with the host dropping nothing at all). The
                            // storm guard still exists: 300/s ~ 2.7 Mbps of
                            // re-send traffic while damaged (a fraction of the
                            // fresh stream at these rates), and interleaved
                            // fragment order means most bursts are now
                            // repaired by parity with no NACK at all.
                            const RATE_PER_SEC: u32 = 300;
                            let (refill_at, tokens) = &mut *budget;
                            let refill_ms = refill_at.elapsed().as_millis() as u32;
                            *tokens = RATE_PER_SEC.min(*tokens + refill_ms * RATE_PER_SEC / 1000);
                            *refill_at = std::time::Instant::now();
                            if *tokens == 0 {
                                shared
                                    .wt_nack_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                continue;
                            }
                            *tokens -= 1;
                            if idx == inphase_protocol::NACK_WHOLE_FRAME {
                                // Every cached fragment of the frame, one
                                // budget token each. Nothing cached is not a
                                // miss worth reporting: keyframes ride the
                                // stream and are never cached, and the client
                                // cannot tell their numbers from a lost delta.
                                let hits: Vec<Vec<u8>> = shared
                                    .wt_resend
                                    .lock()
                                    .iter()
                                    .filter(|(no, _, _)| *no == frame)
                                    .map(|(_, _, enc)| enc.clone())
                                    .collect();
                                for (i, enc) in hits.iter().enumerate() {
                                    if i > 0 {
                                        if *tokens == 0 {
                                            shared
                                                .wt_nack_dropped
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            break;
                                        }
                                        *tokens -= 1;
                                    }
                                    let _ =
                                        crate::media::wt::transport::try_send_datagram(&conn, enc);
                                }
                                continue;
                            }
                            let hit = shared
                                .wt_resend
                                .lock()
                                .iter()
                                .rev()
                                .find(|(no, idx0, _)| *no == frame && *idx0 == idx)
                                .map(|(_, _, enc)| enc.clone());
                            match hit {
                                Some(enc) => {
                                    let _ =
                                        crate::media::wt::transport::try_send_datagram(&conn, &enc);
                                }
                                // The fragment has aged out of the 1024-entry
                                // cache. Counted rather than swallowed: a
                                // client NACKing a fragment the host can no
                                // longer produce is a frame that will only
                                // ever be recovered by an IDR.
                                None => {
                                    shared
                                        .wt_nack_missed
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
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
            while let Ok(bytes) = conn.receive_datagram().await {
                // A displaced session must not touch its successor's input.
                if *shared.active.lock() == Some(session_id) {
                    let _ = shared.events.send(WtClientEvent::Input(bytes.to_vec()));
                }
            }
        });
    }

    let reason = connection.closed().await;
    // The 2026-09-09 18:43 phone session died here SILENTLY: the connection
    // went at +11 s, the frame sender went idle (sent=0 logs nothing), and the
    // only clue was a health warning nine seconds later. Never again - name
    // the moment the video carrier died, and why: a client-side close (its
    // recovery ladder's redial), an idle timeout and a transport error need
    // different fixes, and the reason used to be discarded.
    info!(session_id, %reason, "wt: video connection closed");

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
        // The answer to this request must not depend on the reliable stream
        // alone: a write into a stream the client stopped reading succeeds
        // silently, and the client then waits forever for an IDR that was
        // "sent". See `Shared::key_by_datagram`.
        shared
            .key_by_datagram
            .store(true, std::sync::atomic::Ordering::Relaxed);
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

/// Write one JSON line; `false` when the stream is gone.
async fn write_msg(tx: &mut wtransport::SendStream, msg: &WtHostMessage) -> bool {
    let mut line = match serde_json::to_string(msg) {
        Ok(l) => l,
        Err(_) => return false,
    };
    line.push('\n');
    tx.write_all(line.as_bytes()).await.is_ok()
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

/// Sent-versus-seen datagram accounting for one connection.
///
/// The host samples its cumulative send count when a telemetry line arrives;
/// the client sampled its receive count one frame interval and a one-way delay
/// earlier. Differenced per second, that skew alone reads as +/-2 % "loss" and
/// as negative loss the next second (`sent=596 seen=840`, then a lossy tick),
/// and the bitrate controller refuses to climb on anything >= 1 % - so the
/// noise of the measurement itself was holding the rate down. Over a
/// four-second span the same skew is under half a percent.
///
/// Two answers come out of each sample: the per-tick figure, which the pace
/// backoff wants (it asks "is the client draining *now*", with a 9 %
/// threshold the skew cannot reach), and the windowed figure the controller
/// uses.
#[derive(Debug, Default)]
pub(crate) struct LossWindow {
    samples: std::collections::VecDeque<(u64, u64)>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct LossSample {
    /// (loss ppt, sent, seen) over the last tick; None without enough volume.
    pub tick: Option<(u32, u64, u64)>,
    /// Loss ppt over the whole window; None without enough volume.
    pub window_ppt: Option<u32>,
}

impl LossWindow {
    /// Intervals the controller's figure spans.
    const SPAN: usize = 4;
    /// Datagrams a tick needs before its ratio means anything.
    const MIN_TICK: u64 = 500;

    pub(crate) fn push(&mut self, sent: u64, seen: u64) -> LossSample {
        // A counter that went backwards is a new baseline (the client's
        // count restarts per connection), not a measurement: the first
        // telemetry of every session used to report `loss_pct=100 seen=0`
        // and count a lossy window against the pace.
        if self
            .samples
            .back()
            .is_some_and(|&(s0, r0)| sent < s0 || seen < r0)
        {
            self.samples.clear();
        }
        self.samples.push_back((sent, seen));
        while self.samples.len() > Self::SPAN + 1 {
            self.samples.pop_front();
        }
        let ratio = |(s0, r0): (u64, u64)| {
            let ds = sent - s0;
            let dr = seen - r0;
            (ds, dr, (ds.saturating_sub(dr) * 1000 / ds.max(1)) as u32)
        };
        let n = self.samples.len();
        let tick = (n >= 2)
            .then(|| ratio(self.samples[n - 2]))
            .filter(|&(ds, _, _)| ds >= Self::MIN_TICK)
            .map(|(ds, dr, ppt)| (ppt, ds, dr));
        let window_ppt = (n >= 2)
            .then(|| ratio(self.samples[0]))
            .filter(|&(ds, _, _)| ds >= Self::MIN_TICK)
            .map(|(_, _, ppt)| ppt);
        LossSample { tick, window_ppt }
    }
}

#[cfg(test)]
mod loss_window_tests {
    use super::LossWindow;

    #[test]
    fn sampling_skew_does_not_read_as_loss_over_the_window() {
        let mut w = LossWindow::default();
        let (mut sent, mut seen) = (10_000u64, 0u64);
        w.push(sent, seen);
        // 1300 datagrams/s sent and all received, but the client's snapshot
        // lands either side of a 40-datagram frame burst.
        let mut worst_window = 0;
        for i in 0..20 {
            sent += 1300;
            seen += if i % 2 == 0 { 1260 } else { 1340 };
            let s = w.push(sent, seen);
            if i >= 4 {
                worst_window = worst_window.max(s.window_ppt.unwrap());
            }
        }
        assert!(
            worst_window < 10,
            "window loss {worst_window} ppt from skew alone"
        );
    }

    #[test]
    fn a_per_tick_burst_is_still_visible_to_the_pace_backoff() {
        let mut w = LossWindow::default();
        w.push(0, 0);
        w.push(1000, 1000);
        let s = w.push(2000, 1700);
        assert_eq!(s.tick, Some((300, 1000, 700)));
        assert_eq!(s.window_ppt, Some(150));
    }

    #[test]
    fn a_new_connection_is_a_new_baseline_not_total_loss() {
        let mut w = LossWindow::default();
        w.push(50_000, 40_000);
        w.push(51_000, 41_000);
        // Reconnect: the client's count restarts.
        let s = w.push(52_000, 0);
        assert_eq!(s, Default::default());
        let s = w.push(53_000, 990);
        assert_eq!(s.tick, Some((10, 1000, 990)));
    }

    #[test]
    fn a_trickle_is_not_measured() {
        let mut w = LossWindow::default();
        w.push(0, 0);
        let s = w.push(100, 50);
        assert_eq!(s, Default::default());
    }
}
