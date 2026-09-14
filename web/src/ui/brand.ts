/** InPhase logo kit (web/public/brand/). */

// The intrinsic ratio must match `lockup.svg`'s viewBox (2500×800 = 25:8) exactly
// — a mismatched width/height pair here makes the browser letterbox-squish it.
export function brandLogo(className = "brand-logo"): string {
  return `<img class="${className}" src="/brand/lockup.svg" width="250" height="80" alt="InPhase" />`;
}

/** True when Keyboard Lock is unavailable — Esc always exits fullscreen. */
export function escExitsFullscreen(): boolean {
  const kb = (navigator as Navigator & { keyboard?: { lock?: unknown } })
    .keyboard;
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
