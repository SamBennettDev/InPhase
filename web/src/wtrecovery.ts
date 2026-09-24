/**
 * One recovery state machine for the WT glass (review §15: "one recovery
 * state machine" replaces duplicate throttles and reset/redial cascades).
 *
 * The stage ladder mirrors the measured failure modes:
 *   - a network stall can break the decode reference chain while the
 *     transport stays healthy — reset the decoder and demand an IDR;
 *   - a still-frozen glass after a reset means the path itself is wedged —
 *     redial the whole WT path (the fresh session re-gates on a keyframe).
 *
 * Pure and node-tested: the caller feeds presented-frame counts each watchdog
 * tick and executes whatever action comes back.
 */
export type RecoveryAction = "none" | "reset" | "redial";

export interface WtRecoveryOptions {
  /** Frozen-for-this-long triggers a decoder reset + IDR request. */
  resetAfterMs?: number;
  /** Still frozen after a reset triggers a full redial. */
  redialAfterMs?: number;
  /** Minimum spacing between resets. */
  resetCooldownMs?: number;
  /** Minimum spacing between redials. */
  redialCooldownMs?: number;
  /** How long an outstanding keyframe request holds off a decoder reset. */
  keyframeDeadlineMs?: number;
  /** Injectable clock for tests. */
  now?: () => number;
}

/** What the decoder knows about the freeze it is in. */
export interface RecoveryContext {
  /** The page is hidden: presents stop legitimately (audit §2.6). */
  hidden?: boolean;
  /** When the newest unanswered keyframe request went out (same clock as
   *  `now`), or null when no keyframe is awaited. */
  keyframeRequestedAtMs?: number | null;
}

export class WtRecovery {
  private presented = -1;
  private progressAt = 0;
  private lastResetAt = Number.NEGATIVE_INFINITY;
  private lastRedialAt = Number.NEGATIVE_INFINITY;
  private readonly resetAfterMs: number;
  private readonly redialAfterMs: number;
  private readonly resetCooldownMs: number;
  private readonly redialCooldownMs: number;
  private readonly keyframeDeadlineMs: number;
  private readonly now: () => number;

  constructor(opts: WtRecoveryOptions = {}) {
    this.resetAfterMs = opts.resetAfterMs ?? 3000;
    this.redialAfterMs = opts.redialAfterMs ?? 10000;
    this.resetCooldownMs = opts.resetCooldownMs ?? 4000;
    this.redialCooldownMs = opts.redialCooldownMs ?? 20000;
    // One host gate (1000 ms) plus an IDR's paced drain and an RTT.
    this.keyframeDeadlineMs = opts.keyframeDeadlineMs ?? 2000;
    this.now = opts.now ?? (() => performance.now());
  }

  /** A fresh presented-frame count from the watchdog tick. */
  observe(presentedFrames: number, ctx: RecoveryContext = {}): RecoveryAction {
    const now = this.now();
    // A hidden tab is not a frozen one: suspend the ladder and restart the
    // stall clock, so the first tick back in view does not fire a reset for
    // time spent in the background.
    if (presentedFrames !== this.presented || ctx.hidden === true) {
      this.presented = presentedFrames;
      this.progressAt = now;
      return "none";
    }
    const frozenFor = now - this.progressAt;
    if (frozenFor > this.redialAfterMs && now - this.lastRedialAt > this.redialCooldownMs) {
      this.lastRedialAt = now;
      // A redial implies the decoder reset too: the lower rung must not
      // re-fire while the redial cooldown is still holding the ladder.
      this.lastResetAt = now;
      return "redial";
    }
    if (frozenFor > this.resetAfterMs && now - this.lastResetAt > this.resetCooldownMs) {
      // The reset would cancel the very IDR the freeze is waiting on: it
      // flushes the codec queue and the orderer's held run, and re-asks
      // behind a host gate that just honoured the last request (audit §2.5).
      // Give an outstanding request its deadline; the redial rung still runs.
      const askedAt = ctx.keyframeRequestedAtMs ?? null;
      if (askedAt !== null && now - askedAt < this.keyframeDeadlineMs) return "none";
      this.lastResetAt = now;
      return "reset";
    }
    return "none";
  }

  /** Glass is gone entirely (transport closed / not dialed) — forget history. */
  reset(): void {
    this.presented = -1;
    this.progressAt = this.now();
  }
}
