// WebTransport video client core (ADR-0011 P2): dials QUIC with the
// certificate pinned by hash, presents the one-time token on the control
// stream, then reassembles datagrams into frames and mirrors control
// messages.
//
// Runs wherever it is imported: in the page (legacy path) or inside the
// Dedicated Worker (wtworker.ts) that keeps the datagram drain loop off the
// main thread — the browser's incoming-datagram queue drops from the HEAD
// when the app reads slower than the host injects, and the main thread is
// what rendering and GC contend with (05:08 Chrome: collapse at ~5.5k
// datagrams/s; main-thread drain measures 3.0-3.9k/s).
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
  /** Page origin hostname — workers have no page `location`, so the dialer
   *  supplies it. Optional: falls back to the ambient `location`. */
  hostname?: string;
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
/** Assembly state for one v4 frame in flight. */
interface V4Assembly {
  cnt: number;
  parts: (Uint8Array | null)[];
  got: number;
  captureUs: number;
  key: boolean;
  atMs: number;
  /** NACK rounds sent for this frame + when the last went out. */
  nacks: number;
  lastNackMs: number;
  /** FEC parity payloads by group index (row 1 / row 2). */
  p1: Map<number, Uint8Array> | null;
  p2: Map<number, Uint8Array> | null;
  /** The frame's total data length, from any parity trailer; sizes the final
   *  fragment when parity has to rebuild it. null until a parity arrives. */
  total: number | null;
}

/** Parity payloads end with the frame's total data length (u32 LE) - see
 *  `PARITY_TRAILER_LEN` in crates/protocol/src/wtvideo.rs. */
export const PARITY_TRAILER_LEN = 4;

/** `nack.idx` for "every fragment of this frame" (`NACK_WHOLE_FRAME` in
 *  crates/protocol/src/wtvideo.rs). */
export const NACK_WHOLE_FRAME = 0xffff;

/** When a frame's missing fragments are first asked for, and how often after.
 *
 *  Fixed at 150 / 180 ms these were sized for a 60-130 ms relay, and on a LAN
 *  they were most of the repair: with a 5 ms RTT the re-send lands ~5 ms after
 *  it is asked for, but the frame - and every frame queued behind it - froze
 *  150 ms first. Scaled to the measured RTT, with floors that still let a
 *  paced frame's own fragments arrive before any of them is presumed lost.
 *  RTT unknown (0) keeps the old timings. */
export function nackTiming(rttMs: number): { firstMs: number; gapMs: number } {
  if (!(rttMs > 0)) return { firstMs: 150, gapMs: 180 };
  const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
  return { firstMs: clamp(3 * rttMs + 25, 40, 150), gapMs: clamp(3 * rttMs + 30, 50, 180) };
}

/** Widest frame-number jump treated as loss rather than a new sequence. */
const WHOLE_FRAME_NACK_SPAN = 16;

export interface WtClientStats {
  codec: string | null;
  framesDecoded: number;
  framesDropped: number;
  /** Frames actually drawn to the canvas this session. */
  framesPresented: number;
  /** Decoder's smoothed present rate (fps). Used when the worker reuses a
   *  stale snapshot and the cumulative counter did not move this tick. */
  presentedFps?: number;
  /** Reorder buffer depth (frames held behind a hole) and codec backlog. */
  held: number;
  queueSize: number;
  /** Frames the decoder queue skipped (backpressure drops). */
  behindEvents: number;
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
  /** Frames arriving this build cannot parse = the host updated under us.
   *  The page reloads (sessionStorage-guarded); in a Worker the shell
   *  forwards this to the page, which owns sessionStorage/location. */
  onReload?: () => void;
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


/**
 * Frames kept for the latency percentiles the host's congestion controller
 * reads as `client_lat_p95_ms`.
 *
 * This was 512 (~8.5 s of video at 60 fps), which made the signal far slower
 * than the thing it describes: one tail spike held p95 above the controller's
 * 120 ms threshold for seconds after the route had cleared, and the delay
 * branch cut on every tick off that stale number — 4000 -> 600 kbps in six
 * seconds while the client decoded 60 fps at a 7 ms RTT. Two seconds is still
 * 120 samples for a p95, and it answers the question the controller is asking:
 * is the tail high *now*.
 */
const LAT_SAMPLES = 120;

/** Video channels kept open for forced IDRs (see `openSpareChannel`). */
const MAX_SPARE_CHANNELS = 4;

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
  /** framesAbandoned by cause: a datagram assembly that timed out, and a
   *  stream that ended mid-frame. One total could not say which carrier was
   *  losing frames, and the two have unrelated fixes. */
  private v4Expired = 0;
  /** Highest frame number seen on the datagram carrier, and the frames
   *  already asked for whole - one request each. */
  private v4Highest: number | null = null;
  private wholeNacked = new Set<number>();
  private wholeFrameNacks = 0;
  private streamAborted = 0;
  /** Partial assemblies dropped to bound memory, separately from the ones the
   *  expiry scan collected: the first is self-inflicted, the second is the
   *  route's fault, and only the split makes the difference visible. */
  private assemblyEvicted = 0;
  /** Partial *keyframe* assemblies evicted - only when every assembly in
   *  flight is a keyframe. Each one is an IDR the client asked for and threw
   *  away, so it is counted and re-requested rather than dropped silently. */
  private keyPartialsEvicted = 0;
  /** Frames dropped as the second copy of one already delivered on the other
   *  carrier (the host sends a repair IDR on both, audit §3.5). */
  private framesDuplicate = 0;
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
  private pingTimer: ReturnType<typeof setInterval> | 0 = 0;
  /** Guards onClosed: whatever kills the connection first reports, once. */
  private closedReported = false;
  private pingAtMs = 0;
  private pingSentUs = 0;
  private telemetryTimer: ReturnType<typeof setInterval> | 0 = 0;
  private statsProvider: (() => WtClientStats) | null = null;
  /** Wire-counter snapshot per telemetry tick. The page writes it to
   *  `window.__inphaseWtWire`; the worker shell forwards it. */
  onWire: ((w: Record<string, unknown>) => void) | null = null;
  /** Set by the worker shell (wtworker.ts): the host paces this connection
   *  for a dedicated-thread drain when true. */
  inWorker = false;
  // One-second accounting windows for the telemetry message.
  private winStartedMs = 0;
  private winBytes = 0;
  /** Datagrams read this window = the client's measured drain rate. The host
   *  paces injection below it: the browser's incoming-datagram queue silently
   *  drops from the head when the app reads slower than the host sends
   *  (no flow control on RFC 9221 datagrams, no loss signal back). */
  private winDgrams = 0;
  /** Cumulative decoded count at the last telemetry send, for a true rate. */
  private lastFramesDecoded = 0;
  /** Cumulative presented count at the last telemetry send. */
  private lastFramesPresented = 0;
  /** Last positive present rate (fps). Worker telemetry and the page stats
   *  push are unsynced 1 Hz loops; a tick that reuses the previous snapshot
   *  would otherwise report presented_fps: 0 and trip the host alarm. */
  private lastPresentedRate = 0;
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
    const url = `https://${info.hostname ?? globalThis.location?.hostname}:${info.port}/wt-video`;
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
    this.wtRef = wt;
    this.handlers = h;
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
    this.pingTimer = setInterval(() => {
      if (this.parseFailures > 30 && this.framesReceived === 0) {
        console.warn("wt: frames arriving that this build cannot parse - host updated; reloading");
        h.onReload?.();
      }
      this.pingAtMs = performance.now();
      this.pingSentUs = Math.round(this.pingAtMs * 1000);
      void this.send({ type: "ping", at_us: this.pingSentUs });
    }, 2000);
    // Once-a-second telemetry, shaped like the WebRTC ClientTelemetry so the
    // host's congestion controller can treat both paths identically.
    //
    // In worker mode the cadence belongs to the PAGE: the decoder counters live
    // on the main thread and are pushed here once a second, and `decoded_fps`
    // is a delta of those counters over the interval between telemetry sends.
    // Two independent 1 Hz loops let a tick reuse the previous snapshot: it
    // reports 0 fps, the next one reports double (0 / 120 / 0 / 110 on a 60 fps
    // stream), the host's loss and health laws read a decoder that keeps dying,
    // and the page is fine the whole time. So the page's push IS the tick
    // (`telemetryNow`), and there is no timer here to drift against it.
    this.winStartedMs = performance.now();
    if (!this.inWorker) {
      this.telemetryTimer = setInterval(() => this.sendTelemetry(), 1000);
    }
  }

  /** Compose and send one telemetry message now.
   *
   *  Test hook and worker-mode cadence: the worker shell calls this from its
   *  page-stats handler, so the counters and the window they are differenced
   *  over always describe the same interval. */
  telemetryNow(): void {
    this.sendTelemetry();
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
        // Keep reading while the v4 carrier is on too: forced IDRs ride THIS
        // stream (flow-controlled, cannot drop) while deltas ride datagrams.
        // The channel is long-idle between IDRs, so readExact skips its 2 s
        // wedge watchdog in v4 mode — the park loop that used to be here
        // would have eaten every forced IDR.
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
    // The browser's incoming datagram queue has no flow control and drops
    // what does not fit. At 1440p120 / 80 Mbps a frame is ~80 datagrams, and
    // a Mac Chrome session lost 1-4 % of them on some seconds with QUIC
    // reporting zero packet loss - dropped after arrival, in this queue. A
    // deeper queue absorbs a frame's burst; the reader still drains it.
    try {
      (wt.datagrams as unknown as { incomingHighWaterMark: number }).incomingHighWaterMark = 4096;
    } catch {
      /* not settable in this browser */
    }
    const reader = wt.datagrams.readable.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done || value === undefined) break;
      this.datagramsSeen++;
      this.winDgrams++;
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
    // The loop only ends when the datagram stream ends. Ending silently is how
    // a whole session stalled invisibly: the host logged `sent=0 … evicted=121`
    // (encoding 120 fps into a void) while the UI sat on "waiting for a
    // keyframe", and nothing on either side reported a fault. Say so instead —
    // a dead transport must not look like a decode problem.
    this.reportClosed(h, "wt datagram stream ended");
  }

  // ---- v4: deadline-aware datagram video (research doc P0) ----

  /** Reassembly state for the v4 fragment carrier: frame_no → parts. */
  private v4 = new Map<
    number,
    V4Assembly
  >();
  private v4Received = 0;
  private keysReceived = 0;
  /** Fragments rebuilt locally from FEC parity - no NACK, no RTT. */
  private v4Repaired = 0;
  /** NACKs sent for missing fragments (diagnosis: repair rate vs IDR waits). */
  private nacksSent = 0;
  /** Frame numbers already assembled - late re-sends land here and stop. */
  private v4Done = new Set<number>();
  /** frame_no -> capture_us of frames handed to the page, on either carrier.
   *  v4Done cannot serve: it also tombstones frames that were *abandoned*, and
   *  the stream copy of one of those is exactly the frame we still need. */
  private delivered = new Map<number, number>();
  /** Whether we asked the host for the v4 carrier (and have not asked off). */
  /** Session handle, so a keyframe request can open a fresh channel later. */
  private wtRef: WebTransport | null = null;
  private handlers: WtClientHandlers | null = null;
  /** Extra video channels opened for forced IDRs, newest last. */
  private spares: WebTransportBidirectionalStream[] = [];
  private channelReopenMs = 0;

  private datagramVideoEnabled = false;
  private datagramVideoToggledAtMs = 0;

  /** Insert one v4 fragment (data or parity); deliver the frame when its
   *  data parts are all present. Repair is layered, cheapest first:
   *  1. FEC parity (8+1 groups, 8+2 keyframes): one lost fragment per group
   *     is rebuilt locally - no NACK, no RTT. The 23:21 session showed
   *     NACK-only repair amplifying congestion (nacks 65 -> 10789 in 8 s,
   *     the re-send storm crowding out fresh video, ending client-silent).
   *  2. NACK, bounded: at most 8 holes per frame per round, rounds every
   *     180 ms, max 4 - a fallback, not a flood. */
  private onVideoFragment(d: Uint8Array, h: WtClientHandlers): void {
    const dv = new DataView(d.buffer, d.byteOffset, d.byteLength);
    const frameNo = dv.getUint32(2, true);
    const idx = dv.getUint16(6, true);
    const cnt = dv.getUint16(8, true);
    const captureUs = Number(dv.getBigUint64(10, true));
    const flags = dv.getUint8(1);
    const key = (flags & 1) !== 0;
    const parityRow = (flags >> 1) & 3; // 0 data, 1 row1, 2 row2
    const nowMs = performance.now();
    this.nackSkippedFrames(frameNo);
    if (this.v4Done.has(frameNo)) return; // late re-send for an assembled frame
    // Expire stale assemblies: NACK rounds are the bounded fallback. The
    // expired frame goes into v4Done: the host's re-sends for it keep
    // arriving after expiry, and without the tombstone each one
    // resurrected a fresh partial frame that NACKed again - the unbounded
    // loop that ran nacks to 7183 with decode at 0 (23:59 session).
    const nt = nackTiming(this.lastRttMs);
    for (const [no, st] of this.v4) {
      if (nowMs - st.atMs > 900) {
        this.v4.delete(no);
        this.v4Done.add(no);
        if (this.v4Done.size > 512) {
          this.v4Done.delete(this.v4Done.values().next().value as number);
        }
        this.framesAbandoned++;
        this.v4Expired++;
      } else if (
        nowMs - st.atMs > nt.firstMs &&
        st.nacks < 4 &&
        nowMs - st.lastNackMs > nt.gapMs
      ) {
        st.nacks++;
        st.lastNackMs = nowMs;
        // At most 8 datagrams per round per frame: the 23:21 storm
        // re-NACKed hundreds of holes per round across 8 partial frames.
        //
        // *Which* 8 matters. This loop used to walk every missing index from
        // 0 upward and stop at 8, so a frame with more than 8 holes never
        // requested the higher ones in any of its four rounds - it could not
        // assemble no matter how much the host re-sent, and the audit of a
        // 30-fragment IDR (fragments k..29 arrived, 0..k-1 destroyed by the
        // send buffer) is exactly that shape. And two holes inside one
        // 8-fragment group are a wasted datagram: that group is FEC-dead
        // until one of them is restored, so asking for the second can only
        // help after the first lands. So: one hole per group, rotating the
        // start group each round. A 9-hole frame spread over four groups goes
        // from "never recoverable" to four requests whose partners row-1
        // parity then rebuilds locally, at the same NACK volume.
        const groupCount = Math.ceil(st.cnt / 8);
        let budget = 8;
        for (let g = 0; g < groupCount && budget > 0; g++) {
          const gi = (g + st.nacks) % groupCount;
          const lo = gi * 8;
          const hi = Math.min(lo + 8, st.cnt);
          const missing: number[] = [];
          for (let i = lo; i < hi; i++) if (st.parts[i] === null) missing.push(i);
          if (missing.length === 0) continue;
          // Rotate *within* the group as well: a group with two holes must
          // eventually ask for the second one, or the pair is stuck forever
          // (row 1 can only restore one of them).
          const idx = missing[(st.nacks + gi) % missing.length]!;
          budget--;
          this.nacksSent++;
          void this.send({ type: "nack", frame: no, idx });
        }
      }
    }
    // This datagram may itself be a re-send for a frame the scan just
    // expired - the tombstone must win over state creation.
    if (this.v4Done.has(frameNo)) return;
    let st = this.v4.get(frameNo);
    if (st === undefined) {
      st = {
        cnt,
        parts: new Array(cnt).fill(null),
        got: 0,
        captureUs,
        key,
        atMs: nowMs,
        nacks: 0,
        lastNackMs: 0,
        p1: null,
        p2: null,
        total: null,
      };
      this.v4.set(frameNo, st);
      if (this.v4.size > 24) {
        // Keep only the newest frames: an old partial cannot play out anyway,
        // and 900 ms of a 60 fps stream is 54 of them, so 24 covers any burst
        // the expiry scan has not yet collected. Evict a *delta* before a
        // keyframe when there is a choice - the keyframe is what FrameOrderer
        // is blocked on, and dropping it discarded the one frame that would
        // have released everything behind it (the "aband flat, held climbing,
        // decoder idle" signature: the client threw away the frame it was
        // waiting for, tombstoned it so a re-send could not resurrect it, and
        // never counted it, so nothing on either side could see it happen).
        //
        // Never a keyframe while any delta is in flight - the newcomer
        // included: a lost delta costs one hole the next IDR repairs, a lost
        // IDR partial is unrecoverable (its parity and re-sends are refused
        // once tombstoned) and was the likeliest way a 29 KB IDR "did not
        // assemble" (audit §2.3). The old `?? order[0]` fallback could pick one.
        const order = [...this.v4.keys()].sort((a, b) => a - b);
        const isKey = (n: number) => this.v4.get(n)!.key;
        let victim = order.find((n) => n !== frameNo && !isKey(n));
        if (victim === undefined && !key) victim = frameNo;
        if (victim === undefined) {
          // Every assembly is a keyframe: evicting one is unavoidable. Count
          // it and ask again, so the drop is visible and repaired rather than
          // left for the orderer to discover a budget later.
          victim = order.find((n) => n !== frameNo);
          if (victim !== undefined) {
            this.keyPartialsEvicted++;
            this.requestKeyframe();
          }
        }
        if (victim !== undefined) {
          this.v4.delete(victim);
          this.v4Done.add(victim); // tombstone: no resurrection, no re-NACK
          this.assemblyEvicted++;
          this.framesAbandoned++;
          if (victim === frameNo) return;
        }
      }
    }
    if (parityRow !== 0) {
      // Parity creates the assembly just like data does: its header carries
      // the frame's count, capture time and key flag. When parity was parked
      // until a data fragment turned up, a frame whose data was lost entirely -
      // every single-fragment frame that lost its one datagram - had no
      // assembly, so it was neither rebuilt nor NACKed, only abandoned.
      if (d.byteLength < 18 + PARITY_TRAILER_LEN) return;
      st.total = new DataView(d.buffer, d.byteOffset + d.byteLength - 4, 4).getUint32(0, true);
      if (st.p1 === null) st.p1 = new Map();
      if (st.p2 === null) st.p2 = new Map();
      (parityRow === 1 ? st.p1 : st.p2).set(idx, d.subarray(18, d.byteLength - PARITY_TRAILER_LEN));
      this.tryRepair(st);
      this.maybeDeliver(st, frameNo, h, nowMs);
      return;
    }
    if (idx >= cnt || st.parts[idx] !== null) return; // duplicate/overflow fragment
    st.parts[idx] = d.subarray(18);
    st.got++;
    this.tryRepair(st);
    this.maybeDeliver(st, frameNo, h, nowMs);
  }

  /** Deliver a v4 frame the moment every data fragment is present (whether
   *  the last one arrived on the wire or was rebuilt from parity). */
  private maybeDeliver(
    st: V4Assembly,
    frameNo: number,
    h: WtClientHandlers,
    nowMs: number,
  ): void {
    if (st.got !== st.cnt) return;
    {
      this.v4.delete(frameNo);
      this.v4Done.add(frameNo);
      if (this.v4Done.size > 512) {
        this.v4Done.delete(this.v4Done.values().next().value as number);
      }
      if (!this.firstDelivery(frameNo, st.captureUs)) {
        this.framesDuplicate++;
        return;
      }
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
      if (st.key) this.keysReceived++;
      this.lastFrameAtMs = nowMs;
      if (this.offsetEmaUs !== null) {
        const ageMs = (performance.now() * 1000 - (st.captureUs + this.offsetEmaUs)) / 1000;
        if (ageMs >= 0 && ageMs < 5_000) {
          this.latBuf.push(ageMs);
          if (this.latBuf.length > LAT_SAMPLES) this.latBuf.shift();
        }
      }
      h.onFrame({ frame_no: frameNo, capture_us: st.captureUs, key: st.key, payload });
    }
  }

  /** Record a frame as handed to the page; false if it already was, on
   *  either carrier. Keyed on capture time too, so a frame number reused by a
   *  restarted host is not mistaken for a copy. */
  private firstDelivery(frameNo: number, captureUs: number): boolean {
    if (this.delivered.get(frameNo) === captureUs) return false;
    this.delivered.set(frameNo, captureUs);
    if (this.delivered.size > 512) {
      this.delivered.delete(this.delivered.keys().next().value as number);
    }
    return true;
  }

  /**
   * A datagram for frame F when the last one seen was F-3 means F-2 and F-1
   * sent nothing that arrived - not a fragment, not their parity. A small
   * frame is one fragment plus parity, which QUIC packs into ONE packet, so a
   * single lost packet erases it without trace: no assembly, so no per-
   * fragment NACK and no FEC, and the orderer's 300 ms give-up and an IDR were
   * the only repair (~400 ms freeze every few seconds at 1 % loss). Ask the
   * host for the whole frame straight away; its re-send creates the assembly,
   * and the ordinary NACK/FEC path covers anything still missing.
   *
   * Keyframes ride the stream carrier, so their numbers show up here as gaps
   * too; the host re-sends nothing for them, which costs one small control
   * message per IDR.
   */
  private nackSkippedFrames(frameNo: number): void {
    const last = this.v4Highest;
    const ahead = last === null ? 1 : (frameNo - last) >>> 0;
    if (last !== null && ahead >= 0x80000000) return; // older than the newest: reorder or re-send
    this.v4Highest = frameNo;
    if (last === null || ahead <= 1 || ahead > WHOLE_FRAME_NACK_SPAN) return;
    for (let k = 1; k < ahead; k++) {
      const m = (last + k) >>> 0;
      if (this.v4.has(m) || this.v4Done.has(m) || this.delivered.has(m) || this.wholeNacked.has(m)) continue;
      this.wholeNacked.add(m);
      if (this.wholeNacked.size > 256) {
        this.wholeNacked.delete(this.wholeNacked.values().next().value as number);
      }
      this.wholeFrameNacks++;
      this.nacksSent++;
      void this.send({ type: "nack", frame: m, idx: NACK_WHOLE_FRAME });
    }
  }

  /** True length of the frame's final fragment, or null if not derivable:
   *  the total from a parity trailer, less the non-final fragments (each one
   *  datagram budget - read off any that is present). */
  private finalLength(st: V4Assembly): number | null {
    if (st.total === null) return null;
    if (st.cnt === 1) return st.total;
    const full = st.parts.find((p, i) => p !== null && i < st.cnt - 1);
    if (full === undefined || full === null) return null;
    const len = st.total - (st.cnt - 1) * full.byteLength;
    return len > 0 && len <= full.byteLength ? len : null;
  }

  /** Local fragment rebuild from FEC parity (zero round-trips): row 1
   *  recovers one hole per 8-fragment group; rows 1+2 recover a second loss
   *  when the two fall on different even/odd offsets. Restored fragments
   *  are budget-sized by construction (every non-final fragment is exactly
   *  the datagram budget), so the XOR is aligned. The frame's final fragment
   *  is shorter than the parity width; the parity trailer's total length says
   *  where it ends ({@link finalLength}). */
  private tryRepair(st: V4Assembly): void {
    const groups = Math.ceil(st.cnt / 8);
    const xor = (acc: Uint8Array, p: Uint8Array, skip: (i: number) => boolean, range: [number, number]) => {
      for (let i = range[0]; i < range[1]; i++) {
        if (skip(i)) continue;
        const p0 = st.parts[i]!;
        for (let j = 0; j < p0.byteLength; j++) acc[j] = (acc[j] ?? 0) ^ (p0[j] ?? 0);
      }
    };
    for (let g = 0; g < groups; g++) {
      const lo = g * 8;
      const hi = Math.min(lo + 8, st.cnt);
      const missing: number[] = [];
      for (let i = lo; i < hi; i++) {
        if (st.parts[i] === null) missing.push(i);
      }
      if (missing.length === 0 || missing.length > 2) continue;
      const p1 = st.p1?.get(g);
      const p2 = st.p2?.get(g);
      if (missing.length === 1) {
        const m = missing[0]!;
        if (p1 === undefined) continue;
        const finalLen = m === st.cnt - 1 ? this.finalLength(st) : null;
        if (m === st.cnt - 1 && finalLen === null) continue;
        const acc = new Uint8Array(p1);
        xor(acc, p1, (i) => i === m, [lo, hi]);
        st.parts[m] = finalLen === null ? acc : acc.subarray(0, finalLen);
        st.got++;
        this.v4Repaired++;
      } else if (p1 !== undefined && p2 !== undefined) {
        // Two losses: rebuildable iff one is even-offset, one odd-offset
        // within the group (row 2 covers the odd members).
        const [a, b] = [missing[0]!, missing[1]!];
        const aOdd = (a - lo) % 2 === 1;
        const bOdd = (b - lo) % 2 === 1;
        if (aOdd === bOdd) continue;
        if (a === st.cnt - 1 || b === st.cnt - 1) continue;
        const evenMiss = aOdd ? b : a;
        const oddMiss = aOdd ? a : b;
        // accOdd = row2 ^ present odds = the lost odd-offset fragment.
        const accOdd = new Uint8Array(p2);
        xor(accOdd, p2, (i) => i === evenMiss || i === oddMiss || (i - lo) % 2 === 0, [lo, hi]);
        // accEven = row1 ^ row2 ^ present evens (row1^row2 = E ^ Σe_present:
        // O and the odd sums cancel) = the lost even-offset fragment.
        const accEven = new Uint8Array(p1);
        for (let j = 0; j < accEven.byteLength; j++) accEven[j] = (accEven[j] ?? 0) ^ (p2[j] ?? 0);
        xor(accEven, p1, (i) => i === evenMiss || (i - lo) % 2 === 1, [lo, hi]);
        st.parts[evenMiss] = accEven;
        st.parts[oddMiss] = accOdd;
        st.got += 2;
        this.v4Repaired += 2;
      }
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
    const state = {
      queued: [] as Uint8Array[],
      queuedBytes: 0,
      emptyReads: 0,
      gotData: false,
    };
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
        // The datagram copy of this frame may have assembled first; the page
        // must see one IDR, not two (a second anchors the orderer backwards).
        if (!this.firstDelivery(frame.frame_no, frame.capture_us)) {
          this.framesDuplicate++;
          continue;
        }
        // And the other way round: stop assembling (and NACKing) a datagram
        // copy of a frame the stream already delivered.
        this.v4.delete(frame.frame_no);
        this.v4Done.add(frame.frame_no);
        if (this.v4Done.size > 512) {
          this.v4Done.delete(this.v4Done.values().next().value as number);
        }
        this.framesReceived++;
        // Keys ride the reliable stream in v4 mode; count them where they
        // actually arrive (the v4 path counts its own), or the wire log's
        // keys=0 reads as "the IDR never reassembles" when it arrived here.
        if (frame.key) this.keysReceived++;
        this.lastFrameAtMs = performance.now();
        // Capture → decode-complete age, via the pong clock anchor. The
        // anchor is quantized by RTT/2, so single samples are fuzzy - the
        // percentiles over hundreds of frames are the measurement.
        if (this.offsetEmaUs !== null) {
          const ageMs = (performance.now() * 1000 - (frame.capture_us + this.offsetEmaUs)) / 1000;
          if (ageMs >= 0 && ageMs < 5_000) {
            this.latBuf.push(ageMs);
            if (this.latBuf.length > LAT_SAMPLES) this.latBuf.shift();
          }
        }
        h.onFrame(frame);
      } catch {
        // Stream ended mid-frame (host reset, WebKit truncation, or the
        // connection went). This frame is gone; the next stream is already
        // on its way, and a reset that lands mid-frame costs one frame, not
        // the connection.
        this.framesAbandoned++;
        this.streamAborted++;
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
   *  The watchdog does NOT arm until this stream has delivered at least one
   *  byte: the first IDR still has to wait on encoder warmup and the video-
   *  channel marker RTT, and cancelling that wait aborted the keyframe the
   *  host was writing (7 kbps / 1 incomplete frame, then the glass watchdog
   *  reset the decoder on the first decoded picture).
   */
  private async readExact(
    reader: ReadableStreamDefaultReader<Uint8Array>,
    n: number,
    state: {
      queued: Uint8Array[];
      queuedBytes: number;
      emptyReads: number;
      gotData: boolean;
    },
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
      // In v4 mode the video channel carries only forced IDRs and sits idle
      // between them — no watchdog, or every idle gap would churn channels.
      // The same is true of a brand-new channel waiting for its first byte:
      // there is no flow credit to recover until the host starts writing.
      // A genuinely wedged *mid-frame* channel still recovers: the host's
      // next IDR write times out and resets the stream, which throws here.
      const waitForever = this.datagramVideoEnabled || !state.gotData;
      const res = waitForever
        ? await reader.read()
        : await Promise.race([
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
      state.gotData = true;
      state.queued.push(value);
      state.queuedBytes += value.byteLength;
    }
    return out;
  }

  /** Present rate for this telemetry window, or `undefined` to omit the field.
   *  The host used to serde-default a missing `presented_fps` to 0 and then
   *  diagnose a healthy WT session as the iOS no-draw failure. */
  private presentRate(stats: WtClientStats | undefined, dtSec: number): number | undefined {
    if (!stats) return undefined;
    if (stats.framesPresented > this.lastFramesPresented) {
      const rate = Math.round(((stats.framesPresented - this.lastFramesPresented) / dtSec) * 100) / 100;
      if (rate > 0) this.lastPresentedRate = rate;
      return rate;
    }
    if ((stats.presentedFps ?? 0) > 0) {
      this.lastPresentedRate = stats.presentedFps!;
      return stats.presentedFps;
    }
    if (this.lastPresentedRate > 0 && stats.framesPresented > 0) {
      return this.lastPresentedRate;
    }
    // Decoding while the canvas has never painted: the real no-draw case.
    if (stats.framesDecoded > this.lastFramesDecoded && stats.framesPresented === 0) {
      return 0;
    }
    return undefined;
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
    // freeze this session was diagnosed by guessing between them. The page
    // hook publishes to window; the worker shell forwards to the page.
    this.onWire?.({
      framesReceived: this.framesReceived,
      framesAbandoned: this.framesAbandoned,
      v4Expired: this.v4Expired,
      streamAborted: this.streamAborted,
      v4Repaired: this.v4Repaired,
      nacksSent: this.nacksSent,
      wholeFrameNacks: this.wholeFrameNacks,
      assemblyEvicted: this.assemblyEvicted,
      keyPartialsEvicted: this.keyPartialsEvicted,
      framesDuplicate: this.framesDuplicate,
      parseFailures: this.parseFailures,
      streamsWedged: this.streamsWedged,
      datagramsSeen: this.datagramsSeen,
      lastFrameAgeMs: this.lastFrameAtMs === 0 ? -1 : Math.round(nowMs - this.lastFrameAtMs),
    });
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
      // JSON.stringify drops `undefined`. Omit until we know a rate: older
      // hosts serde-default a missing field to 0.0 and shout "presenting
      // none". Send 0 only for the real iOS no-draw case (decoding, never
      // painted). A stale worker snapshot keeps the last positive rate.
      presented_fps: this.presentRate(stats, dtSec),
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
      // Measured drain rate this window (datagrams/s). 0 on the first
      // window; the host falls back to a conservative default until it
      // sees a real number.
      drain_pps: Math.round(this.winDgrams / dtSec),
      // Worker-drain flag: the host raises the injection pace for this
      // connection when true (in-page fallbacks stay at the measured-safe
      // 3800 pps).
      worker: this.inWorker,
      // Capture → decode-complete latency (docs/research/performance-latency-
      // 2026-09-09.md §measurement): frame capture_us mapped onto the client
      // clock via the pong anchor. Percentiles, not an EMA - the spikes define
      // remote-play quality. Null until the clock syncs (first anchored pong).
      lat_p50_ms: this.latPercentile(0.5),
      lat_p95_ms: this.latPercentile(0.95),
      // Decode-side stall diagnosis: frames the reorder buffer holds behind a
      // hole, frames the decoder queue skipped, and the codec's own backlog.
      decode_held: stats?.held ?? 0,
      decode_behind_events: stats?.behindEvents ?? 0,
      decode_queue_size: stats?.queueSize ?? 0,
      // v4 fragment repair: NACKs sent + keyframes assembled. held=8 with
      // keys=0 means the IDR never reassembled; nacks rising with decode
      // recovering means the repair path is doing its job.
      nacks_sent: this.nacksSent,
      keys_received: this.keysReceived,
      // §13: the host needs real support data from real clients.
      audio_opus_supported: opusSupportProbe(),
    };
    void this.send(payload);
    this.lastFramesDecoded = stats?.framesDecoded ?? this.lastFramesDecoded;
    this.lastFramesPresented = stats?.framesPresented ?? this.lastFramesPresented;
    this.winStartedMs = nowMs;
    this.winBytes = 0;
    this.winDgrams = 0;
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
    if (this.openSpareChannel()) return;
    void this.send({ type: "keyframe_request" });
  }

  /** Open an EXTRA video channel for the IDR this request is about to buy.
   *
   *  The host writes a forced IDR to the channel it has *cached*, and a cached
   *  channel is one it cannot tell is still being read: a write into a reader
   *  that stopped succeeds into the void. The host prefers the newest marker it
   *  has been given ("freshly installed channel"), so giving it one right
   *  before the request puts the IDR on a stream we are definitely reading -
   *  and the datagram copy is no fallback: measured over 373 repair IDRs, the
   *  ones under 8 KB recovered the client 100 % of the time and the ones of
   *  30-100 KB only 15 %, because a 30-fragment burst on Wi-Fi loses something.
   *
   *  This deliberately does NOT cancel the channel already being read. That
   *  channel is the host's cached fallback, and cancelling it before the new
   *  marker lands replaces a working path with a dead one: 21 % of seconds
   *  below 50 fps with 11-19 s stalls, against 2 % before, when an earlier
   *  version of this fix did exactly that (2026-09-20 04:48-04:56).
   *
   *  Returns true when a channel was opened; its own request is sent once the
   *  marker is out, so the caller must not send a second one. */
  private openSpareChannel(): boolean {
    const now = performance.now();
    const wt = this.wtRef;
    const h = this.handlers;
    if (wt === null || h === null || now - this.channelReopenMs < 1000) return false;
    this.channelReopenMs = now;
    void (async () => {
      let asked = false;
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
        // Let the marker land before asking: the IDR is answered in the next
        // few milliseconds, and if it is produced before the host has the new
        // sink it rides the cached stream instead - the whole point is lost.
        await new Promise((r) => setTimeout(r, 30));
        asked = true;
        void this.send({ type: "keyframe_request" });
        this.spares.push(vch);
        // Keep the spare alive and read: the IDR that answers this request
        // arrives on it. Bound the pile - a session that needs this fix uses
        // one channel per repair, and they are dead weight once the host has
        // cached a newer one.
        while (this.spares.length > MAX_SPARE_CHANNELS) {
          const old = this.spares.shift();
          void (old as WebTransportBidirectionalStream).readable.cancel().catch(() => {});
        }
        await this.readOneFrame(vch.readable as ReadableStream<Uint8Array>, h);
      } catch {
        // The spare died before it asked: fall back to the plain request rather
        // than leave the picture waiting on a channel that never opened. If it
        // died later the request is already out and a second one would only be
        // refused by the host's one-per-second throttle.
        if (!asked) void this.send({ type: "keyframe_request" });
      }
    })();
    return true;
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
    if (this.pingTimer !== 0) clearInterval(this.pingTimer);
    if (this.telemetryTimer !== 0) clearInterval(this.telemetryTimer);
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
