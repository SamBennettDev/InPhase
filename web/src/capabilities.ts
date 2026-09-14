// Browser capability probing (architecture report §7 steps 2–3, §10, §12, §24).
// The host uses these to decide what it can *promise*, not to gate the connection.

import type {
  ClientFeatures,
  DecodeHint,
  RtpCodecCapability,
  VideoCodec,
} from "./signaling.js";

export function detectFeatures(): ClientFeatures {
  const rtcReceiverProto = (
    window as unknown as { RTCRtpReceiver?: { prototype: object } }
  ).RTCRtpReceiver?.prototype;
  return {
    secure_context: window.isSecureContext === true,
    jitter_buffer_target:
      !!rtcReceiverProto && "jitterBufferTarget" in rtcReceiverProto,
    keyboard_lock:
      "keyboard" in navigator &&
      "lock" in (navigator as Navigator & { keyboard?: object }).keyboard!,
    pointer_lock: "requestPointerLock" in Element.prototype,
    pointer_lock_unadjusted_movement: true, // feature-tested for real at lock time
    request_video_frame_callback:
      "requestVideoFrameCallback" in HTMLVideoElement.prototype,
    gamepad: "getGamepads" in navigator,
  };
}

/** One playback endpoint on the host that InPhase can loopback-capture (§11). */
export interface HostAudioEndpoint {
  id: string;
  name: string;
  is_default: boolean;
  active: boolean;
}
export type AudioHealth =
  | "disabled"
  | "healthy"
  | "degraded"
  | "failed"
  | "restarting";
export interface HostCapabilities {
  audio: {
    enabled: boolean;
    capture: {
      device_id: string; // "" = follow the system default
      name: string;
      is_default: boolean;
      active: boolean;
      health: AudioHealth;
      signal_detected: boolean | null;
    };
    endpoints: HostAudioEndpoint[];
  };
}

/** Which browser media features are actually usable *here* (§10, §12, §24).
 *  HTTP origins get almost none of the secure-context ones. */
export interface MediaCapabilities {
  secureContext: boolean;
  mediaDevices: boolean;
  audioOutputEnumeration: boolean;
  audioOutputSelection: boolean;
  keyboardLock: boolean;
  pointerLock: boolean;
  fullscreen: boolean;
}
export function mediaCapabilities(): MediaCapabilities {
  const md = navigator.mediaDevices as
    | (MediaDevices & { selectAudioOutput?: unknown })
    | undefined;
  const mediaEl = HTMLMediaElement.prototype as HTMLMediaElement & {
    setSinkId?: unknown;
  };
  return {
    secureContext: window.isSecureContext === true,
    mediaDevices: !!md,
    audioOutputEnumeration: typeof md?.enumerateDevices === "function",
    // Output *selection* needs both the picker and setSinkId — much narrower
    // than enumeration, and still marked experimental in some browsers.
    audioOutputSelection:
      typeof md?.selectAudioOutput === "function" &&
      typeof mediaEl.setSinkId === "function",
    keyboardLock:
      "keyboard" in navigator &&
      typeof (navigator as Navigator & { keyboard?: { lock?: unknown } })
        .keyboard?.lock === "function",
    pointerLock: "requestPointerLock" in Element.prototype,
    fullscreen: "requestFullscreen" in Element.prototype,
  };
}

/** Authenticated — only returns once the browser is paired. */
export async function fetchHostCapabilities(): Promise<HostCapabilities | null> {
  try {
    const r = await fetch("/api/v1/capabilities");
    return r.ok ? ((await r.json()) as HostCapabilities) : null;
  } catch {
    return null;
  }
}

/** Pick which host playback endpoint InPhase records. Applies on next connect. */
export async function setHostAudioDevice(id: string | null): Promise<boolean> {
  try {
    const r = await fetch("/api/v1/audio/capture-device", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ device: id }),
    });
    return r.ok;
  } catch {
    return false;
  }
}

/**
 * Codecs this browser can actually decode for the requested mode.
 *
 * Derived from the decode hints alone. It used to intersect them with
 * `RTCRtpReceiver.getCapabilities("video")`, which asks what **WebRTC** can
 * receive - and there is no WebRTC video any more. Chrome carries no HEVC in
 * WebRTC, so `video/H265` was never in that list and the intersection was
 * always empty: every Chrome and Edge client was told "this browser can't
 * decode H.265" while `VideoDecoder` decoded it perfectly well.
 *
 * The mime strings stay in RTP shape because the host still gates the session
 * on `client_has("video/H265")`.
 */
export function usableVideoCodecs(
  modes: {
    codec: VideoCodec;
    width: number;
    height: number;
    framerate: number;
  }[],
  hints: Awaited<ReturnType<typeof decodeHints>>,
): RtpCodecCapability[] {
  const seen = new Set<string>();
  const out: RtpCodecCapability[] = [];
  for (const h of hints) {
    if (!h.supported) continue;
    // H.265 only (user directive): the advertised list has one entry.
    const mime = "video/H265";
    if (seen.has(mime)) continue;
    seen.add(mime);
    out.push({ mime_type: mime, clock_rate: 90_000 });
  }
  return out;
}

/**
 * Codec strings to try for a mode, cheapest first.
 *
 * The level in an HEVC codec string is a *ceiling*, and a decoder rejects a
 * config whose level it cannot meet - so probing one fixed level would
 * false-negative on a device that supports the stream but not that ceiling.
 * Any accepted candidate proves the browser can decode the codec at this size.
 */
function codecCandidates(codec: VideoCodec): string[] {
  return codec === "h265"
    ? [
        "hev1.1.6.L93.B0",
        "hev1.1.6.L123.B0",
        "hev1.1.6.L153.B0",
        "hev1.1.6.L186.B0",
      ]
    : ["avc1.42e01f", "avc1.4d401f", "avc1.640028"];
}

export function receiveAudioCodecs(): RtpCodecCapability[] {
  const caps = RTCRtpReceiver.getCapabilities?.("audio");
  if (!caps) return [];
  return caps.codecs
    .filter((c) => /audio\/(OPUS|PCMU|PCMA|G722)/i.test(c.mimeType))
    .map((c) => ({
      mime_type: c.mimeType,
      clock_rate: c.clockRate ?? 48000,
      channels: c.channels ?? 2,
      sdp_fmtp_line: c.sdpFmtpLine ?? undefined,
    }));
}

/**
 * Can this browser decode each mode?
 *
 * Asks **WebCodecs**, because `VideoDecoder` is what decodes the stream. The
 * old probe asked `mediaCapabilities.decodingInfo({type: "webrtc", ...})`,
 * left over from when video rode WebRTC. Chrome and Edge report no HEVC for
 * WebRTC (they have no HEVC RTP payload) while decoding it happily through
 * WebCodecs, so this gate refused to start a session on the very browsers the
 * error message told the user to switch to (2026-09-08).
 *
 * `smooth` / `power_efficient` are informational, so they come from
 * MediaCapabilities' *file* probe when it is available and default to false.
 */
export async function decodeHints(
  modes: {
    codec: VideoCodec;
    width: number;
    height: number;
    framerate: number;
  }[],
): Promise<DecodeHint[]> {
  const VD = (globalThis as { VideoDecoder?: typeof VideoDecoder })
    .VideoDecoder;
  const mc = (
    navigator as Navigator & { mediaCapabilities?: MediaCapabilities }
  ).mediaCapabilities;
  const out: DecodeHint[] = [];
  for (const m of modes) {
    let supported = false;
    let codecString: string | null = null;
    if (VD?.isConfigSupported) {
      for (const codec of codecCandidates(m.codec)) {
        try {
          const r = await VD.isConfigSupported({
            codec,
            codedWidth: m.width,
            codedHeight: m.height,
            optimizeForLatency: true,
          });
          if (r.supported) {
            supported = true;
            codecString = codec;
            break;
          }
        } catch {
          // A malformed-for-this-browser codec string: try the next one.
        }
      }
    }
    // Quality hints only - never authority over `supported`.
    let smooth = false;
    let powerEfficient = false;
    if (supported && codecString && mc?.decodingInfo) {
      try {
        const info = await mc.decodingInfo({
          type: "file",
          video: {
            contentType: `video/mp4; codecs="${codecString.replace(/^hev1/, "hvc1")}"`,
            width: m.width,
            height: m.height,
            bitrate: 30_000_000,
            framerate: m.framerate,
          },
        });
        smooth = info.smooth;
        powerEfficient = info.powerEfficient;
      } catch {
        /* hints are optional */
      }
    }
    out.push({ ...m, supported, smooth, power_efficient: powerEfficient });
  }
  return out;
}

/** §7 step 5: apply codec preference on the receive transceiver before answering. */
export function preferCodec(pc: RTCPeerConnection, codec: VideoCodec) {
  const wanted = codec === "h265" ? "video/H265" : "video/H264";
  const caps = RTCRtpReceiver.getCapabilities?.("video");
  if (!caps) return;
  const ordered = [
    ...caps.codecs.filter(
      (c) => c.mimeType.toLowerCase() === wanted.toLowerCase(),
    ),
    ...caps.codecs.filter(
      (c) => c.mimeType.toLowerCase() !== wanted.toLowerCase(),
    ),
  ];
  for (const t of pc.getTransceivers()) {
    if (!("setCodecPreferences" in t)) continue;
    const kind = t.receiver.track?.kind ?? t.sender.track?.kind;
    if (kind === "audio") continue;
    if (kind != null && kind !== "video") continue;
    try {
      t.setCodecPreferences(ordered);
    } catch {
      /* not fatal */
    }
  }
}
