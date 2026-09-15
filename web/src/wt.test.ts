import test from "node:test";
import assert from "node:assert/strict";
import { WtVideoClient } from "./wt.js";

test("sendInput is a no-op when the core is not connected", async () => {
  const client = new WtVideoClient();
  assert.equal(await client.sendInput(new Uint8Array([1, 2, 3])), false);
});

test("stats accessors do not throw before dial", async () => {
  const client = new WtVideoClient();
  assert.equal(client.rttMs(), 0);
  assert.equal(client.inboundKbps(), 0);
  assert.equal(client.staleMs(), 0);
  assert.equal(client.datagramCount, 0);
  assert.equal(client.clockOffsetUs(), null);
  assert.equal(client.syncErrorMs(), null);
  assert.equal(client.active, false);
  assert.equal(client.inputReady(), false);
  await client.send({ type: "keyframe_request" });
});
