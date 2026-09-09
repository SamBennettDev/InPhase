// WebTransport video client (ADR-0011 P2): dials QUIC with the certificate
// pinned by hash, presents the one-time token on the control stream, then
// reassembles datagrams into frames and mirrors control messages.
//
// Everything here is *additive* to the WebRTC path, which keeps flowing for
// audio, input, and control only — there is no WebRTC video (ADR-0011
// final): if this transport fails the glass freezes until the watchdog
// resets/redials the WT path itself.

import {
  parseFrame,
  WT_VIDEO_HEADER_LEN,
  type WtFrame,
} from "./wtvideo.js";

export interface WtVideoInfo {
  token: string;
  port: number;
  cert_sha256: string;
}

export interface WtVideoConfig {
  /** Bumped on every config push; the client acknowledges it (§5). */
  epoch: number;
  codec: string;
  width: number;
  height: number;
  fps: number;
  start_bitrate_kbps?: number;
  description?: Uint8Array | null;
}

/** Playout stats from the decoder, reported to the host once per second. */
export interface WtClientStats {
  codec: string | null;
  framesDecoded: number;
  framesDropped: number;
  /** Real freezes (present gaps far beyond the frame interval) - never a
   *  hardcoded zero (review §14). */
  freezeCount: number;
  totalFreezeMs: number;
}

export interface WtClientHandlers {
  onVideoConfig: (cfg: WtVideoConfig) => void;
  /** One complete frame, read off its own unidirectional stream. */
  onFrame: (f: WtFrame) => void;
  /** The transport died after being live; WebRTC video remains. */
  onClosed: (why: string) => void;
  /** Control-stream RTT estimate, ms. */
  onRtt?: (rttMs: number) => void;
  /** Host says the route cannot carry the chosen bitrate (§ respect inputs). */
  onRouteWarning?: (detail: string) => void;
  /** One Opus packet over WT audio datagrams (§11-on-WT). */
  onAudio?: (opus: Uint8Array, ptsUs: number) => void;
  /** Any other control-stream message (frame_stamps, session_ready, ...). */
  onControlMessage?: (data: string) => void;
}

/** Join the chunks a stream arrived in. Most frames come in one. */
function concat(parts: Uint8Array[], total: number): Uint8Array {
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.byteLength;
  }
  return out;
}

function hexToBytes32(hex: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(new ArrayBuffer(32));
  for (let i = 0; i < 32; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}


export class WtVideoClient {
  private wt: WebTransport | null = null;
  private controlWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private datagramsSeen = 0;
  /** Frames fully received off their own stream. */
  private framesReceived = 0;
  /** performance.now() of the last frame handed to the decoder. */
  private lastFrameAtMs = 0;
  /** Streams reset or dropped before the frame completed. */
  private framesAbandoned = 0;
  /** Streams whose reads never completed - cancelled to free flow credit. */
  private streamsWedged = 0;
  /** Writer for client -> host datagrams (input, §13). Null when the
   *  browser (or connection) has no datagram send path. */
  private inputWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  /** Dedicated input stream (no head-of-line blocking behind NACKs). */
  private inputStreamWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  /** Liveness: the pong clock and the datagram clock (silent-connection watchdog). */
  private lastPongAt = 0;
  private lastDatagramAt = 0;
  private pingTimer = 0;
  /** Guards onClosed: whatever kills the connection first reports, once. */
  private closedReported = false;
  private pingAtMs = 0;
  private pingSentUs = 0;
  private telemetryTimer = 0;
  private statsProvider: (() => WtClientStats) | null = null;
  // One-second accounting windows for the telemetry message.
  private winStartedMs = 0;
  private winBytes = 0;
  /** Cumulative decoded count at the last telemetry send, for a true rate. */
  private lastFramesDecoded = 0;
  private lastRttMs = 0;
  /** Host-clock − client-clock offset in µs (capture-clock aligned),
   *  EMA over pong samples; null until the first anchored pong. */
  private offsetEmaUs: number | null = null;
  private anchorWarned = false;
  /** Capture → decode-complete ages (ms) of recent frames, for the
   *  capture-to-present timeline (research doc §measurement). Bounded to the
   *  last 512 frames so percentiles reflect the current route, not history. */
  private latBuf: number[] = [];
  /** Per-frame fragment accounting for loss estimation. Counts frames that
   *  never assemble as well as those that do — see `fragloss.ts` for why that
   *  distinction is the difference between a usable loss signal and one that
   *  reports zero precisely when the route is worst. */
  /** Negotiated frame rate, for the NACK pacing window. */
  /** Consecutive undecodable datagrams (wire-mismatch detector). */
  private parseFailures = 0;

  get active(): boolean {
    return this.wt !== null;
  }

  /** Any live input writer? False while still dialing - callers skip. */
  inputReady(): boolean {
    return this.inputWriter !== null || this.inputStreamWriter !== null || this.controlWriter !== null;
  }

  /** Last pong-derived RTT (ms), 0 until the first pong. */  rttMs(): number {
    return Math.round(this.lastRttMs * 100) / 100;
  }

  /** Wire bytes seen this second (HUD kbps). */
  inboundKbps(): number {
    return Math.round((this.winBytes * 8) / 10) / 100;
  }

  async sendInput(bytes: Uint8Array): Promise<boolean> {
    // Dual-path input. iOS Safari never delivers client -> host datagrams
    // (2026-09-08: zero Input events from the phone across every session
    // while control-stream messages flowed), so the §12 packet ALSO rides
    // the reliable control stream. The host's sequence-freshness gate
    // applies whichever copy arrives first and drops the duplicate.
    let sent = false;
    const w = this.inputWriter;
    if (w) {
      try {
        await w.write(bytes);
        sent = true;
      } catch {
        this.inputWriter = null;
      }
    }
    if (this.inputStreamWriter) {
      try {
        const framed = new Uint8Array(bytes.length + 2);
        framed[0] = bytes.length >> 8;
        framed[1] = bytes.length & 0xff;
        framed.set(bytes, 2);
        await this.inputStreamWriter.write(framed);
        sent = true;
      } catch {
        this.inputStreamWriter = null;
      }
    }
    if (!sent) {
      // Last resort: the control stream (reliable but serialized behind
      // NACKs - only for browsers without the dedicated stream).
      let bin = "";
      for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
      const b64 = btoa(bin);
      void this.send({ type: "input", data_b64: b64.replace(/=+$/, "") });
      sent = this.controlWriter !== null;
    }
    return sent;
  }

  /** Ms since the last sign of life from the connection (pong or datagram). */
  staleMs(): number {
    const last = Math.max(this.lastPongAt, this.lastDatagramAt);
    return last === 0 ? 0 : performance.now() - last;
  }

  get datagramCount(): number {
    return this.datagramsSeen;
  }

  /** Playout stats the periodic telemetry message reports to the host. */
  setStatsProvider(p: () => WtClientStats): void {
    this.statsProvider = p;
  }

  async dial(info: WtVideoInfo, h: WtClientHandlers): Promise<void> {
    if (!("WebTransport" in globalThis)) throw new Error("WebTransport unsupported in this browser");
    const url = `https://${location.hostname}:${info.port}/wt-video`;
    console.info(`wt: dialing ${url} (pin ${info.cert_sha256.slice(0, 8)}…)`);
    const hash = hexToBytes32(info.cert_sha256);
    const wt = new WebTransport(url, {
      // Pinned self-signed host cert: trust comes from the authenticated
      // signaling socket delivering this hash, not from any CA (ADR-0011).
      serverCertificateHashes: [{ algorithm: "sha-256", value: hash }],
    });
    this.wt = wt;
    try {
      await wt.ready;
    } catch (e) {
      this.wt = null;
    this.inputWriter = null;
      throw new Error(`WebTransport handshake failed: ${String(e)}`);
    }
    wt.closed.then(
      (info) => h.onClosed(`clean (${info.closeCode})`),
      (e) => h.onClosed(String(e)),
    );

    // Control stream: token first, everything else is refused until then.
    const stream = await wt.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    this.controlWriter = writer;
    await writer.write(
      new TextEncoder().encode(JSON.stringify({ type: "auth", token: info.token }) + "\n"),
    );
    void this.readControl(stream.readable, h).catch((e) =>
      this.reportClosed(h, `wt control stream: ${String(e)}`),
    );
    void this.readDatagrams(wt, h).catch((e) =>
      this.reportClosed(h, `wt datagrams: ${String(e)}`),
    );
    // Video: frames arrive on the CLIENT-OPENED video channel (below) and, as
    // a fallback for older hosts, on server-initiated unidirectional streams.
    // iOS WebKit does not reliably deliver server-initiated streams to JS -
    // three failure shapes in one day (2026-09-09): v2 never-FIN credit
    // exhaustion, v3 per-frame early EOFs, v3 persistent stream silent after
    // one frame - while client-opened channels (control, input) never failed.
    // The new host sends video only on the client-opened channel; the old
    // host's per-frame uni streams still arrive here.
    void this.readFrameStreams(wt, h).catch((e) =>
      this.reportClosed(h, `wt video streams: ${String(e)}`),
    );
    void this.videoChannelLoop(wt, h).catch((e) =>
      this.reportClosed(h, `wt video channel: ${String(e)}`),
    );
    // Input datagrams (§13). Absent writer = browser without the send path
    // (older Safari); input then stays on the WebRTC data channel.
    try {
      this.inputWriter = wt.datagrams.writable.getWriter();
    } catch {
      this.inputWriter = null;
    }
    // The dedicated input stream: raw §12 packets, 2-byte BE length framing.
    // iOS never delivers client -> host datagrams; the control stream works
    // but serializes input behind NACK bursts (felt input lag under loss).
    try {
      const istream = await wt.createBidirectionalStream();
      this.inputStreamWriter = istream.writable.getWriter();
    } catch {
      this.inputStreamWriter = null;
    }
    // Periodic control ping → Pong gives the WT RTT (host answers on the
    // same stream; see media/wt.rs control reader).
    // Wire-mismatch watchdog: frames arriving that this build cannot parse
    // means the host was updated under us. Self-heal exactly once. (The
    // build-id guard usually catches this first; this is the backstop.)
    this.pingTimer = window.setInterval(() => {
      if (this.parseFailures > 30 && this.framesReceived === 0
          && !sessionStorage.getItem("inphase_reload")) {
        sessionStorage.setItem("inphase_reload", "1");
        console.warn("wt: frames arriving that this build cannot parse - host updated; reloading");
        location.reload();
      }
      this.pingAtMs = performance.now();
      this.pingSentUs = Math.round(this.pingAtMs * 1000);
      void this.send({ type: "ping", at_us: this.pingSentUs });
    }, 2000);
    // Once-a-second telemetry, shaped like the WebRTC ClientTelemetry so the
    // host's congestion controller can treat both paths identically.
    this.winStartedMs = performance.now();
    this.telemetryTimer = window.setInterval(() => this.sendTelemetry(), 1000);
  }

  /** Open and read the video channel: a bidirectional stream WE open, mark
   *  with a framed JSON packet, then read frames from. Reopened on any
   *  failure until the connection itself is gone - the host resets the
   *  channel when a write overruns its budget, and the fresh channel is how
   *  sending resumes. */
  private async videoChannelLoop(wt: WebTransport, h: WtClientHandlers): Promise<void> {
    const gone = wt.closed.then(() => "gone" as const);
    for (;;) {
      try {
        const vch = await wt.createBidirectionalStream();
        const marker = new TextEncoder().encode(JSON.stringify({ type: "video_channel" }));
        const framed = new Uint8Array(2 + marker.length);
        framed[0] = marker.length >> 8;
        framed[1] = marker.length & 0xff;
        framed.set(marker, 2);
        const writer = vch.writable.getWriter();
        await writer.write(framed);
        writer.releaseLock();
        // The marker needs an RTT to land, and frames queued before the host
        // has the channel are dropped - so ask for a fresh IDR now instead of
        // sitting on deltas that decode into nothing.
        void this.send({ type: "keyframe_request" });
        // v4 carrier: the channel deliberately goes silent (video moved to
        // datagrams). Park the reader instead of letting the 2 s watchdog
        // churn channels - the reopen loop cost one wedged stream and one
        // keyframe request every ~2.2 s for as long as v4 ran (21:53 log).
        // When the carrier reverts, the same channel resumes reading - the
        // host kept its sink, so the fallback needs no re-handshake.
        while (this.datagramVideoEnabled) {
          const closed = await Promise.race([
            gone,
            new Promise((r) => setTimeout(r, 500)),
          ]);
          if (closed === "gone") return;
        }
        await this.readOneFrame(vch.readable as ReadableStream<Uint8Array>, h);
      } catch {
        // channel reset or connection gone; reopen after a beat
      }
      if ((await Promise.race([gone, new Promise((r) => setTimeout(r, 250))])) === "gone") {
        return;
      }
    }
  }

  /** Audio stays on datagrams: a 10 ms Opus packet fits in one, and a late
   *  one is worth less than the next one. */
  private async readDatagrams(wt: WebTransport, h: WtClientHandlers): Promise<void> {
    const reader = wt.datagrams.readable.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done || value === undefined) break;
      this.datagramsSeen++;
      this.lastDatagramAt = performance.now();
      this.winBytes += value.byteLength;
      // Audio datagrams carry the 0x41 tag (see transport.rs); v4 video
      // fragments start with version byte 4 (see protocol wtvideo.rs). The
      // two never collide, so the reader demultiplexes on the first byte.
      if (value.byteLength > 13 && value[0] === 0x41) {
        const view = new DataView(value.buffer, value.byteOffset, value.byteLength);
        const ptsUs = Number(view.getBigUint64(5, false));
        h.onAudio?.(value.subarray(13), ptsUs);
        // A delivered audio datagram is the capability probe: host→client
        // datagrams work, so video can use them too (research doc Safari
        // matrix). Asked once per connection; the host logs the switch.
        if (!this.datagramVideoEnabled && this.datagramsSeen > 100) {
          this.datagramVideoEnabled = true;
          this.datagramVideoToggledAtMs = performance.now();
          void this.send({ type: "enable_datagram_video" });
        }
      } else if (value.byteLength >= 18 && value[0] === 4) {
        this.onVideoFragment(value, h);
      }
    }
  }

  // ---- v4: deadline-aware datagram video (research doc P0) ----

  /** Reassembly state for the v4 fragment carrier: frame_no → parts. */
  private v4 = new Map<
    number,
    { cnt: number; parts: (Uint8Array | null)[]; got: number; captureUs: number; key: boolean; atMs: number }
  >();
  private v4Received = 0;
  /** Whether we asked the host for the v4 carrier (and have not asked off). */
  private datagramVideoEnabled = false;
  private datagramVideoToggledAtMs = 0;

  /** Insert one v4 fragment; deliver the frame when its last part lands.
   *  Fragments of a frame missing its 150 ms assembly window are dropped -
   *  expired video is worthless (research doc freshness budget). */
  private onVideoFragment(d: Uint8Array, h: WtClientHandlers): void {
    const dv = new DataView(d.buffer, d.byteOffset, d.byteLength);
    const frameNo = dv.getUint32(2, true);
    const idx = dv.getUint16(6, true);
    const cnt = dv.getUint16(8, true);
    const captureUs = Number(dv.getBigUint64(10, true));
    const key = (dv.getUint8(1) & 1) !== 0;
    const nowMs = performance.now();
    // Expire stale assemblies first: a frame that never completed past its
    // playout window is abandoned, and its slot freed.
    for (const [no, st] of this.v4) {
      if (nowMs - st.atMs > 150) {
        this.v4.delete(no);
        this.framesAbandoned++;
      }
    }
    let st = this.v4.get(frameNo);
    if (st === undefined) {
      st = { cnt, parts: new Array(cnt).fill(null), got: 0, captureUs, key, atMs: nowMs };
      this.v4.set(frameNo, st);
      if (this.v4.size > 4) {
        // Keep only the newest frames; an old partial cannot playout anyway.
        const oldest = [...this.v4.keys()].sort((a, b) => a - b)[0];
        if (oldest !== undefined && oldest !== frameNo) this.v4.delete(oldest);
      }
    }
    if (idx >= cnt || st.parts[idx] !== null) return; // duplicate/overflow fragment
    st.parts[idx] = d.subarray(18);
    st.got++;
    if (st.got === cnt) {
      this.v4.delete(frameNo);
      let len = 0;
      for (const p of st.parts) len += p!.byteLength;
      const payload = new Uint8Array(len);
      let off = 0;
      for (const p of st.parts) {
        payload.set(p!, off);
        off += p!.byteLength;
      }
      this.v4Received++;
      this.framesReceived++;
      this.lastFrameAtMs = nowMs;
      if (this.offsetEmaUs !== null) {
        const ageMs = (performance.now() * 1000 - (captureUs + this.offsetEmaUs)) / 1000;
        if (ageMs >= 0 && ageMs < 5_000) {
          this.latBuf.push(ageMs);
          if (this.latBuf.length > 512) this.latBuf.shift();
        }
      }
      h.onFrame({ frame_no: frameNo, capture_us: captureUs, key, payload });
    }
  }

  /**
   * Video: one frame per unidirectional stream.
   *
   * Each stream carries ONE OR MORE self-delimited frames (v3 header: 18
   * bytes with the payload length). The host currently sends one frame per
   * stream; older sessions sent exactly one too - but the phone's WebKit
   * (2026-09-09 18:57) ended every per-frame stream before its payload, so
   * the host now batches frames onto one persistent stream. Reading in a
   * loop handles both shapes on any stream this build receives.
   * Streams are handled concurrently, because QUIC delivers them
   * independently and serialising here would reintroduce the head-of-line
   * blocking the whole transport exists to avoid.
   */
  private async readFrameStreams(wt: WebTransport, h: WtClientHandlers): Promise<void> {
    const streams = wt.incomingUnidirectionalStreams.getReader();
    for (;;) {
      const { value, done } = await streams.read();
      if (done || value === undefined) break;
      void this.readOneFrame(value as ReadableStream<Uint8Array>, h);
    }
  }

  private async readOneFrame(
    stream: ReadableStream<Uint8Array>,
    h: WtClientHandlers,
  ): Promise<void> {
    const reader = stream.getReader();
    // Per-stream read state: queued chunk excess + empty-read run (see
    // readExact). One abandon per stream; a multi-frame stream that dies
    // mid-frame loses only its tail.
    const state = { queued: [] as Uint8Array[], queuedBytes: 0, emptyReads: 0 };
    for (;;) {
      try {
        const head = await this.readExact(reader, WT_VIDEO_HEADER_LEN, state);
        const len = new DataView(head.buffer, head.byteOffset, head.byteLength)
          .getUint32(14, true);
        const payload = await this.readExact(reader, len, state);
        const buf = concat([head, payload], head.byteLength + payload.byteLength);
        this.winBytes += buf.byteLength;
        const frame = parseFrame(buf);
        if (frame === null) {
          // A frame this build cannot read means the host was updated under us.
          this.parseFailures++;
          return;
        }
        this.parseFailures = 0;
        this.framesReceived++;
        this.lastFrameAtMs = performance.now();
        // Capture → decode-complete age, via the pong clock anchor. The
        // anchor is quantized by RTT/2, so single samples are fuzzy - the
        // percentiles over hundreds of frames are the measurement.
        if (this.offsetEmaUs !== null) {
          const ageMs = (performance.now() * 1000 - (frame.capture_us + this.offsetEmaUs)) / 1000;
          if (ageMs >= 0 && ageMs < 5_000) {
            this.latBuf.push(ageMs);
            if (this.latBuf.length > 512) this.latBuf.shift();
          }
        }
        h.onFrame(frame);
      } catch {
        // Stream ended mid-frame (host reset, WebKit truncation, or the
        // connection went). This frame is gone; the next stream is already
        // on its way, and a reset that lands mid-frame costs one frame, not
        // the connection.
        this.framesAbandoned++;
        void reader.cancel().catch(() => {});
        return;
      }
    }
  }

  /** Read exactly `n` bytes off a stream reader, or fail.
   *
   *  Two WebKit realities shaped this (2026-09-09 phone sessions):
   *  - Chunks can EXCEED the bytes still needed (the transport coalesces
   *    writes), so excess is queued for the next readExact, never discarded.
   *    Discarding desyncs every frame after the first.
   *  - A stream can yield ZERO-BYTE chunks forever - reads resolve instantly,
   *    so no watchdog fires, no bytes ever arrive, and the loop spins
   *    silently (the recv=1-forever signature). A run of empty reads is
   *    therefore fatal to the stream, not something to spin on.
   *  Each real read also races a 2 s watchdog: a stalled stream is cancelled
   *  (STOP_SENDING returns flow credit) instead of blocking the frame loop.
   */
  private async readExact(
    reader: ReadableStreamDefaultReader<Uint8Array>,
    n: number,
    state: { queued: Uint8Array[]; queuedBytes: number; emptyReads: number },
  ): Promise<Uint8Array> {
    const out = new Uint8Array(n);
    let off = 0;
    while (off < n) {
      if (state.queuedBytes > 0) {
        const head = state.queued[0]!;
        const take = Math.min(head.byteLength, n - off);
        out.set(head.subarray(0, take), off);
        off += take;
        state.queuedBytes -= take;
        if (take === head.byteLength) state.queued.shift();
        else state.queued[0] = head.subarray(take);
        state.emptyReads = 0;
        continue;
      }
      const res = await Promise.race([
        reader.read(),
        new Promise<"wedged">((r) => setTimeout(() => r("wedged"), 2000)),
      ]);
      if (res === "wedged") {
        this.streamsWedged++;
        console.warn("wt: frame stream wedged - cancelled to release flow credit");
        void reader.cancel().catch(() => {});
        throw new Error("wedged");
      }
      const { value, done } = res;
      if (done || value === undefined) throw new Error("stream ended early");
      if (value.byteLength === 0) {
        // Zero bytes is a legal chunk, but a storm of them is the WebKit
        // silent-stream bug: fail fast so the caller abandons and reopens.
        if (++state.emptyReads > 64) {
          console.warn(`wt: ${state.emptyReads} empty chunks in a row - stream is a zombie`);
          this.streamsWedged++;
          void reader.cancel().catch(() => {});
          throw new Error("empty-chunk storm");
        }
        continue;
      }
      state.emptyReads = 0;
      state.queued.push(value);
      state.queuedBytes += value.byteLength;
    }
    return out;
  }

  /** One `client_telemetry`-shaped message per second over the control stream.
   *  Same shape as the WebRTC path's telemetry so the host's congestion
   *  controller (`media/bitrate.rs`) treats both carriers identically. */
  private sendTelemetry(): void {
    const nowMs = performance.now();
    const dtSec = Math.max(0.001, (nowMs - this.winStartedMs) / 1000);
    // v4 safety net: if the datagram carrier delivered nothing 5 s after we
    // asked for it - while the connection is otherwise alive (this line runs
    // on the control stream) - switch back to the reliable stream carrier.
    if (this.datagramVideoEnabled
        && this.v4Received === 0
        && nowMs - this.datagramVideoToggledAtMs > 5_000) {
      this.datagramVideoEnabled = false;
      console.warn("wt: datagram video carrier silent - reverting to the stream");
      void this.send({ type: "disable_datagram_video" });
    }
    const stats = this.statsProvider?.();
    // Wire-side counters, next to the decoder's on `window.__inphaseWt`.
    // Without these, "no frames arriving" and "arriving but not decoding" are
    // the same observation from outside - and they have opposite fixes. Every
    // freeze this session was diagnosed by guessing between them.
    (window as unknown as Record<string, unknown>).__inphaseWtWire = {
      framesReceived: this.framesReceived,
      framesAbandoned: this.framesAbandoned,
      parseFailures: this.parseFailures,
      streamsWedged: this.streamsWedged,
      datagramsSeen: this.datagramsSeen,
      lastFrameAgeMs: this.lastFrameAtMs === 0 ? -1 : Math.round(nowMs - this.lastFrameAtMs),
    };
    // No fragment-loss estimate any more: QUIC retransmits within a stream, so
    // the client cannot see loss and does not need to. What it can report is
    // frames that never completed - the host reset the stream, or the
    // connection went - which is the number that actually matters.
    const payload = {
      type: "client_telemetry",
      at_us: Math.round(nowMs * 1000),
      codec: stats?.codec ?? null,
      frames_decoded: stats?.framesDecoded ?? 0,
      // Rate over this window, not the cumulative count divided by it - that
      // reported 1617 fps on a 60 fps stream and fed the host nonsense.
      decoded_fps: stats
        ? Math.round(((stats.framesDecoded - this.lastFramesDecoded) / dtSec) * 100) / 100
        : 0,
      frames_dropped: stats?.framesDropped ?? 0,
      // WT has no browser jitter buffer; the adaptive present delay is the
      // closest analogue and is what the HUD should show.
      jitter_buffer_target_ms: 0,
      jitter_buffer_delay_ms: 0,
      packets_lost: 0,
      rtt_ms: Math.round(this.lastRttMs * 100) / 100,
      inbound_bitrate_kbps: Math.round((this.winBytes * 8) / dtSec / 1000),
      // Frames that arrived but never assembled. On a route that cannot carry
      // the current resolution this is the whole story, and it is invisible in
      // every other counter: decode fps stays 0 with no error anywhere.
      frames_dropped_incomplete: this.framesAbandoned,
      freeze_count: stats?.freezeCount ?? 0,
      total_freeze_ms: stats?.totalFreezeMs ?? 0,
      // Wire-side counters again, for the host's log: the split between
      // "frames arrived and never assembled" (route) and "streams never
      // yielded bytes" (WebKit read wedging) is the diagnosis.
      frames_received: this.framesReceived,
      streams_wedged: this.streamsWedged,
      datagrams_seen: this.datagramsSeen,
      // Capture → decode-complete latency (docs/research/performance-latency-
      // 2026-09-09.md §measurement): frame capture_us mapped onto the client
      // clock via the pong anchor. Percentiles, not an EMA - the spikes define
      // remote-play quality. Null until the clock syncs (first anchored pong).
      lat_p50_ms: this.latPercentile(0.5),
      lat_p95_ms: this.latPercentile(0.95),
      // §13: the host needs real support data from real clients.
      audio_opus_supported: opusSupportProbe(),
    };
    void this.send(payload);
    this.lastFramesDecoded = stats?.framesDecoded ?? this.lastFramesDecoded;
    this.winStartedMs = nowMs;
    this.winBytes = 0;
  }

  private async readControl(
    readable: ReadableStream<Uint8Array>,
    h: WtClientHandlers,
  ): Promise<void> {
    const reader = readable.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        this.onControlLine(line, h);
      }
    }
    // Clean EOF: the host went away without an error frame.
    this.reportClosed(h, "wt control stream ended");
  }

  private onControlLine(line: string, h: WtClientHandlers): void {
    let m: { type?: string } & Record<string, unknown>;
    try {
      m = JSON.parse(line) as { type?: string } & Record<string, unknown>;
    } catch {
      return;
    }
    if (m.type === "video_config") {
      let description: Uint8Array | null = null;
      if (typeof m.description_b64 === "string") {
        const bin = atob(m.description_b64);
        description = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) description[i] = bin.charCodeAt(i);
      }
      const fps = Number(m.fps ?? 0);
      h.onVideoConfig({
        epoch: Number(m.epoch ?? 0),
        codec: String(m.codec ?? ""),
        width: Number(m.width ?? 0),
        height: Number(m.height ?? 0),
        fps,
        start_bitrate_kbps: m.start_bitrate_kbps === undefined ? undefined : Number(m.start_bitrate_kbps),
        description,
      });
    } else if (m.type === "pong" && typeof m.at_us === "number") {
      // The host echoes our performance.now()-µs timestamp.
      this.lastRttMs = performance.now() - Number(m.at_us) / 1000;
      h.onRtt?.(this.lastRttMs);
      // NTP-style offset: host receive time (on the capture clock) minus the
      // client's ping↔pong midpoint. Assumes the host handled the ping
      // instantly — error bounded by rtt/2, smoothed by the EMA.
      const hostUs = Number(m.host_us ?? 0);
      if (hostUs > 0) {
        const t2Us = performance.now() * 1000;
        // offset maps capture-clock → client clock: client_midpoint −
        // host_capture_now. The inverse sign fed lat ages ≈ 2× the epoch
        // gap (≈6 s), which the 0..5 s plausibility gate discarded - every
        // percentile reported -1 for the whole 21:51 session.
        const sample = (this.pingSentUs + t2Us) / 2 - hostUs;
        const wasUnsynced = this.offsetEmaUs === null;
        this.offsetEmaUs = this.offsetEmaUs === null ? sample : 0.8 * this.offsetEmaUs + 0.2 * sample;
        if (wasUnsynced) {
          console.info(
            `wt clock sync: offset ${(this.offsetEmaUs / 1000).toFixed(1)} ms (host_us ${hostUs})`,
          );
        }
      } else if (!this.anchorWarned) {
        this.anchorWarned = true;
        console.info("wt pong without host_us — host clock anchor not set yet");
      }
    } else if (m.type === "route_warning" && typeof m.detail === "string") {
      h.onRouteWarning?.(m.detail);
    } else if (m.type === "error") {
      this.reportClosed(h, `host refused: ${String(m.code ?? "?")}`);
    } else {
      h.onControlMessage?.(line);
    }
  }

  /** Report connection death exactly once, whatever kills it first. */
  private reportClosed(h: WtClientHandlers, why: string): void {
    if (this.closedReported) return;
    this.closedReported = true;
    h.onClosed(why);
  }

  async send(msg: Record<string, unknown>): Promise<void> {
    if (this.controlWriter === null) return;
    try {
      await this.controlWriter.write(new TextEncoder().encode(JSON.stringify(msg) + "\n"));
    } catch {
      // stream gone; the closed handler reports it
    }
  }

  requestKeyframe(): void {
    void this.send({ type: "keyframe_request" });
  }

  /** Host↔client clock offset (µs, capture-clock aligned) once synced. */
  clockOffsetUs(): number | null {
    return this.offsetEmaUs;
  }

  /** Percentile (0-1) of recent capture → decode ages, or -1 while unknown
   *  (clock unsynced or no frames yet). */
  latPercentile(p: number): number {
    if (this.offsetEmaUs === null || this.latBuf.length === 0) return -1;
    const sorted = [...this.latBuf].sort((a, b) => a - b);
    const idx = Math.min(sorted.length - 1, Math.floor(p * sorted.length));
    return Math.round((sorted[idx] ?? -1) * 10) / 10;
  }

  /** Upper bound on the offset error (ms): the pong midpoint assumption is
   *  off by at most rtt/2. null while unsynced. */
  syncErrorMs(): number | null {
    return this.offsetEmaUs === null ? null : this.lastRttMs / 2;
  }

  close(): void {
    this.closedReported = true; // intentional close: no onClosed callback
    if (this.pingTimer !== 0) window.clearInterval(this.pingTimer);
    if (this.telemetryTimer !== 0) window.clearInterval(this.telemetryTimer);
    this.pingTimer = 0;
    this.telemetryTimer = 0;
    void this.controlWriter?.close().catch(() => {});
    this.controlWriter = null;
    this.wt?.close();
    this.wt = null;
    this.inputWriter = null;
  }
}

// §13: WebCodecs Opus decode support on this browser, probed once; the result
// rides client telemetry. Absent AudioDecoder or a failed probe = undefined.
let opusProbeResult: boolean | undefined;
let opusProbeStarted = false;
export function opusSupportProbe(): boolean | undefined {
  if (!opusProbeStarted) {
    opusProbeStarted = true;
    try {
      if ("AudioDecoder" in globalThis) {
        void AudioDecoder.isConfigSupported({
          codec: "opus",
          sampleRate: 48000,
          numberOfChannels: 2,
        })
          .then((r) => {
            opusProbeResult = r.supported;
            console.info(`wt: opus-over-WT audio probe: ${r.supported ? "supported" : "unsupported"}`);
          })
          .catch(() => {
            opusProbeResult = false;
            console.info("wt: opus-over-WT audio probe: failed");
          });
      } else {
        opusProbeResult = false;
        console.info("wt: opus-over-WT audio probe: no AudioDecoder");
      }
    } catch {
      opusProbeResult = false;
    }
  }
  return opusProbeResult;
}
