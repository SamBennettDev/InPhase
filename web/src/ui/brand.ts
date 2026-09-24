/** InPhase logo kit (web/public/brand/). */

/** What the signal line under the header shows: flat when the PC is out of
 *  reach, a still wave when it is ready, a travelling wave while it streams. */
export type SignalState = "down" | "idle" | "ready" | "live";

/** The brand wave as a live status line (see `.signal` in app.css). */
export function signalLine(id: string): string {
  return `<div class="signal" id="${id}" data-state="idle" aria-hidden="true"></div>`;
}

export function setSignal(el: Element | null, state: SignalState): void {
  if (el && el.getAttribute("data-state") !== state) el.setAttribute("data-state", state);
}

/** The narrow centred column shared by every screen before the app proper:
 *  pairing, certificate setup, "can't reach your PC". */
export function gate(body: string, tag: "main" | "div" = "main"): string {
  return `<${tag} class="gate"><div class="gate-col">${brandLogo("brand-logo brand-logo--gate")}${body}</div></${tag}>`;
}

// The intrinsic ratio must match `lockup.svg`'s viewBox (564×160) exactly
// — a mismatched width/height pair here makes the browser letterbox-squish it.
export function brandLogo(className = "brand-logo"): string {
  return `<img class="${className}" src="/brand/lockup.svg" width="282" height="80" alt="InPhase" />`;
}

/** True when Keyboard Lock is unavailable — Esc always exits fullscreen. */
export function escExitsFullscreen(): boolean {
  const kb = (navigator as Navigator & { keyboard?: { lock?: unknown } }).keyboard;
  return !window.isSecureContext || typeof kb?.lock !== "function";
}

export function browserInputHint(): string | null {
  if (!escExitsFullscreen()) return null;
  const ua = navigator.userAgent;
  if (/Firefox\//i.test(ua)) {
    return "Firefox: Esc exits fullscreen and releases the mouse. Click the video to capture it again.";
  }
  if (/Safari\//i.test(ua) && !/Chrome\//i.test(ua)) {
    return "Safari: Esc exits fullscreen. Click the video to capture the mouse again.";
  }
  return "Esc exits fullscreen on this browser. Click the video to capture the mouse again.";
}
