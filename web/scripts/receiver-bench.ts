// SPDX-License-Identifier: GPL-3.0-or-later
// Receiver CPU only. Payload generation is outside the measured interval.
import assert from "node:assert/strict";
import { performance } from "node:perf_hooks";
import { writeFileSync } from "node:fs";
import { WtVideoClient, type WtClientHandlers } from "../src/wtcore.js";
import { encodeFragments } from "../tests/performance/network-simulation.js";

const frames = 240;
const payload = new Uint8Array(64_113).fill(0x73);
const data = Array.from({ length: frames }, (_, no) => encodeFragments({
  frame_no: no, capture_us: no * 8333, key: false, payload,
}).filter((d) => (d[1]! >> 1) !== 0 || new DataView(d.buffer).getUint16(6, true) % 8 !== 3));
const samples: number[] = [];
for (let round = 0; round < 7; round++) {
  const client = new WtVideoClient();
  const core = client as unknown as { onVideoFragment(d: Uint8Array, h: WtClientHandlers): void };
  let received = 0;
  const handlers: WtClientHandlers = { onVideoConfig: () => {}, onClosed: () => {}, onFrame(f) {
    assert.equal(Buffer.compare(f.payload, payload), 0, "parity repaired every byte");
    received++;
  } };
  const start = performance.now();
  for (const frame of data) for (const d of frame) core.onVideoFragment(d, handlers);
  const elapsed = performance.now() - start;
  assert.equal(received, frames);
  client.close();
  if (round > 0) samples.push(elapsed); // one warmup, six measured rounds
}
samples.sort((a, b) => a - b);
const result = { kind: "receiver-cpu-microbenchmark", framesPerRound: frames,
  payloadBytes: payload.length, datagramsPerRound: data.reduce((n, d) => n + d.length, 0),
  lossPattern: "data offset 3 in each full FEC group", warmupRounds: 1,
  samplesMs: samples.map((n) => Math.round(n * 10) / 10),
  medianMsPerFrame: Math.round((samples[2]! + samples[3]!) / 2 / frames * 1000) / 1000 };
console.log(JSON.stringify(result));
if (process.argv[2]) writeFileSync(process.argv[2], JSON.stringify(result, null, 2) + "\n");
