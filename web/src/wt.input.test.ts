// SPDX-License-Identifier: GPL-3.0-or-later
import test from "node:test";
import assert from "node:assert/strict";
import { WtVideoClient } from "./wtcore.js";

interface InputPaths {
  inputWriter: WritableStreamDefaultWriter<Uint8Array> | null;
  inputStreamWriter: WritableStreamDefaultWriter<Uint8Array> | null;
}

function writer(write: (bytes: Uint8Array) => Promise<void>, desiredSize = 1): WritableStreamDefaultWriter<Uint8Array> {
  return { write, desiredSize } as unknown as WritableStreamDefaultWriter<Uint8Array>;
}

test("a blocked datagram does not hold up the reliable input copy", async () => {
  const c = new WtVideoClient();
  let unblock!: () => void;
  let reliable = false;
  const paths = c as unknown as InputPaths;
  paths.inputWriter = writer(() => new Promise<void>((resolve) => { unblock = resolve; }));
  const bytes = new Uint8Array([1, 2, 3]);
  paths.inputStreamWriter = writer(async (framed: Uint8Array) => {
    assert.deepEqual([...framed], [0, 3, ...bytes]);
    reliable = true;
  });
  const done = c.sendInput(bytes);
  try {
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.equal(reliable, true, "reliable copy starts before datagrams unblock");
    assert.equal(await Promise.race([done, Promise.resolve("still waiting")]), true);
  } finally {
    unblock();
    await done;
    c.close();
  }
});

test("input succeeds when one path fails", async () => {
  const c = new WtVideoClient();
  const paths = c as unknown as InputPaths;
  paths.inputWriter = writer(async () => { throw new Error("datagrams closed"); });
  paths.inputStreamWriter = writer(async () => {});
  assert.equal(await c.sendInput(new Uint8Array([1])), true);
  assert.equal(paths.inputWriter, null);
  c.close();
});

test("backpressured datagrams do not accumulate copies of newer input", async () => {
  const c = new WtVideoClient();
  const paths = c as unknown as InputPaths;
  let copies = 0;
  paths.inputWriter = writer(async () => { throw new Error("must not queue here"); }, 0);
  paths.inputStreamWriter = writer(async () => { copies++; });
  for (let i = 0; i < 20; i++) assert.equal(await c.sendInput(new Uint8Array([i])), true);
  assert.equal(copies, 20);
  c.close();
});

test("closing a session clears input readiness", () => {
  const c = new WtVideoClient();
  (c as unknown as InputPaths).inputStreamWriter = writer(async () => {});
  assert.equal(c.inputReady(), true);
  c.close();
  assert.equal(c.inputReady(), false);
});
