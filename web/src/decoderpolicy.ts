// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Two policies the decoder needs and cannot test in place: WtDecoder builds a
// canvas in its constructor, so it needs a DOM to exist at all, and the
// browsers available to CI cannot decode HEVC even when they claim they can.
// Pure logic over a clock is testable anywhere, so the parts that were wrong
// live here.

/**
 * Minimum spacing of the page's keyframe requests.
 *
 * Every request passes two more 1000 ms gates: the worker's spare-channel
 * reopen (`wtcore.openSpareChannel`) and the host's own refusal
 * (`session.rs`). At 500 ms the page asked twice for every IDR it could get,
 * and a refused ask is a stall the client *thinks* is being repaired - the
 * measured "three requests, two refused" (audit §2.1, §3.1). A little over the
 * gates, so send jitter does not land the next ask a millisecond early.
 */
export const KEYFRAME_REQUEST_INTERVAL_MS = 1050;

/**
 * Rate limit for keyframe requests.
 *
 * This replaces a throttle that never throttled: `referenceGap()` stamped
 * `lastKeyReqMs` with `Date.now()` while `requestKeyframe()` compared it
 * against `performance.now()`. The two clocks are about 1.75e12 apart, so the
 * comparison was meaningless in both directions - every gap sent a request,
 * and the guard that was supposed to cap them at one per 500 ms never fired.
 * On a decode error that turned into an IDR request per error, and the host
 * re-encodes a full keyframe for each one.
 *
 * One clock, one field, one place that stamps it.
 */
export class KeyframeThrottle {
  private lastMs: number | null = null;

  constructor(private readonly minIntervalMs: number = KEYFRAME_REQUEST_INTERVAL_MS) {}

  /** True if a request may be sent now; stamps the clock when it says yes. */
  allow(nowMs: number): boolean {
    if (this.lastMs !== null && nowMs - this.lastMs < this.minIntervalMs) return false;
    this.lastMs = nowMs;
    return true;
  }

  reset(): void {
    this.lastMs = null;
  }
}

/** At most this many decoder rebuilds inside {@link REBUILD_WINDOW_MS}. */
export const MAX_DECODER_REBUILDS = 5;
export const REBUILD_WINDOW_MS = 10_000;

export type RestartVerdict = "rebuild" | "fallback-software" | "step-down" | "give-up";

/** A hardware decoder that fails this soon after its first picture is treated
 *  like one that never produced any: the driver, not the stream. */
export const EARLY_FAILURE_MS = 5_000;

/**
 * How many times to rebuild a decoder that keeps failing.
 *
 * The error handler used to rebuild unconditionally: a decode error closed the
 * codec, the handler reconfigured it, the next chunk failed the same way, and
 * round it went. Captured in a real browser on 2026-09-08 - hundreds of
 * `configure()` calls inside 100 ms, which is a wedged main thread and a page
 * that takes no input, not a recovering stream.
 *
 * Some configurations never work no matter how often they are rebuilt (this
 * machine's Chromium reports HEVC support, configures cleanly, then fails
 * every decode). Those must end in a named failure the user can read, not an
 * infinite retry.
 */
/** Failures of a decoder that had been working, within this window, that mean
 *  "overloaded" rather than "a lossy route recovering". */
export const OVERLOAD_WINDOW_MS = 60_000;
/** How many such failures step the stream down. */
export const OVERLOAD_FAILURES = 2;

export interface StreamMode {
  width: number;
  height: number;
  fps: number;
}

/** 16:9 rungs, largest first; 720p is the floor. */
const RUNGS: readonly [number, number][] = [
  [3840, 2160],
  [2560, 1440],
  [1920, 1080],
  [1280, 720],
];

/**
 * The next mode to try when this device's decoder cannot sustain `m`, or null
 * at the floor.
 *
 * Frame rate goes first: 120 -> 60 at the same resolution. A decoder that
 * falls over at 1440p120 is being asked for twice the pixels per second it
 * gets at 1440p60, and resolution is what the user sees on a large display -
 * only once 60 fps still fails does the picture get smaller, one rung at a
 * time, keeping the rate. Measured on an iPhone (Safari 27, 2026-09-24):
 * Safari's own HEVC decoder, fed by a bare test page, failed after 306-613
 * frames at 1440p120 and ran 90 s clean at 1440p60 and at 1080p120.
 */
export function nextStepDown(m: StreamMode): StreamMode | null {
  if (m.fps > 60) return { width: m.width, height: m.height, fps: 60 };
  const lower = RUNGS.find(([, h]) => h < m.height);
  return lower ? { width: lower[0], height: lower[1], fps: m.fps } : null;
}

export class DecoderRestartPolicy {
  private failures = 0;
  /** When a decoder that had produced pictures failed, newest last. */
  private workingFailures: number[] = [];
  private windowStartMs = 0;
  private firstOutputMs: number | null = null;

  constructor(
    private readonly maxRebuilds: number = MAX_DECODER_REBUILDS,
    private readonly windowMs: number = REBUILD_WINDOW_MS,
  ) {}

  /**
   * A decoder error arrived. Says whether rebuilding is still worth it.
   *
   * `softwareAvailable`: the caller can still re-create the codec with
   * `hardwareAcceleration: "prefer-software"`. A hardware path that fails
   * before (or just after) its first picture, or fails in a burst, is a driver
   * problem the software decoder does not share: Chromium with VA-API on an
   * AMD Renoir errored on the host's H.264, every rebuild then answered
   * "Unsupported configuration", and the budget was spent in ~15 ms - the
   * session was declared undecodable by a browser that decodes it fine in
   * software. The switch gets a fresh budget; the caller makes it once.
   */
  onError(nowMs: number, softwareAvailable = false, stepDownAvailable = false): RestartVerdict {
    // Failures spread out over time are a lossy route recovering; a burst of
    // them is a configuration that cannot work.
    if (this.failures === 0 || nowMs - this.windowStartMs > this.windowMs) {
      this.failures = 0;
      this.windowStartMs = nowMs;
    }
    this.failures += 1;
    const exhausted = this.failures > this.maxRebuilds;
    const early = this.firstOutputMs === null || nowMs - this.firstOutputMs < EARLY_FAILURE_MS;
    // A decoder that was producing pictures and then fails is overloaded,
    // not incompatible - the hardware path works, just not at this rate.
    // Software decode of the same mode is slower still, so the way out is a
    // smaller mode (see nextStepDown), once the failure repeats.
    if (!early) {
      this.workingFailures = this.workingFailures.filter((t) => nowMs - t < OVERLOAD_WINDOW_MS);
      this.workingFailures.push(nowMs);
      if (stepDownAvailable && (this.workingFailures.length >= OVERLOAD_FAILURES || exhausted)) {
        this.workingFailures = [];
        this.failures = 0;
        return "step-down";
      }
    }
    if (softwareAvailable && (early || exhausted)) {
      this.failures = 0;
      return "fallback-software";
    }
    return exhausted ? "give-up" : "rebuild";
  }

  /** A frame decoded, so the decoder works: forget the failures. */
  onProgress(nowMs: number = 0): void {
    this.failures = 0;
    if (this.firstOutputMs === null) this.firstOutputMs = nowMs;
  }

  get failureCount(): number {
    return this.failures;
  }
}

/**
 * Decoder queue depth past which the stream is skipped forward.
 *
 * WebCodecs queues everything handed to it. On a live 60 fps stream a decoder
 * that has fallen behind never catches up on its own: every frame submitted
 * adds to a backlog that is already stale, latency grows without bound, and
 * the picture freezes on the last frame that made it to the canvas while the
 * transport keeps delivering perfectly.
 *
 * A browser capture on 2026-09-09 shows the shape exactly - frames received
 * held at 60/s while decoded fell 64, 58, 42, 35, 26, 15, 1 and the canvas
 * hash stopped changing. Nothing in the client looked at `decodeQueueSize`,
 * so nothing could see it happening.
 *
 * Skipping to the next keyframe discards the backlog instead of playing it
 * late, which is the right trade for a live stream.
 */
export const MAX_DECODE_QUEUE = 6;

/** True when the decoder is too far behind to be worth feeding. */
export function decoderIsBehind(queueSize: number, limit: number = MAX_DECODE_QUEUE): boolean {
  return queueSize > limit;
}

/**
 * How many consecutive over-limit samples mean "behind" rather than "busy".
 *
 * A queue over the limit for a few submissions is not a decoder that has fallen
 * behind. The frame-order gate hands back a whole held run the instant a NACK
 * re-send fills its hole — a dozen or more frames at once, every one of them
 * needing to decode in order (infinite GOP) — and the client decodes faster
 * than realtime, so that drains by itself. Tripping on the first sample above
 * the limit turned every *repaired* hole into another keyframe: drop the run,
 * reset the codec, ask the host for an IDR. A real backlog stays over the limit.
 */
export const BEHIND_PERSISTENCE = 24;

/** True when the over-limit queue has persisted long enough to skip ahead. */
export function behindIsSustained(
  streak: number,
  limit: number = BEHIND_PERSISTENCE,
): boolean {
  return streak > limit;
}

/** A present gap past this (>6 frame intervals at 60 fps) is a freeze. */
export const FREEZE_GAP_MS = 250;

/**
 * Is a gap between two presents a freeze worth counting?
 *
 * Not if the page was hidden at any point during it: a background tab stops
 * compositing and throttles the decoder's task loop, so presents legitimately
 * stop. Counting that gap reported every tab switch as a multi-second freeze
 * to the host (audit §2.6).
 */
export function shouldCountFreeze(gapMs: number, hiddenDuringGap: boolean): boolean {
  return !hiddenDuringGap && gapMs > FREEZE_GAP_MS;
}

export interface DecoderConfigKey {
  codec: string;
  width: number;
  height: number;
  description?: Uint8Array | null;
}

/**
 * Would applying `next` change anything about the running decoder?
 *
 * A duplicate `video_config` (the host re-sends one per epoch) used to
 * reconfigure anyway: codec queue flushed, orderer reset, a forced IDR - and
 * the 18:34 session lost its startup IDR to exactly that. The description is
 * compared by bytes: each message decodes into a fresh array, so reference
 * equality called every HEVC/AVCC duplicate a change.
 */
export function sameDecoderConfig(
  last: DecoderConfigKey | null,
  next: DecoderConfigKey,
): boolean {
  if (last === null) return false;
  if (last.codec !== next.codec || last.width !== next.width || last.height !== next.height) {
    return false;
  }
  const a = last.description ?? null;
  const b = next.description ?? null;
  if (a === null || b === null) return a === b;
  if (a.byteLength !== b.byteLength) return false;
  for (let i = 0; i < a.byteLength; i++) if (a[i] !== b[i]) return false;
  return true;
}
