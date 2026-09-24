// On-screen keyboard for touch devices: a hidden <input> that summons the
// native keyboard, translated to the canonical scancode protocol.
//
// iOS soft keyboards do not populate `KeyboardEvent.code`, so:
//   * named keys (Enter, Backspace, Tab, Esc, arrows) come from `keydown.key`;
//   * typed characters come from `beforeinput` and are mapped char -> scancode.

import { SCANCODE } from "./keycodes.js";

type SendKey = (physicalCode: number, down: boolean) => void;

const NAMED: Record<string, string> = {
  Enter: "Enter",
  Backspace: "Backspace",
  Tab: "Tab",
  Escape: "Escape",
  ArrowUp: "ArrowUp",
  ArrowDown: "ArrowDown",
  ArrowLeft: "ArrowLeft",
  ArrowRight: "ArrowRight",
  " ": "Space",
};

// char -> { code, shift }
const CHAR: Record<string, { code: string; shift: boolean }> = {};
(() => {
  const row = (chars: string, codes: string[]) => {
    [...chars].forEach((c, i) => (CHAR[c] = { code: codes[i]!, shift: false }));
  };
  row("abcdefghijklmnopqrstuvwxyz", [..."abcdefghijklmnopqrstuvwxyz"].map((c) => "Key" + c.toUpperCase()));
  [..."ABCDEFGHIJKLMNOPQRSTUVWXYZ"].forEach((c) => (CHAR[c] = { code: "Key" + c, shift: true }));
  row("1234567890", ["Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9", "Digit0"]);
  const sym: [string, string, boolean][] = [
    ["-", "Minus", false], ["_", "Minus", true],
    ["=", "Equal", false], ["+", "Equal", true],
    ["[", "BracketLeft", false], ["{", "BracketLeft", true],
    ["]", "BracketRight", false], ["}", "BracketRight", true],
    ["\\", "Backslash", false], ["|", "Backslash", true],
    [";", "Semicolon", false], [":", "Semicolon", true],
    ["'", "Quote", false], ['"', "Quote", true],
    [",", "Comma", false], ["<", "Comma", true],
    [".", "Period", false], [">", "Period", true],
    ["/", "Slash", false], ["?", "Slash", true],
    ["`", "Backquote", false], ["~", "Backquote", true],
    ["!", "Digit1", true], ["@", "Digit2", true], ["#", "Digit3", true], ["$", "Digit4", true],
    ["%", "Digit5", true], ["^", "Digit6", true], ["&", "Digit7", true], ["*", "Digit8", true],
    ["(", "Digit9", true], [")", "Digit0", true],
  ];
  for (const [ch, code, shift] of sym) CHAR[ch] = { code, shift };
})();

export class SoftKeyboard {
  readonly el: HTMLInputElement;
  private open = false;

  constructor(private readonly send: SendKey) {
    this.el = document.createElement("input");
    this.el.className = "soft-kb";
    this.el.setAttribute("autocapitalize", "off");
    this.el.setAttribute("autocomplete", "off");
    this.el.setAttribute("autocorrect", "off");
    this.el.setAttribute("spellcheck", "false");
    this.el.setAttribute("aria-hidden", "true");

    this.el.addEventListener("keydown", this.onKeyDown);
    this.el.addEventListener("beforeinput", this.onBeforeInput);
    this.el.addEventListener("blur", () => (this.open = false));
  }

  toggle() {
    this.open ? this.hide() : this.show();
  }
  show() {
    this.open = true;
    this.el.value = "";
    this.el.focus({ preventScroll: true });
  }
  hide() {
    this.open = false;
    this.el.blur();
  }
  get isOpen() {
    return this.open;
  }

  private tap(code: string, shift = false) {
    const sc = SCANCODE[code];
    if (sc === undefined) return;
    const shiftSc = SCANCODE["ShiftLeft"]!;
    if (shift) this.send(shiftSc, true);
    this.send(sc, true);
    this.send(sc, false);
    if (shift) this.send(shiftSc, false);
  }

  private onKeyDown = (e: KeyboardEvent) => {
    const named = NAMED[e.key];
    if (named) {
      e.preventDefault();
      this.tap(named);
    }
    // keep the field empty so backspace always fires as a key, not a no-op
    this.el.value = "";
  };

  private onBeforeInput = (e: InputEvent) => {
    if (e.inputType === "insertText" && e.data) {
      for (const ch of e.data) {
        const m = CHAR[ch];
        if (m) this.tap(m.code, m.shift);
        else if (ch === " ") this.tap("Space");
      }
    } else if (e.inputType === "insertLineBreak") {
      this.tap("Enter");
    } else if (e.inputType.startsWith("deleteContent")) {
      this.tap("Backspace");
    }
    e.preventDefault();
    this.el.value = "";
  };
}
