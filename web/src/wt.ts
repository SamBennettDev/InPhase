// Main-thread facade for the WebTransport video client.
//
// The real client (wtcore.ts) runs inside a Dedicated Worker when the
// browser supports WebTransport there — the datagram drain loop must not
// contend with rendering on the main thread, or the browser's
// incoming-datagram queue silently drops from the head (05:08 Chrome:
// collapse at ~5.5k datagrams/s). The public surface is identical to the
// old in-page client, so play.ts is unchanged; when the worker path is
// unavailable (older Safari, no worker WebTransport) the same class runs
// in the page exactly as before.

import {
  WtVideoClient as WtCore,
  opusSupportProbe,
  type WtClientHandlers,
  type WtClientStats,
  type WtVideoInfo,
} from "./wtcore.js";

export { opusSupportProbe };
export type { WtClientHandlers, WtClientStats, WtVideoInfo };
export type { WtVideoConfig } from "./wtcore.js";

/** Page-only reload backstop for the wire-mismatch watchdog. */
function pageReload(): void {
  if (!sessionStorage.getItem("inphase_reload")) {
    sessionStorage.setItem("inphase_reload", "1");
    location.reload();
  }
}

export class WtVideoClient {
  private core: WtCore | null = null;
  private worker: Worker | null = null;
  private handlers: WtClientHandlers | null = null;
  private statsProvider: (() => WtClientStats) | null = null;
  private statsPusher: ReturnType<typeof setInterval> | 0 = 0;
  private dialed = false;
  private deltaSeeded = false;
  // Main-thread caches of worker-side state, refreshed per pong / telemetry.
  private lastRtt = 0;
  private offsetPageUs: number | null = null;
  private clockDeltaEmaUs = 0;
  private stale = { atPage: 0, staleMs: 0 };
  private winKbps = 0;

  get active(): boolean {
    return this.worker !== null ? this.dialed : this.core?.active ?? false;
  }

  inputReady(): boolean {
    return this.worker !== null ? this.dialed : this.core?.inputReady() ?? false;
  }

  rttMs(): number {
    return this.worker !== null
      ? Math.round(this.lastRtt * 100) / 100
      : (this.core?.rttMs() ?? 0);
  }

  inboundKbps(): number {
    return this.worker !== null ? this.winKbps : (this.core?.inboundKbps() ?? 0);
  }

  /** Ms since the last sign of life (pong or datagram), worker clocks folded
   *  into page time: snapshot age + staleness measured worker-side at the
   *  snapshot. */
  staleMs(): number {
    if (this.worker === null) return this.core?.staleMs() ?? 0;
    if (this.stale.atPage === 0) return 0;
    return performance.now() - this.stale.atPage + this.stale.staleMs;
  }

  get datagramCount(): number {
    return this.worker !== null ? 0 : (this.core?.datagramCount ?? 0);
  }

  /** Playout stats the periodic telemetry message reports to the host. In
   *  worker mode the page pushes them once a second (the decoder lives here). */
  setStatsProvider(p: () => WtClientStats): void {
    this.statsProvider = p;
    this.core?.setStatsProvider(p);
    // Don't wait a full second for the first push: worker telemetry would
    // otherwise send presented_fps 0 (ZERO_STATS) while the glass is up.
    if (this.worker !== null) this.worker.postMessage({ t: "stats", s: p() });
  }

  async dial(info: WtVideoInfo, h: WtClientHandlers): Promise<void> {
    this.handlers = h;
    const full: WtVideoInfo = { ...info, hostname: location.hostname };
    if (typeof Worker !== "undefined") {
      try {
        await this.dialViaWorker(full, h);
        this.dialed = true;
        // The decoder + FrameOrderer live on this thread: push their stats to
        // the worker once a second so its telemetry merges wire + decode.
        this.statsPusher = setInterval(() => {
          if (this.statsProvider) this.worker?.postMessage({ t: "stats", s: this.statsProvider() });
        }, 1000);
        return;
      } catch (e) {
        this.killWorker();
        if (!String(e).includes("unsupported")) throw e;
        console.info("wt: WebTransport unavailable in workers - running on the main thread");
      }
    }
    this.dialed = true;
    this.core = new WtCore();
    this.core.onWire = (w) => {
      (window as unknown as Record<string, unknown>).__inphaseWtWire = w;
    };
    if (this.statsProvider) this.core.setStatsProvider(this.statsProvider);
    // The core calls onReload instead of touching page globals; in fallback
    // (in-page) mode that IS the page, so wire it here.
    await this.core.dial(full, { ...h, onReload: pageReload });
  }

  private dialViaWorker(info: WtVideoInfo, h: WtClientHandlers): Promise<void> {
    const worker = new Worker(new URL("./wtworker.ts", import.meta.url), { type: "module" });
    this.worker = worker;
    worker.onmessage = (e: MessageEvent) => this.onWorkerMessage(e.data);
    worker.onerror = () => h.onClosed("wt worker crashed");
    return new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error("wt worker dial timed out")), 15_000);
      const settle = (fn: () => void) => {
        clearTimeout(timeout);
        fn();
      };
      worker.onmessage = (e: MessageEvent) => {
        const m = e.data as { t: string; msg?: string };
        if (m.t === "dial-ok") return settle(resolve);
        if (m.t === "unsupported") {
          this.killWorker();
          return reject(new Error("WebTransport unsupported in worker"));
        }
        if (m.t === "dial-err") return reject(new Error(m.msg ?? "worker dial failed"));
        this.onWorkerMessage(m);
      };
      worker.postMessage({ t: "dial", info });
    });
  }

  private onWorkerMessage(m: Record<string, unknown> & { t: string }): void {
    const h = this.handlers;
    if (h === null) return;
    switch (m.t) {
      case "frame":
        h.onFrame(m.f as Parameters<WtClientHandlers["onFrame"]>[0]);
        break;
      case "audio":
        h.onAudio?.(m.opus as Uint8Array, m.ptsUs as number);
        break;
      case "video-config":
        h.onVideoConfig(m.cfg as Parameters<WtClientHandlers["onVideoConfig"]>[0]);
        break;
      case "closed":
        h.onClosed(m.why as string);
        break;
      case "route-warning":
        h.onRouteWarning?.(m.detail as string);
        break;
      case "control-message":
        h.onControlMessage?.(m.line as string);
        break;
      case "rtt": {
        const nowUs = performance.now() * 1000;
        this.lastRtt = m.rttMs as number;
        // Worker and page clocks share Date but not the performance.now()
        // origin. Each pong carries the worker's now(); the difference to the
        // page clock at receipt is what the capture-clock offset needs before
        // the decoder uses it on this thread. First sample seeds directly,
        // then an EMA absorbs postMessage jitter.
        const delta = nowUs - (m.workerNowUs as number);
        if (m.offsetUs === null || m.offsetUs === undefined) {
          this.offsetPageUs = null;
        } else {
          this.clockDeltaEmaUs = this.deltaSeeded
            ? 0.8 * this.clockDeltaEmaUs + 0.2 * delta
            : delta;
          this.deltaSeeded = true;
          this.offsetPageUs = (m.offsetUs as number) + this.clockDeltaEmaUs;
        }
        this.stale = { atPage: performance.now(), staleMs: m.staleMs as number };
        h.onRtt?.(m.rttMs as number);
        break;
      }
      case "wire":
        this.winKbps = m.kbps as number;
        (window as unknown as Record<string, unknown>).__inphaseWtWire = m.w;
        break;
      case "reload":
        pageReload();
        break;
      default:
        break;
    }
  }

  /** Host↔client clock offset (µs, capture-clock aligned) once synced,
   *  adjusted onto the PAGE clock for the decoder's glass ages. */
  clockOffsetUs(): number | null {
    return this.worker !== null ? this.offsetPageUs : (this.core?.clockOffsetUs() ?? null);
  }

  /** Upper bound on the offset error (ms). null while unsynced. */
  syncErrorMs(): number | null {
    return this.worker !== null
      ? this.offsetPageUs === null
        ? null
        : this.lastRtt / 2
      : (this.core?.syncErrorMs() ?? null);
  }

  async send(msg: Record<string, unknown>): Promise<void> {
    if (this.worker !== null) this.worker.postMessage({ t: "send", msg });
    else await this.core?.send(msg);
  }

  requestKeyframe(): void {
    void this.send({ type: "keyframe_request" });
  }

  async sendInput(bytes: Uint8Array): Promise<boolean> {
    if (this.worker !== null) {
      // Copy: the encoder owns its buffer and may reuse it.
      this.worker.postMessage({ t: "input", bytes: bytes.slice() });
      return true;
    }
    if (this.core === null) return false;
    return this.core.sendInput(bytes);
  }

  private killWorker(): void {
    this.worker?.terminate();
    this.worker = null;
  }

  close(): void {
    if (this.statsPusher !== 0) clearInterval(this.statsPusher);
    this.statsPusher = 0;
    if (this.worker !== null) {
      this.worker.postMessage({ t: "close" });
      // Give the worker a beat to close the QUIC connection cleanly.
      setTimeout(() => this.killWorker(), 250);
      this.core = null;
      return;
    }
    this.core?.close();
    this.core = null;
  }
}
