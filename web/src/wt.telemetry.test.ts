import test from "node:test";
import assert from "node:assert/strict";
import { WtVideoClient, type WtClientStats } from "./wtcore.js";

/** Minimal snapshot shaped like the page's decoder provider. */
function snapshot(framesDecoded: number): WtClientStats {
  return {
    codec: "hvc1",
    framesDecoded,
    framesPresented: framesDecoded,
    framesDropped: 0,
    held: 0,
    queueSize: 0,
    behindEvents: 0,
    freezeCount: 0,
    totalFreezeMs: 0,
  };
}

function captureSends(client: WtVideoClient): Record<string, unknown>[] {
  const sent: Record<string, unknown>[] = [];
  (client as unknown as { send: (m: Record<string, unknown>) => Promise<void> }).send =
    async (m) => {
      sent.push(m);
    };
  return sent;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function telemetry(sent: Record<string, unknown>[]): Record<string, unknown>[] {
  return sent.filter((m) => m.type === "client_telemetry");
}

/**
 * In worker mode the page owns the telemetry cadence: the decoder counters live
 * on the main thread, are pushed once a second, and each push composes one
 * message. Two unsynchronised 1 Hz loops (a worker timer and the page's push)
 * let a tick difference a stale snapshot: it reports 0 fps, the next reports
 * double, and the host's loss and health laws read a decoder that keeps dying
 * while the picture is fine. Live, that showed as decoded_fps 0 / 120 / 0 / 110
 * on a 60 fps stream, and as `permanent-black signature` errors in the host
 * log during sessions that were decoding perfectly.
 */
test("each push composes exactly one telemetry message, from fresh counters", () => {
  const client = new WtVideoClient();
  const sent = captureSends(client);
  client.inWorker = true;

  let decoded = 0;
  client.setStatsProvider(() => snapshot(decoded));

  client.telemetryNow();
  assert.equal(telemetry(sent).length, 1, "one push, one message");
  client.telemetryNow();
  assert.equal(telemetry(sent).length, 2, "and never two per push");
});

test("a steady decoder reports a steady rate, never 0 and never double", async () => {
  const client = new WtVideoClient();
  const sent = captureSends(client);
  client.inWorker = true;

  let decoded = 0;
  client.setStatsProvider(() => snapshot(decoded));

  client.telemetryNow(); // baseline: there is no previous window to difference
  for (let i = 0; i < 4; i++) {
    await sleep(100);
    decoded += 6; // 60 fps
    client.telemetryNow();
  }

  const rates = telemetry(sent)
    .slice(1)
    .map((m) => m.decoded_fps as number);
  assert.equal(rates.length, 4);
  for (const r of rates) {
    // The measured window is what the delta is divided by, so a steady decoder
    // cannot read 0 on one tick and 120 on the next.
    assert.ok(r > 30 && r < 90, `a steady 60 fps stream reported ${r}`);
  }
});
