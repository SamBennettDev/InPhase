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

/** Wrap-aware "is `a` behind `b`" over a u32 frame counter. */
function isBefore(a: number, b: number): boolean {
  return (a - b) >>> 0 >= 0x80000000;
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

  constructor(private readonly maxReorder: number = MAX_REORDER) {}

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

  /** Forget everything and wait for a fresh keyframe. */
  reset(): void {
    this.pending.clear();
    this.nextSeq = null;
    this.resyncNeeded = false;
    this.farAhead = 0;
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
  private isPlausibleNext(seq: number, anchor: number): boolean {
    const ahead = (seq - anchor) >>> 0;
    return ahead < 0x80000000 && ahead <= this.maxReorder;
  }

  /**
   * Offer one frame. Returns the frames that are now contiguous and ready to
   * decode, oldest first — usually zero or one, more when a late keyframe
   * releases the deltas that were waiting on it.
   */
  accept(f: WtFrame): WtFrame[] {
    if (f.key) {
      // A keyframe is a resync point: anchor here whenever it turns up, and
      // keep the frames that raced ahead of it — they are early, not garbage.
      for (const seq of [...this.pending.keys()]) {
        if (!this.isPlausibleNext(seq, f.frame_no)) this.pending.delete(seq);
      }
      this.nextSeq = f.frame_no;
      this.resyncNeeded = false;
      this.farAhead = 0;
    } else if (this.nextSeq === null) {
      // No anchor yet. Hold it: the keyframe is very likely already in flight
      // behind this frame, and dropping it here is what caused the stall.
      if (this.pending.size < this.maxReorder) this.pending.set(f.frame_no, f);
      return [];
    } else if (isBefore(f.frame_no, this.nextSeq)) {
      return []; // already decoded past this
    } else if (!this.isPlausibleNext(f.frame_no, this.nextSeq)) {
      // Too far ahead to be late delivery of *this* sequence. A couple of these
      // are the previous pipeline's stragglers; a run of them means the frame
      // we are waiting for was dropped and the stream has moved on.
      this.farAhead += 1;
      if (this.farAhead > this.maxReorder) {
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
      this.nextSeq = (this.nextSeq + 1) >>> 0;
      ready.push(next);
    }
    // A full window with no progress is a hole that will not fill: the frame we
    // are waiting for was dropped, and everything behind it is undecodable with
    // an infinite GOP. Stop waiting and ask for an anchor.
    if (this.pending.size >= this.maxReorder) {
      this.pending.clear();
      this.nextSeq = null;
      this.resyncNeeded = true;
    }
    return ready;
  }
}
