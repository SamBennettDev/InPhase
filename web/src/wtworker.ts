// Dedicated Worker that owns the WebTransport connection: the datagram read
// loop, v4 reassembly, FEC repair and control-stream I/O all run here, off
// the main thread.
//
// Why: the browser's incoming-datagram queue has no flow control (RFC 9221)
// and silently drops from the HEAD when the app reads slower than the host
// injects. On the main thread the read loop contends with rendering, GC and
// decoder callbacks — measured drain 3.0-3.9k datagrams/s (05:38 Chrome),
// against 9.1k/s needed for 80 Mbps. In a worker the loop is the only thing
// on the thread.
//
// Complete frames go to the page zero-copy (transferable ArrayBuffers); the
// VideoDecoder, FrameOrderer and canvas stay on the main thread.

import {
  WtVideoClient,
  type WtClientHandlers,
  type WtClientStats,
} from "./wtcore.js";

interface Ctx {
  postMessage(m: unknown, transfer?: Transferable[]): void;
  onmessage: ((e: MessageEvent) => void) | null;
}
const ctx = self as unknown as Ctx;

let core: WtVideoClient | null = null;
let latestStats: WtClientStats | null = null;
const ZERO_STATS: WtClientStats = {
  codec: null,
  framesDecoded: 0,
  framesPresented: 0,
  framesDropped: 0,
  held: 0,
  queueSize: 0,
  behindEvents: 0,
  freezeCount: 0,
  totalFreezeMs: 0,
};

ctx.onmessage = (e: MessageEvent) => {
  const m = e.data as Record<string, unknown> & { t: string };
  if (m.t === "dial") {
    if (core !== null) return; // already dialed
    if (!("WebTransport" in globalThis)) {
      // Older Safari: WebTransport exists only in the page (or not at all).
      ctx.postMessage({ t: "unsupported" });
      return;
    }
    const c = new WtVideoClient();
    core = c;
    // Telemetry flag: the host raises this connection's injection pace.
    c.inWorker = true;
    const h: WtClientHandlers = {
      onVideoConfig: (cfg) => ctx.postMessage({ t: "video-config", cfg }),
      // Zero-copy handoff: the reassembly payload never leaves this thread
      // as a copy — its buffer transfers to the page.
      onFrame: (f) => ctx.postMessage({ t: "frame", f }, [f.payload.buffer]),
      onClosed: (why) => ctx.postMessage({ t: "closed", why }),
      onRtt: (rtt) =>
        ctx.postMessage({
          t: "rtt",
          rttMs: rtt,
          offsetUs: c.clockOffsetUs(),
          workerNowUs: Math.round(performance.now() * 1000),
          staleMs: c.staleMs(),
        }),
      onRouteWarning: (detail) => ctx.postMessage({ t: "route-warning", detail }),
      onAudio: (opus, ptsUs) => {
        const copy = opus.slice();
        ctx.postMessage({ t: "audio", opus: copy, ptsUs }, [copy.buffer]);
      },
      onControlMessage: (line) => ctx.postMessage({ t: "control-message", line }),
      onReload: () => ctx.postMessage({ t: "reload" }),
    };
    c.onWire = (w) => ctx.postMessage({ t: "wire", w, kbps: c.inboundKbps() });
    c.setStatsProvider(() => latestStats ?? ZERO_STATS);
    c.dial(m.info as Parameters<WtVideoClient["dial"]>[0], h).then(
      () => ctx.postMessage({ t: "dial-ok" }),
      (err) => ctx.postMessage({ t: "dial-err", msg: String(err) }),
    );
  } else if (m.t === "keyframe") {
    // The page asks for an IDR; the core decides how (it opens a spare video
    // channel first, so the answer arrives on a reliable stream).
    core?.requestKeyframe();
  } else if (m.t === "send") {
    void core?.send(m.msg as Record<string, unknown>);
  } else if (m.t === "input") {
    void core?.sendInput(m.bytes as Uint8Array);
  } else if (m.t === "stats") {
    // The page's decoder counters, pushed once a second. Telemetry is composed
    // right here so its `decoded_fps` delta and the window it is divided by
    // describe the same interval — see `WtVideoClient::telemetryNow`.
    latestStats = m.s as WtClientStats;
    core?.telemetryNow();
  } else if (m.t === "close") {
    core?.close();
    core = null;
  }
};
