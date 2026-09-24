// WebCodecs decode + adaptive playout for the WT video path (ADR-0011 P2).
//
// Playout discipline — the anti-jitter-buffer:
// * a decoded frame presents as soon as either a successor frame is decoded
//   (a full frame of slack) or the adaptive present delay has elapsed;
// * the queue never holds more than one pending frame — a fresher frame
//   evicts the older one, which counts as a drop. Buffering deeper is exactly
//   the 145 ms pathology ADR-0011 removes;
// * the delay grows on underruns and shrinks after a long low-slack streak,
//   so steady state settles at the smallest delay that still renders
//   continuously;
// * there is no WebRTC video underneath (ADR-0011 final); this canvas is
//   the only glass, and the watchdog in play.ts resets/redials on stalls.

import {
  DecoderRestartPolicy,
  behindIsSustained,
  decoderIsBehind,
  KeyframeThrottle,
  RefreshCoalescer,
  shouldCountFreeze,
} from "./decoderpolicy.js";
import { FrameOrderer } from "./frameorder.js";
import type { WtFrame } from "./wtvideo.js";

/** Glass age (ms): now minus the frame's capture time moved onto the client
 *  clock. Same shape as wt.ts's fragment ages — `captureUs + offset` lands
 *  the host capture stamp on the client clock. The inverse sign here was
 *  the bug fixed for the telemetry path in the 21:51 session but missed in
 *  the decoder: the 0..5 s plausibility gate discarded a −3.5e9 ms age, so
 *  glassEmaMs/presentEmaMs stayed 0 and the HUD showed "-- ms e2e" for
 *  every WT session (02:36 Chrome). */
export function glassAgeMs(nowUs: number, captureUs: number, offsetUs: number): number {
  return (nowUs - (captureUs + offsetUs)) / 1000;
}

export interface WtDecoderStats {
  framesDecoded: number;
  framesDropped: number;
  /** Reorder buffer depth (frames held behind a hole) and codec backlog. */
  held: number;
  queueSize: number;
  /** Frames the decoder queue skipped (backpressure drops). */
  behindEvents: number;
  /** EMA of submit→output decode latency, ms. */
  decodeMs: number;
  /** Current adaptive present delay, ms. */
  /** True once the decoder has produced at least one frame. */
  rendering: boolean;
  /** Presented frames since connect (for the watchdog's progress check). */
  framesPresented: number;
  /** Real freezes (present gaps far beyond the frame interval) — never a
   *  hardcoded zero (review §14). */
  freezeCount: number;
  totalFreezeMs: number;
  /** True capture→glass age (EMA, ms) once the clock is synced; else 0. */
  e2eMs: number;
  /** Presented-frame rate over the last stats window, fps. */
  presentedFps: number;
  /** Decoded-frame rate over the last stats window, fps. */
  decodedFps: number;
  /** How frames reach the canvas: a direct draw, or via an ImageBitmap. */
  renderMode: "direct" | "bitmap";
}

/** One ImageBitmap conversion in flight; a newer frame always supersedes
 *  the older one's output. Pure so the newest-wins rule is node-testable:
 *  the renderer applies the verdicts. */
export class PresentGate {
  private seq = 0;
  private inFlight: number | null = null;

  /** A conversion is in flight? Callers must then CLOSE the new frame
   *  unconverted instead of superseding (see presentNow). */
  busy(): boolean {
    return this.inFlight !== null;
  }

  /** A new frame begins conversion. Returns the ticket. */
  begin(): number {
    this.seq++;
    this.inFlight = this.seq;
    return this.seq;
  }

  /** A conversion finished: "draw" only if it is still the newest. */
  settle(ticket: number): "draw" | "discard" {
    if (this.inFlight !== ticket) return "discard";
    this.inFlight = null;
    return "draw";
  }
}

export class WtDecoder {
  private decoder: VideoDecoder | null = null;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  /** iOS Safari draws decoded VideoFrames to a 2D canvas as a silent no-op
   *  (black screen, decode still fine). Probed once on the first frame; the
   *  bitmap path is the portable fallback. */
  private renderMode: "direct" | "bitmap" = "direct";
  /** Frames to wait before re-probing after a dark (inconclusive) probe. */
  private probeSkip = 0;
  private renderProbed = false;
  /** Sequence of the ImageBitmap conversion in flight (0 = none). */
  /** Newest-wins gate for the (at most one) bitmap conversion in flight. */
  private gate = new PresentGate();
  private decodeEmaMs = 0;
  private framesDecoded = 0;
  private framesDropped = 0;
  /** chunk timestamp (µs) → wall time it was submitted to the decoder. */
  /** Puts independently-arriving streams back into decode order. */
  private readonly order = new FrameOrderer();
  private submitTimes = new Map<number, number>();
  /** Consecutive submissions made while the decode queue was over the limit:
   *  a repair release is transient, a decoder that has fallen behind is not. */
  private behindStreak = 0;
  /** Highest frame_no submitted to the decoder. Datagrams complete out of
   *  order on any real network; decoding is strictly sequential, so frames
   *  past a gap poison the reference chain and must never reach the codec. */
  /** Frames held until their predecessors arrive - see onFrame. */
  /** The next frame number that can be decoded, or null before the first
   *  keyframe anchors the sequence. */
  /** Keyframe-request throttle, paced to the worker's and host's 1000 ms
   *  gates (see KEYFRAME_REQUEST_INTERVAL_MS): an ask either gate refuses is
   *  a repair the client believes is coming and is not. */
  private readonly keyThrottle = new KeyframeThrottle();
  /** performance.now() of the newest keyframe request no keyframe has
   *  answered yet, or null. The watchdog reads it so its reset does not
   *  flush the IDR that request is buying (audit §2.5). */
  private keyRequestedAtMs: number | null = null;
  /** The page went hidden since the last present: that gap is not a freeze. */
  private hiddenSincePresent = false;
  private readonly onVisibility = (): void => {
    if (document.hidden) this.hiddenSincePresent = true;
  };
  /** WebCodecs acceleration preference; `prefer-software` once hardware has
   *  failed, for the rest of the session. */
  private hwAccel: NonNullable<VideoDecoderConfig["hardwareAcceleration"]> = "no-preference";
  /** isConfigSupported said no to the software config: never try it again. */
  private softwareRefused = false;
  /** How many more times a failing decoder is worth rebuilding. */
  private readonly restarts = new DecoderRestartPolicy();
  /** Set once rebuilding has been abandoned: stop touching the codec. */
  private dead = false;
  /** How often the decoder was too far behind to feed (HUD/diagnosis). */
  private behindEvents = 0;
  /** Gap-start seq of the recovery we are currently waiting to complete. */
  private awaitingResyncFrom: number | null = null;
  /** Seq of the frame that exposed the current gap (for the gap log). */
  private lastGapSeq: number | null = null;
  private stopped = false;
  private announced = false;
  /** True capture→glass age EMA, ms — needs the host clock offset. */
  private glassEmaMs = 0;
  /** Capture→presented age EMA, ms — the number the HUD reports. */
  private presentEmaMs = 0;
  private presentSamples = 0;
  private framesPresented = 0;
  /** Glass-age diagnostics: last raw sample + accepted-sample count. */
  private lastAgeMs = 0;
  private ageSamples = 0;
  private firstAgeLogged = false;
  /** Previous stats() snapshot for per-window fps rates. */
  private statsSnap = { tMs: 0, decoded: 0, presented: 0 };
  private decodedFps = 0;
  private presentedFps = 0;
  /** Remembered for codec revival after a fatal decode error. */
  private lastCfg: {
    codec: string;
    width: number;
    height: number;
    fps: number;
    description?: Uint8Array | null;
  } | null = null;

  constructor(
    private readonly onFirstFrame: () => void,
    private readonly onNeedKeyframe: () => void,
    private readonly clockOffsetUs: () => number | null = () => null,
    /** The decode path is unusable and will not recover; tell the user. */
    private readonly onFatal?: (why: string) => void,
    /** This device's decoder cannot sustain the mode: `available` says a
     *  smaller one exists, `apply` switches to it (see nextStepDown). */
    private readonly stepDown?: { available: () => boolean; apply: (why: string) => void },
  ) {
    this.canvas = document.createElement("canvas");
    this.canvas.className = "wt-video";
    const ctx = this.canvas.getContext("2d", { alpha: false, desynchronized: true });
    if (ctx === null) throw new Error("2d canvas unavailable");
    this.ctx = ctx;
    document.addEventListener("visibilitychange", this.onVisibility);
  }

  get root(): HTMLCanvasElement {
    return this.canvas;
  }

  /** The last configuration applied (for duplicate-config detection), or
   *  null before the first configure. */
  currentConfig(): {
    codec: string;
    width: number;
    height: number;
    description?: Uint8Array | null;
  } | null {
    return this.lastCfg ?? null;
  }

  configure(cfg: {
    codec: string;
    width: number;
    height: number;
    fps: number;
    description?: Uint8Array | null;
  }, newStream = true): void {
    if (this.dead) return;
    this.lastCfg = cfg;
    this.canvas.width = cfg.width;
    this.canvas.height = cfg.height;
    // No `description` = Annex-B byte stream (GStreamer's H.264 output). If
    // the host encoder emits AVCC instead, decode errors fire and we request
    // a keyframe + report — the WebRTC fallback stays live either way.
    // HEVC: `hvc1` requires an out-of-band HVCC description; the host's tap
    // carries Annex-B, so rewrite to `hev1` (in-band parameter sets) when no
    // description accompanies the config.
    // §6.1: skip internal reorder queues (`latencyMode: "realtime"` is the
    // newer spelling of the same directive). `latencyMode` cast past the
    // shipped TS lib, which predates it.
    //
    // `hardwareAcceleration` is deliberately NOT `prefer-hardware`. That reads
    // like a hint to bias toward the GPU, but WebCodecs treats it as a
    // requirement: when Chrome cannot satisfy it the decoder does NOT throw
    // from `configure()` — it fires the async `error` callback and closes,
    // so the failure looks like a stream that never arrives. Measured here:
    // `configure()` returns cleanly, then within 600 ms the decoder reports
    // "Unsupported configuration" and `state === "closed"`, for H.264 as much
    // as H.265, because this box has no hardware decoder to satisfy it. The
    // default (`no-preference`) still picks hardware whenever it exists, so
    // nothing is lost by not demanding it - until that hardware fails, and
    // then `prefer-software` for the rest of the session (see onError).
    const config = this.decoderConfig(cfg);
    if (this.decoder === null) {
      this.decoder = new VideoDecoder({
        output: (vf) => this.onDecodedFrame(vf),
        error: (e) => {
          // Unbounded rebuilding here was the freeze: the rebuilt codec failed
          // on its very next chunk and the handler rebuilt it again, hundreds
          // of times inside 100 ms, wedging the main thread so the page took
          // no input at all. Rebuild a bounded number of times, then say so.
          if (this.dead) return;
          const verdict = this.restarts.onError(
            performance.now(),
            this.hwAccel !== "prefer-software" && !this.softwareRefused,
            this.stepDown?.available() ?? false,
          );
          if (verdict === "step-down") {
            // Stop here: the session is about to be replaced at a smaller
            // mode, and rebuilding this decoder would only fail again.
            this.dead = true;
            console.warn(`wt decoder overloaded - stepping the stream down (${e.message})`);
            this.stepDown?.apply(e.message);
            return;
          }
          if (verdict === "fallback-software") {
            this.fallBackToSoftware(e.message);
            return;
          }
          if (verdict === "give-up") {
            this.dead = true;
            console.error(`wt decoder gave up after repeated errors: ${e.message}`);
            this.onFatal?.(
              "The video decoder failed repeatedly on this stream — " +
                `this browser cannot decode it (${e.message}).`,
            );
            return;
          }
          console.warn("wt decoder error:", e.message);
          // A decoding error usually closes the codec — rebuild it so the
          // path survives; discard queued frames (they depended on the
          // broken chain) and hold frames until the host's forced IDR.
          this.referenceGap();
          if (this.decoder && this.decoder.state === "closed" && this.lastCfg) {
            this.decoder = null;
            this.configure(this.lastCfg, false);
          }
        },
      });
    }
    if (this.decoder.state !== "unconfigured") this.decoder.reset();
    // A fresh config needs a fresh anchor: drop the hold buffer and wait
    // for the next keyframe before decoding anything. A new stream, too: its
    // numbering and PTS owe nothing to the frames decoded before (a rebuild
    // of the same stream keeps them - `newStream` false).
    this.order.reset(newStream);
    // Only a new stream may ask at once. A rebuild re-asking past the throttle
    // was one IDR request per decode error (audit §3.1).
    if (newStream) this.keyThrottle.reset();
    this.decoder.configure(config);
    // A (re)configured decoder needs a keyframe it will never otherwise see:
    // the host runs an effectively-infinite GOP and forces IDRs only on
    // connect or client request. The 18:34 session proved the failure shape:
    // a second config mid-startup (the WT path now races the WebRTC answer)
    // reconfigured AFTER the startup IDR had already passed, and the decoder
    // then waited forever - loss 0%, every fragment arriving, decode 0.
    this.requestKeyframe();
  }

  /** The WebCodecs config for a host video_config, on the current
   *  acceleration preference. */
  private decoderConfig(cfg: {
    codec: string;
    description?: Uint8Array | null;
  }): VideoDecoderConfig & { latencyMode?: "realtime" } {
    return {
      codec:
        cfg.codec.startsWith("hvc1") && !cfg.description
          ? (("hev1" + cfg.codec.slice(4)) as VideoDecoderConfig["codec"])
          : cfg.codec,
      optimizeForLatency: true,
      latencyMode: "realtime",
      hardwareAcceleration: this.hwAccel,
      ...(cfg.description ? { description: cfg.description } : {}),
    };
  }

  /**
   * Re-create the codec on the software decoder, once per session.
   *
   * Chromium with VA-API (AMD Renoir) failed the host's H.264 in hardware and
   * every rebuild then answered "Unsupported configuration": five rebuilds in
   * ~15 ms and the session was declared undecodable, by a browser that plays
   * the same stream fine in software. The preference outlives this config -
   * a later video_config on the same session stays on software.
   */
  private fallBackToSoftware(why: string): void {
    const cfg = this.lastCfg;
    if (cfg === null) return;
    // Set before the probe resolves: a second error in the meantime must not
    // start a second fallback.
    this.hwAccel = "prefer-software";
    // The error callback normally arrives with the codec already closed.
    if (this.decoder !== null && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
    void VideoDecoder.isConfigSupported(this.decoderConfig(cfg))
      .then((r) => r.supported === true, () => false)
      .then((ok) => {
        if (this.dead || this.stopped) return;
        if (ok) {
          console.warn(`wt: hardware decoder failed - falling back to software decode (${why})`);
        } else {
          // Nothing to fall back to: the ordinary rebuild budget decides.
          this.hwAccel = "no-preference";
          this.softwareRefused = true;
          console.warn(`wt decoder error: ${why} (no software decoder for this config)`);
        }
        if (this.decoder === null && this.lastCfg !== null) {
          this.configure(this.lastCfg, false);
        }
      });
  }

  /**
   * Flush the codec and leave it ready for the next keyframe.
   *
   * WebCodecs' `reset()` returns the decoder to "unconfigured", not to an
   * empty configured state, and nothing configured it again: the backlog path
   * then decoded its keyframe into an unconfigured codec ("Cannot call
   * 'decode' on an unconfigured codec"), and from then on `onFrame` dropped
   * every arrival because the state was not "configured". Live 2026-09-23: 12 s
   * of frames arriving at 60 fps with none decoded, through two watchdog
   * resets that did the same thing, until the WT redial built a new decoder.
   */
  private resetCodec(): void {
    if (this.decoder === null || this.decoder.state === "closed") return;
    this.decoder.reset();
    if (this.lastCfg) this.decoder.configure(this.decoderConfig(this.lastCfg));
  }

  /** Throttled IDR request to the host (media session forces one). */
  /** Unrecoverable reference-chain break: drop queued frames, reset the
   *  sequence gate, and wait for a complete fresh keyframe (ADR-0011 P2). */
  private referenceGap(): void {
    if (this.awaitingResyncFrom === null) this.awaitingResyncFrom = this.lastGapSeq;
    this.order.reset();
    // The log rides the same throttle as the request: an error burst used to
    // print a line per error, which is its own kind of freeze.
    if (this.requestKeyframe()) {
      console.warn(`wt reference gap at ${this.lastGapSeq} - re-keying`);
    }
  }

  /** Ask the host for an IDR, at most once per throttle window. Returns
   *  whether the request was actually sent. */
  private requestKeyframe(): boolean {
    const now = performance.now();
    if (!this.keyThrottle.allow(now)) return false;
    this.keyRequestedAtMs = now;
    this.order.keyframeRequested();
    this.onNeedKeyframe();
    return true;
  }

  /** The route's RTT: shortens the orderer's repair wait on a fast route. */
  setRttMs(rttMs: number): void {
    this.order.setRttMs(rttMs);
  }

  /** Capture -> present age of the video, ms (null until measured): what
   *  audio is scheduled against for lip sync. */
  presentAgeMs(): number | null {
    return this.presentEmaMs > 0 ? this.presentEmaMs : null;
  }

  /** When the newest unanswered keyframe request went out, or null. */
  awaitingKeyframeSince(): number | null {
    return this.keyRequestedAtMs;
  }

  /** Presented frames so far - the watchdog's cheap progress counter (stats()
   *  also rolls the fps window, so it must not run at the watchdog's rate). */
  get presentedCount(): number {
    return this.framesPresented;
  }

  onFrame(f: WtFrame): void {
    if (this.stopped || this.decoder === null || this.decoder.state !== "configured") return;
    // Ordering lives in FrameOrderer - pure logic over frame numbers, tested
    // directly. Streams complete independently, so frames arrive out of order
    // and a late keyframe must still anchor the sequence.
    for (const ready of this.order.accept(f)) this.submit(ready);
    if (this.order.needsResync) {
      this.lastGapSeq = f.frame_no;
      this.referenceGap();
    } else if (this.order.keyframeDue) {
      // The orderer spaces re-requests by held video (so each has time to
      // land), the throttle by wall time (so no gate behind it refuses one).
      this.requestKeyframe();
    }
  }

  private submit(f: WtFrame): void {
    if (this.decoder === null || this.decoder.state !== "configured") return;
    // Backpressure. Handing more frames to a decoder that is already behind
    // only grows a backlog of stale frames - the picture freezes while the
    // transport keeps delivering. Drop to the next keyframe instead; on a live
    // stream a fresh anchor beats several seconds of late video.
    if (decoderIsBehind(this.decoder.decodeQueueSize)) {
      this.behindStreak++;
    } else {
      this.behindStreak = 0;
    }
    if (behindIsSustained(this.behindStreak)) {
      // A keyframe is the exit from a stuck queue, never a candidate for the
      // backpressure drop: resetting the codec and feeding the anchor
      // recovers immediately, while dropping it leaves the queue holding
      // stale deltas whose references are gone - the 22:04 stall decoded
      // exactly 1 fps because every recovery IDR was discarded here and the
      // queue only drained as WebKit errored stale chunks one per second.
      if (f.key) {
        this.resetCodec();
        this.submitTimes.clear();
        this.behindStreak = 0;
      } else {
        this.framesDropped++;
        this.behindEvents++;
        this.referenceGap();
        return;
      }
    }
    this.submitTimes.set(f.capture_us, performance.now());
    if (this.submitTimes.size > 256) {
      const oldest = this.submitTimes.keys().next().value;
      if (oldest !== undefined) this.submitTimes.delete(oldest);
    }
    const chunk = new EncodedVideoChunk({
      type: f.key ? "key" : "delta",
      timestamp: f.capture_us, // EncodedVideoChunk timestamps are µs
      data: f.payload,
    });
    try {
      this.decoder.decode(chunk);
      if (f.key) this.keyRequestedAtMs = null;
    } catch (e) {
      // A rejected chunk means the decoder needs a fresh IDR (ordering race
      // after a reconfigure, codec mismatch, …) — full recovery, not just a
      // re-key: nothing queued can decode past a poisoned reference chain.
      console.warn("wt decode submit failed:", String(e));
      this.referenceGap();
    }
  }

  /** One-time: draw the frame small to a scratch canvas and count lit
   *  pixels. A no-op direct draw (iOS Safari 18) leaves them zeroed.
   *
   *  A dark frame looks the same, and choosing the ImageBitmap renderer is
   *  not free: it allocates a full-size RGBA bitmap per frame (~14.7 MB at
   *  1440p, ~1.8 GB/s at 120 fps), which iOS Safari 27 could not sustain -
   *  the page was killed ~25 s into every 1440p120 session (2026-09-24), while
   *  a direct draw works there. So a dark direct draw is checked against the
   *  bitmap route on a clone of the same frame, and only a bitmap that shows
   *  pixels the direct draw did not switches the renderer. */
  private probeRenderMode(vf: VideoFrame): void {
    this.renderProbed = true;
    const litPixels = (src: CanvasImageSource): number => {
      const c = document.createElement("canvas");
      c.width = 8;
      c.height = 8;
      const cx = c.getContext("2d", { alpha: false })!;
      cx.drawImage(src, 0, 0, 8, 8);
      const d = cx.getImageData(0, 0, 8, 8).data;
      let lit = 0;
      for (let i = 0; i < d.length; i += 4) {
        if ((d[i] ?? 0) | (d[i + 1] ?? 0) | (d[i + 2] ?? 0)) lit++;
      }
      return lit;
    };
    let direct: number;
    try {
      direct = litPixels(vf as unknown as CanvasImageSource);
    } catch {
      this.renderMode = "bitmap";
      console.info("wt: direct VideoFrame draw unavailable - using ImageBitmap renderer");
      return;
    }
    if (direct >= 2) return;
    const probe = vf.clone();
    void createImageBitmap(probe)
      .then((bmp) => {
        const viaBitmap = litPixels(bmp);
        bmp.close();
        if (viaBitmap >= 2) {
          this.renderMode = "bitmap";
          console.info("wt: direct VideoFrame draw produced no pixels - using ImageBitmap renderer");
        } else {
          // Dark on both routes: the frame is dark, not the draw broken.
          // Look again a second or so later.
          this.renderProbed = false;
          this.probeSkip = 60;
        }
      })
      .catch(() => {})
      .finally(() => probe.close());
  }

  private onDecodedFrame(vf: VideoFrame): void {
    const submit = this.submitTimes.get(vf.timestamp);
    this.submitTimes.delete(vf.timestamp);
    // Missing submits (PTS reuse, stale outputs) must not drag the EMA to 0.
    if (submit !== undefined) {
      const decodeMs = performance.now() - submit;
      this.decodeEmaMs = this.decodeEmaMs === 0 ? decodeMs : 0.9 * this.decodeEmaMs + 0.1 * decodeMs;
    }
    this.framesDecoded++;
    // The codec produced a frame, so whatever failed before was transient.
    this.restarts.onProgress(performance.now());

    // True glass age: vf.timestamp is the host capture_us; shift it onto the
    // client clock with the pong-synced offset (offset = host − client, so
    // now_client + offset lands on the capture clock). Wild samples (offset
    // still settling) are ignored.
    const offset = this.clockOffsetUs();
    if (offset !== null) {
      // Same shape as wt.ts's fragment ages: capture time moved onto the
      // client clock, then subtracted from now. The inverse sign here (the
      // bug fixed for the telemetry path in the 21:51 session but missed in
      // the decoder) fed the 0..5 s plausibility gate a −3.5e9 ms age, so
      // glassEmaMs/presentEmaMs stayed 0 and the HUD showed "-- ms e2e"
      // for every WT session (02:36 Chrome).
      const ageMs = glassAgeMs(performance.now() * 1000, vf.timestamp, offset);
      this.lastAgeMs = Math.round(ageMs * 10) / 10;
      if (this.firstAgeLogged === false && ageMs >= 0 && ageMs < 5000) {
        this.firstAgeLogged = true;
        console.info(`wt first glass age: ${ageMs.toFixed(1)} ms`);
      }
      if (ageMs >= 0 && ageMs < 5000) {
        this.ageSamples++;
        this.glassEmaMs = this.glassEmaMs === 0 ? ageMs : 0.9 * this.glassEmaMs + 0.1 * ageMs;
      }
    }

    // Present before announcing: onFirstFrame runs hudTick, and the
    // freeze watchdog keys off framesPresented. Announcing first left that
    // counter at 0 on the very tick the glass came up, so a session that
    // had been observe(0)-ing since dial reset the decoder on frame one.
    this.presentNow(vf);
    if (!this.announced) {
      this.announced = true;
      this.onFirstFrame();
    }
  }

  /** Immediate presentation: draw the frame the moment the decoder hands it
   *  over. No playout hold, no successor waiting — browser compositing and
   *  display scanout are the only latency after this call. */
  /** One draw per display refresh (see RefreshCoalescer). */
  private readonly refresh = new RefreshCoalescer<VideoFrame>();
  private refreshArmed = false;
  /** Frames decoded but superseded before any refresh could show them. */
  framesCoalesced = 0;

  private armRefresh(): void {
    if (this.refreshArmed) return;
    this.refreshArmed = true;
    requestAnimationFrame(() => {
      this.refreshArmed = false;
      if (this.stopped) return;
      const held = this.refresh.onRefresh();
      if (held !== null) {
        this.drawFrame(held);
        this.armRefresh();
      }
    });
  }

  private presentNow(vf: VideoFrame): void {
    const verdict = this.refresh.offer(vf);
    if (verdict.action === "hold") {
      if (verdict.release !== null) {
        verdict.release.close();
        this.framesCoalesced++;
      }
    } else {
      this.drawFrame(vf);
    }
    this.armRefresh();
  }

  private drawFrame(vf: VideoFrame): void {
    if (this.canvas.width !== vf.displayWidth || this.canvas.height !== vf.displayHeight) {
      this.canvas.width = vf.displayWidth;
      this.canvas.height = vf.displayHeight;
    }
    if (!this.renderProbed) {
      if (this.probeSkip > 0) this.probeSkip--;
      else this.probeRenderMode(vf);
    }
    if (this.renderMode === "direct") {
      this.ctx.drawImage(vf as unknown as CanvasImageSource, 0, 0, vf.displayWidth, vf.displayHeight);
      this.finishPresent(vf);
      return;
    }
    // Portable path: rasterize through an ImageBitmap (Safari-safe). One
    // conversion in flight - genuinely one: a frame arriving while a
    // conversion runs is closed unconverted. Superseding instead (the old
    // behaviour) made every conversion resolve stale on slower Safari GPUs:
    // decode 60 fps, present 0, the pending-conversion pile saturated the
    // main thread, the stream readers starved and the glass froze (~10 s in,
    // iPhone Safari 2026-09-09). Effective present rate = conversion rate.
    if (this.gate.busy()) {
      this.framesDropped++;
      vf.close();
      return;
    }
    const ticket = this.gate.begin();
    void createImageBitmap(vf).then(
      (bmp) => {
        if (this.gate.settle(ticket) === "discard") {
          bmp.close(); // superseded before it could be shown
          return;
        }
        this.ctx.drawImage(bmp, 0, 0, vf.displayWidth, vf.displayHeight);
        bmp.close();
        this.finishPresent(vf);
      },
      () => {
        this.gate.settle(ticket);
        vf.close();
        this.framesDropped++;
      },
    );
  }

  /** Freeze accounting (review §14): a gap between presents far beyond the
   *  frame interval is a real freeze — never a hardcoded zero. */
  private lastPresentMs = 0;
  freezeCount = 0;
  totalFreezeMs = 0;

  /** Shared present tail for every rendering path: age at PRESENT, not
   *  decode — the true glass latency — then close the frame and count it.
   *  Canvas draw completion is still not proof pixels appeared. */
  private finishPresent(vf: VideoFrame): void {
    const nowMs = performance.now();
    const hidden = this.hiddenSincePresent || document.hidden;
    this.hiddenSincePresent = false;
    if (this.lastPresentMs > 0) {
      const gap = nowMs - this.lastPresentMs;
      // A stall, not pacing - and not a background tab (audit §2.6).
      if (shouldCountFreeze(gap, hidden)) {
        this.freezeCount++;
        this.totalFreezeMs += Math.round(gap);
      }
    }
    this.lastPresentMs = nowMs;
    const offset = this.clockOffsetUs();
    if (offset !== null) {
      const presentAgeMs = glassAgeMs(performance.now() * 1000, vf.timestamp, offset);
      if (presentAgeMs >= 0 && presentAgeMs < 5000) {
        this.presentEmaMs = this.presentEmaMs === 0 ? presentAgeMs : 0.9 * this.presentEmaMs + 0.1 * presentAgeMs;
        this.presentSamples++;
      }
    }
    vf.close();
    this.framesPresented++;
  }

  requestKey(): void {
    this.onNeedKeyframe();
  }

  /**
   * Recovery after a stall broke the reference chain: drop everything in
   * flight and wait for a fresh IDR. Keeps the session and the transport up —
   * only the decode state is rebuilt.
   */
  reset(): void {
    this.refresh.clear()?.close();
    this.submitTimes.clear();
    this.order.reset();
    this.announced = true; // already rendering — don't re-run first-frame wiring
    if (this.decoder !== null && this.decoder.state !== "closed") {
      this.resetCodec();
    } else if (this.lastCfg) {
      this.decoder = null;
      this.configure(this.lastCfg, false);
    }
    // Through the throttle like every other ask: the watchdog firing right
    // after a request would otherwise double it, and the host refuses the
    // second (audit §3.1).
    this.requestKeyframe();
  }

  stats(): WtDecoderStats {
    // Per-window rates: the HUD wants fps, not cumulative counters.
    const nowMs = performance.now();
    const dt = (nowMs - this.statsSnap.tMs) / 1000;
    if (this.statsSnap.tMs !== 0 && dt >= 0.5) {
      const dRate = (this.framesDecoded - this.statsSnap.decoded) / dt;
      const pRate = (this.framesPresented - this.statsSnap.presented) / dt;
      this.decodedFps = this.decodedFps === 0 ? dRate : 0.6 * this.decodedFps + 0.4 * dRate;
      this.presentedFps = this.presentedFps === 0 ? pRate : 0.6 * this.presentedFps + 0.4 * pRate;
    }
    this.statsSnap = { tMs: nowMs, decoded: this.framesDecoded, presented: this.framesPresented };
    const s = {
      framesDecoded: this.framesDecoded,
      framesDropped: this.framesDropped,
      // Reorder buffer depth and codec backlog: the 22:04 stall showed
      // recv=60/s with decoded ~1 fps - these counters are how the next
      // session's log tells "frames held behind a hole" from "codec stuck".
      held: this.order.held,
      queueSize: this.decoder?.decodeQueueSize ?? 0,
      decodeMs: Math.round(this.decodeEmaMs * 100) / 100,
      rendering: this.framesDecoded > 0,
      framesPresented: this.framesPresented,
      freezeCount: this.freezeCount,
      totalFreezeMs: this.totalFreezeMs,
      e2eMs: Math.round((this.presentEmaMs > 0 ? this.presentEmaMs : this.glassEmaMs) * 10) / 10,
      decodedFps: Math.round(this.decodedFps),
      renderMode: this.renderMode,
      presentedFps: Math.round(this.presentedFps),
      lastAgeMs: this.lastAgeMs,
      ageSamples: this.ageSamples,
      presentEmaMs: Math.round(this.presentEmaMs * 10) / 10,
      presentSamples: this.presentSamples,
      // Non-zero means the decoder could not keep up and the stream was
      // skipped forward - the difference between "network problem" and
      // "this device cannot decode this mode".
      behindEvents: this.behindEvents,
      decodeQueue: this.decoder?.decodeQueueSize ?? 0,
    };
    // Console-diagnosable without opening the HUD panel.
    (window as unknown as Record<string, unknown>).__inphaseWt = s;
    return s;
  }

  stop(): void {
    this.stopped = true;
    this.refresh.clear()?.close();
    document.removeEventListener("visibilitychange", this.onVisibility);
    // A decode error may already have closed the codec — closing again throws.
    if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
  }
}
