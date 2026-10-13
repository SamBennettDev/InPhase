// SPDX-License-Identifier: GPL-3.0-or-later
// Deterministic virtual-time impairment of the production v4 receiver and
// frame-order gate. This is not a QUIC, encoder, or GPU benchmark.
import assert from "node:assert/strict";
import { FrameOrderer, MAX_HELD_FRAMES } from "../../src/frameorder.js";
import { NACK_WHOLE_FRAME, WtVideoClient, type WtClientHandlers } from "../../src/wtcore.js";
import type { WtFrame } from "../../src/wtvideo.js";

export interface NetworkProfile {
  name: string;
  rttMs: number;
  jitterMs: number;
  loss: number;
  outage?: [number, number];
  burst?: { everyMs: number; durationMs: number };
}

export const NETWORK_PROFILES: NetworkProfile[] = [
  { name: "lan", rttMs: 4, jitterMs: 0, loss: 0 },
  { name: "wifi-jitter", rttMs: 20, jitterMs: 30, loss: 0 },
  { name: "loss-1pct", rttMs: 20, jitterMs: 10, loss: 0.01 },
  { name: "loss-5pct", rttMs: 40, jitterMs: 20, loss: 0.05 },
  { name: "wan", rttMs: 120, jitterMs: 50, loss: 0.01 },
  { name: "burst", rttMs: 20, jitterMs: 10, loss: 0, burst: { everyMs: 2000, durationMs: 20 } },
  { name: "outage-1s", rttMs: 20, jitterMs: 10, loss: 0, outage: [8000, 9000] },
];

type Event = { at: number; serial: number; run: () => void };
class Events {
  private heap: Event[] = [];
  private serial = 0;
  private before(a: Event, b: Event): boolean {
    return a.at < b.at || (a.at === b.at && a.serial < b.serial);
  }
  push(at: number, run: () => void): void {
    const e = { at, serial: this.serial++, run };
    let i = this.heap.length;
    this.heap.push(e);
    while (i > 0) {
      const p = (i - 1) >>> 1;
      if (!this.before(e, this.heap[p]!)) break;
      this.heap[i] = this.heap[p]!;
      i = p;
    }
    this.heap[i] = e;
  }
  pop(): Event | undefined {
    const first = this.heap[0];
    const last = this.heap.pop();
    if (this.heap.length && last) {
      let i = 0;
      while (i * 2 + 1 < this.heap.length) {
        let c = i * 2 + 1;
        if (c + 1 < this.heap.length && this.before(this.heap[c + 1]!, this.heap[c]!)) c++;
        if (!this.before(this.heap[c]!, last)) break;
        this.heap[i] = this.heap[c]!;
        i = c;
      }
      this.heap[i] = last;
    }
    return first;
  }
}

function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state / 2 ** 32;
  };
}

const CAPTURE_EPOCH_US = 1_000_000_000_000;
const FRAGMENT_BYTES = 1100;
function payloadFor(no: number, length: number): Uint8Array {
  const p = new Uint8Array(length);
  for (let i = 0; i < length; i++) p[i] = (i * 31 + no * 17) & 255;
  return p;
}

/** v4 wire layout and 8+1 / 8+2 XOR rows, matching protocol/wtvideo.rs. */
function fragment(f: WtFrame): Uint8Array[] {
  const count = Math.ceil(f.payload.length / FRAGMENT_BYTES);
  const encode = (idx: number, row: number, body: Uint8Array): Uint8Array => {
    const d = new Uint8Array(18 + body.length);
    const v = new DataView(d.buffer);
    v.setUint8(0, 4);
    v.setUint8(1, Number(f.key) | (row << 1));
    v.setUint32(2, f.frame_no, true);
    v.setUint16(6, idx, true);
    v.setUint16(8, count, true);
    v.setBigUint64(10, BigInt(f.capture_us), true);
    d.set(body, 18);
    return d;
  };
  const data = Array.from({ length: count }, (_, i) =>
    encode(i, 0, f.payload.subarray(i * FRAGMENT_BYTES, (i + 1) * FRAGMENT_BYTES)));
  const parity: Uint8Array[] = [];
  for (let g = 0; g * 8 < count; g++) {
    for (let row = 1; row <= (f.key || count >= 16 ? 2 : 1); row++) {
      const p = new Uint8Array(FRAGMENT_BYTES + 4);
      for (let i = g * 8; i < Math.min(count, g * 8 + 8); i++) {
        if (row === 2 && i % 2 === 0) continue;
        const body = data[i]!.subarray(18);
        for (let j = 0; j < body.length; j++) p[j] = p[j]! ^ body[j]!;
      }
      new DataView(p.buffer).setUint32(FRAGMENT_BYTES, f.payload.length, true);
      parity.push(encode(g, row, p));
    }
  }
  // Spread a burst across groups, as fragment_send_order does on the host.
  const interleaved: Uint8Array[] = [];
  for (let offset = 0; offset < 8; offset++) {
    for (let i = offset; i < count; i += 8) interleaved.push(data[i]!);
  }
  return [...interleaved, ...parity];
}

interface ReceiverAccess {
  onVideoFragment(d: Uint8Array, h: WtClientHandlers): void;
  lastRttMs: number;
  v4: Map<number, unknown>;
  v4Repaired: number;
  framesAbandoned: number;
  send(m: Record<string, unknown>): Promise<void>;
}

export interface NetworkResult {
  profile: string;
  fps: number;
  seed: number;
  simulatedSeconds: number;
  generated: number;
  admitted: number;
  admittedPercent: number;
  ageP50Ms: number;
  ageP95Ms: number;
  maxGapMs: number;
  maxHeld: number;
  maxAssemblies: number;
  repairedFragments: number;
  abandoned: number;
  nacks: number;
  resyncs: number;
  lastProgressMs: number;
}

export function verifyNetworkResult(r: NetworkResult, profile: NetworkProfile): void {
  assert.ok(r.admitted > 0, "picture starts");
  assert.ok(r.lastProgressMs >= r.simulatedSeconds * 1000 - 1000, "picture still progresses at the end");
  if (profile.loss === 0 && !profile.outage && !profile.burst) {
    assert.equal(r.admitted, r.generated, "no frame lost on a lossless route");
    assert.equal(r.resyncs, 0, "ordinary jitter must not force IDRs");
  }
  const floor = profile.name === "loss-5pct" ? 95 : profile.name === "wan" ? 99 : profile.name === "loss-1pct" ? 98 : 0;
  assert.ok(r.admittedPercent >= floor, `${profile.name}: admitted ${r.admittedPercent}% < ${floor}%`);
  if (profile.outage) assert.ok(r.maxGapMs < 2000, `1s outage took ${r.maxGapMs}ms to recover`);
  assert.ok(r.nacks < r.generated * 3, `repair traffic bounded: ${r.nacks} for ${r.generated} frames`);
}

export function simulateNetwork(profile: NetworkProfile, fps: number, seed: number, seconds = 20): NetworkResult {
  const events = new Events();
  const rand = random(seed);
  let now = 1;
  const duration = seconds * 1000;
  const order = new FrameOrderer();
  order.setRttMs(profile.rttMs);
  const client = new WtVideoClient();
  const receiver = client as unknown as ReceiverAccess;
  receiver.lastRttMs = profile.rttMs;
  const cache = new Map<number, Uint8Array[]>();
  let keyNext = true;
  let lastKeyAsk = -Infinity;
  let previousNo: number | null = null;
  let lastProgress = 0;
  let maxGap = 0;
  let admitted = 0;
  let generated = 0;
  let nacks = 0;
  let resyncs = 0;
  let maxHeld = 0;
  let maxAssemblies = 0;
  const ages: number[] = [];
  const requestKey = () => {
    if (now - lastKeyAsk < 1000) return;
    lastKeyAsk = now;
    order.keyframeRequested();
    // Model a reliable control message and the host's 1s IDR request gate.
    events.push(now + profile.rttMs / 2, () => { keyNext = true; });
  };
  const h: WtClientHandlers = {
    onVideoConfig: () => {}, onClosed: () => {},
    onFrame(f) {
      assert.deepEqual(f.payload, payloadFor(f.frame_no, f.key ? 51_713 : 12_113), "FEC/retransmission must preserve bytes");
      for (const ready of order.accept(f)) {
        assert.ok(ready.key || previousNo === null || ready.frame_no === ((previousNo + 1) >>> 0), "never admit a broken delta chain");
        previousNo = ready.frame_no;
        ages.push(now - (ready.capture_us - CAPTURE_EPOCH_US) / 1000);
        if (lastProgress) maxGap = Math.max(maxGap, now - lastProgress);
        lastProgress = now;
        admitted++;
      }
      maxHeld = Math.max(maxHeld, order.held);
      if (order.needsResync) {
        resyncs++;
        order.reset();
        requestKey();
      } else if (order.keyframeDue) requestKey();
    },
  };
  const lost = (sentAt: number) => rand() < profile.loss ||
    (profile.outage !== undefined && sentAt >= profile.outage[0] && sentAt < profile.outage[1]) ||
    (profile.burst !== undefined && sentAt % profile.burst.everyMs < profile.burst.durationMs);
  const send = (d: Uint8Array, sentAt: number) => {
    if (lost(sentAt)) return;
    events.push(sentAt + profile.rttMs / 2 + rand() * profile.jitterMs, () => {
      receiver.onVideoFragment(d, h);
      maxAssemblies = Math.max(maxAssemblies, receiver.v4.size);
    });
  };
  receiver.send = async (m) => {
    if (m.type !== "nack") return;
    nacks++;
    const sentAt = now;
    events.push(sentAt + profile.rttMs / 2, () => {
      const saved = cache.get(Number(m.frame));
      if (!saved) return;
      const requested = Number(m.idx) === NACK_WHOLE_FRAME ? saved : saved.filter((d) =>
        (d[1]! >> 1) === 0 && new DataView(d.buffer).getUint16(6, true) === Number(m.idx));
      requested.forEach((d, i) => send(d, now + i * 0.1));
    });
  };
  for (let no = 0; no < seconds * fps; no++) {
    const captureMs = 1 + no * 1000 / fps;
    events.push(captureMs, () => {
      const key = keyNext;
      keyNext = false;
      const f: WtFrame = { frame_no: no, capture_us: CAPTURE_EPOCH_US + Math.round(captureMs * 1000), key, payload: payloadFor(no, key ? 51_713 : 12_113) };
      generated++;
      const ds = fragment(f);
      cache.set(no, ds);
      cache.delete(no - 151);
      ds.forEach((d, i) => send(d, now + i * 0.1));
    });
  }
  const originalNow = performance.now;
  try {
    performance.now = () => now;
    for (let e = events.pop(); e !== undefined; e = events.pop()) {
      now = e.at;
      if (now > duration + 2000) break;
      e.run();
      assert.ok(order.held <= MAX_HELD_FRAMES, "reorder memory bounded");
      assert.ok(receiver.v4.size <= 24, "partial-frame memory bounded");
    }
  } finally {
    performance.now = originalNow;
    client.close();
  }
  ages.sort((a, b) => a - b);
  const percentile = (p: number) => Math.round((ages[Math.min(ages.length - 1, Math.floor(ages.length * p))] ?? 0) * 10) / 10;
  return {
    profile: profile.name, fps, seed, simulatedSeconds: seconds, generated, admitted,
    admittedPercent: Math.round(admitted / generated * 10000) / 100,
    ageP50Ms: percentile(0.5), ageP95Ms: percentile(0.95), maxGapMs: Math.round(maxGap * 10) / 10,
    maxHeld, maxAssemblies, repairedFragments: receiver.v4Repaired, abandoned: receiver.framesAbandoned,
    nacks, resyncs, lastProgressMs: Math.round(lastProgress),
  };
}
