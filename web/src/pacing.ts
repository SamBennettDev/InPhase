// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Frame pacing (an optional setting, off by default): hand each frame to the
// decoder at its capture time plus a small, adaptive delay instead of the
// moment it arrives, so frames reach the screen spaced as they were captured.
//
// Without it, every frame that arrives late - a Wi-Fi channel hop, a cellular
// burst - is a visible hitch, and the frames behind it arrive bunched and are
// skipped. With it, frames are spaced the way the PC captured them, at the
// cost of the delay: about the spread of recent arrival times.
//
// The delay needs no clock sync. A frame's "lateness" is how much longer it
// took than the fastest recent frame (arrival time minus capture time, minus
// the window's minimum), so the unknown offset between the two clocks cancels.
// The delay is the 95th percentile of recent lateness: it rises at once when
// the route gets worse and falls slowly when it calms, because every step
// down is itself a small jump forward. A frame that misses its slot anyway is
// decoded immediately.
//
// Measured 2026-10-10 (Chrome on the host PC, hardware decode, 1080p60 of
// moving content): hitches 194 and 165 per ~2,076 frames off, 6 and 9 on;
// capture-to-screen median +7 ms.
//
// Pure logic over timestamps passed in, so it is tested without a DOM.

/** Recent frames the delay is sized from: ~2 s at 120 fps, 4 s at 60. */
export const PACING_WINDOW = 240;
/** Upper bound on the added delay, ms. A route worse than this needs a lower
 *  frame rate or bitrate, not a bigger buffer. */
export const MAX_PACING_DELAY_MS = 40;
/** How fast the delay comes back down once the route calms, ms per second. */
export const PACING_DECAY_MS_PER_S = 4;
/** Slack on top of the percentile, ms. */
const MARGIN_MS = 1;
/** Encoded frames held at most: 40 ms at 120 fps with room to spare. */
export const MAX_PACED_FRAMES = 16;

export class PlayoutClock {
  /** arrivalMs - captureMs per recent frame, oldest first. */
  private readonly transit: number[] = [];
  private delay = 0;
  private lastMs = 0;

  /** A frame that arrived at `arrivedMs` (client clock) and that the PC
   *  captured at `captureUs` (PC clock). */
  observe(captureUs: number, arrivedMs: number): void {
    this.transit.push(arrivedMs - captureUs / 1000);
    if (this.transit.length > PACING_WINDOW) this.transit.shift();
    const base = Math.min(...this.transit);
    const late = this.transit.map((t) => t - base).sort((a, b) => a - b);
    const target = Math.min(
      MAX_PACING_DELAY_MS,
      late[Math.min(late.length - 1, Math.floor(late.length * 0.95))]! + MARGIN_MS,
    );
    if (target >= this.delay) {
      this.delay = target;
    } else if (this.lastMs > 0) {
      const dt = Math.max(0, arrivedMs - this.lastMs) / 1000;
      this.delay = Math.max(target, this.delay - PACING_DECAY_MS_PER_S * dt);
    }
    this.lastMs = arrivedMs;
  }

  /** The added delay now, ms. */
  get delayMs(): number {
    return this.delay;
  }

  /** When (client clock, ms) the frame captured at `captureUs` is due. */
  dueMs(captureUs: number): number {
    if (this.transit.length === 0) return 0;
    return captureUs / 1000 + Math.min(...this.transit) + this.delay;
  }

  reset(): void {
    this.transit.length = 0;
    this.delay = 0;
    this.lastMs = 0;
  }
}

/**
 * Encoded frames waiting for their slot, in arrival order.
 *
 * Pacing holds frames BEFORE the decoder: a decoded VideoFrame pins one of the
 * decoder's output buffers, and holding a few of them stalled decoding outright
 * (Chromium: decode time 183 ms, 25 of 60 fps shown). Encoded chunks cost the
 * decoder nothing. They are released in order - each references the ones
 * before it, so none is ever dropped here, and a full queue releases its oldest
 * early instead.
 */
export class PacedQueue<T> {
  private readonly items: { item: T; due: number }[] = [];

  get size(): number {
    return this.items.length;
  }

  /** When the oldest frame is due, or null when empty. */
  get nextDue(): number | null {
    return this.items[0]?.due ?? null;
  }

  /** Queue a frame; returns any released early because the queue is full. */
  push(item: T, due: number): T[] {
    this.items.push({ item, due });
    const over = this.items.length - MAX_PACED_FRAMES;
    return over > 0 ? this.items.splice(0, over).map((x) => x.item) : [];
  }

  /** Frames due by `nowMs`, oldest first. A frame due later than one behind
   *  it (the delay fell between them) holds that one back: order wins. */
  take(nowMs: number): T[] {
    let n = 0;
    while (n < this.items.length && this.items[n]!.due <= nowMs) n++;
    return this.items.splice(0, n).map((x) => x.item);
  }

  /** Empty the queue, oldest first. */
  clear(): T[] {
    return this.items.splice(0).map((x) => x.item);
  }
}
