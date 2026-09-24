import test from "node:test";
import assert from "node:assert/strict";
import { WtVideoClient } from "./wtcore.js";

// The readExact contract, pinned against the WebKit chunk realities observed
// on the 2026-09-09 phone sessions: excess bytes in a coalesced chunk are
// queued for the next readExact (never discarded - discarding desyncs every
// frame after the first), and a zero-byte-chunk storm is fatal to the stream
// instead of a silent forever-spin.

function frame(no: number, payloadLen: number, fill: number): Uint8Array {
  const buf = new Uint8Array(18 + payloadLen);
  const dv = new DataView(buf.buffer);
  dv.setUint8(0, 3);
  dv.setUint32(2, no, true);
  dv.setUint32(14, payloadLen, true);
  buf.fill(fill, 18);
  return buf;
}

interface FrameLike {
  frame_no: number;
}

async function drain(chunks: Uint8Array[]): Promise<{
  c: WtVideoClient;
  frames: number[];
}> {
  const c = new WtVideoClient();
  const frames: number[] = [];
  const stream = new ReadableStream<Uint8Array>({
    start(ctrl) {
      for (const ch of chunks) ctrl.enqueue(ch);
      ctrl.close(); // the loop must settle when the stream is spent
    },
  });
  await (
    c as unknown as {
      readOneFrame(s: ReadableStream<Uint8Array>, h: unknown): Promise<void>;
    }
  ).readOneFrame(stream, {
    onFrame: (f: FrameLike) => frames.push(f.frame_no),
  });
  return { c, frames };
}

test("a chunk carrying more than one frame's bytes is not discarded", async () => {
  // Frame A and frame B coalesce into ONE chunk (as WebKit delivers batched
  // writes). If excess beyond each readExact were dropped, B's header would
  // be garbage and B would never arrive.
  const a = frame(1, 900, 0xaa);
  const b = frame(2, 900, 0xbb);
  const merged = new Uint8Array(a.byteLength + b.byteLength);
  merged.set(a, 0);
  merged.set(b, a.byteLength);
  const { c, frames } = await drain([merged]);
  assert.deepEqual(frames, [1, 2]);
  assert.equal((c as unknown as { framesReceived: number }).framesReceived, 2);
});

test("an empty-chunk storm is fatal to the stream, not a silent spin", async () => {
  const { c, frames } = await drain([
    frame(1, 4, 1),
    ...Array.from({ length: 100 }, () => new Uint8Array(0)),
  ]);
  assert.deepEqual(frames, [1]);
  assert.equal((c as unknown as { framesAbandoned: number }).framesAbandoned, 1);
  assert.equal((c as unknown as { streamsWedged: number }).streamsWedged, 1);
});

test("a slow first byte is not a 2s wedge", async () => {
  // The startup IDR has to wait on encoder warmup and the video-channel
  // marker RTT. Cancelling that wait (STOP_SENDING) aborted the keyframe
  // the host was writing — 7 kbps, 1 incomplete frame, then the glass
  // watchdog reset the decoder. Mid-frame stalls still wedge (gotData).
  const c = new WtVideoClient();
  const frames: number[] = [];
  let ctrl!: ReadableStreamDefaultController<Uint8Array>;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      ctrl = c;
    },
  });
  const done = (
    c as unknown as {
      readOneFrame(s: ReadableStream<Uint8Array>, h: unknown): Promise<void>;
    }
  ).readOneFrame(stream, {
    onFrame: (f: FrameLike) => frames.push(f.frame_no),
  });
  await new Promise((r) => setTimeout(r, 2500));
  assert.equal(
    (c as unknown as { streamsWedged: number }).streamsWedged,
    0,
    "no data yet is not a wedge",
  );
  ctrl.enqueue(frame(1, 4, 1));
  ctrl.close();
  await done;
  assert.deepEqual(frames, [1]);
  assert.equal((c as unknown as { streamsWedged: number }).streamsWedged, 0);
});
