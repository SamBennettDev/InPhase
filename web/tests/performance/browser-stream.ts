// SPDX-License-Identifier: GPL-3.0-or-later
// Synthetic moving video through the real WebCodecs + canvas player.
// This exercises browser decode/presentation; it does not measure host capture.
import { WtDecoder } from "../../src/wtdecoder.js";
import type { WtFrame } from "../../src/wtvideo.js";

export interface BrowserStreamOptions {
  width: number;
  height: number;
  fps: number;
  durationMs: number;
  jitterMs: number;
  pacing: boolean;
  outage?: [number, number];
}

export async function runBrowserStream(opts: BrowserStreamOptions) {
  const { width, height, fps, durationMs, jitterMs, pacing } = opts;
  const source = new OffscreenCanvas(width, height);
  const cx = source.getContext("2d")!;
  const chunks: { key: boolean; payload: Uint8Array }[] = [];
  const errors: string[] = [];
  const codec = "avc1.420034"; // level 5.2 also fits 1440p120
  const encoder = new VideoEncoder({
    output(chunk) {
      const payload = new Uint8Array(chunk.byteLength);
      chunk.copyTo(payload);
      chunks.push({ key: chunk.type === "key", payload });
    },
    error(e) { errors.push(`encode: ${e.message}`); },
  });
  encoder.configure({ codec, width, height, framerate: fps, bitrate: 8_000_000,
    latencyMode: "realtime", avc: { format: "annexb" }, hardwareAcceleration: "prefer-software" });
  // One second of motion, every loop starts with a clean reference chain.
  for (let i = 0; i < fps; i++) {
    cx.fillStyle = `hsl(${i * 7 % 360}, 75%, 40%)`;
    cx.fillRect(0, 0, width, height);
    for (let j = 0; j < 20; j++) {
      cx.fillStyle = `hsl(${(i * 13 + j * 29) % 360}, 90%, 70%)`;
      cx.fillRect((i * 23 + j * 97) % width, (i * 11 + j * 53) % height, width / 9, height / 7);
    }
    const vf = new VideoFrame(source, { timestamp: Math.round(i * 1e6 / fps) });
    encoder.encode(vf, { keyFrame: i === 0 });
    vf.close();
    if (encoder.encodeQueueSize > 4) await new Promise((resolve) => encoder.addEventListener("dequeue", resolve, { once: true }));
  }
  await encoder.flush();
  encoder.close();
  if (chunks.length !== fps || errors.length) throw new Error(`fixture encode failed: ${chunks.length}/${fps}: ${errors.join(", ")}`);
  let keyRequests = 0;
  const decoder = new WtDecoder(() => {}, () => { keyRequests++; }, () => 0,
    (why) => errors.push(why), undefined, { canvas2d: true });
  document.body.replaceChildren(decoder.root);
  decoder.configure({ codec, width, height, fps });
  decoder.setPacing(pacing);
  decoder.setRttMs(20);
  // Keep the clock epoch and each reference chain intact when deliveries reorder.
  const start = performance.now() + 50;
  const frames: { at: number; frame: WtFrame }[] = [];
  const count = Math.floor(durationMs * fps / 1000);
  let state = 42;
  for (let i = 0; i < count; i++) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    const captureMs = start + i * 1000 / fps;
    const relativeMs = captureMs - start;
    if (opts.outage && relativeMs >= opts.outage[0] && relativeMs < opts.outage[1]) continue;
    const c = chunks[i % chunks.length]!;
    frames.push({ at: captureMs + 5 + state / 2 ** 32 * jitterMs,
      frame: { frame_no: i, capture_us: Math.round(captureMs * 1000), key: c.key, payload: c.payload } });
  }
  frames.sort((a, b) => a.at - b.at);
  let index = 0;
  let maxQueue = 0;
  let maxHeld = 0;
  const observed = decoder as unknown as { decoder: VideoDecoder | null; order: { held: number } };
  const statsTimer = window.setInterval(() => decoder.stats(), 1000);
  const pump = () => {
    const now = performance.now();
    while (index < frames.length && frames[index]!.at <= now) decoder.onFrame(frames[index++]!.frame);
    maxQueue = Math.max(maxQueue, observed.decoder?.decodeQueueSize ?? 0);
    maxHeld = Math.max(maxHeld, observed.order.held);
    if (index < frames.length) timer = window.setTimeout(pump, Math.max(1, frames[index]!.at - performance.now()));
  };
  let timer = window.setTimeout(pump, 1);
  try {
    await new Promise((resolve) => setTimeout(resolve, durationMs + jitterMs + 350));
    const stats = decoder.stats();
    const framesSummary = decoder.frames.summary(fps);
    const ctx = decoder.root.getContext("2d")!;
    const pixels = ctx.getImageData(0, 0, 8, 8).data;
    const lit = pixels.some((v, i) => i % 4 !== 3 && v > 20);
    return { ...opts, stats, framesSummary, keyRequests, errors, maxQueue, maxHeld, lit,
      fed: index, encoded: chunks.length, pacingDelayMs: decoder.pacingDelayMs,
      userAgent: navigator.userAgent, softwareFixtureEncoder: true };
  } finally {
    clearTimeout(timer);
    clearInterval(statsTimer);
    decoder.stop();
  }
}
