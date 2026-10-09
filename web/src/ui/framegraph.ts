// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// The stats overlay's frametime graph (see frametimes.ts for what is timed).
//
// Two strips over the same frames, newest on the right:
// - Frametime: the gap between frames reaching the screen, against a dashed
//   line at the interval a smooth stream would show. A flat line is smooth;
//   spikes are hitches, drawn in red when over 1.5x the line.
// - Frame age by stage, as stacked bars: network (capture on the PC to
//   arrival, so host encode too), decode, wait for a refresh, and show. A
//   tall bar with a tall network segment is the network; with a tall decode
//   segment, this device.
//
// It redraws ten times a second, not every refresh: drawing on the thread
// that presents the video must not become the stutter it measures.

import { FRAME_WINDOW as FRAME_SLOTS, JANK_FACTOR, type FrameSample, type FrameSummary } from "../frametimes.js";

export interface FrameGraphSource {
  samples: readonly FrameSample[];
  targetMs: number;
  summary: FrameSummary;
}

const W = 240;
const H = 40;
const REDRAW_MS = 100;
/** Width of one bar in the stage strip, CSS px. */
const COL_PX = 2;

const C = {
  line: "#22d3ee",
  jank: "#f0616d",
  target: "rgba(238, 241, 246, 0.45)",
  grid: "rgba(238, 241, 246, 0.12)",
  net: "#8b5cf6",
  decode: "#22d3ee",
  wait: "#f5b544",
  show: "#c4c9d4",
  back: "rgba(8, 10, 14, 0.55)",
};

export class FrameGraph {
  readonly root: HTMLElement;
  private readonly ft: HTMLCanvasElement;
  private readonly st: HTMLCanvasElement;
  private readonly ftLabel: HTMLElement;
  private readonly stLabel: HTMLElement;
  private timer = 0;

  constructor(private readonly source: () => FrameGraphSource | null) {
    this.root = document.createElement("div");
    this.root.className = "frame-graph";
    this.root.innerHTML = `
      <div class="fg-head"><span>frametime</span><span data-l="ft"></span></div>
      <canvas data-c="ft"></canvas>
      <div class="fg-head"><span>frame age</span><span data-l="st"></span></div>
      <canvas data-c="st"></canvas>
      <div class="fg-legend">
        <span><i style="background:${C.net}"></i>network</span>
        <span><i style="background:${C.decode}"></i>decode</span>
        <span><i style="background:${C.wait}"></i>wait</span>
        <span><i style="background:${C.show}"></i>show</span>
      </div>`;
    this.ft = this.root.querySelector<HTMLCanvasElement>('[data-c="ft"]')!;
    this.st = this.root.querySelector<HTMLCanvasElement>('[data-c="st"]')!;
    this.ftLabel = this.root.querySelector<HTMLElement>('[data-l="ft"]')!;
    this.stLabel = this.root.querySelector<HTMLElement>('[data-l="st"]')!;
  }

  start(): void {
    if (this.timer !== 0) return;
    this.timer = window.setInterval(() => this.draw(), REDRAW_MS);
    this.draw();
  }

  stop(): void {
    clearInterval(this.timer);
    this.timer = 0;
  }

  private draw(): void {
    const src = this.source();
    if (src === null || !this.root.isConnected) return;
    const { samples, targetMs, summary } = src;
    const fmt = (v: number) => (v >= 10 ? v.toFixed(0) : v.toFixed(1));
    this.ftLabel.textContent = summary.frames
      ? `${fmt(summary.p50)} ms · p99 ${fmt(summary.p99)} · max ${fmt(summary.max)}`
      : "--";
    const net = summary.netMs !== null ? fmt(summary.netMs) : "?";
    this.stLabel.textContent = summary.frames
      ? `${net} + ${fmt(summary.decodeMs)} + ${fmt(summary.waitMs)} + ${fmt(summary.showMs)} ms`
      : "--";
    this.drawFrametimes(samples, targetMs, summary.jank);
    this.drawStages(samples);
  }

  private ctx(c: HTMLCanvasElement): CanvasRenderingContext2D {
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    if (c.width !== W * dpr) {
      c.width = W * dpr;
      c.height = H * dpr;
    }
    const g = c.getContext("2d")!;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    g.fillStyle = C.back;
    g.fillRect(0, 0, W, H);
    return g;
  }

  private drawFrametimes(samples: readonly FrameSample[], targetMs: number, jank: number): void {
    const g = this.ctx(this.ft);
    // The target sits a third of the way up; spikes to 3x fill the strip.
    const top = Math.max(targetMs * 3, 20);
    const y = (ms: number) => H - Math.min(ms, top) * (H / top);
    g.strokeStyle = C.target;
    g.setLineDash([3, 3]);
    g.beginPath();
    g.moveTo(0, y(targetMs) + 0.5);
    g.lineTo(W, y(targetMs) + 0.5);
    g.stroke();
    g.setLineDash([]);
    const n = samples.length;
    if (n < 2) return;
    const step = W / (FRAME_SLOTS - 1);
    const x0 = W - (n - 1) * step;
    g.strokeStyle = C.line;
    g.lineWidth = 1.25;
    g.beginPath();
    let started = false;
    samples.forEach((s, i) => {
      if (s.intervalMs <= 0) {
        started = false;
        return;
      }
      const px = x0 + i * step;
      if (!started) g.moveTo(px, y(s.intervalMs));
      else g.lineTo(px, y(s.intervalMs));
      started = true;
    });
    g.stroke();
    g.fillStyle = C.jank;
    samples.forEach((s, i) => {
      if (targetMs > 0 && s.intervalMs > targetMs * JANK_FACTOR) {
        g.fillRect(x0 + i * step - 1, y(s.intervalMs) - 1, 2.5, 2.5);
      }
    });
    if (jank > 0) {
      g.font = "9px ui-monospace, monospace";
      g.textAlign = "right";
      g.fillText(`${jank} hitch${jank === 1 ? "" : "es"}`, W - 3, 9);
    }
  }

  private drawStages(samples: readonly FrameSample[]): void {
    const g = this.ctx(this.st);
    const n = samples.length;
    if (n === 0) return;
    const total = (s: FrameSample) => (s.netMs ?? 0) + s.decodeMs + s.waitMs + s.showMs;
    // Scale to the slowest frame shown, in 25 ms steps, so a quiet stream
    // is not drawn as a flat line along the bottom.
    let peak = 0;
    for (const s of samples) peak = Math.max(peak, total(s));
    const top = Math.max(25, Math.ceil(peak / 25) * 25);
    const k = H / top;
    g.strokeStyle = C.grid;
    for (let ms = 25; ms < top; ms += 25) {
      g.beginPath();
      g.moveTo(0, H - ms * k + 0.5);
      g.lineTo(W, H - ms * k + 0.5);
      g.stroke();
    }
    // More frames than pixels: each column shows the slowest frame it
    // covers, so a one-frame hitch is as visible as a long one.
    const step = W / FRAME_SLOTS;
    const x0 = W - n * step;
    const worst = new Map<number, FrameSample>();
    samples.forEach((s, i) => {
      const col = Math.floor((x0 + i * step) / COL_PX);
      const w = worst.get(col);
      if (w === undefined || total(s) > total(w)) worst.set(col, s);
    });
    for (const [col, s] of worst) {
      let base = H;
      const seg = (ms: number, color: string) => {
        if (ms <= 0) return;
        const h = ms * k;
        g.fillStyle = color;
        g.fillRect(col * COL_PX, base - h, COL_PX - 0.5, h);
        base -= h;
      };
      seg(s.netMs ?? 0, C.net);
      seg(s.decodeMs, C.decode);
      seg(s.waitMs, C.wait);
      seg(s.showMs, C.show);
    }
    g.fillStyle = "rgba(238, 241, 246, 0.6)";
    g.font = "9px ui-monospace, monospace";
    g.textAlign = "left";
    g.fillText(`${top} ms`, 3, 9);
  }
}
