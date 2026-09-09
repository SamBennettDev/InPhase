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
 * Pure and node-tested: the caller feeds presented-frame counts each stats
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
  /** Injectable clock for tests. */
  now?: () => number;
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
  private readonly now: () => number;

  constructor(opts: WtRecoveryOptions = {}) {
    this.resetAfterMs = opts.resetAfterMs ?? 3000;
    this.redialAfterMs = opts.redialAfterMs ?? 10000;
    this.resetCooldownMs = opts.resetCooldownMs ?? 4000;
    this.redialCooldownMs = opts.redialCooldownMs ?? 20000;
    this.now = opts.now ?? (() => performance.now());
  }

  /** A fresh presented-frame count from the decoder's stats tick. */
  observe(presentedFrames: number): RecoveryAction {
    const now = this.now();
    if (presentedFrames !== this.presented) {
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
