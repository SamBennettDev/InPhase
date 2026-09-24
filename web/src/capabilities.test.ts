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
  /** `mediaCapabilities.decodingInfo({type:"file"})` — the authority on HEVC. */
  fileSupported?: (codec: string) => boolean;
}

function install(s: FakeSupport): void {
  const g = globalThis as Record<string, unknown>;
  g.VideoDecoder = s.videoDecoder
    ? { isConfigSupported: async (c: { codec: string }) => ({ supported: s.videoDecoder!(c.codec) }) }
    : undefined;
  // `navigator` is a getter-only global in Node.
  Object.defineProperty(g, "navigator", {
    configurable: true,
    value: {
      mediaCapabilities: {
        decodingInfo: async (cfg: { type: string; video?: { contentType: string } }) => {
          if (cfg.type === "webrtc")
            return { supported: s.webrtcHevc ?? false, smooth: false, powerEfficient: false };
          const ok = s.fileSupported ? s.fileSupported(cfg.video?.contentType ?? "") : true;
          return { supported: ok, smooth: true, powerEfficient: true };
        },
      },
    },
  });
  g.RTCRtpReceiver = {
    getCapabilities: () => ({ codecs: (s.rtpReceiverCodecs ?? []).map((m) => ({ mimeType: m, clockRate: 90000 })) }),
  };
}

const MODES = [{ codec: "h265" as const, width: 1920, height: 1080, framerate: 60 }];

async function load() {
  // Fresh module each time: the probe reads the globals installed above.
  return (await import(`./capabilities.js?t=${Math.random()}`)) as typeof import("./capabilities.js");
}

test("Chrome decodes HEVC through WebCodecs even with no HEVC in WebRTC", async () => {
  install({
    videoDecoder: (c) => c.startsWith("hev1"),
    webrtcHevc: false, // Chrome: no HEVC RTP payload
    rtpReceiverCodecs: ["video/VP8", "video/VP9", "video/H264"], // and none in the receiver list
  });
  const { decodeHints, usableVideoCodecs } = await load();
  const hints = await decodeHints(MODES);
  assert.equal(hints[0]?.supported, true, "WebCodecs decodes it - the hint must say so");
  const codecs = usableVideoCodecs(MODES, hints);
  assert.ok(
    codecs.some((c) => /h265/i.test(c.mime_type)),
    "H.265 must be advertised; the host gates the session on it",
  );
});

test("a browser with no HEVC decoder falls back to H.264, it does not refuse", async () => {
  // Chrome on Linux: decodes H.264, has no HEVC at all. This used to advertise
  // an empty list — the client refused to start and blamed H.264, which it had
  // never probed and which decodes fine at 4K. H.264 is the floor (ADR-0005).
  install({ videoDecoder: (c) => c.startsWith("avc1"), rtpReceiverCodecs: ["video/H264"] });
  const { decodeHints, usableVideoCodecs } = await load();
  const BOTH = [
    { codec: "h265" as const, width: 1920, height: 1080, framerate: 60 },
    { codec: "h264" as const, width: 1920, height: 1080, framerate: 60 },
  ];
  const hints = await decodeHints(BOTH);
  assert.equal(hints[0]?.supported, false, "no HEVC decoder");
  assert.equal(hints[1]?.supported, true, "H.264 decodes");
  assert.deepEqual(
    usableVideoCodecs(BOTH, hints).map((c) => c.mime_type),
    ["video/H264"],
    "the H.264 floor must be advertised",
  );
});

test("HEVC is advertised ahead of H.264 when both decode", async () => {
  install({ videoDecoder: () => true });
  const { decodeHints, usableVideoCodecs } = await load();
  const BOTH = [
    { codec: "h265" as const, width: 1920, height: 1080, framerate: 60 },
    { codec: "h264" as const, width: 1920, height: 1080, framerate: 60 },
  ];
  assert.deepEqual(
    usableVideoCodecs(BOTH, await decodeHints(BOTH)).map((c) => c.mime_type),
    ["video/H265", "video/H264"],
    "preference order is the probe order",
  );
});

test("isConfigSupported alone is not enough - the file probe overrules it", async () => {
  // Measured on Chrome/Linux: HEVC answers `true` to isConfigSupported and then
  // throws "Unsupported configuration" at configure(), while
  // mediaCapabilities.decodingInfo({type:"file"}) answers `false` because the
  // browser really has no HEVC decoder. Trusting the optimistic answer is how a
  // session got an HEVC stream it could not decode, with no fallback (§30).
  install({ videoDecoder: () => true, fileSupported: (c) => !/hvc1|hev1/i.test(c) });
  const { decodeHints } = await load();
  const hints = await decodeHints([
    { codec: "h265" as const, width: 2560, height: 1440, framerate: 120 },
    { codec: "h264" as const, width: 2560, height: 1440, framerate: 120 },
  ]);
  assert.equal(hints[0]?.supported, false, "HEVC must not survive the file probe");
  assert.equal(hints[1]?.supported, true, "H.264 is the floor and still decodes");
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

/**
 * No WebTransport means no video at all (ADR-0011 final). The session used to
 * pair, claim the host and then sit on a blank stage forever, logging
 * "browser lacks WebTransport" to a console nobody reads.
 */
test("a browser without WebTransport is refused up front, by name", async () => {
  const { webTransportBlocker, playBlocker } = await load();
  assert.equal(webTransportBlocker({ webTransport: true, secureContext: true }), null);
  assert.match(
    webTransportBlocker({ webTransport: false, secureContext: true }) ?? "",
    /doesn't support WebTransport/,
  );
  assert.match(
    webTransportBlocker({ webTransport: false, secureContext: false }) ?? "",
    /secure \(https\)/,
    "on http the address is the fix, not the browser",
  );
  const g = globalThis as Record<string, unknown>;
  const had = { wt: g.WebTransport, sc: g.isSecureContext };
  try {
    g.isSecureContext = true;
    g.WebTransport = undefined; // Playwright WebKit on Linux
    assert.match(playBlocker() ?? "", /WebTransport/);
    g.WebTransport = class {};
    assert.equal(playBlocker(), null);
  } finally {
    g.WebTransport = had.wt;
    g.isSecureContext = had.sc;
  }
});
