// Report the browser's video-decode capabilities over CDP, so a run can prove
// it was hardware-decoding rather than silently falling back to software.
//
//   node gpucheck.mjs --port 9222
//
// Exits non-zero if hardware video decode is not available.

import { WebSocket } from "ws";

const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`);
  return i > -1 ? process.argv[i + 1] : d;
};
const HOST = arg("host", "127.0.0.1");
const PORT = +arg("port", "9222");
const REQUIRE = arg("require", "hw");

const version = await (await fetch(`http://${HOST}:${PORT}/json/version`)).json();
const ws = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((r, j) => { ws.once("open", r); ws.once("error", j); });

let id = 0;
const rpc = (method, params = {}) => {
  const myId = ++id;
  ws.send(JSON.stringify({ id: myId, method, params }));
  return new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error(`timeout ${method}`)), 20000);
    const h = (raw) => {
      const m = JSON.parse(raw);
      if (m.id !== myId) return;
      clearTimeout(to);
      ws.off("message", h);
      m.error ? reject(new Error(`${method}: ${m.error.message}`)) : resolve(m.result);
    };
    ws.on("message", h);
  });
};

const { gpu } = await rpc("SystemInfo.getInfo");
const dev = (gpu?.devices || [])[0] || {};
const feat = gpu?.featureStatus || {};
// `videoDecoding` is top-level on gpu, NOT per-device (Chrome 151).
const codecs = gpu?.videoDecoding || dev.videoDecoding || [];

const pick = (re) => codecs.find((c) => re.test(c.codec || ""));
const shape = (c) =>
  c ? { decode: !!c.decode, encode: !!c.encode, minRes: c.minResolution, maxRes: c.maxResolution } : null;

const deviceString = dev.deviceString || "";
const software = /llvmpipe|swiftshader|software/i.test(deviceString);

const report = {
  browser: version.Browser,
  glRenderer: dev.deviceString ?? null,
  glVendor: dev.driverVendor ?? null,
  softwareRendering: software,
  videoDecode: feat.video_decode ?? "unknown",
  videoEncode: feat.video_encode ?? "unknown",
  gpuCompositing: feat.gpu_compositing ?? "unknown",
  hardwareDecodeCodecs: codecs.filter((c) => c.decode).map((c) => c.codec),
  h264: shape(pick(/h264|avc/i)),
  hevc: shape(pick(/hevc|h265/i)),
};

console.log(JSON.stringify(report, null, 2));

ws.close();

const hw = report.videoDecode === "enabled" && !report.softwareRendering;
// `gpu.videoDecoding` is empty on some Chrome builds (including 151 as built by
// Playwright), so codec presence is advisory only — `decodecap.mjs` is what
// actually proves hardware decode, by measuring its CPU cost.
const codecOk = report.h264?.decode || report.hevc?.decode || report.hardwareDecodeCodecs.length > 0;
const ok = REQUIRE === "any" ? true : hw;

if (!ok && REQUIRE === "hw") {
  console.error(
    `\nFAIL: hardware video decode is not active ` +
      `(video_decode=${report.videoDecode}, softwareRendering=${report.softwareRendering})\n` +
      `  renderer: ${report.glRenderer}`
  );
  process.exit(1);
}
console.log(
  ok
    ? `\nOK: hardware video decode enabled${codecOk ? "" : " (codec list not exposed by this build)"}`
    : "\nNOTE: hardware video decode not active"
);
