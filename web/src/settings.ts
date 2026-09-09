// Per-browser stream settings, remembered across sessions (localStorage).
// The host clamps whatever we ask for (`http::signal::clamp_mode` /
// `build_config`), so the UI just needs sane choices.

export type Preset = "low_latency" | "balanced" | "quality" | "custom";

export type StreamTarget =
  | { type: "desktop" }
  | { type: "game"; id: string; name?: string };

export interface StreamSettings {
  width: number;
  height: number;
  fps: number;
  maxBitrateKbps: number;
  preset: Preset;
  streamTarget: StreamTarget;
  /** Playback volume 0–1 and mute for the game audio track. */
  volume: number;
  muted: boolean;
  /** Client audio-output device id for `setSinkId` (secure-context only;
   *  "" = the browser/OS default). */
  audioOutputId: string;
  /** HUD stats overlay (perf / stream lines) visibility. */
  showMetrics: boolean;
}

export const RESOLUTIONS: { label: string; width: number; height: number }[] = [
  { label: "720p", width: 1280, height: 720 },
  { label: "1080p", width: 1920, height: 1080 },
  { label: "1440p", width: 2560, height: 1440 },
  { label: "2160p", width: 3840, height: 2160 },
];
export const FPS_CHOICES = [30, 60, 120];
export const BITRATE_CHOICES_KBPS = [4000, 8000, 12000, 20000, 30000, 50000, 80000];

const KEY = "inphase.settings";

const DEFAULTS: StreamSettings = {
  width: 1920,
  height: 1080,
  fps: 60,
  maxBitrateKbps: 20000,
  preset: "low_latency",
  // No receiver jitter buffer — by design, not a setting (§10). Every frame is
  // presented the instant it decodes; `webrtc.ts` pins
  // `jitterBufferTarget=0` / `playoutDelayHint=0` itself and there is no UI to
  // change it. (Safari enforces its own internal floor of ~2 frames; Chrome
  // honours 0 exactly.)
  streamTarget: { type: "desktop" },
  volume: 1,
  muted: false,
  audioOutputId: "",
  showMetrics: true,
};

// Bump when a stored default becomes actively harmful and must be re-migrated
// for existing browsers.
//   v2: the `bufferMs: 0` default shipped briefly and froze cellular/Tailscale
//       streams (no NACK/RTX recovery slack) — force such stores to 120.
//   v3: product decision — the receiver jitter buffer is gone entirely (§10);
//       "render every frame the instant it decodes" is not configurable. The
//       `bufferMs` field no longer exists and any stored value is ignored.
const SCHEMA = 3;

export function loadSettings(): StreamSettings {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) {
      const s = JSON.parse(raw) as Partial<StreamSettings> & { v?: number };
      return {
        width: clampDimension(Number(s.width) || DEFAULTS.width, 640, 3840),
        height: clampDimension(Number(s.height) || DEFAULTS.height, 360, 2160),
        fps: clampFps(Number(s.fps) || DEFAULTS.fps),
        maxBitrateKbps: clampBitrate(Number(s.maxBitrateKbps) || DEFAULTS.maxBitrateKbps),
        preset: (s.preset as Preset) || DEFAULTS.preset,
        // A stored `bufferMs` (pre-v3) is deliberately not read: there is no
        // jitter-buffer setting any more — see the SCHEMA notes.
        streamTarget: parseStreamTarget(s.streamTarget),
        volume: Math.max(0, Math.min(1, numOr(s.volume, DEFAULTS.volume))),
        muted: s.muted ?? DEFAULTS.muted,
        audioOutputId: typeof s.audioOutputId === "string" ? s.audioOutputId : "",
        showMetrics: s.showMetrics ?? DEFAULTS.showMetrics,
      };
    }
  } catch {
    /* private mode / bad json */
  }
  return { ...DEFAULTS };
}

export function saveSettings(s: StreamSettings): void {
  try {
    localStorage.setItem(KEY, JSON.stringify({ ...s, v: SCHEMA }));
  } catch {
    /* ignore */
  }
}

export function clampBitrate(kbps: number): number {
  return Math.max(2000, Math.min(120000, Math.round(kbps)));
}

/** Number(v) but 0 stays 0 — only NaN/undefined falls back to `dflt`. */
function numOr(v: unknown, dflt: number): number {
  const n = Number(v);
  return Number.isFinite(n) ? n : dflt;
}

export function clampFps(fps: number): number {
  return Math.max(15, Math.min(240, Math.round(fps) || 60));
}

/** Even values only — H.264 chroma subsampling needs an even width/height. */
export function clampDimension(px: number, min: number, max: number): number {
  const v = Math.max(min, Math.min(max, Math.round(px) || min));
  return v - (v % 2);
}

export function bitrateLabel(kbps: number): string {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(kbps % 1000 ? 1 : 0)} Mbps` : `${kbps} kbps`;
}

function parseStreamTarget(v: unknown): StreamTarget {
  if (v && typeof v === "object" && (v as StreamTarget).type === "game") {
    const g = v as { type: "game"; id?: string; name?: string };
    if (typeof g.id === "string" && g.id) return { type: "game", id: g.id, name: g.name };
  }
  return { type: "desktop" };
}
