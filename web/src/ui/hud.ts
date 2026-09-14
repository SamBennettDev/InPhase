// Top-right stats overlay (Rivatuner-style) + settings panel (§20, §25.2).

import {
  RESOLUTIONS,
  FPS_CHOICES,
  bitrateLabel,
  clampBitrate,
  clampFps,
  clampDimension,
  type StreamSettings,
} from "../settings.js";
import {
  createIcons,
  Volume2,
  Volume1,
  VolumeX,
  TriangleAlert,
  Settings,
  Maximize,
} from "lucide";

/** Re-render any `<i data-lucide>` placeholders inside `scope` as SVGs. */
export function refreshIcons(scope: ParentNode = document) {
  createIcons({
    icons: { Volume2, Volume1, VolumeX, TriangleAlert, Settings, Maximize },
    nameAttr: "data-lucide",
  });
  void scope;
}

export interface HudMetrics {
  decodedFps: number; // real stream rate (frames the decoder receives)
  presentedFps: number; // rate the browser paints — capped by display Hz
  rttMs: number; // media-path RTT (getStats candidate-pair)
  inputRttMs: number; // control data-channel round trip
  e2eEstMs: number; // estimated glass-to-glass (transit + buffer + decode + present + host)
  width: number;
  height: number;
  inboundKbps: number;
  codec: string;
  jitterBufferMs?: number;
  decodeMs?: number;
  packetsLost?: number;
  /** Inbound audio verdict from telemetry, or null if audio is off. */
  audioState?:
    | "no-track"
    | "no-packets"
    | "no-samples"
    | "silent"
    | "signal"
    | null;
  audioHostFailed?: boolean;
  /** Path label ("LAN" | "VPN" | "Direct" …), or null before ICE settles. */
  path?: string | null;
  /** Tooltip phrase for the path label — never contains an address. */
  pathDetail?: string | null;
}

export class Hud {
  readonly root: HTMLElement;
  private panelOpen = false;

  constructor(
    private settings: StreamSettings,
    private readonly onApply: (s: StreamSettings) => void,
    private readonly onDisconnect: () => void,
    private readonly onSendKey: (scanCode: number) => void,
    private readonly onAudio: (
      volume: number,
      muted: boolean,
    ) => void = () => {},
    /** Mount the shared audio settings section into a container (in-session). */
    private readonly mountAudio: (el: HTMLElement) => void = () => {},
    /** Metrics-overlay toggle (persisted by the Session). */
    private readonly onMetrics: (v: boolean) => void = () => {},
  ) {
    this.root = document.createElement("div");
    this.root.className = "hud";
    this.root.innerHTML = `
      <div class="stats-overlay" aria-live="polite">
        <div class="stats-line" data-line="perf"></div>
        <div class="stats-line" data-line="stream"></div>
      </div>
      <div class="hud-controls">
        <button class="hud-btn" data-act="mute" title="Mute / unmute" hidden></button>
        <button class="hud-btn" data-key="0x01" title="Escape">ESC</button>
        <button class="hud-btn" data-act="fs" title="Fullscreen"><i data-lucide="maximize"></i></button>
        <button class="hud-btn" data-act="panel" title="Settings"><i data-lucide="settings"></i></button>
      </div>
      <div class="hud-panel" hidden></div>`;
    for (const b of this.root.querySelectorAll<HTMLElement>("[data-key]")) {
      b.addEventListener("click", () =>
        this.onSendKey(Number(b.dataset["key"])),
      );
    }
    this.root
      .querySelector('[data-act="panel"]')!
      .addEventListener("click", () => this.togglePanel());
    this.root
      .querySelector('[data-act="fs"]')!
      .addEventListener("click", () => {
        const stage = this.root.closest(".stage");
        if (!stage) return;
        if (document.fullscreenElement)
          void document.exitFullscreen().catch(() => {});
        else void stage.requestFullscreen?.().catch(() => {});
      });
    this.root
      .querySelector('[data-act="mute"]')!
      .addEventListener("click", () => {
        this.settings.muted = !this.settings.muted;
        this.pushAudio();
      });
    // HUD clicks/keys must never reach the stage's InputManager: a gear or
    // panel click used to bubble to the surface handler and re-capture the
    // pointer - "I pressed an icon on the paused screen and it resumed".
    for (const type of ["click", "mousedown", "keydown", "keyup"] as const) {
      this.root.addEventListener(type, (e) => e.stopPropagation());
    }
    this.paintMute();
    this.statsOverlay = this.root.querySelector<HTMLElement>(".stats-overlay")!;
    this.statsOverlay.style.display = this.settings.showMetrics ? "" : "none";
    this.buildPanel();
  }

  private statsOverlay: HTMLElement;

  private audioState: HudMetrics["audioState"] = null;
  private audioHostFailed = false;

  /** Show the mute button + reflect the current state (called by the Session
   *  once it knows whether the stream carries audio). */
  setHasAudio(has: boolean) {
    this.root.querySelector<HTMLElement>('[data-act="mute"]')!.hidden = !has;
  }

  private paintMute() {
    const b = this.root.querySelector<HTMLElement>('[data-act="mute"]')!;
    const silent =
      this.audioHostFailed ||
      this.audioState === "silent" ||
      this.audioState === "no-samples" ||
      this.audioState === "no-packets";
    let icon = "volume-2";
    if (this.settings.muted || this.settings.volume === 0) {
      icon = "volume-x";
      b.title = "Muted";
    } else if (this.audioHostFailed) {
      icon = "triangle-alert";
      b.title = "Host audio failed — reconnect to recover";
    } else if (silent) {
      icon = "volume-1";
      b.title = "No audio signal from the host's capture source";
    } else {
      b.title = this.audioState === "signal" ? "Audio OK" : "Mute / unmute";
    }
    b.innerHTML = `<i data-lucide="${icon}"></i>`;
    refreshIcons(b);
    b.classList.toggle(
      "warn",
      (silent || this.audioHostFailed) && !this.settings.muted,
    );
    const v = this.root.querySelector<HTMLInputElement>('[data-s="vol"]');
    if (v) v.value = String(Math.round(this.settings.volume * 100));
  }

  private pushAudio() {
    this.paintMute();
    this.onAudio(this.settings.volume, this.settings.muted);
  }

  update(m: HudMetrics) {
    if (m.audioState !== undefined) this.audioState = m.audioState;
    this.audioHostFailed = !!m.audioHostFailed;
    this.paintMute();

    const decoded = Math.round(
      m.decodedFps > 1 ? m.decodedFps : m.presentedFps,
    );
    const presented = Math.round(m.presentedFps);
    const fpsText =
      presented > 1 && Math.abs(presented - decoded) > 2
        ? `${decoded}/${presented} fps`
        : `${decoded || "--"} fps`;

    const perfParts: StatPart[] = [
      { text: fpsText },
      {
        text: m.e2eEstMs > 0 ? `${Math.round(m.e2eEstMs)} ms e2e` : "-- ms e2e",
        title: "estimated glass-to-glass latency",
        tint: { v: m.e2eEstMs, good: 80, bad: 160 },
      },
      {
        text: `${Math.round(m.rttMs)} ms net`,
        tint: { v: m.rttMs, good: 25, bad: 60 },
      },
    ];
    if (m.inputRttMs > 0) {
      perfParts.push({
        text: `${Math.round(m.inputRttMs)} ms in`,
        tint: { v: m.inputRttMs, good: 25, bad: 60 },
      });
    }
    if (m.decodeMs != null && m.decodeMs > 0) {
      perfParts.push({ text: `${m.decodeMs.toFixed(1)} ms dec` });
    }
    if (m.jitterBufferMs != null && m.jitterBufferMs > 0) {
      perfParts.push({ text: `${Math.round(m.jitterBufferMs)} ms buf` });
    }
    this.renderLine("perf", perfParts);

    const streamParts: StatPart[] = [];
    if (m.width > 0) streamParts.push({ text: `${m.width}×${m.height}` });
    if (m.inboundKbps > 0)
      streamParts.push({ text: bitrateLabel(Math.round(m.inboundKbps)) });
    const codec = m.codec ? m.codec.replace(/^video\//i, "").toUpperCase() : "";
    if (codec) streamParts.push({ text: codec });
    if (m.path)
      streamParts.push({ text: m.path, title: m.pathDetail || "network path" });
    if (m.packetsLost != null && m.packetsLost > 0) {
      streamParts.push({ text: `${m.packetsLost} lost`, warn: true });
    }
    if (!streamParts.length) streamParts.push({ text: "--" });
    this.renderLine("stream", streamParts);
  }

  /** Right-aligned line of `a · b · c` segments. */
  private renderLine(line: "perf" | "stream", parts: StatPart[]) {
    const el = this.root.querySelector<HTMLElement>(`[data-line="${line}"]`)!;
    el.replaceChildren();
    parts.forEach((part, i) => {
      if (i > 0) {
        const sep = document.createElement("span");
        sep.className = "stats-sep";
        sep.textContent = " · ";
        el.appendChild(sep);
      }
      const span = document.createElement("span");
      span.textContent = part.text;
      if (part.title) span.title = part.title;
      if (part.warn) span.style.color = "var(--warn)";
      else if (part.tint)
        tint(span, part.tint.v, part.tint.good, part.tint.bad);
      el.appendChild(span);
    });
  }

  private togglePanel() {
    this.panelOpen = !this.panelOpen;
    this.root.querySelector<HTMLElement>(".hud-panel")!.hidden =
      !this.panelOpen;
  }

  private buildPanel() {
    const p = this.root.querySelector<HTMLElement>(".hud-panel")!;
    const resList = RESOLUTIONS.map(
      (r) => `<option value="${r.width}x${r.height}">${r.label}</option>`,
    ).join("");
    const fpsList = FPS_CHOICES.map(
      (f) => `<option value="${f}"></option>`,
    ).join("");
    const s = this.settings;
    p.innerHTML = `
      <label>Resolution (w × h)
        <span class="hud-pair">
          <input data-s="w" type="number" inputmode="numeric" min="640" max="3840" step="2" value="${s.width}" list="hud-res" />
          <input data-s="h" type="number" inputmode="numeric" min="360" max="2160" step="2" value="${s.height}" />
        </span>
        <datalist id="hud-res">${resList}</datalist>
      </label>
      <label>Frame rate (fps)
        <input data-s="fps" type="number" inputmode="numeric" min="15" max="240" step="1" value="${s.fps}" list="hud-fps" />
        <datalist id="hud-fps">${fpsList}</datalist>
      </label>
      <label>Max bitrate (Mbps)
        <input data-s="br" type="number" inputmode="decimal" min="2" max="120" step="0.5" value="${(s.maxBitrateKbps / 1000).toString()}" />
      </label>
      <p class="sub">Jitter buffer: none — frames render as they arrive.</p>
      <label>Preset<select data-s="preset">
        <option value="low_latency">Low Latency</option>
        <option value="balanced">Balanced</option>
        <option value="quality">Quality</option>
      </select></label>
      <label>Volume
        <input data-s="vol" type="range" min="0" max="100" step="1" value="${Math.round(s.volume * 100)}" />
      </label>
      <label class="hud-chk"><input data-s="metrics" type="checkbox" ${s.showMetrics ? "checked" : ""} />
        Show metrics overlay</label>
      <div class="home-audio" data-audio-mount></div>
      <div class="hud-panel-btns">
        <button data-act="apply">Apply &amp; reconnect</button>
        <button data-act="disc" class="secondary">Disconnect</button>
      </div>`;
    this.mountAudio(p.querySelector<HTMLElement>("[data-audio-mount]")!);
    const el = (k: string) =>
      p.querySelector<HTMLInputElement | HTMLSelectElement>(`[data-s="${k}"]`)!;
    (el("preset") as HTMLSelectElement).value = this.settings.preset;

    // Metrics overlay applies live and persists immediately.
    (el("metrics") as HTMLInputElement).addEventListener("change", (e) => {
      const v = (e.target as HTMLInputElement).checked;
      this.settings.showMetrics = v;
      this.statsOverlay.style.display = v ? "" : "none";
      this.onMetrics(v);
    });

    // Volume applies live (no reconnect).
    el("vol").addEventListener("input", () => {
      this.settings.volume =
        Number((el("vol") as HTMLInputElement).value) / 100;
      if (this.settings.volume > 0) this.settings.muted = false;
      this.pushAudio();
    });

    p.querySelector('[data-act="apply"]')!.addEventListener("click", () => {
      const num = (k: string) => Number((el(k) as HTMLInputElement).value);
      this.settings = {
        ...this.settings,
        width: clampDimension(num("w"), 640, 3840),
        height: clampDimension(num("h"), 360, 2160),
        fps: clampFps(num("fps")),
        maxBitrateKbps: clampBitrate(num("br") * 1000),
        preset: (el("preset") as HTMLSelectElement)
          .value as StreamSettings["preset"],
      };
      this.togglePanel();
      this.onApply(this.settings);
    });
    p.querySelector('[data-act="disc"]')!.addEventListener("click", () =>
      this.onDisconnect(),
    );
  }
}

interface StatPart {
  text: string;
  title?: string;
  tint?: { v: number; good: number; bad: number };
  warn?: boolean;
}

function tint(el: HTMLElement, v: number, good: number, bad: number) {
  el.style.color =
    v <= 0
      ? ""
      : v < good
        ? "var(--ok)"
        : v < bad
          ? "var(--warn)"
          : "var(--err)";
}
