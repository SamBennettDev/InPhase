import { test } from "node:test";
import assert from "node:assert/strict";
import { defaultFpsForRefresh, FPS_CHOICES } from "./settings.js";

test("a 120 Hz+ display defaults to 120 fps, anything slower to 60", () => {
  assert.equal(defaultFpsForRefresh(59.94), 60);
  assert.equal(defaultFpsForRefresh(75), 60);
  assert.equal(defaultFpsForRefresh(119.88), 120);
  assert.equal(defaultFpsForRefresh(144), 120);
  assert.equal(defaultFpsForRefresh(240), 120);
  assert.ok(FPS_CHOICES.includes(120), "120 must be a selectable rate");
});
