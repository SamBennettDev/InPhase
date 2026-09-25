// The library's live desktop tile: a view-only stream of the PC's desktop.
//
// It is the real stream, not a separate capture: the same signaling socket
// (`?preview=1`), the same WebTransport video path and the same WebCodecs
// decoder as a player, at tile size (the host clamps a preview to 960x540 at
// 30 fps and 2.5 Mbps, desktop only). The host treats it as idle - any player
// displaces it, it displaces nobody, and it never carries input or sound.

import { SignalSocket, type SignalMessage, SIGNALING_PROTOCOL_VERSION } from "../signaling.js";
import { WtVideoClient, type WtVideoInfo } from "../wt.js";
import { WtDecoder } from "../wtdecoder.js";
import { decodeHints, detectFeatures, usableVideoCodecs } from "../capabilities.js";
import { sameDecoderConfig } from "../decoderpolicy.js";

const MODE = { width: 960, height: 540, fps: 30, maxBitrateKbps: 2500 };

export class DesktopPreview {
  private sock: SignalSocket | null = null;
  private client: WtVideoClient | null = null;
  private decoder: WtDecoder | null = null;
  private running = false;
  private redialTimer = 0;

  constructor(
    /** Where the decoder's canvas goes. */
    private readonly host: HTMLElement,
    /** A frame is on the canvas (first one after each start). */
    private readonly onPicture: () => void,
  ) {}

  get active(): boolean {
    return this.running;
  }

  /** The picture as an image URL for a thumbnail, or null before the first
   *  frame. The decoder draws with the 2D canvas, so it reads back. */
  snapshot(): string | null {
    const c = this.decoder?.root;
    return c && c.width > 0 ? c.toDataURL("image/jpeg", 0.7) : null;
  }

  async start(): Promise<void> {
    if (this.running || !("WebTransport" in globalThis)) return;
    this.running = true;
    const modes = (["h265", "h264"] as const).map((codec) => ({
      codec,
      width: MODE.width,
      height: MODE.height,
      framerate: MODE.fps,
    }));
    const hints = await decodeHints(modes);
    const videoCodecs = usableVideoCodecs(modes, hints);
    if (!this.running || videoCodecs.length === 0) {
      this.stop();
      return;
    }
    const sock = new SignalSocket(
      (m) => this.onSignal(m),
      () => this.stop(),
      "/api/v1/signal?preview=1",
    );
    this.sock = sock;
    try {
      await sock.ready;
    } catch {
      this.stop();
      return;
    }
    if (this.sock !== sock) return;
    sock.send({
      type: "client_hello",
      protocol_version: SIGNALING_PROTOCOL_VERSION,
      browser: navigator.userAgent,
      requested_mode: {
        width: MODE.width,
        height: MODE.height,
        fps: MODE.fps,
        preset: "low_latency",
        codec_preference: hints[0]?.supported ? "h265" : "h264",
        max_bitrate_kbps: MODE.maxBitrateKbps,
        stream_target: { type: "desktop" },
      },
    });
    sock.send({
      type: "client_capabilities",
      rtp_video_codecs: videoCodecs,
      rtp_audio_codecs: [],
      decode_hints: hints,
      features: detectFeatures(),
    });
  }

  /** End the preview: the host frees its session at once, so a player's
   *  claim right after this does not wait on it. */
  stop(): void {
    this.running = false;
    clearTimeout(this.redialTimer);
    this.redialTimer = 0;
    this.client?.close();
    this.client = null;
    this.decoder?.stop();
    this.decoder?.root.remove();
    this.decoder = null;
    const sock = this.sock;
    this.sock = null;
    try {
      sock?.close();
    } catch {
      /* already closed */
    }
  }

  private onSignal(m: SignalMessage): void {
    if (m.type === "wt_video_info") this.dial(m);
    // Busy (someone is playing), or anything else going wrong: the tile
    // falls back to its placeholder; the page restarts us when the PC is free.
    else if (m.type === "error") this.stop();
  }

  private dial(info: WtVideoInfo): void {
    if (!this.running) return;
    this.client?.close();
    this.decoder?.stop();
    this.decoder?.root.remove();
    const client = new WtVideoClient();
    const decoder = new WtDecoder(
      () => this.onPicture(),
      () => client.requestKeyframe(),
      () => client.clockOffsetUs(),
      () => this.stop(),
      undefined,
      { canvas2d: true },
    );
    this.client = client;
    this.decoder = decoder;
    this.host.append(decoder.root);
    client
      .dial(info, {
        onVideoConfig: (cfg) => {
          void VideoDecoder.isConfigSupported({
            codec: cfg.codec,
            codedWidth: cfg.width,
            codedHeight: cfg.height,
            description: cfg.description ?? undefined,
          })
            .then((probe) => {
              if (!probe.supported) throw new Error("unsupported");
              if (!sameDecoderConfig(decoder.currentConfig(), cfg)) decoder.configure(cfg);
              void client.send({ type: "config_ack", epoch: cfg.epoch });
            })
            .catch(() => client.close());
        },
        onFrame: (f) => decoder.onFrame(f),
        onRtt: (rtt) => decoder.setRttMs(rtt),
        onClosed: () => {
          if (this.client !== client || !this.running) return;
          // The token that dial used is spent; ask for a fresh one over the
          // still-open signaling socket (a certificate rotation lands here).
          this.redialTimer = window.setTimeout(() => {
            this.redialTimer = 0;
            if (this.running) this.sock?.send({ type: "wt_video_info_request" });
          }, 1000);
        },
      })
      .then(() => {
        // The host's rate controller reads the same telemetry a player sends.
        client.setStatsProvider(() => {
          const s = decoder.stats();
          return {
            codec: null,
            framesDecoded: s.framesDecoded,
            framesPresented: s.framesPresented,
            presentedFps: s.presentedFps,
            freezeCount: s.freezeCount,
            totalFreezeMs: s.totalFreezeMs,
            framesDropped: s.framesDropped,
            held: s.held,
            queueSize: s.queueSize,
            behindEvents: s.behindEvents,
          };
        });
      })
      .catch(() => this.stop());
  }
}
