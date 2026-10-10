// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Per-frame timing for the stats overlay's frametime graph.
//
// Every video frame passes four points on this side: it arrives (the last
// fragment is in and the frame reaches the page), it is decoded, it is drawn
// to the canvas, and a display refresh shows it. The timeline stamps each
// point, keyed by the frame's capture timestamp, and keeps one sample per
// frame that reached the screen:
//
// - `intervalMs` is the gap since the previous frame reached the screen: the
//   frametime a player feels. Uneven intervals are stutter even when the
//   average fps looks right.
// - The stage times say where a frame's age went: `netMs` (capture on the PC
//   to arrival, so host encode + network; needs the synced clock), `decodeMs`
//   (arrival to decoded, including any wait behind a missing frame), `waitMs`
//   (held by frame pacing before decode, and decoded to drawn: held for a
//   refresh) and `showMs` (drawn to the refresh that shows it).
//
// "Shown" is the next requestAnimationFrame after the draw - the frame the
// browser composites the canvas into. The panel scans out a refresh or so
// later; that constant is not measurable from a page and is left out.
//
// Pure logic over timestamps passed in, so it is tested without a DOM.

export interface FrameSample {
  /** When the frame reached the screen (refresh timestamp, ms). */
  shownMs: number;
  /** Gap since the previous frame reached the screen, ms (0 for the first). */
  intervalMs: number;
  /** Capture on the PC to arrival here, ms; null before the clock syncs. */
  netMs: number | null;
  decodeMs: number;
  waitMs: number;
  showMs: number;
}

export interface FrameSummary {
  frames: number;
  /** Frametime percentiles over the window, ms. */
  p50: number;
  p99: number;
  max: number;
  /** Frames that took more than 1.5x the expected interval. */
  jank: number;
  /** Mean stage times, ms (net is null without clock sync). */
  netMs: number | null;
  decodeMs: number;
  waitMs: number;
  showMs: number;
  /** Median recent gap between refreshes - the display's refresh, ms. */
  refreshMs: number;
  /** Frames drawn but replaced before any refresh showed them. */
  overdrawn: number;
}

interface Pending {
  arriveMs: number;
  /** Handed to the decoder: later than arrival when frame pacing held it. */
  releasedMs: number;
  netMs: number | null;
  decodedMs: number;
  drawnMs: number;
}

/** Frames kept for the graph: about 2.5 s at 120 fps, 5 s at 60. */
export const FRAME_WINDOW = 300;
/** A frame this many times the expected interval counts as a hitch. */
export const JANK_FACTOR = 1.5;
/** Refresh gaps the display-rate estimate looks at. */
const REFRESH_GAPS = 120;
/** Frames in flight tracked at once; more means stamps are leaking. */
const MAX_IN_FLIGHT = 64;

export class FrameTimeline {
  private readonly inFlight = new Map<number, Pending>();
  /** Drawn, waiting for the refresh that shows it. */
  private onCanvas: Pending | null = null;
  private readonly ring: FrameSample[] = [];
  private lastShownMs = 0;
  private lastRefreshMs = 0;
  /** Recent gaps between refreshes, ms (newest last). */
  private readonly refreshGaps: number[] = [];
  private overdrawnCount = 0;

  constructor(private readonly window = FRAME_WINDOW) {}

  /** The frame is complete and handed to the decoder path. */
  arrived(captureUs: number, nowMs: number, netMs: number | null): void {
    if (this.inFlight.size >= MAX_IN_FLIGHT) {
      const oldest = this.inFlight.keys().next().value;
      if (oldest !== undefined) this.inFlight.delete(oldest);
    }
    this.inFlight.set(captureUs, {
      arriveMs: nowMs,
      releasedMs: nowMs,
      netMs: netMs !== null && netMs >= 0 && netMs < 5000 ? netMs : null,
      decodedMs: 0,
      drawnMs: 0,
    });
  }

  /** Frame pacing held the frame until now before decoding it. */
  released(captureUs: number, nowMs: number): void {
    const p = this.inFlight.get(captureUs);
    if (p) p.releasedMs = nowMs;
  }

  decoded(captureUs: number, nowMs: number): void {
    const p = this.inFlight.get(captureUs);
    if (p) p.decodedMs = nowMs;
  }

  /** Dropped before the screen (coalesced, behind, reset). */
  discard(captureUs: number): void {
    this.inFlight.delete(captureUs);
  }

  /**
   * The frame was drawn to the canvas. `refreshMs` is the current refresh's
   * timestamp when the draw happens inside a requestAnimationFrame callback:
   * that refresh shows it. Otherwise the next refresh does.
   */
  drawn(captureUs: number, nowMs: number, refreshMs: number | null = null): void {
    const p = this.inFlight.get(captureUs);
    this.inFlight.delete(captureUs);
    if (!p) return;
    p.drawnMs = nowMs;
    if (p.decodedMs === 0) p.decodedMs = nowMs;
    if (this.onCanvas !== null) this.overdrawnCount++;
    this.onCanvas = p;
    if (refreshMs !== null) this.show(refreshMs);
  }

  /** A display refresh began (requestAnimationFrame timestamp). */
  refresh(refreshMs: number): void {
    if (this.lastRefreshMs > 0) {
      const gap = refreshMs - this.lastRefreshMs;
      if (gap > 2) {
        this.refreshGaps.push(gap);
        if (this.refreshGaps.length > REFRESH_GAPS) this.refreshGaps.shift();
      }
    }
    this.lastRefreshMs = refreshMs;
    this.show(refreshMs);
  }

  private show(refreshMs: number): void {
    const p = this.onCanvas;
    if (p === null) return;
    this.onCanvas = null;
    // A draw stamped after the refresh began (the callback ran late in the
    // frame) is shown by this refresh all the same.
    const shownMs = Math.max(refreshMs, p.drawnMs);
    this.push({
      shownMs,
      intervalMs: this.lastShownMs > 0 ? shownMs - this.lastShownMs : 0,
      netMs: p.netMs,
      decodeMs: Math.max(0, p.decodedMs - p.releasedMs),
      // Held for its pacing slot before decode, then for a refresh after.
      waitMs: Math.max(0, p.releasedMs - p.arriveMs) + Math.max(0, p.drawnMs - p.decodedMs),
      showMs: Math.max(0, shownMs - p.drawnMs),
    });
    this.lastShownMs = shownMs;
  }

  private push(s: FrameSample): void {
    this.ring.push(s);
    if (this.ring.length > this.window) this.ring.shift();
  }

  /** Oldest first. */
  samples(): readonly FrameSample[] {
    return this.ring;
  }

  /** A stall or stream reset: the next frame starts a fresh interval. */
  reset(): void {
    this.inFlight.clear();
    this.onCanvas = null;
    this.lastShownMs = 0;
  }

  /** The interval a smooth stream shows frames at: the stream's own, or the
   *  display's refresh when that is slower (120 fps on a 60 Hz panel shows
   *  every 16.7 ms, and that is not stutter). */
  targetMs(streamFps: number): number {
    const stream = streamFps > 0 ? 1000 / streamFps : 0;
    return Math.max(stream, this.refreshIntervalMs());
  }

  /** The display's refresh interval: the median recent gap between
   *  refreshes. Not the shortest - one early callback (iPhone Safari at
   *  60 Hz has them) made a 60 Hz page look like 120 Hz, and every normal
   *  frame then counted as a hitch. */
  refreshIntervalMs(): number {
    const g = [...this.refreshGaps].sort((a, b) => a - b);
    return g.length ? g[Math.floor(g.length / 2)]! : 0;
  }

  /** Over the whole window, or only frames shown after `sinceMs`. */
  summary(streamFps: number, sinceMs = 0): FrameSummary {
    const ring = sinceMs > 0 ? this.ring.filter((x) => x.shownMs > sinceMs) : this.ring;
    const s = ring.filter((x) => x.intervalMs > 0);
    const iv = s.map((x) => x.intervalMs).sort((a, b) => a - b);
    const pct = (q: number) => (iv.length ? iv[Math.min(iv.length - 1, Math.floor(q * iv.length))]! : 0);
    const target = this.targetMs(streamFps);
    const mean = (f: (x: FrameSample) => number) =>
      ring.length ? ring.reduce((a, x) => a + f(x), 0) / ring.length : 0;
    const withNet = ring.filter((x) => x.netMs !== null);
    return {
      frames: ring.length,
      p50: pct(0.5),
      p99: pct(0.99),
      max: iv.length ? iv[iv.length - 1]! : 0,
      jank: target > 0 ? iv.filter((v) => v > target * JANK_FACTOR).length : 0,
      netMs: withNet.length ? withNet.reduce((a, x) => a + x.netMs!, 0) / withNet.length : null,
      decodeMs: mean((x) => x.decodeMs),
      waitMs: mean((x) => x.waitMs),
      showMs: mean((x) => x.showMs),
      refreshMs: this.refreshIntervalMs(),
      overdrawn: this.overdrawnCount,
    };
  }
}
