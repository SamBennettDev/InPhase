import { test } from "node:test";
import assert from "node:assert/strict";

/**
 * The decode gate must ask the decoder that actually runs.
 *
 * Video does not ride WebRTC any more, but the gate still probed
 * `mediaCapabilities.decodingInfo({type: "webrtc"})` and intersected the
 * result with `RTCRtpReceiver.getCapabilities("video")`. Chrome and Edge
 * carry no HEVC RTP payload, so both said "no H.265" while `VideoDecoder`
 * decoded it fine - and the client refused to start with a message telling
 * the user to switch to the browser they were already using.
 */

interface FakeSupport {
  videoDecoder?: (codec: string) => boolean;
  webrtcHevc?: boolean;
  rtpReceiverCodecs?: string[];
}

function install(s: FakeSupport): void {
  const g = globalThis as Record<string, unknown>;
  g.VideoDecoder = s.videoDecoder
    ? {
        isConfigSupported: async (c: { codec: string }) => ({
          supported: s.videoDecoder!(c.codec),
        }),
      }
    : undefined;
  // `navigator` is a getter-only global in Node.
  Object.defineProperty(g, "navigator", {
    configurable: true,
    value: {
      mediaCapabilities: {
        decodingInfo: async (cfg: { type: string }) => {
          if (cfg.type === "webrtc")
            return {
              supported: s.webrtcHevc ?? false,
              smooth: false,
              powerEfficient: false,
            };
          return { supported: true, smooth: true, powerEfficient: true };
        },
      },
    },
  });
  g.RTCRtpReceiver = {
    getCapabilities: () => ({
      codecs: (s.rtpReceiverCodecs ?? []).map((m) => ({
        mimeType: m,
        clockRate: 90000,
      })),
    }),
  };
}

const MODES = [
  { codec: "h265" as const, width: 1920, height: 1080, framerate: 60 },
];

async function load() {
  // Fresh module each time: the probe reads the globals installed above.
  return (await import(
    `./capabilities.js?t=${Math.random()}`
  )) as typeof import("./capabilities.js");
}

test("Chrome decodes HEVC through WebCodecs even with no HEVC in WebRTC", async () => {
  install({
    videoDecoder: (c) => c.startsWith("hev1"),
    webrtcHevc: false, // Chrome: no HEVC RTP payload
    rtpReceiverCodecs: ["video/VP8", "video/VP9", "video/H264"], // and none in the receiver list
  });
  const { decodeHints, usableVideoCodecs } = await load();
  const hints = await decodeHints(MODES);
  assert.equal(
    hints[0]?.supported,
    true,
    "WebCodecs decodes it - the hint must say so",
  );
  const codecs = usableVideoCodecs(MODES, hints);
  assert.ok(
    codecs.some((c) => /h265/i.test(c.mime_type)),
    "H.265 must be advertised; the host gates the session on it",
  );
});

test("a browser with no HEVC decoder is reported unsupported", async () => {
  install({
    videoDecoder: (c) => c.startsWith("avc1"),
    rtpReceiverCodecs: ["video/H264"],
  });
  const { decodeHints, usableVideoCodecs } = await load();
  const hints = await decodeHints(MODES);
  assert.equal(hints[0]?.supported, false);
  assert.deepEqual(usableVideoCodecs(MODES, hints), [], "nothing to advertise");
});

test("no WebCodecs at all is unsupported, not optimistically true", async () => {
  // The old probe defaulted to supported:true when the API was missing. With
  // no WebRTC video to fall back to, that is a black screen, not a fallback.
  install({ rtpReceiverCodecs: ["video/H264", "video/H265"] });
  const { decodeHints } = await load();
  const hints = await decodeHints(MODES);
  assert.equal(hints[0]?.supported, false);
});

test("the level in the probe is a ceiling, so candidates are tried in turn", async () => {
  // A decoder that only accepts 5.1 must still be found supported.
  install({ videoDecoder: (c) => c === "hev1.1.6.L153.B0" });
  const { decodeHints } = await load();
  const hints = await decodeHints(MODES);
  assert.equal(hints[0]?.supported, true, "one accepted candidate is enough");
});
