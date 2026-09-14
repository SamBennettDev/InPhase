// Touch controller (architecture report §12.1, §25.2).
//
// iOS/Android have no Pointer Lock / physical keyboard, so the phone presents a
// **client-side** control surface emitting the same canonical binary protocol.
//
//   * whole screen  -> trackpad: 1-finger drag = relative mouse move,
//                       quick tap = left click, 2-finger tap = right click,
//                       2-finger drag = wheel, long-press = hold left.
//   * bottom-right  -> L / R mouse buttons.
//   * top-right ⋯   -> menu: Keyboard · Fullscreen · Disconnect.

import { InputEncoder, InputKind, type GamepadStateWire } from "./protocol.js";
import { SoftKeyboard } from "./softkeyboard.js";
import { readGamepad } from "./gamepad.js";
import type { InputSink } from "./manager.js";

const MOVE_SCALE = 1.5;
const TAP_MS = 200;
const TAP_SLOP = 10;
const LONGPRESS_MS = 350;
const SNAPSHOT_HZ = 40;

export class TouchController {
  private enc = new InputEncoder();
  private held = new Set<number>();
  private mouseButtons = 0;
  private root = document.createElement("div");
  private kb: SoftKeyboard;
  private snapshotTimer = 0;
  private gamepadRaf = 0;
  private gamepad: GamepadStateWire | null = null;
  private lastGamepadKey = "";
  private active = false;

  private touches = new Map<number, { x: number; y: number; sx: number; sy: number; t: number; moved: boolean }>();
  private longPressTimer = 0;
  private twoFingerScroll = false;
  private lastScrollY = 0;

  onDisconnect: (() => void) | null = null;

  constructor(
    private readonly surface: HTMLElement,
    private readonly sink: InputSink,
  ) {
    this.root.className = "touch-ui";
    this.kb = new SoftKeyboard((code, down) => this.key(code, down));
  }

  attach() {
    this.buildUi();
    this.surface.append(this.root);
    this.root.append(this.kb.el);
    const pad = this.root.querySelector<HTMLElement>(".pad")!;
    pad.addEventListener("touchstart", this.onPadStart, { passive: false });
    pad.addEventListener("touchmove", this.onPadMove, { passive: false });
    pad.addEventListener("touchend", this.onPadEnd, { passive: false });
    pad.addEventListener("touchcancel", this.onPadEnd, { passive: false });
    document.addEventListener("visibilitychange", this.onHide);
    window.addEventListener("blur", this.releaseAll);
    this.snapshotTimer = window.setInterval(this.sendSnapshot, 1000 / SNAPSHOT_HZ);
    this.gamepadRaf = requestAnimationFrame(this.pollGamepad);
    this.active = true;
  }

  detach() {
    clearInterval(this.snapshotTimer);
    cancelAnimationFrame(this.gamepadRaf);
    document.removeEventListener("visibilitychange", this.onHide);
    window.removeEventListener("blur", this.releaseAll);
    this.releaseAll();
    this.root.remove();
    this.active = false;
  }

  get isCaptured() {
    return this.active;
  }

  private buildUi() {
    this.root.innerHTML = `
      <div class="pad"></div>
      <button class="tbtn menu-btn" data-act="menu">⋯</button>
      <div class="sheet" hidden>
        <button class="tbtn" data-act="kb">⌨ Keyboard</button>
        <button class="tbtn" data-act="fs">⛶ Fullscreen</button>
        <button class="tbtn" data-act="quit">✕ Disconnect</button>
      </div>
      <div class="mouse">
        <button class="tbtn" data-mouse="0">L</button>
        <button class="tbtn" data-mouse="1">R</button>
      </div>`;

    for (const el of this.root.querySelectorAll<HTMLElement>("button[data-mouse]")) {
      const bit = Number(el.dataset["mouse"]);
      el.addEventListener("touchstart", (e) => { e.preventDefault(); e.stopPropagation(); el.classList.add("on"); this.mouseButton(bit, true); }, { passive: false });
      const up = (e: Event) => { e.preventDefault(); el.classList.remove("on"); this.mouseButton(bit, false); };
      el.addEventListener("touchend", up, { passive: false });
      el.addEventListener("touchcancel", up, { passive: false });
    }

    const sheet = this.root.querySelector<HTMLElement>(".sheet")!;
    for (const el of this.root.querySelectorAll<HTMLElement>("button[data-act]")) {
      el.addEventListener("touchend", (e) => {
        e.preventDefault();
        e.stopPropagation();
        switch (el.dataset["act"]) {
          case "menu": sheet.hidden = !sheet.hidden; break;
          case "kb": sheet.hidden = true; this.kb.toggle(); break;
          case "fs": sheet.hidden = true; this.requestFullscreen(); break;
          case "quit": sheet.hidden = true; this.onDisconnect?.(); break;
        }
      }, { passive: false });
    }
  }

  private requestFullscreen() {
    const s = this.surface as HTMLElement & { webkitRequestFullscreen?: () => void };
    if (s.requestFullscreen) void s.requestFullscreen().catch(() => {});
    else if (s.webkitRequestFullscreen) s.webkitRequestFullscreen();
    else {
      const v = this.surface.querySelector<HTMLVideoElement & { webkitEnterFullscreen?: () => void }>("video");
      v?.webkitEnterFullscreen?.();
    }
  }

  // ---- trackpad --------------------------------------------------------

  private onPadStart = (e: TouchEvent) => {
    e.preventDefault();
    if (this.kb.isOpen) this.kb.hide();
    for (const t of Array.from(e.changedTouches)) {
      this.touches.set(t.identifier, { x: t.clientX, y: t.clientY, sx: t.clientX, sy: t.clientY, t: performance.now(), moved: false });
    }
    if (this.touches.size === 1) {
      this.longPressTimer = window.setTimeout(() => this.mouseButton(0, true), LONGPRESS_MS);
    }
    if (this.touches.size === 2) {
      clearTimeout(this.longPressTimer);
      this.twoFingerScroll = true;
      this.lastScrollY = avgY(this.touches);
    }
  };

  private onPadMove = (e: TouchEvent) => {
    e.preventDefault();
    for (const t of Array.from(e.changedTouches)) {
      const p = this.touches.get(t.identifier);
      if (!p) continue;
      const dx = t.clientX - p.x;
      const dy = t.clientY - p.y;
      p.x = t.clientX; p.y = t.clientY;
      if (Math.hypot(t.clientX - p.sx, t.clientY - p.sy) > TAP_SLOP) p.moved = true;
      if (this.touches.size === 1 && !this.twoFingerScroll) {
        clearTimeout(this.longPressTimer);
        this.move(dx, dy);
      }
    }
    if (this.twoFingerScroll && this.touches.size === 2) {
      const y = avgY(this.touches);
      const delta = y - this.lastScrollY;
      this.lastScrollY = y;
      if (Math.abs(delta) > 3) this.wheel(0, -Math.sign(delta));
    }
  };

  private onPadEnd = (e: TouchEvent) => {
    e.preventDefault();
    clearTimeout(this.longPressTimer);
    const ending = Array.from(e.changedTouches);
    const twoFingerTap =
      this.touches.size === 2 &&
      [...this.touches.values()].every((p) => !p.moved && performance.now() - p.t < TAP_MS);
    for (const t of ending) {
      const p = this.touches.get(t.identifier);
      this.touches.delete(t.identifier);
      if (!p) continue;
      const quick = performance.now() - p.t < TAP_MS && !p.moved;
      if (!twoFingerTap && quick && !this.twoFingerScroll && this.touches.size === 0) this.click(0);
    }
    if (twoFingerTap) this.click(1);
    if (this.touches.size === 0) {
      if (this.mouseButtons & 1) this.mouseButton(0, false);
      this.twoFingerScroll = false;
    }
  };

  // ---- emit -----------------------------------------------------------

  private move(dx: number, dy: number) {
    this.sink.sendInput(this.enc.encode({ kind: InputKind.MouseMove, dx: cI16(dx * MOVE_SCALE), dy: cI16(dy * MOVE_SCALE), wheelX: 0, wheelY: 0 }));
  }
  private wheel(x: number, y: number) {
    this.sink.sendInput(this.enc.encode({ kind: InputKind.MouseMove, dx: 0, dy: 0, wheelX: x, wheelY: y }));
  }
  private mouseButton(bit: number, down: boolean) {
    if (down) this.mouseButtons |= 1 << bit;
    else this.mouseButtons &= ~(1 << bit);
    this.sink.sendInput(this.enc.encode({ kind: InputKind.MouseButtons, buttons: this.mouseButtons }));
  }
  private click(bit: number) {
    this.mouseButton(bit, true);
    setTimeout(() => this.mouseButton(bit, false), 30);
  }
  private key(scan: number, down: boolean) {
    if (down) this.held.add(scan);
    else this.held.delete(scan);
    this.sink.sendInput(this.enc.encode({ kind: InputKind.Key, physicalCode: scan, down, modifiers: 0 }));
  }

  /** Inject a single key press (on-screen buttons). */
  tapKey(scan: number) {
    this.key(scan, true);
    setTimeout(() => this.key(scan, false), 25);
  }

  private pollGamepad = () => {
    const gp = readGamepad();
    this.gamepad = gp;
    if (gp) {
      const key = JSON.stringify(gp);
      if (key !== this.lastGamepadKey) {
        this.lastGamepadKey = key;
        this.sink.sendInput(this.enc.encode({ kind: InputKind.Gamepad, state: gp }));
      }
    }
    this.gamepadRaf = requestAnimationFrame(this.pollGamepad);
  };

  private sendSnapshot = () => {
    if (!this.active) return;
    this.sink.sendInput(this.enc.encode({
      kind: InputKind.Snapshot,
      state: {
        mouseButtons: this.mouseButtons,
        modifiers: 0,
        heldKeys: [...this.held],
        gamepad: this.gamepad ?? undefined,
      },
    }));
  };

  private onHide = () => {
    if (document.visibilityState === "hidden") this.releaseAll();
  };

  releaseAll = () => {
    this.held.clear();
    this.mouseButtons = 0;
    this.gamepad = null;
    this.lastGamepadKey = "";
    this.touches.clear();
    this.twoFingerScroll = false;
    clearTimeout(this.longPressTimer);
    this.sink.sendInput(this.enc.encode({ kind: InputKind.Snapshot, state: { mouseButtons: 0, modifiers: 0, heldKeys: [] } }));
    for (const el of this.root.querySelectorAll(".on")) el.classList.remove("on");
  };
}

function cI16(n: number): number {
  return Math.max(-32768, Math.min(32767, Math.round(n)));
}
function avgY(m: Map<number, { y: number }>): number {
  let s = 0;
  for (const p of m.values()) s += p.y;
  return s / Math.max(1, m.size);
}
