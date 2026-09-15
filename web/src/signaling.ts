// JSON signaling message types — mirror of `crates/protocol/src/signaling.rs`
// (architecture report §15). Tagged by `type`.
//
// The socket rides the host's own HTTPS (a real Tailscale / Let's Encrypt
// cert), so frames are plain JSON. The session cookie authenticates the upgrade
// and an Ed25519 device challenge/response binds the session to a paired
// controller key.

import { getControllerIdentity } from "./controller-key.js";
import { bs } from "./bytes.js";

export const SIGNALING_PROTOCOL_VERSION = 2;

export type VideoCodec = "h264" | "h265";
export type QualityPreset = "low_latency" | "balanced" | "quality" | "custom";

export type StreamTarget =
  | { type: "desktop" }
  | { type: "game"; id: string };

export interface RequestedMode {
  width: number;
  height: number;
  fps: number;
  preset: QualityPreset;
  codec_preference?: VideoCodec;
  max_bitrate_kbps?: number;
  stream_target?: StreamTarget;
}

export interface RtpCodecCapability {
  mime_type: string;
  clock_rate: number;
  channels?: number;
  sdp_fmtp_line?: string;
}

export interface DecodeHint {
  codec: VideoCodec;
  width: number;
  height: number;
  framerate: number;
  supported: boolean;
  smooth: boolean;
  power_efficient: boolean;
}

export interface ClientFeatures {
  secure_context: boolean;
  jitter_buffer_target: boolean;
  keyboard_lock: boolean;
  pointer_lock: boolean;
  pointer_lock_unadjusted_movement: boolean;
  request_video_frame_callback: boolean;
  gamepad: boolean;
}

export interface SessionConfig {
  codec: VideoCodec;
  width: number;
  height: number;
  fps: number;
  preset: QualityPreset;
  start_bitrate_kbps: number;
  max_bitrate_kbps: number;
  min_bitrate_kbps: number;
  jitter_buffer_target_ms: number;
  input: { keyboard: boolean; mouse: boolean; gamepad: boolean; backend: string };
  encoder_backend: string;
  /** ICE servers for candidate gathering. Empty on a strict-LAN host.
   *  Browser `stun:host:port` form; never TURN. */
  ice_servers?: string[];
}

export interface IceCandidateMsg {
  candidate: string;
  sdp_mid?: string | null;
  sdp_mline_index?: number | null;
}

export type SignalErrorCode =
  | "protocol_version" | "busy" | "no_hardware_encoder" | "no_common_codec"
  | "capture_unavailable" | "negotiation_failed" | "unauthorized" | "internal";

export interface ClientTelemetry {
  at_us: number;
  codec: string | null;
  frames_decoded: number;
  decoded_fps: number;
  frames_dropped: number;
  /** Omitted by older WT clients; the host must not treat a hole as 0. */
  presented_fps?: number;
  decode_time_ms_p50: number;
  decode_time_ms_p95: number;
  jitter_buffer_target_ms: number;
  jitter_buffer_delay_ms: number;
  /** Audio receiver's jitter-buffer delay per frame (ms, windowed). The
   *  browser syncs A/V playout by holding video back to audio's playout
   *  point, so while audio is on this is video's effective floor. */
  audio_jitter_buffer_ms: number;
  packets_lost: number;
  rtt_ms: number;
  inbound_bitrate_kbps: number;
  freeze_count: number;
  total_freeze_ms: number;
  /** Inbound audio-track verdict (§11). */
  audio_state?: "no-track" | "no-packets" | "no-samples" | "silent" | "signal";
  /** §13: WebCodecs Opus decode support on this browser (probe result). */
  audio_opus_supported?: boolean;
  audio_level?: number;
  /** WT wire counters (v3) — how many streams the client read to completion,
   *  how many wedged past the 2 s watchdog, how many audio datagrams seen. */
  frames_received?: number;
  streams_wedged?: number;
  datagrams_seen?: number;
  /** Capture → decode-complete latency percentiles (ms), via the pong clock
   *  anchor. Negative until the clock syncs. */
  lat_p50_ms?: number;
  lat_p95_ms?: number;
}

export type SignalMessage =
  | { type: "client_hello"; protocol_version: number; browser: string; requested_mode: RequestedMode }
  | {
      type: "client_capabilities";
      rtp_video_codecs: RtpCodecCapability[];
      rtp_audio_codecs: RtpCodecCapability[];
      decode_hints: DecodeHint[];
      features: ClientFeatures;
    }
  | { type: "session_config"; codec: VideoCodec; width: number; height: number; fps: number;
      preset: QualityPreset; start_bitrate_kbps: number; max_bitrate_kbps: number;
      min_bitrate_kbps: number; jitter_buffer_target_ms: number;
      input: SessionConfig["input"]; encoder_backend: string; ice_servers?: string[] }
  | { type: "auth_challenge"; nonce: string }
  | { type: "auth_response"; controller_id: string; signature: string }
  | { type: "offer"; sdp: string }
  | { type: "answer"; sdp: string }
  | { type: "ice"; candidate: string; sdp_mid?: string | null; sdp_mline_index?: number | null }
  | { type: "session_ready" }
  | { type: "error"; code: SignalErrorCode; message: string }
  | { type: "ping"; at_us: number }
  | { type: "pong"; at_us: number }
  | { type: "client_telemetry" } & Partial<ClientTelemetry>
  | { type: "wt_video_info"; token: string; port: number; cert_sha256: string }
  | { type: "wt_video_info_request" }
  | { type: "bye" };

function b64e(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
}
function b64d(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * Same-origin signaling socket over the host's HTTPS. Answers the host's
 * `auth_challenge` with an Ed25519 signature from the paired controller key
 * before `ready` resolves; after that, frames are plain JSON `SignalMessage`s.
 */
export class SignalSocket {
  private ws: WebSocket;
  readonly ready: Promise<void>;
  private authed = false;
  private closed = false;

  constructor(
    private readonly onMessage: (m: SignalMessage) => void,
    private readonly onClose: (ev: CloseEvent) => void,
  ) {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    this.ws = new WebSocket(`${proto}//${location.host}/api/v1/signal`);
    this.ws.addEventListener("close", (e) => {
      this.closed = true;
      this.onClose(e);
    });
    this.ready = new Promise<void>((resolve, reject) => {
      this.ws.addEventListener("error", () => reject(new Error("signal socket error")), {
        once: true,
      });
      this.ws.addEventListener("message", (e) => {
        void this.onFrame(e.data as string, resolve, reject);
      });
    });
  }

  private async onFrame(
    data: string,
    resolveReady: () => void,
    rejectReady: (e: Error) => void,
  ): Promise<void> {
    let m: SignalMessage;
    try {
      m = JSON.parse(data) as SignalMessage;
    } catch {
      return;
    }

    if (m.type === "auth_challenge") {
      try {
        const ctrl = await getControllerIdentity();
        if (!ctrl) throw new Error("this browser can't hold a device key — update your browser");
        const sig = await ctrl.sign(bs(b64d(m.nonce)));
        this.rawSend({
          type: "auth_response",
          controller_id: ctrl.publicKeyHex,
          signature: b64e(sig),
        });
        this.authed = true;
        resolveReady();
      } catch (e) {
        rejectReady(e instanceof Error ? e : new Error("auth failed"));
      }
      return;
    }

    if (m.type === "error" && !this.authed) {
      rejectReady(new Error(m.message));
      return;
    }
    this.onMessage(m);
  }

  private rawSend(m: SignalMessage): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(m));
  }

  send(m: SignalMessage): void {
    if (this.closed || !this.authed) return;
    this.rawSend(m);
  }

  close(): void {
    this.send({ type: "bye" });
    this.ws.close();
  }
}
