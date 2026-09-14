// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Two policies the decoder needs and cannot test in place: WtDecoder builds a
// canvas in its constructor, so it needs a DOM to exist at all, and the
// browsers available to CI cannot decode HEVC even when they claim they can.
// Pure logic over a clock is testable anywhere, so the parts that were wrong
// live here.

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

  constructor(private readonly minIntervalMs: number = 500) {}

  /** True if a request may be sent now; stamps the clock when it says yes. */
  allow(nowMs: number): boolean {
    if (this.lastMs !== null && nowMs - this.lastMs < this.minIntervalMs)
      return false;
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

export type RestartVerdict = "rebuild" | "give-up";

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
export class DecoderRestartPolicy {
  private failures = 0;
  private windowStartMs = 0;

  constructor(
    private readonly maxRebuilds: number = MAX_DECODER_REBUILDS,
    private readonly windowMs: number = REBUILD_WINDOW_MS,
  ) {}

  /** A decoder error arrived. Says whether rebuilding is still worth it. */
  onError(nowMs: number): RestartVerdict {
    // Failures spread out over time are a lossy route recovering; a burst of
    // them is a configuration that cannot work.
    if (this.failures === 0 || nowMs - this.windowStartMs > this.windowMs) {
      this.failures = 0;
      this.windowStartMs = nowMs;
    }
    this.failures += 1;
    return this.failures > this.maxRebuilds ? "give-up" : "rebuild";
  }

  /** A frame decoded, so the decoder works: forget the failures. */
  onProgress(): void {
    this.failures = 0;
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
export function decoderIsBehind(
  queueSize: number,
  limit: number = MAX_DECODE_QUEUE,
): boolean {
  return queueSize > limit;
}
