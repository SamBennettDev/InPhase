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

/** A parity row as the host sends it: the XOR row, then the frame's total
 *  data length (u32 LE) - the trailer that sizes a rebuilt final fragment. */
function withTrailer(row: Uint8Array, total: number): Uint8Array {
  const out = new Uint8Array(row.byteLength + 4);
  out.set(row);
  new DataView(out.buffer).setUint32(row.byteLength, total, true);
  return out;
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
  dgrams.push(frag(1, 0, cnt, withTrailer(row1, payload.length), 1, true));
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
  dgrams.push(frag(2, 0, cnt, withTrailer(row1, payload.length), 1, true));
  dgrams.push(frag(2, 0, cnt, withTrailer(row2, payload.length), 2, true));
  // Drop one even-offset (2) and one odd-offset (5) data fragment.
  dgrams.splice(5, 1);
  dgrams.splice(2, 1);

  const { frames } = feed(dgrams);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload);
});

test("a one-datagram frame whose datagram is lost is rebuilt from its parity", () => {
  // Most frames on light content fit one datagram, so the lone fragment is
  // also the final one. Before the length trailer that could never be
  // rebuilt, and a parity arriving with no data created no assembly - every
  // lost datagram became an abandoned frame and an IDR request.
  const payload = new Uint8Array(517);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 31 + 7) & 0xff;
  const { frames } = feed([frag(9, 0, 1, withTrailer(payload, payload.length), 1)]);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload);
});

test("a lost short final fragment is rebuilt to its true length", () => {
  const budget = 40;
  const payload = new Uint8Array(3 * budget + 13);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 5 + 1) & 0xff;
  const frags = [0, 1, 2, 3].map((i) => payload.subarray(i * budget, Math.min((i + 1) * budget, payload.length)));
  const row1 = new Uint8Array(budget);
  for (const f of frags) for (let j = 0; j < f.length; j++) row1[j] = (row1[j] ?? 0) ^ f[j]!;
  // Parity first (datagrams reorder), then every data fragment but the tail.
  const { frames } = feed([
    frag(11, 0, 4, withTrailer(row1, payload.length), 1),
    frag(11, 0, 4, frags[0]!, 0),
    frag(11, 1, 4, frags[1]!, 0),
    frag(11, 2, 4, frags[2]!, 0),
  ]);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload, "exact bytes - no zero padding from the parity width");
});

test("a frame known only from its parity is still NACKed", async () => {
  // Both data fragments lost, parity arrived: the frame exists, so the repair
  // path must ask for it rather than let it expire unseen.
  const c = new WtVideoClient();
  const sent: { type: string; frame?: number; idx?: number }[] = [];
  (c as unknown as { send: (m: Record<string, unknown>) => Promise<void> }).send = (m) => {
    sent.push(m as { type: string });
    return Promise.resolve();
  };
  const call = (buf: Uint8Array) =>
    (c as unknown as { onVideoFragment(d: Uint8Array, h: unknown): void }).onVideoFragment(buf, { onFrame: () => {} });
  const parity = withTrailer(new Uint8Array(40), 70);
  call(frag(21, 0, 2, parity, 1));
  const orig = performance.now;
  try {
    (performance as { now: () => number }).now = () => orig.call(performance) + 200;
    call(frag(21, 0, 2, parity, 1)); // any arrival drives the scan
  } finally {
    (performance as { now: () => number }).now = orig;
  }
  const nacks = sent.filter((m) => m.type === "nack" && m.frame === 21);
  assert.ok(nacks.length >= 1, "the parity-only frame is asked for");
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

test("holes past the eighth are requested too - one per FEC group, rotating", () => {
  // The old selection walked missing indices from 0 and stopped at 8, so a
  // frame with nine holes asked for the lowest eight in every round and never
  // for the ninth - it could not assemble however much the host re-sent, and
  // the frame was thrown away at expiry. That is the shape of the
  // 30-fragment IDR whose leading fragments the sender's queue destroyed
  // (fragments k..29 arrived, 0..k-1 never did).
  const cnt = 32; // four groups of eight
  const budget = 30;
  const payload = new Uint8Array(cnt * budget);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 13) & 0xff;
  const frags: Uint8Array[] = [];
  for (let i = 0; i < cnt; i++) frags.push(payload.subarray(i * budget, (i + 1) * budget));

  // Nine holes: one in group 0, two in group 1, one in group 2, five in
  // group 3. Group 1's pair is FEC-dead until one is restored, so asking for
  // both in the same round wastes a datagram; index 30 is the ninth hole and
  // is the tail, which parity can never rebuild - it must be asked for.
  const holes = [1, 9, 10, 17, 24, 25, 26, 27, 30];

  const c = new WtVideoClient();
  const sent: { type: string; idx?: number }[] = [];
  (c as unknown as { send: (m: Record<string, unknown>) => Promise<void> }).send =
    (m: Record<string, unknown>) => {
      sent.push(m as { type: string });
      return Promise.resolve();
    };
  const call = (buf: Uint8Array) =>
    (c as unknown as { onVideoFragment(d: Uint8Array, h: unknown): void }).onVideoFragment(buf, {
      onFrame: () => {},
    });
  for (let i = 0; i < cnt; i++) {
    if (!holes.includes(i)) call(frag(40, i, cnt, frags[i]!, 0, false));
  }

  // Four rounds, 200 ms apart (past the 150 ms first window and the 180 ms
  // round cadence, inside the 900 ms expiry). Each round is triggered by a
  // duplicate arrival, which is all the arrival-driven scan needs.
  const orig = performance.now;
  const asked: number[] = [];
  const perRound: number[][] = [];
  try {
    for (let r = 0; r < 4; r++) {
      const before = sent.length;
      (performance as { now: () => number }).now = () => orig.call(performance) + 200 * (r + 1);
      call(frag(40, 0, cnt, frags[0]!, 0, false));
      const round = sent.slice(before).filter((s) => s.type === "nack").map((s) => s.idx as number);
      perRound.push(round);
      asked.push(...round);
    }
  } finally {
    (performance as { now: () => number }).now = orig;
  }

  for (const round of perRound) {
    assert.ok(round.length <= 8, "bounded to 8 datagrams per round");
    const groups = new Set(round.map((i) => Math.floor(i / GROUP)));
    assert.equal(groups.size, round.length, "at most one hole per FEC group per round");
  }
  const union = new Set(asked);
  // Decisive: the ninth hole is the frame's tail, which parity can never
  // rebuild. The old selection asked for the lowest eight in every round and
  // never once for this one. (A single group holding five holes cannot be
  // fully covered in four rounds by any one-per-group policy - and is
  // FEC-dead at three - so the union is asserted to reach at least the eight
  // the old loop managed, plus that tail.)
  assert.ok(union.has(30), "the ninth hole is requested (the old loop never asked for it)");
  assert.ok(union.size >= 8, `at least as much coverage as before (got ${union.size})`);
});

// ---- assembly eviction and cross-carrier dedupe (audit §2.3, §3.5) ----

type Core = {
  onVideoFragment(d: Uint8Array, h: unknown): void;
  readOneFrame(s: ReadableStream<Uint8Array>, h: unknown): Promise<void>;
  requestKeyframe(): void;
  keyPartialsEvicted: number;
  framesDuplicate: number;
};

/** One client, both carriers, and a count of the core's own IDR asks. */
function rig() {
  const c = new WtVideoClient();
  const core = c as unknown as Core;
  const got: { no: number; key: boolean }[] = [];
  let asks = 0;
  core.requestKeyframe = () => {
    asks++;
  };
  const h = { onFrame: (f: { frame_no: number; key: boolean }) => got.push({ no: f.frame_no, key: f.key }) };
  return {
    core,
    got,
    asks: () => asks,
    dgram: (d: Uint8Array) => core.onVideoFragment(d, h),
    stream: (frames: Uint8Array[]) =>
      core.readOneFrame(
        new ReadableStream<Uint8Array>({
          start(ctrl) {
            for (const f of frames) ctrl.enqueue(f);
            ctrl.close();
          },
        }),
        h,
      ),
  };
}

/** A v3 stream frame whose capture stamp matches `frag`'s for the same number. */
function streamFrame(frameNo: number, payload: Uint8Array, key = false): Uint8Array {
  const buf = new Uint8Array(HDR + payload.byteLength);
  const dv = new DataView(buf.buffer);
  dv.setUint8(0, 3);
  dv.setUint8(1, key ? 1 : 0);
  dv.setUint32(2, frameNo, true);
  dv.setBigUint64(6, 1000n * BigInt(frameNo), true);
  dv.setUint32(14, payload.byteLength, true);
  buf.set(payload, HDR);
  return buf;
}

const part = (n: number) => new Uint8Array(40).fill(n & 0xff);

test("a partial keyframe survives any number of newer delta assemblies", () => {
  const r = rig();
  // Half of a 24-fragment IDR, then 40 newer deltas each still missing a
  // fragment - far past the 24-assembly bound.
  for (let i = 0; i < 12; i++) r.dgram(frag(100, i, 24, part(i), 0, true));
  for (let n = 101; n <= 140; n++) r.dgram(frag(n, 0, 2, part(n), 0));
  for (let i = 12; i < 24; i++) r.dgram(frag(100, i, 24, part(i), 0, true));
  assert.deepEqual(
    r.got.filter((f) => f.key).map((f) => f.no),
    [100],
    "the IDR assembles; the deltas were the ones evicted",
  );
  assert.equal(r.core.keyPartialsEvicted, 0);
});

test("when every assembly is a keyframe the newcomer delta is evicted, not an IDR", () => {
  const r = rig();
  for (let n = 1; n <= 24; n++) r.dgram(frag(n, 0, 2, part(n), 0, true));
  r.dgram(frag(200, 0, 2, part(200), 0));
  assert.equal(r.core.keyPartialsEvicted, 0);
  r.dgram(frag(1, 1, 2, part(1), 0, true));
  assert.deepEqual(r.got.map((f) => f.no), [1], "the oldest IDR partial was kept");
});

test("an unavoidable keyframe eviction is counted and re-requested", () => {
  const r = rig();
  for (let n = 1; n <= 26; n++) r.dgram(frag(n, 0, 2, part(n), 0, true));
  assert.equal(r.core.keyPartialsEvicted, 2);
  assert.equal(r.asks(), 2, "each lost IDR partial asks for a fresh one");
});

test("an IDR delivered on the stream is not delivered again by its datagram copy", async () => {
  const r = rig();
  await r.stream([streamFrame(7, part(7), true)]);
  r.dgram(frag(7, 0, 2, part(7), 0, true));
  r.dgram(frag(7, 1, 2, part(7), 0, true));
  assert.deepEqual(r.got, [{ no: 7, key: true }]);
});

test("an IDR assembled from datagrams is not delivered again by its stream copy", async () => {
  const r = rig();
  r.dgram(frag(9, 0, 2, part(9), 0, true));
  r.dgram(frag(9, 1, 2, part(9), 0, true));
  // The stream copy arrives, followed by a genuinely new frame on the same stream.
  await r.stream([streamFrame(9, part(9), true), streamFrame(10, part(10))]);
  assert.deepEqual(r.got.map((f) => f.no), [9, 10], "one IDR, and the stream carries on");
  assert.equal(r.core.framesDuplicate, 1);
});

test("the stream copy of an evicted IDR partial still reaches the page", async () => {
  const r = rig();
  for (let n = 1; n <= 25; n++) r.dgram(frag(n, 0, 2, part(n), 0, true));
  assert.equal(r.core.keyPartialsEvicted, 1);
  await r.stream([streamFrame(1, part(1), true)]);
  assert.deepEqual(r.got.map((f) => f.no), [1], "a tombstone is not a delivery");
});

test("a frame that left no trace is asked for whole, once", () => {
  // One lost packet can carry a small frame's only fragment AND its parity:
  // the client sees frame 3 after frame 1 and nothing of frame 2 at all.
  const c = new WtVideoClient();
  const sent: { type: string; frame?: number; idx?: number }[] = [];
  (c as unknown as { send: (m: Record<string, unknown>) => Promise<void> }).send = (m) => {
    sent.push(m as { type: string });
    return Promise.resolve();
  };
  const call = (buf: Uint8Array) =>
    (c as unknown as { onVideoFragment(d: Uint8Array, h: unknown): void }).onVideoFragment(buf, { onFrame: () => {} });
  const p = new Uint8Array(100);
  call(frag(1, 0, 1, p, 0));
  call(frag(3, 0, 1, p, 0));
  call(frag(4, 0, 1, p, 0));
  call(frag(2, 0, 1, p, 0)); // late: reorder, not a new gap
  const whole = sent.filter((m) => m.type === "nack" && m.idx === 0xffff);
  assert.deepEqual(whole.map((m) => m.frame), [2], "frame 2 requested whole, exactly once");
});

test("repair timing scales with RTT, keeping the old timings when RTT is unknown", async () => {
  const { nackTiming } = await import("./wtcore.js");
  assert.deepEqual(nackTiming(0), { firstMs: 150, gapMs: 180 });
  const lan = nackTiming(5);
  assert.ok(lan.firstMs <= 45 && lan.gapMs <= 50, `LAN repair must not wait 150 ms (${JSON.stringify(lan)})`);
  assert.deepEqual(nackTiming(120), { firstMs: 150, gapMs: 180 }, "a relay-class RTT hits the old ceilings");
  // Four rounds on a LAN finish well inside the orderer's 300 ms repair budget.
  assert.ok(lan.firstMs + 3 * lan.gapMs < 300);
});

test("a large delta frame with two holes in one group is rebuilt from rows 1+2", () => {
  // Deltas of >= 16 fragments now carry row 2 like keyframes do.
  const cnt = 16;
  const budget = 24;
  const payload = new Uint8Array(cnt * budget);
  for (let i = 0; i < payload.length; i++) payload[i] = (i * 11 + 3) & 0xff;
  const frags = Array.from({ length: cnt }, (_, i) => payload.subarray(i * budget, (i + 1) * budget));
  const dgrams: Uint8Array[] = [];
  for (let g = 0; g < 2; g++) {
    const row1 = new Uint8Array(budget);
    const row2 = new Uint8Array(budget);
    for (let i = g * 8; i < g * 8 + 8; i++) {
      for (let j = 0; j < budget; j++) {
        row1[j] = (row1[j] ?? 0) ^ frags[i]![j]!;
        if ((i - g * 8) % 2 === 1) row2[j] = (row2[j] ?? 0) ^ frags[i]![j]!;
      }
    }
    dgrams.push(frag(31, g, cnt, withTrailer(row1, payload.length), 1));
    dgrams.push(frag(31, g, cnt, withTrailer(row2, payload.length), 2));
  }
  // Group 0 loses fragments 2 (even) and 5 (odd): FEC-dead with row 1 alone.
  frags.forEach((f, i) => {
    if (i !== 2 && i !== 5) dgrams.push(frag(31, i, cnt, f, 0));
  });
  const { frames } = feed(dgrams);
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0]!.payload, payload);
});
