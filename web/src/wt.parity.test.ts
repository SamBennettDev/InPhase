import test from "node:test";
import assert from "node:assert/strict";
import { WtVideoClient } from "./wtcore.js";


// FEC parity repair for the v4 datagram carrier, pinned against the 23:21
// session: at ~9% fragment loss a NACK-only repair amplified into a
// re-send storm (nacks 65 -> 10789 in 8 s). Row 1 parity must rebuild a
// single lost fragment locally (no NACK, no RTT); rows 1+2 must rebuild a
// second loss when the two split even/odd within the group.

const HDR = 18;

function frag(
  frameNo: number,
  idx: number,
  cnt: number,
  payload: Uint8Array,
  parity: 0 | 1 | 2,
  key = false,
): Uint8Array {
  const buf = new Uint8Array(HDR + payload.byteLength);
  const dv = new DataView(buf.buffer);
  dv.setUint8(0, 4);
  dv.setUint8(1, (key ? 1 : 0) | (parity << 1));
  dv.setUint32(2, frameNo, true);
  dv.setUint16(6, idx, true);
  dv.setUint16(8, cnt, true);
  dv.setBigUint64(10, 1000n * BigInt(frameNo), true);
  buf.set(payload, HDR);
  return buf;
}

interface FrameLike {
  frame_no: number;
  payload: Uint8Array;
}

function feed(dgrams: Uint8Array[]): { frames: { no: number; payload: Uint8Array }[]; c: WtVideoClient } {
  const c = new WtVideoClient();
  const frames: { no: number; payload: Uint8Array }[] = [];
  for (const d of dgrams) {
    (c as unknown as {
      onVideoFragment(d: Uint8Array, h: unknown): void;
    }).onVideoFragment(d, {
      onFrame: (f: FrameLike) =>
        frames.push({ no: f.frame_no, payload: f.payload }),
    });
  }
  return { frames, c };
}

const GROUP = 8;

test("row-1 parity rebuilds one lost fragment with no round-trip", () => {
  const cnt = GROUP;
  const budget = 40;
  const payload = new Uint8Array(cnt * budget);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 7) & 0xff;
  const frags: Uint8Array[] = [];
  for (let i = 0; i < cnt; i++) {
    frags.push(payload.subarray(i * budget, (i + 1) * budget));
  }
  // Row 1: XOR of all group members (zero-padded to budget; all are budget
  // sized here except never the last of the FRAME, and cnt === GROUP).
  const row1 = new Uint8Array(budget);
  for (const f of frags) for (let j = 0; j < budget; j++) row1[j] = (row1[j] ?? 0) ^ (f[j] ?? 0);

  const dgrams = frags.map((f, i) => frag(1, i, cnt, f, 0, true));
  dgrams.push(frag(1, 0, cnt, row1, 1, true));
  // The wire drops data fragment 3; parity must restore it byte-exact.
  dgrams.splice(3, 1);

  const { frames } = feed(dgrams);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload);
});

test("rows 1+2 rebuild two losses when they split even/odd", () => {
  const cnt = GROUP;
  const budget = 32;
  const payload = new Uint8Array(cnt * budget);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 13 + 5) & 0xff;
  const frags: Uint8Array[] = [];
  for (let i = 0; i < cnt; i++) {
    frags.push(payload.subarray(i * budget, (i + 1) * budget));
  }
  const row1 = new Uint8Array(budget);
  const row2 = new Uint8Array(budget);
  for (let i = 0; i < cnt; i++) {
    for (let j = 0; j < budget; j++) {
      row1[j] = (row1[j] ?? 0) ^ (frags[i]![j] ?? 0);
      if (i % 2 === 1) row2[j] = (row2[j] ?? 0) ^ (frags[i]![j] ?? 0);
    }
  }
  const dgrams = frags.map((f, i) => frag(2, i, cnt, f, 0, true));
  dgrams.push(frag(2, 0, cnt, row1, 1, true));
  dgrams.push(frag(2, 0, cnt, row2, 2, true));
  // Drop one even-offset (2) and one odd-offset (5) data fragment.
  dgrams.splice(5, 1);
  dgrams.splice(2, 1);

  const { frames } = feed(dgrams);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload);
});

test("a hole the parity cannot cover still NACKs - bounded", async () => {
  // Three losses in one group exceed the code's reach: the frame must not
  // be delivered, and the NACK fallback must fire with FEWER than all
  // holes (8 per round cap) once the 150 ms window passes.
  const cnt = GROUP;
  const budget = 16;
  const payload = new Uint8Array(cnt * budget);
  const frags: Uint8Array[] = [];
  for (let i = 0; i < cnt; i++) {
    frags.push(payload.subarray(i * budget, (i + 1) * budget));
  }
  const row1 = new Uint8Array(budget);
  for (const f of frags) for (let j = 0; j < budget; j++) row1[j] = (row1[j] ?? 0) ^ (f[j] ?? 0);
  const dgrams = frags.map((f, i) => frag(3, i, cnt, f, 0, true));
  dgrams.push(frag(3, 0, cnt, row1, 1, true));
  dgrams.splice(1, 1); // idx 1
  dgrams.splice(2, 1); // idx 3 (after splice)
  dgrams.splice(3, 1); // idx 5 (after second splice)

  const { frames, c } = feed(dgrams);
  assert.equal(frames.length, 0, "3 losses in one group: not recoverable");
  // Advance past the 150 ms NACK window (inside the 900 ms expiry) and
  // re-feed a duplicate fragment to trigger the expiry scan. NACKs are spied on `send` (no control writer
  // in the test, so the real send would no-op silently).
  const sent: { type: string; idx?: number }[] = [];
  (c as unknown as { send: (m: Record<string, unknown>) => Promise<void> }).send =
    (m: Record<string, unknown>) => {
      sent.push(m as { type: string });
      return Promise.resolve();
    };
  const orig = performance.now;
  (performance as { now: () => number }).now = () => orig.call(performance) + 200;
  try {
    (c as unknown as { onVideoFragment(d: Uint8Array, h: unknown): void }).onVideoFragment(
      frag(3, 0, cnt, frags[0]!, 0, true),
      { onFrame: () => {} },
    );
  } finally {
    (performance as { now: () => number }).now = () => orig.call(performance);
  }
  const nacks = sent.filter((s) => s.type === "nack");
  assert.ok(nacks.length > 0 && nacks.length <= 8, "NACK round fires, bounded to 8");
});

test("an expired frame is tombstoned - late re-sends do not resurrect it", () => {
  // 23:59: expiry deleted the frame WITHOUT tombstoning; the host's
  // re-sends kept arriving after expiry, each one resurrected a fresh
  // partial that NACKed again - the unbounded loop behind nacks=7183.
  const cnt = 2;
  const budget = 8;
  const dgrams = [
    frag(9, 0, cnt, new Uint8Array(budget).fill(1), 0, false),
    // fragment 1 never arrives; the frame expires at 900 ms
  ];
  const { c } = feed(dgrams);
  const orig = performance.now;
  const fire = (buf: Uint8Array, atMs: number) => {
    (performance as { now: () => number }).now = () => orig.call(performance) + 10_000 + 200;
    try {
      (c as unknown as { onVideoFragment(d: Uint8Array, h: unknown): void }).onVideoFragment(buf, {
        onFrame: () => {},
      });
    } finally {
      (performance as { now: () => number }).now = () => orig.call(performance);
    }
  };
  fire(dgrams[0]!, 0);
  const priv = c as unknown as { framesAbandoned: number; v4Done: Set<number>; v4: Map<number, unknown> };
  assert.ok(priv.framesAbandoned === 1, "frame expired");
  // A late re-send of fragment 0 must be ignored, not resurrect the frame.
  fire(dgrams[0]!, 1);
  assert.equal(priv.v4.size, 0, "no resurrection");
  assert.equal(priv.framesAbandoned, 1, "abandon count unchanged");
});
