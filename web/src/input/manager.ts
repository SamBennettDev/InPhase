// Input capture orchestrator (architecture report §12).
//
// Owns pointer/keyboard/gamepad capture, produces binary packets on the `input`
// channel, and sends an authoritative state snapshot at ~40 Hz on the same
// channel so a lost key-up cannot wedge a key (§12.2). Releases everything on
// blur / visibility loss / disconnect (§12.2, §22).

import {
  InputEncoder,
  InputKind,
  FLAG_UNADJUSTED_MOVEMENT,
  type GamepadStateWire,
} from "./protocol.js";
import { scanCodeFor, modifiersFrom } from "./keycodes.js";
import { readGamepad } from "./gamepad.js";

export interface InputSink {
  sendInput: (bytes: Uint8Array) => void;
}

const SNAPSHOT_HZ = 40;

export class InputManager {
  private enc = new InputEncoder();
  private held = new Set<number>();
  private mouseButtons = 0;
  private modifiers = 0;
  private gamepad: GamepadStateWire | null = null;
  private lastGamepadKey = "";
  private captured = false;
  private pointerLocked = false;
  private snapshotTimer = 0;
  private rafId = 0;
  private unadjustedActive = false;

  /** Fired when capture is released via hotkey (Esc / F11 / Ctrl+Shift+Q). */
  onReleaseHotkey: (() => void) | null = null;
  /** Fired on Ctrl+Shift+Q specifically. When set, it *replaces* the normal
   *  release for that chord (the play window uses it to quit outright). */
  onQuitHotkey: (() => void) | null = null;
  /** True once Keyboard Lock is active (Chromium + secure context + fullscreen)
   *  — physical Esc / Tab / Meta then reach the game instead of the browser. */
  keyboardLocked = false;

  constructor(
    private readonly surface: HTMLElement,
    private readonly sink: InputSink,
  ) {}

  /** Enter fullscreen. Capture (pointer + keyboard lock) follows in `onFsChange`
   *  — capture is bound to the fullscreen state, so leaving fullscreen (Esc, the
   *  green button, ⌃⌘F) releases everything automatically. */
  async capture() {
    if (isFullscreen()) {
      void this.engageCapture();
      return;
    }
    const el = this.surface as HTMLElement & {
      requestFullscreen?: () => Promise<void>;
      webkitRequestFullscreen?: () => void;
    };
    try {
      if (el.requestFullscreen) await el.requestFullscreen();
      else el.webkitRequestFullscreen?.();
    } catch {
      /* denied / not allowed — nothing gets captured */
    }
  }

  /** Pointer lock + (Chromium, HTTPS) keyboard lock. Runs once we're fullscreen. */
  private async engageCapture() {
    const el = this.surface as HTMLElement & {
      requestPointerLock: (opts?: {
        unadjustedMovement?: boolean;
      }) => Promise<void> | void;
    };
    try {
      const p = el.requestPointerLock({ unadjustedMovement: true });
      if (p instanceof Promise) await p;
      this.unadjustedActive = true;
    } catch {
      try {
        el.requestPointerLock();
      } catch {
        /* Safari can reject a re-lock; keyboard still works */
      }
      this.unadjustedActive = false;
    }
    // §12.1: Keyboard Lock — Chromium only, secure context only, needs fullscreen.
    const kb = (
      navigator as Navigator & {
        keyboard?: {
          lock?: (k?: string[]) => Promise<void>;
          unlock?: () => void;
        };
      }
    ).keyboard;
    if (window.isSecureContext && kb?.lock) {
      try {
        await kb.lock(["Escape", "Tab", "MetaLeft", "MetaRight"]);
        this.keyboardLocked = true;
      } catch {
        this.keyboardLocked = false;
      }
    }
  }

  private onFsChange = () => {
    if (isFullscreen()) {
      this.captured = true;
      void this.engageCapture();
    } else if (this.captured) {
      this.captured = false;
      this.releaseAll();
      this.onReleaseHotkey?.();
    }
  };

  /** Release capture by leaving fullscreen (`onFsChange` does the teardown). */
  release() {
    const d = document as Document & { webkitExitFullscreen?: () => void };
    if (document.fullscreenElement)
      void document.exitFullscreen().catch(() => {});
    else if (isFullscreen()) d.webkitExitFullscreen?.();
    else {
      this.releaseAll();
      this.onReleaseHotkey?.();
    }
  }

  /** Inject a single key press (on-screen ESC/etc. buttons). */
  tapKey(scanCode: number) {
    this.sendKey(scanCode, true);
    setTimeout(() => this.sendKey(scanCode, false), 25);
  }
  private sendKey(scanCode: number, down: boolean) {
    if (down) this.held.add(scanCode);
    else this.held.delete(scanCode);
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.Key,
        physicalCode: scanCode,
        down,
        modifiers: this.modifiers,
      }),
    );
  }

  attach() {
    document.addEventListener("pointerlockchange", this.onLockChange);
    document.addEventListener("fullscreenchange", this.onFsChange);
    document.addEventListener("webkitfullscreenchange", this.onFsChange);
    this.surface.addEventListener("click", this.onSurfaceClick);
    window.addEventListener("blur", this.releaseAll);
    document.addEventListener("visibilitychange", this.onVisibility);
    this.surface.addEventListener("mousedown", this.onMouseButton);
    this.surface.addEventListener("mouseup", this.onMouseButton);
    this.surface.addEventListener("mousemove", this.onMouseMove);
    this.surface.addEventListener("wheel", this.onWheel, { passive: false });
    this.surface.addEventListener("contextmenu", this.onContextMenu);
    window.addEventListener("keydown", this.onKey);
    window.addEventListener("keyup", this.onKey);
    this.snapshotTimer = window.setInterval(
      this.sendSnapshot,
      1000 / SNAPSHOT_HZ,
    );
    this.rafId = requestAnimationFrame(this.pollGamepad);
  }

  detach() {
    document.removeEventListener("pointerlockchange", this.onLockChange);
    document.removeEventListener("fullscreenchange", this.onFsChange);
    document.removeEventListener("webkitfullscreenchange", this.onFsChange);
    this.surface.removeEventListener("click", this.onSurfaceClick);
    window.removeEventListener("blur", this.releaseAll);
    document.removeEventListener("visibilitychange", this.onVisibility);
    this.surface.removeEventListener("mousedown", this.onMouseButton);
    this.surface.removeEventListener("mouseup", this.onMouseButton);
    this.surface.removeEventListener("mousemove", this.onMouseMove);
    this.surface.removeEventListener("wheel", this.onWheel);
    this.surface.removeEventListener("contextmenu", this.onContextMenu);
    window.removeEventListener("keydown", this.onKey);
    window.removeEventListener("keyup", this.onKey);
    clearInterval(this.snapshotTimer);
    cancelAnimationFrame(this.rafId);
    this.releaseAll();
  }

  get isCaptured() {
    return this.captured;
  }

  private onLockChange = () => {
    this.pointerLocked = document.pointerLockElement === this.surface;
    // Pointer lock can drop while still fullscreen (Safari); only a fullscreen
    // exit releases — `onFsChange` owns that. A lost lock just stops mouse move.
  };

  private onSurfaceClick = () => {
    if (!isFullscreen()) void this.capture();
    else if (!this.pointerLocked) void this.engageCapture(); // Safari: grab mouse
  };

  private onVisibility = () => {
    if (document.visibilityState === "hidden") this.releaseAll();
  };

  private onMouseMove = (e: MouseEvent) => {
    if (!this.pointerLocked) return;
    this.sink.sendInput(
      this.enc.encode(
        {
          kind: InputKind.MouseMove,
          dx: clampI16(e.movementX),
          dy: clampI16(e.movementY),
          wheelX: 0,
          wheelY: 0,
        },
        this.unadjustedActive ? FLAG_UNADJUSTED_MOVEMENT : 0,
      ),
    );
  };

  private onWheel = (e: WheelEvent) => {
    if (!this.captured) return;
    e.preventDefault();
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.MouseMove,
        dx: 0,
        dy: 0,
        wheelX: clampI16(Math.sign(e.deltaX)),
        wheelY: clampI16(Math.sign(e.deltaY)),
      }),
    );
  };

  // MouseEvent.button (0=left 1=middle 2=right 3=x1 4=x2) -> protocol bit
  // (0=left 1=right 2=middle 3=x1 4=x2), matching MouseEvent.buttons.
  private static readonly BTN_BIT = [0, 2, 1, 3, 4];

  private onMouseButton = (e: MouseEvent) => {
    if (!this.captured) return;
    e.preventDefault();
    const bit = InputManager.BTN_BIT[e.button] ?? 0;
    this.setButton(bit, e.type === "mousedown");
  };

  private setButton(bit: number, down: boolean) {
    if (down) this.mouseButtons |= 1 << bit;
    else this.mouseButtons &= ~(1 << bit);
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.MouseButtons,
        buttons: this.mouseButtons,
      }),
    );
  }

  // macOS two-finger tap fires `contextmenu` but not always mousedown/up for the
  // secondary button — synthesise a right click.
  private onContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    if (!this.captured) return;
    this.setButton(1, true);
    setTimeout(() => this.setButton(1, false), 30);
  };

  private onKey = (e: KeyboardEvent) => {
    // F11 toggles capture (replaces the click-to-capture overlay). Handled
    // before the capture guard so it works from the idle state too.
    if (e.type === "keydown" && e.code === "F11") {
      e.preventDefault();
      if (this.captured) this.release();
      else void this.capture();
      return;
    }

    // Ctrl+Shift+Q: in the play window it quits outright (`onQuitHotkey`) and
    // must work even from the paused/overlay state, so it's handled before the
    // capture guard. Without a quit handler it just releases capture.
    if (e.type === "keydown" && e.ctrlKey && e.shiftKey && e.code === "KeyQ") {
      e.preventDefault();
      if (this.onQuitHotkey) this.onQuitHotkey();
      else if (this.captured) this.release();
      return;
    }

    if (!this.captured) return;

    // Escape releases capture, *unless* Keyboard Lock is active (HTTPS) — then
    // Esc is forwarded to the game for pause menus and F11 is the way out. On
    // HTTP the browser drops pointer lock on Esc anyway; this keeps the
    // fullscreen/keyboard-lock teardown in one place.
    if (e.type === "keydown" && e.code === "Escape" && !this.keyboardLocked) {
      e.preventDefault();
      this.release();
      return;
    }

    const scan = scanCodeFor(e.code);
    if (scan === undefined) return;
    e.preventDefault();
    const down = e.type === "keydown";
    if (down) this.held.add(scan);
    else this.held.delete(scan);
    this.modifiers = modifiersFrom(e);
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.Key,
        physicalCode: scan,
        down,
        modifiers: this.modifiers,
      }),
    );
  };

  private pollGamepad = () => {
    // A gamepad player does not need mouse capture — poll unconditionally.
    // Bulletproof: any throw here must NOT break the poll loop.
    try {
      const gp = readGamepad();
      this.gamepad = gp; // keeps the 40 Hz snapshot heartbeat carrying the pad
      if (gp) {
        const key = JSON.stringify(gp);
        if (key !== this.lastGamepadKey) {
          this.lastGamepadKey = key;
          this.sink.sendInput(
            this.enc.encode({ kind: InputKind.Gamepad, state: gp }),
          );
        }
      }
    } catch (err) {
      console.error("[InPhase] pollGamepad error", err);
    }
    this.rafId = requestAnimationFrame(this.pollGamepad);
  };

  private sendSnapshot = () => {
    // Keep the heartbeat alive whenever we have *something* to report — mouse/
    // keyboard capture, or a connected gamepad.
    if (!this.captured && !this.gamepad) return;
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.Snapshot,
        state: {
          mouseButtons: this.captured ? this.mouseButtons : 0,
          modifiers: this.captured ? this.modifiers : 0,
          heldKeys: this.captured ? [...this.held] : [],
          gamepad: this.gamepad ?? undefined,
        },
      }),
    );
  };

  releaseAll = () => {
    this.held.clear();
    this.mouseButtons = 0;
    this.modifiers = 0;
    this.gamepad = null;
    this.lastGamepadKey = "";
    // One empty snapshot tells the host to release everything (§12.2).
    this.sink.sendInput(
      this.enc.encode({
        kind: InputKind.Snapshot,
        state: { mouseButtons: 0, modifiers: 0, heldKeys: [] },
      }),
    );
    if (document.pointerLockElement === this.surface)
      document.exitPointerLock();
    if (this.keyboardLocked) {
      (
        navigator as Navigator & { keyboard?: { unlock?: () => void } }
      ).keyboard?.unlock?.();
      this.keyboardLocked = false;
    }
  };
}

function clampI16(n: number): number {
  return Math.max(-32768, Math.min(32767, Math.round(n)));
}

function isFullscreen(): boolean {
  const d = document as Document & { webkitFullscreenElement?: Element };
  return !!(document.fullscreenElement || d.webkitFullscreenElement);
}
