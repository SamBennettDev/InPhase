// Gamepad API sampling (architecture report §12.1): poll each frame, map to the
// canonical Xbox-style layout (button bit indices match the browser "standard"
// mapping: 0 A, 1 B, 2 X, 3 Y, 4 LB, 5 RB, 6 LT, 7 RT, 8 Back, 9 Start, 10 L3,
// 11 R3, 12 DUp, 13 DDown, 14 DLeft, 15 DRight, 16 Guide).

import type { GamepadStateWire } from "./protocol.js";

function axis(v: number): number {
  return Math.max(-32768, Math.min(32767, Math.round(v * 32767)));
}
function trigger(v: number): number {
  return Math.max(0, Math.min(65535, Math.round(v * 65535)));
}

const DP_UP = 1 << 12, DP_DOWN = 1 << 13, DP_LEFT = 1 << 14, DP_RIGHT = 1 << 15;

/** Decode a POV-hat axis to D-pad bits (HTML5 8-way hat: -1 = up, then
 *  clockwise; a resting hat reads out of [-1,1] and counts as centred). */
function hatToDpad(v: number): number {
  if (v == null || v > 1.01 || v < -1.01) return 0; // centred
  const dir = Math.round((v + 1) * 3.5) % 8;
  // 0 up · 1 up-right · 2 right · 3 down-right · 4 down · 5 down-left · 6 left · 7 up-left
  return [
    DP_UP, DP_UP | DP_RIGHT, DP_RIGHT, DP_DOWN | DP_RIGHT,
    DP_DOWN, DP_DOWN | DP_LEFT, DP_LEFT, DP_UP | DP_LEFT,
  ][dir] ?? 0;
}

/** Index of the D-pad hat axis, if this pad reports one. Standard-mapped pads
 *  use buttons 12–15; many non-standard pads (Firefox Xbox, DualShock, lots of
 *  BT controllers) expose an unpaired trailing axis instead — axis 9 on a
 *  10-axis pad, or the last axis when the count is odd (sticks/triggers pair up
 *  into an even count, so an odd one out is the hat). */
function hatAxisIndex(axes: readonly number[]): number {
  if (axes.length > 9) return 9;
  if (axes.length >= 5 && axes.length % 2 === 1) return axes.length - 1;
  return -1;
}

/** Current gamepad state, or null if none is connected. No de-duplication —
 *  callers compare against their own previous snapshot (a shared module-level
 *  cache leaked state across reconnects and could wedge input). */
export function readGamepad(): GamepadStateWire | null {
  const pads = navigator.getGamepads?.() ?? [];
  const gp =
    pads.find((p): p is Gamepad => !!p && p.mapping === "standard") ??
    pads.find((p): p is Gamepad => !!p);
  if (!gp) return null;

  let buttons = 0;
  Array.from(gp.buttons).forEach((b, i) => {
    if (i < 32 && (b.pressed || b.value > 0.5)) buttons |= 1 << i;
  });

  // Non-standard pads report the D-pad as a hat axis instead of buttons 12–15.
  if ((buttons & (DP_UP | DP_DOWN | DP_LEFT | DP_RIGHT)) === 0) {
    const hi = hatAxisIndex(gp.axes);
    if (hi >= 0) buttons |= hatToDpad(gp.axes[hi] ?? 2);
  }

  const state: GamepadStateWire = {
    buttons,
    lx: axis(gp.axes[0] ?? 0),
    ly: axis(gp.axes[1] ?? 0),
    rx: axis(gp.axes[2] ?? 0),
    ry: axis(gp.axes[3] ?? 0),
    lt: trigger(gp.buttons[6]?.value ?? 0),
    rt: trigger(gp.buttons[7]?.value ?? 0),
  };
  return state;
}
