// WebCodecs decode benchmark: the client's real decode ceiling, measured on the
// same API InPhase's WebTransport path uses (web/src/wtdecoder.ts).
//
//   node wccap.mjs --port 9555 --file /tmp/uhd120.264 --codec avc1.42C03C --expect 120
//
// Why WebCodecs rather than a <video> element: rVFC only fires when a compositor
// actually presents a frame, so under a headless/ozone surface it reports 0 fps
// no matter how fast the decoder is. VideoDecoder measures decode itself.
//
// The CPU figure is the hardware/software discriminator — this box does 4K120
// H.264 in ~0.20 s of user CPU via VA-API and ~3.11 s in software.

import { WebSocket } from "ws";
import { createServer } from "node:http";
import { createReadStream, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { cpus } from "node:os";

const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`);
  return i > -1 ? process.argv[i + 1] : d;
};
const HOST = arg("host", "127.0.0.1");
const PORT = +arg("port", "9222");
const FILE = arg("file", "");
const CODEC = arg("codec", "avc1.42C03C");
const EXPECT = +arg("expect", "120");
const MODE = arg("mode", "max"); // max | realtime

if (!FILE) { console.error("--file is required"); process.exit(2); }

const HZ = 100;
function chromePids() {
  const s = new Set();
  for (const d of readdirSync("/proc")) {
    if (!/^\d+$/.test(d)) continue;
    try { if (/chrome$/.test(realpathSync(`/proc/${d}/exe`))) s.add(+d); } catch {}
  }
  return s;
}
function cpuJiffies(pids) {
  let t = 0;
  for (const p of pids) {
    try {
      const st = readFileSync(`/proc/${p}/stat`, "utf8");
      const rest = st.slice(st.lastIndexOf(")") + 2).split(" ");
      t += (+rest[11] || 0) + (+rest[12] || 0);
    } catch {}
  }
  return t;
}

const PAGE = `<!doctype html><meta charset=utf-8><title>wc bench</title>
<body><script>
const START = (d, i) => d[i]===0&&d[i+1]===0&&d[i+2]===0&&d[i+3]===1 ? 4
                    : d[i]===0&&d[i+1]===0&&d[i+2]===1 ? 3 : 0;

window.runBench = async (url, codec, mode, expectFps) => {
  const buf = await (await fetch(url)).arrayBuffer();
  const d = new Uint8Array(buf);

  // Split Annex-B into access units on access-unit delimiters (NAL type 9).
  const marks = [];
  for (let i = 0; i < d.length - 4; ) {
    const l = START(d, i);
    if (l) { if ((d[i + l] & 0x1f) === 9) marks.push(i); i += l; } else i++;
  }
  if (marks.length < 2) return JSON.stringify({ error: 'no access units found' });
  const units = marks.map((s, k) => d.subarray(s, k + 1 < marks.length ? marks[k + 1] : d.length));
  // Anything before the first AUD is parameter sets (SPS/PPS); keep them on the
  // first unit or the decoder has no configuration.
  if (marks[0] > 0) units[0] = d.subarray(0, units[0].length + marks[0]);

  // An access unit is a keyframe when it carries an IDR (type 5) or a fresh SPS
  // (type 7). Reading a fixed offset is wrong — the AUD comes first.
  const isKey = (u) => {
    for (let i = 0; i < u.length - 4; ) {
      const l = START(u, i);
      if (l) { const t = u[i + l] & 0x1f; if (t === 5 || t === 7) return true; i += l; } else i++;
    }
    return false;
  };
  const nalTypes = (u) => {
    const out = [];
    for (let i = 0; i < u.length - 4; ) {
      const l = START(u, i);
      if (l) { out.push(u[i + l] & 0x1f); i += l; } else i++;
      if (out.length > 10) break;
    }
    return out;
  };

  let decoded = 0, errored = null, config = null;
  const dec = new VideoDecoder({
    output: (f) => { decoded++; f.close(); },
    error: (e) => { errored = errored || ('decoder: ' + e.message); },
  });

  const wanted = [
    { codec, codedWidth: 3840, codedHeight: 2160, hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true },
    { codec, codedWidth: 3840, codedHeight: 2160, optimizeForLatency: true },
  ];
  for (const w of wanted) {
    try {
      const s = await VideoDecoder.isConfigSupported(w);
      if (s.supported) { config = w; break; }
    } catch (e) {}
  }
  if (!config) return JSON.stringify({ error: 'no supported decoder config for ' + codec });

  try { dec.configure(config); } catch (e) { return JSON.stringify({ error: 'configure: ' + e.message }); }

  const t0 = performance.now();
  const budgetMs = mode === 'realtime' ? (units.length / expectFps) * 1000 : Infinity;
  for (const u of units) {
    if (errored) break;
    // Keep the queue shallow so we measure decode, not buffer depth.
    while (dec.decodeQueueSize > 6) await new Promise(r => setTimeout(r, 0));
    if (mode === 'realtime') {
      const target = t0 + (decoded / expectFps) * 1000;
      const now = performance.now();
      if (now < target) await new Promise(r => setTimeout(r, target - now));
    }
    try { dec.decode(new EncodedVideoChunk({ type: isKey(u) ? 'key' : 'delta', timestamp: decoded * (1e6 / expectFps), data: u })); }
    catch (e) { errored = errored || ('decode: ' + e.message); break; }
  }
  try { await dec.flush(); } catch (e) { errored = errored || ('flush: ' + e.message); }
  const elapsed = performance.now() - t0;
  try { dec.close(); } catch {}

  return JSON.stringify({
    accessUnits: units.length,
    decoded,
    errored,
    elapsedMs: +elapsed.toFixed(0),
    achievedFps: +(decoded / (elapsed / 1000)).toFixed(2),
    bandwidthMbps: +((d.length * 8) / (elapsed / 1000) / 1e6).toFixed(1),
    config,
    diag: {
      firstMarkOffset: marks[0],
      unit0Bytes: units[0].length,
      unit0NalTypes: nalTypes(units[0]),
      unit1NalTypes: units[1] ? nalTypes(units[1]) : null,
      keyUnits: units.slice(0, 12).map(isKey),
      decoderState: dec.state,
      bytesTotal: d.length,
    },
  });
};
</script></body>`;

const stat = statSync(FILE);
const srv = createServer((req, res) => {
  const u = req.url.split("?")[0];
  if (u === "/" || u === "/index.html") {
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    return res.end(PAGE);
  }
  if (u === "/clip.264") {
    const range = req.headers.range;
    if (range) {
      const m = /bytes=(\d+)-(\d*)/.exec(range);
      const start = +m[1], end = m[2] ? +m[2] : stat.size - 1;
      res.writeHead(206, { "content-type": "application/octet-stream",
        "content-range": `bytes ${start}-${end}/${stat.size}`, "accept-ranges": "bytes",
        "content-length": end - start + 1 });
      return createReadStream(FILE, { start, end }).pipe(res);
    }
    res.writeHead(200, { "content-type": "application/octet-stream", "content-length": stat.size, "accept-ranges": "bytes" });
    return createReadStream(FILE).pipe(res);
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => srv.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${srv.address().port}`;

const version = await (await fetch(`http://${HOST}:${PORT}/json/version`)).json();
const ws = new WebSocket(version.webSocketDebuggerUrl, { maxPayload: 256 * 1024 * 1024 });
await new Promise((r, j) => { ws.once("open", r); ws.once("error", j); });

let id = 0;
const rpc = (method, params = {}, sessionId, timeoutMs = 120000) => {
  const myId = ++id;
  const p = { id: myId, method, params };
  if (sessionId) p.sessionId = sessionId;
  ws.send(JSON.stringify(p));
  return new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error(`timeout ${method}`)), timeoutMs);
    const h = (raw) => {
      const m = JSON.parse(raw);
      if (m.id !== myId) return;
      clearTimeout(to); ws.off("message", h);
      m.error ? reject(new Error(`${method}: ${m.error.message}`)) : resolve(m.result);
    };
    ws.on("message", h);
  });
};

const { targetId } = await rpc("Target.createTarget", { url: base });
const { sessionId } = await rpc("Target.attachToTarget", { targetId, flatten: true });
const S = (m, p, t) => rpc(m, p, sessionId, t);
await S("Page.enable", {});
await S("Runtime.enable", {});
await new Promise((r) => setTimeout(r, 2500));

const pids = chromePids();
const c0 = cpuJiffies(pids);
const t0 = Date.now();
const r = await S("Runtime.evaluate", {
  expression: `window.runBench(${JSON.stringify(base + "/clip.264")}, ${JSON.stringify(CODEC)}, ${JSON.stringify(MODE)}, ${EXPECT})`,
  awaitPromise: true, returnByValue: true,
});
const wallMs = Date.now() - t0;
const c1 = cpuJiffies(chromePids());

const m = JSON.parse(r.result?.value || "{}");
if (m.error) { console.error("FAIL:", m.error); ws.close(); srv.close(); process.exit(1); }

const cpuCores = +(((c1 - c0) / HZ) * 1000 / wallMs).toFixed(2);
const cores = cpus().length;
const looksHardware = cpuCores < Math.max(1.0, cores * 0.45);

const verdict = {
  mode: MODE,
  codec: CODEC,
  accessUnits: m.accessUnits,
  decodedFrames: m.decoded,
  achievedFps: m.achievedFps,
  bandwidthMbps: m.bandwidthMbps,
  wallS: +(wallMs / 1000).toFixed(1),
  cpuCoresUsed: cpuCores,
  cpuCoresAvailable: cores,
  decodeKind: looksHardware ? "hardware" : "software",
  decoderConfig: m.config,
  errored: m.errored,
  diag: m.diag,
  targetFps: EXPECT,
  meetsTarget: MODE === "max" ? m.achievedFps >= EXPECT : impressionsOk(m, EXPECT),
};
function impressionsOk(m, expect) {
  return m.decoded >= Math.floor(m.accessUnits * 0.99) && !m.errored;
}

console.log("\n" + JSON.stringify(verdict, null, 2));

await rpc("Target.closeTarget", { targetId }).catch(() => {});
ws.close(); srv.close();
process.exit(verdict.errored ? 1 : 0);
