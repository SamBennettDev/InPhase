// Per-frame pipeline tracing (architecture report §20).
//
// Every presented video frame carries VideoFrameCallbackMetadata — RTP
// timestamp, when its last packet arrived, decode duration, when it was handed
// to the compositor, and when it's expected on glass. From those we reconstruct
// the client half of the latency budget per frame and call out the outliers:
//
//   frame rtp=2882910 | recv->glass 41.2ms  (jbuf 33.8  decode 1.6  present 5.8)
//   +18ms vs median — last packet was late
//
// The host half (capture / encode-submit / packet-send, keyed by the same RTP
// timestamp) is delivered separately on the control channel; when present it's
// merged in for a full capture->glass breakdown.

interface Vfc {
  requestVideoFrameCallback?: (
    cb: (now: number, meta: VfcMeta) => void,
  ) => number;
  cancelVideoFrameCallback?: (h: number) => void;
}
interface VfcMeta {
  presentedFrames: number;
  presentationTime: number; // DOMHighResTimeStamp — submitted to compositor
  expectedDisplayTime: number; // DOMHighResTimeStamp — expected on glass
  mediaTime: number;
  rtpTimestamp?: number; // 90 kHz, wraps at 2^32 — our end-to-end frame id
  receiveTime?: number; // DOMHighResTimeStamp — last packet of the frame arrived
  processingDuration?: number; // seconds — decode (packet->decoded frame ready)
  captureTime?: number; // DOMHighResTimeStamp — needs abs-capture-time from sender
}

/** Host-side stamps for one frame, from the control channel (µs, host clock). */
export interface HostFrameStamp {
  rtp: number;
  capture_us: number;
  encode_us: number;
  send_us: number;
}

interface FrameTrace {
  rtp: number;
  now: number; // callback time — frame is up
  interval: number; // since previous presented frame
  recvToUp: number; // receiveTime -> now  (jitter-buffer wait + decode + queue)
  jbufWait: number; // recvToUp - decode
  decode: number;
  present: number; // now -> expectedDisplayTime
  captureToUp?: number; // if captureTime present
}

interface Band {
  p50: number;
  p95: number;
  max: number;
}

/** Structured form of one `report()` window — also what the HUD and any
 *  external probe read (mirrored to `window.__inphaseFrameStats`). */
export interface FrameStats {
  t: number; // seconds since the probe started
  n: number; // frames in this window
  displayHz: number;
  intervalMs: Band; // presented-frame spacing — the frame-pacing metric
  jitterMs: number; // stddev of the interval — pacing jitter
  jbufMs: Band; // jitter-buffer wait
  decodeMs: { p50: number; p95: number };
  presentMs: { p50: number; p95: number };
  skippedFrames: number;
  hostStamps: boolean;
  encodeMs?: Band; // host: capture -> encode-submit
  netMs?: Band; // host send -> last packet in (network + pacing)
  captureToGlassMs?: Band; // full glass-to-glass, per frame
  outliers: Record<string, number>[];
}

function pct(sorted: number[], p: number): number {
  if (!sorted.length) return 0;
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]!;
}
const r1 = (n: number) => Math.round(n * 10) / 10;
const band = (xs: number[]): Band => {
  const s = xs.slice().sort((a, b) => a - b);
  return {
    p50: r1(pct(s, 0.5)),
    p95: r1(pct(s, 0.95)),
    max: r1(s.at(-1) ?? 0),
  };
};
function stddev(xs: number[]): number {
  if (xs.length < 2) return 0;
  const m = xs.reduce((a, b) => a + b, 0) / xs.length;
  return Math.sqrt(xs.reduce((a, b) => a + (b - m) * (b - m), 0) / xs.length);
}

export class FrameProbe {
  private handle = 0;
  private timer = 0;
  private rafId = 0;
  private lastNow = 0;
  private lastPresented = 0;
  private lastRaf = 0;
  private rafDeltas: number[] = [];
  private traces: FrameTrace[] = [];
  private skipped = 0;
  private started = performance.now();
  /** rtpTimestamp -> host stamps, filled from the control channel. */
  private hostStamps = new Map<number, HostFrameStamp>();
  private clockOffsetMs = 0; // host_clock_ms + offset ≈ client performance.now()
  private haveOffset = false;
  private lastStats: FrameStats | null = null;

  /** Optional: the old WebRTC <video>; WT-only sessions probe via rAF + wtGlass. */
  constructor(
    private readonly session?: { readonly video: HTMLVideoElement },
  ) {}

  /** Supplies the WT glass's live stats while it owns the video; null while
   *  the WebRTC path is showing. When present, the frame log labels the glass
   *  `wt` and carries its measured latency — otherwise the numbers below are
   *  the hidden fallback stream's and say nothing about what's on screen. */
  wtGlass:
    | (() => {
        e2eMs: number;
        presentedFps: number;
        syncErrMs: number | null;
        framesDecoded: number;
        framesDropped: number;
        rendering: boolean;
      } | null)
    | null = null;

  /** Feed a batch of host frame stamps (from a `frame_stamps` control message).
   *  `hostNowUs` is the host wall clock when it sent the batch; `oneWayMs` is
   *  half the control-channel RTT (from ping/pong) — subtracting it lines the
   *  host clock up with `performance.now()` to within the LAN's one-way jitter,
   *  a fraction of a millisecond in practice. */
  ingestHostStamps(frames: HostFrameStamp[], hostNowUs: number, oneWayMs = 0) {
    if (hostNowUs > 0) {
      this.clockOffsetMs = performance.now() - hostNowUs / 1000 - oneWayMs;
      this.haveOffset = true;
    }
    for (const f of frames) {
      this.hostStamps.set(f.rtp >>> 0, f);
      if (this.hostStamps.size > 600) {
        const oldest = this.hostStamps.keys().next().value;
        if (oldest !== undefined) this.hostStamps.delete(oldest);
      }
    }
  }

  /** Latest computed window, for a HUD/probe that repaints faster than 3 Hz. */
  latest(): FrameStats | null {
    return this.lastStats;
  }

  /** Measured median glass-to-glass (ms) once host stamps are flowing, else 0. */
  measuredE2eMs(): number {
    return this.lastStats?.captureToGlassMs?.p50 ?? 0;
  }

  start() {
    this.rafId = requestAnimationFrame(this.measureRefresh);
    const v = this.session?.video as (HTMLVideoElement & Vfc) | undefined;
    if (v?.requestVideoFrameCallback) {
      const cb = (now: number, meta: VfcMeta) => {
        this.onFrame(now, meta);
        this.handle = v.requestVideoFrameCallback!(cb);
      };
      this.handle = v.requestVideoFrameCallback(cb);
    }
    this.timer = window.setInterval(() => {
      try {
        this.report();
      } catch (e) {
        console.warn("probe tick failed:", String(e));
      }
    }, 3000);
    (globalThis as Record<string, unknown>)["__inphaseProbe"] = this;
  }

  stop() {
    clearInterval(this.timer);
    cancelAnimationFrame(this.rafId);
    const v = this.session?.video as (HTMLVideoElement & Vfc) | undefined;
    v?.cancelVideoFrameCallback?.(this.handle);
    if ((globalThis as Record<string, unknown>)["__inphaseProbe"] === this) {
      delete (globalThis as Record<string, unknown>)["__inphaseProbe"];
    }
  }

  private measureRefresh = (now: number) => {
    if (this.lastRaf) {
      const d = now - this.lastRaf;
      if (d > 1 && d < 100) this.rafDeltas.push(d);
      if (this.rafDeltas.length > 240) this.rafDeltas.shift();
    }
    this.lastRaf = now;
    this.rafId = requestAnimationFrame(this.measureRefresh);
  };

  private refreshHz(): number {
    if (this.rafDeltas.length < 10) return 0;
    const s = this.rafDeltas.slice().sort((a, b) => a - b);
    return Math.round(1000 / s[Math.floor(s.length / 2)]!);
  }

  private onFrame(now: number, m: VfcMeta) {
    const interval = this.lastNow ? now - this.lastNow : 0;
    if (this.lastPresented) {
      const skip = m.presentedFrames - this.lastPresented - 1;
      if (skip > 0) this.skipped += skip;
    }
    this.lastNow = now;
    this.lastPresented = m.presentedFrames;

    const decode = (m.processingDuration ?? 0) * 1000;
    const recvToUp = m.receiveTime ? now - m.receiveTime : 0;
    const t: FrameTrace = {
      rtp: (m.rtpTimestamp ?? 0) >>> 0,
      now,
      interval,
      recvToUp,
      jbufWait: Math.max(0, recvToUp - decode),
      decode,
      present: Math.max(0, m.expectedDisplayTime - now),
      captureToUp: m.captureTime ? now - m.captureTime : undefined,
    };
    this.traces.push(t);
    if (this.traces.length > 400) this.traces.shift();
  }

  /** host µs -> client performance.now() ms */
  private toClientMs(hostUs: number): number {
    return hostUs / 1000 + this.clockOffsetMs;
  }

  private report() {
    const tr = this.traces;
    this.traces = [];
    if (tr.length < 5) return;

    const intervals = tr
      .map((t) => t.interval)
      .filter((x) => x > 0)
      .sort((a, b) => a - b);
    const jbuf = tr.map((t) => t.jbufWait).sort((a, b) => a - b);
    const decode = tr.map((t) => t.decode).sort((a, b) => a - b);
    const present = tr.map((t) => t.present).sort((a, b) => a - b);
    const jbufMed = pct(jbuf, 0.5);
    const intMed = pct(intervals, 0.5);

    // Host-merged bands: encode submit, network leg, full glass-to-glass.
    const enc: number[] = [];
    const net: number[] = [];
    const c2g: number[] = [];
    if (this.haveOffset) {
      for (const t of tr) {
        const h = this.hostStamps.get(t.rtp);
        if (!h || !h.capture_us || !h.encode_us) continue;
        const capMs = this.toClientMs(h.capture_us);
        enc.push(this.toClientMs(h.encode_us) - capMs);
        net.push(t.now - t.recvToUp - this.toClientMs(h.send_us));
        c2g.push(t.now + t.present - capMs);
      }
    }

    // Outliers: a frame shown late (interval well over one refresh) OR one that
    // sat unusually long in the jitter buffer.
    const bad = tr
      .filter((t) => t.interval > intMed * 1.5 + 4 || t.jbufWait > jbufMed + 12)
      .slice(-6)
      .map((t) => {
        const host = this.hostStamps.get(t.rtp);
        const parts: Record<string, number> = {
          rtp: t.rtp,
          interval: r1(t.interval),
          jbuf: r1(t.jbufWait),
          decode: r1(t.decode),
          present: r1(t.present),
          late_vs_med: r1(t.jbufWait - jbufMed),
        };
        if (host && this.haveOffset) {
          const capMs = this.toClientMs(host.capture_us);
          parts["encode"] = r1(this.toClientMs(host.encode_us) - capMs);
          parts["net"] = r1(t.now - t.recvToUp - this.toClientMs(host.send_us));
          parts["capture_to_glass"] = r1(t.now + t.present - capMs);
        }
        return parts;
      });

    const wt = this.wtGlass?.() ?? null;
    const s: FrameStats & { glass?: string; wt?: Record<string, unknown> } = {
      t: Math.round((performance.now() - this.started) / 1000),
      n: tr.length,
      displayHz: this.refreshHz(),
      intervalMs: band(intervals),
      jitterMs: r1(stddev(intervals)),
      jbufMs: band(jbuf),
      decodeMs: { p50: r1(pct(decode, 0.5)), p95: r1(pct(decode, 0.95)) },
      presentMs: { p50: r1(pct(present, 0.5)), p95: r1(pct(present, 0.95)) },
      skippedFrames: this.skipped,
      hostStamps: c2g.length > 0,
      ...(c2g.length > 0
        ? { encodeMs: band(enc), netMs: band(net), captureToGlassMs: band(c2g) }
        : {}),
      glass: wt || !this.session ? "wt" : "webrtc",
      ...(wt
        ? {
            wt: {
              e2eMs: wt.e2eMs,
              presentedFps: wt.presentedFps,
              syncErrMs: wt.syncErrMs === null ? null : r1(wt.syncErrMs),
              decoded: wt.framesDecoded,
              dropped: wt.framesDropped,
              rendering: wt.rendering,
            },
          }
        : {}),
      outliers: bad,
    };
    this.lastStats = s;
    (globalThis as Record<string, unknown>)["__inphaseFrameStats"] = s;
    console.info("[InPhase frame]", JSON.stringify(s));
    this.skipped = 0;
  }
}
