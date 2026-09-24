// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Decode ordering for frames that arrive on independent QUIC streams.
//
// One frame per unidirectional stream means streams complete independently, so
// frames arrive out of order — a 400 KB keyframe finishes *after* the small
// delta frames queued behind it. HEVC has to be decoded in order, so something
// has to put them back.
//
// This is pure logic over frame numbers with no decoder in sight, because the
// bug it exists to prevent was invisible in every test we had: the datagram-era
// gate treated a late keyframe as stale and dropped it, and since a keyframe is
// always the largest thing on the wire it lost that race every single time. The
// client asked for a keyframe forever and decoded nothing on a link delivering
// 6.6 Mbps at 0 % loss, and nothing short of a real phone could see it.

import type { WtFrame } from "./wtvideo.js";

/**
 * Frames held while waiting for a missing one.
 *
 * This covers *stream-completion jitter* only — a handful of frames — and is
 * deliberately not sized to ride out loss. With an effectively-infinite GOP a
 * genuinely missing frame makes everything behind it undecodable, so waiting is
 * pure latency: at 48 this held ~0.8 s of video hostage on every dropped frame.
 */
export const MAX_REORDER = 8;

/**
 * Video held hostage before a hole is declared fatal — the horizon the repair
 * ladder needs, in time rather than in frames.
 *
 * The client has two answers to a hole and they were racing: the frame-order
 * gate gave up after MAX_REORDER frames (8 frames at 60 fps = **133 ms**) while
 * the NACK ladder's first repair round only fires 150 ms after the hole's first
 * fragment (`wtcore.onVideoFragment`), with the re-send landing an RTT later.
 * So the gate declared a hole fatal ~17 ms *before* its own repair attempt went
 * out, asked for an IDR, and reset the sequence: a keyframe every ~1 s, each
 * one forcing the host to encode an IDR, whose burst then inflated the client's
 * tail delay until the bitrate controller cut — the 2026-09-20 sawtooth, with
 * `held` cycling 0 → 8 → 0 and `keys` climbing one per second while the decoder
 * sat idle (`queue=0 behind=0`, so this was never backpressure).
 *
 * The window is now the cadence-independent budget below, so a repair that
 * lands inside it costs nothing at all.
 */
export const REORDER_BUDGET_MS = 300;

/** How long an anchored hole may hold the picture before it is given up for
 *  an IDR, on a route with `rttMs` round trips (0 = unknown -> the full
 *  {@link REORDER_BUDGET_MS}).
 *
 *  Scaled so the client's four RTT-paced repair rounds (`nackTiming` in
 *  wtcore.ts: ~40 ms, then every ~50 ms on a 5 ms LAN) and their re-sends all
 *  land inside it, with margin - and no longer. A fixed 300 ms waited out
 *  ~100 ms past the last repair that could still arrive, and every
 *  unrepairable burst froze the picture that much longer before re-keying
 *  (Mac, 1440p120, 2026-09-24: ~390 ms per event). */
export function repairBudgetMs(rttMs: number): number {
  if (!(rttMs > 0)) return REORDER_BUDGET_MS;
  return Math.min(REORDER_BUDGET_MS, Math.max(180, 10 * rttMs + 170));
}

/** Memory backstop: a hole this deep is not a hole, the stream moved on. */
export const MAX_HELD_FRAMES = 64;

/**
 * Held video between keyframe re-requests while waiting for an anchor.
 *
 * Waiting for an IDR the client already asked for is not a hole: the IDR is
 * the largest thing on the wire and lands last. Re-asking every budget window
 * outran both the page throttle and the host's 1000 ms gate, so most asks were
 * refused (2026-09 audit §2.1: "three requests, two refused").
 */
export const KEY_REREQUEST_MS = 2 * REORDER_BUDGET_MS;

/** Wrap-aware "is `a` behind `b`" over a u32 frame counter. */
function isBefore(a: number, b: number): boolean {
  return ((a - b) >>> 0) >= 0x80000000;
}

export class FrameOrderer {
  private pending = new Map<number, WtFrame>();
  /** Next frame number that can be decoded; null until a keyframe anchors. */
  private nextSeq: number | null = null;
  /** Set when the sequence has moved on without us: ask for a keyframe. */
  private resyncNeeded = false;
  /** Consecutive frames rejected for being beyond the reorder window.
   *
   *  One or two are the previous pipeline's stragglers and mean nothing. A run
   *  of them means a frame we are waiting for is never coming and the stream
   *  has left us behind - only then is a keyframe worth asking for. */
  private farAhead = 0;
  /** EMA of the capture-time step between frames, for a horizon in *time*. */
  private cadenceUs = 16_667;
  private lastCaptureUs: number | null = null;
  /** Newest capture time seen, the orderer's only clock. Survives reset(): it
   *  times keyframe re-requests, which outlive the sequence they repair. */
  private newestUs = 0;
  /** The last frame handed to the decoder. Never cleared by reset(): a
   *  keyframe at or behind it is a second copy (stream + datagram) or a stale
   *  repair, and anchoring on it rewinds the sequence into a hole nothing can
   *  fill - which costs another IDR. */
  private lastReleased: { no: number; captureUs: number } | null = null;
  /** While unanchored: a keyframe request is due now. */
  private keyDue = false;
  /** Capture time before which no re-request is due; null = due at once. */
  private keyDueAtUs: number | null = null;

  /** Give-up point for an anchored hole; see {@link repairBudgetMs}. */
  private budgetMs = REORDER_BUDGET_MS;

  constructor(private readonly maxReorder: number = MAX_REORDER) {}

  /** Track the route's RTT (see {@link repairBudgetMs}). */
  setRttMs(rttMs: number): void {
    this.budgetMs = repairBudgetMs(rttMs);
  }

  /** How far ahead of the cursor a frame may be and still be "late", not "a
   *  different sequence": the repair budget converted to frames at the
   *  observed cadence (8 frames is only a floor — a 60 fps stream needs 18 to
   *  cover {@link REORDER_BUDGET_MS}). */
  private horizon(): number {
    const frames = Math.ceil((REORDER_BUDGET_MS * 1000) / Math.max(1, this.cadenceUs));
    return Math.max(this.maxReorder, frames);
  }

  /** Declare an *anchored* hole fatal once the held video passes the repair
   *  budget. Never called while unanchored - see {@link holdUnanchored}.
   *
   *  Returns whether the sequence was just reset. */
  private budgetSpent(newestUs: number): boolean {
    if (this.pending.size === 0) return false;
    const spent =
      this.pending.size > MAX_HELD_FRAMES || this.heldSpanMs(newestUs) >= this.budgetMs;
    if (spent) {
      this.pending.clear();
      this.nextSeq = null;
      this.resyncNeeded = true;
    }
    return spent;
  }

  /**
   * Hold a frame while no keyframe anchors the sequence.
   *
   * No timer applies here. The run behind an IDR the client already asked for
   * is exactly what that IDR will release, and clearing it every 300 ms (the
   * old behaviour) meant the IDR that finally landed anchored nothing: its
   * deltas were gone, the hole behind it spent the budget again, and one lost
   * frame became a 1-4 s freeze of back-to-back IDR requests (audit §2.1).
   * Memory is bounded by evicting the *oldest* capture - frames from before the
   * requested IDR, which it would prune anyway - never the run racing ahead.
   */
  private holdUnanchored(f: WtFrame): void {
    if (!this.pending.has(f.frame_no) && this.pending.size >= MAX_HELD_FRAMES) {
      let oldest: WtFrame | null = null;
      for (const p of this.pending.values()) {
        if (oldest === null || p.capture_us < oldest.capture_us) oldest = p;
      }
      if (oldest === null || f.capture_us <= oldest.capture_us) return;
      this.pending.delete(oldest.frame_no);
    }
    this.pending.set(f.frame_no, f);
    if (this.keyDueAtUs === null || this.newestUs >= this.keyDueAtUs) this.keyDue = true;
  }

  /** At or behind the last released frame, and not captured after it: a
   *  frame the decoder has already had (or passed). Both, because either one
   *  alone restarts with a pipeline on some host build (frame numbers used to,
   *  PTS still does), and a new stream mistaken for a replay never decodes. */
  private isReplay(f: WtFrame): boolean {
    const last = this.lastReleased;
    if (last === null) return false;
    const atOrBehind = f.frame_no === last.no || isBefore(f.frame_no, last.no);
    return atOrBehind && f.capture_us <= last.captureUs;
  }

  /** How much video is currently held hostage on the oldest pending frame. */
  private heldSpanMs(newestUs: number): number {
    let oldest = Number.POSITIVE_INFINITY;
    for (const f of this.pending.values()) {
      if (f.capture_us < oldest) oldest = f.capture_us;
    }
    return Number.isFinite(oldest) ? (newestUs - oldest) / 1000 : 0;
  }

  /** True while no keyframe has anchored the sequence. */
  get needsKeyframe(): boolean {
    return this.nextSeq === null;
  }

  /** Set when a hole did not fill; cleared by {@link reset}. */
  get needsResync(): boolean {
    return this.resyncNeeded;
  }

  get held(): number {
    return this.pending.size;
  }

  /** Unanchored and a keyframe request is due. Stays true until the caller
   *  reports one sent ({@link keyframeRequested}), so a request the page
   *  throttle refused is retried on the next arrival rather than lost. */
  get keyframeDue(): boolean {
    return this.nextSeq === null && this.keyDue;
  }

  /** A keyframe request went out: the next is due {@link KEY_REREQUEST_MS} of
   *  held video from now, giving this one time to land. */
  keyframeRequested(): void {
    this.keyDue = false;
    this.keyDueAtUs = this.newestUs + KEY_REREQUEST_MS * 1000;
  }

  /** Forget the sequence and wait for a fresh keyframe. The replay guard and
   *  the re-request schedule survive a plain reset - both describe what the
   *  decoder and the host have already been told, and a reference gap is
   *  exactly when the late second copy of the old IDR turns up. A new stream
   *  (`newStream`, a decoder reconfigure) forgets them too. */
  reset(newStream = false): void {
    if (newStream) {
      this.lastReleased = null;
      this.newestUs = 0;
      this.keyDue = false;
      this.keyDueAtUs = null;
    }
    this.pending.clear();
    this.nextSeq = null;
    this.resyncNeeded = false;
    this.farAhead = 0;
    this.lastCaptureUs = null;
  }

  /**
   * Is `seq` a frame this sequence could plausibly still be waiting for?
   *
   * Anything behind the anchor is spent. Anything further ahead than the
   * reorder window is not late delivery - it is a different sequence. The frame
   * counter restarts at 0 with each pipeline, and the previous pipeline's
   * in-flight frames arrive on the new session numbered in the hundreds: they
   * are numerically *ahead* of the new keyframe, so they were held forever,
   * pinned the buffer near its limit, and the first genuine reorder tipped it
   * over. Clear, resync, ask for a keyframe, repeat - 6.8 Mbps arriving at 0%
   * loss and not one frame decoded (2026-09-08).
   */
  private isPlausibleNext(seq: number, anchor: number, reach: number = 0): boolean {
    const ahead = (seq - anchor) >>> 0;
    return ahead < 0x80000000 && ahead <= Math.max(this.horizon(), reach);
  }

  /**
   * Offer one frame. Returns the frames that are now contiguous and ready to
   * decode, oldest first — usually zero or one, more when a late keyframe
   * releases the deltas that were waiting on it.
   */
  accept(f: WtFrame): WtFrame[] {
    if (this.lastCaptureUs !== null) {
      const step = f.capture_us - this.lastCaptureUs;
      if (step > 0 && step < 1_000_000) this.cadenceUs = this.cadenceUs * 0.9 + step * 0.1;
    }
    this.lastCaptureUs = f.capture_us;
    // capture_us is the pipeline's PTS, so it restarts with each pipeline
    // while frame numbers carry on: a capture far behind the newest is a new
    // clock, and a re-request scheduled on the old one would never fall due.
    if (f.capture_us > this.newestUs) {
      this.newestUs = f.capture_us;
    } else if (this.newestUs - f.capture_us > 5_000_000) {
      this.newestUs = f.capture_us;
      this.keyDueAtUs = null;
    }
    if (f.key) {
      // The same IDR arrives twice by design (stream + datagram copy, audit
      // §3.5), and a stale repair IDR can land after a newer one. Anchoring on
      // either rewinds the cursor behind frames already decoded: the deltas
      // after it are never coming again, so the hole spends the budget and
      // asks for yet another IDR. Drop it; whatever was pending is untouched.
      if (this.isReplay(f)) return [];
      // A keyframe is a resync point: anchor here whenever it turns up, and
      // keep the frames that raced ahead of it — they are early, not garbage.
      // The whole unanchored hold is kept, not just the reorder horizon: an
      // IDR slower than 300 ms still releases every delta queued behind it.
      for (const seq of [...this.pending.keys()]) {
        if (!this.isPlausibleNext(seq, f.frame_no, MAX_HELD_FRAMES)) this.pending.delete(seq);
      }
      this.nextSeq = f.frame_no;
      this.resyncNeeded = false;
      this.farAhead = 0;
      this.keyDue = false;
      this.keyDueAtUs = null;
    } else if (this.nextSeq === null) {
      // No anchor yet. Hold it: the keyframe is very likely already in flight
      // behind this frame, and dropping it here is what caused the stall.
      if (!this.isReplay(f)) this.holdUnanchored(f);
      return [];
    } else if (isBefore(f.frame_no, this.nextSeq)) {
      return []; // already decoded past this
    } else if (this.budgetSpent(f.capture_us)) {
      // A hole that has held the repair budget of video is not waiting for a
      // re-send any more. Checked before the plausibility test because an
      // arrival too far ahead to hold is still an arrival: it is the video
      // stacked behind the hole that decides, not whether we keep this frame.
      return [];
    } else if (!this.isPlausibleNext(f.frame_no, this.nextSeq)) {
      // Too far ahead to be late delivery of *this* sequence. A couple of these
      // are the previous pipeline's stragglers; a run of them means the frame
      // we are waiting for was dropped and the stream has moved on.
      this.farAhead += 1;
      if (this.farAhead > this.horizon()) {
        this.pending.clear();
        this.nextSeq = null;
        this.resyncNeeded = true;
      }
      return [];
    }

    this.farAhead = 0;
    this.pending.set(f.frame_no, f);

    const ready: WtFrame[] = [];
    while (this.nextSeq !== null) {
      const next = this.pending.get(this.nextSeq);
      if (next === undefined) break;
      this.pending.delete(this.nextSeq);
      // Re-check every frame the walk reaches, not only the arrival (audit
      // §3.7): whatever path put it in `pending`, nothing the decoder has
      // already had is handed to it twice. Vacate the slot and wait.
      if (this.isReplay(next)) break;
      this.nextSeq = (this.nextSeq + 1) >>> 0;
      ready.push(next);
      this.lastReleased = { no: next.frame_no, captureUs: next.capture_us };
    }
    // A hole that will not fill leaves everything behind it undecodable with an
    // infinite GOP: once the budget is spent, stop waiting and ask for an anchor.
    this.budgetSpent(f.capture_us);
    return ready;
  }
}
