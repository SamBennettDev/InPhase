// The `KeyboardEvent.code` → Windows set-1 scan-code contract: browser key
// codes are normalised deliberately and reserved / browser-handled keys are
// tested. Mirrors `crates/host/src/input/backends/sendinput.rs`.

import { test } from "node:test";
import assert from "node:assert/strict";

import { SCANCODE, scanCodeFor, RESERVED_PREFIXES } from "./keycodes.js";

test("core gaming keys map to the expected set-1 scan codes", () => {
  const expect: Record<string, number> = {
    KeyW: 0x11,
    KeyA: 0x1e,
    KeyS: 0x1f,
    KeyD: 0x20,
    Space: 0x39,
    ShiftLeft: 0x2a,
    ControlLeft: 0x1d,
    Escape: 0x01,
    Enter: 0x1c,
    Digit1: 0x02,
    F1: 0x3b,
  };
  for (const [code, sc] of Object.entries(expect)) {
    assert.equal(scanCodeFor(code), sc, code);
  }
});

test("extended keys carry the 0xE0 high byte", () => {
  for (const code of [
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "ControlRight",
    "AltRight",
    "NumpadEnter",
    "NumpadDivide",
    "Home",
    "End",
    "Insert",
    "Delete",
    "MetaLeft",
  ]) {
    const sc = scanCodeFor(code);
    assert.ok(sc !== undefined, `${code} missing`);
    assert.ok(sc! >= 0xe000, `${code} = ${sc!.toString(16)} is not extended`);
  }
});

test("reserved / browser-handled keys are deliberately unmapped", () => {
  for (const code of [
    "PrintScreen",
    "Pause",
    "F13",
    "F24",
    "BrowserBack",
    "BrowserForward",
    "BrowserRefresh",
    "LaunchApp1",
    "MediaPlayPause",
    "AudioVolumeUp",
    "AudioVolumeMute",
    "Power",
    "Sleep",
    "Eject",
    "Fn",
    "Lang1",
  ]) {
    assert.equal(
      scanCodeFor(code),
      undefined,
      `${code} should not be forwarded`,
    );
  }
});

test("no mapped code collides and every scan code is a u16", () => {
  const seen = new Map<number, string>();
  for (const [code, sc] of Object.entries(SCANCODE)) {
    assert.ok(
      Number.isInteger(sc) && sc > 0 && sc <= 0xffff,
      `${code} out of range`,
    );
    const prev = seen.get(sc);
    // ShiftLeft/ShiftRight etc. are distinct codes with distinct scan codes;
    // a genuine duplicate would be a bug.
    assert.equal(prev, undefined, `${code} duplicates scan code of ${prev}`);
    seen.set(sc, code);
  }
});

test("RESERVED_PREFIXES actually cover reserved codes", () => {
  const matches = (c: string) => RESERVED_PREFIXES.some((p) => c.startsWith(p));
  assert.ok(matches("BrowserHome"));
  assert.ok(matches("MediaTrackNext"));
  assert.ok(!matches("KeyM"));
});
