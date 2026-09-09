// Drive a Chrome/Chromium InPhase client over the DevTools protocol, pair it,
// let it stream, and harvest the per-window FrameStats the client logs as
// `[InPhase frame] {…}` (see web/src/diag.ts). Writes <label>.raw.json +
// <label>.summary.json and prints a summary table.
//
//   node cdp-probe.mjs --port 9222 --origin http://127.0.0.1:47800 \
//     --pin 421372 --duration 180 --label pc-loopback-idle --out /path/dir
//
// Works against a local Chromium or, via `ssh -L 9222:127.0.0.1:9222`, against
// the gaming PC's Chrome.

import { WebSocket } from "ws";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`);
  return i > -1 ? process.argv[i + 1] : d;
};
const HOST = arg("host", "127.0.0.1");
const PORT = +arg("port", "9222");
const ORIGIN = arg("origin", "http://127.0.0.1:47800").replace(/\/$/, "");
const PIN = arg("pin", "");
const DURATION = +arg("duration", "180");
const LABEL = arg("label", "run");
const OUT = arg("out", ".");
const SETTINGS = arg("settings", ""); // e.g. '{"width":2560,"height":1440,"fps":60,"maxBitrateKbps":40000,"bufferMs":0,"preset":"low_latency"}'

const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function httpJSON(path) {
  const r = await fetch(`http://${HOST}:${PORT}${path}`);
  return r.json();
}

// ---- attach to a fresh page target ------------------------------------------
let msgId = 0;
function rpc(ws, method, params = {}, sessionId) {
  const id = ++msgId;
  const payload = { id, method, params };
  if (sessionId) payload.sessionId = sessionId;
  ws.send(JSON.stringify(payload));
  return new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error(`timeout ${method}`)), 30000);
    const h = (raw) => {
      const m = JSON.parse(raw);
      if (m.id !== id) return;
      clearTimeout(to);
      ws.off("message", h);
      m.error ? reject(new Error(`${method}: ${m.error.message}`)) : resolve(m.result);
    };
    ws.on("message", h);
  });
}

const version = await httpJSON("/json/version");
log("browser:", version.Browser);
const browserWs = new WebSocket(version.webSocketDebuggerUrl, { maxPayload: 512 * 1024 * 1024 });
await new Promise((r, j) => { browserWs.once("open", r); browserWs.once("error", j); });

// Reuse a single dedicated page target across runs so tabs don't accumulate
// (stale InPhase tabs auto-reconnect and fight for the one player slot, ADR-007).
const list = await httpJSON("/json/list");
const existing = list.filter((t) => t.type === "page");
let targetId;
if (existing.length) {
  targetId = existing[0].id;
  log("reusing target", targetId.slice(0, 8), (existing[0].url || "").slice(0, 40));
  // park any other pages so only one client can be live
  for (const t of existing.slice(1)) {
    await rpc(browserWs, "Target.attachToTarget", { targetId: t.id, flatten: true })
      .then(({ sessionId }) => rpc(browserWs, "Page.navigate", { url: "about:blank" }, sessionId))
      .catch(() => {});
  }
} else {
  ({ targetId } = await rpc(browserWs, "Target.createTarget", { url: "about:blank" }));
  log("created target", targetId.slice(0, 8));
}
const { sessionId } = await rpc(browserWs, "Target.attachToTarget", { targetId, flatten: true });
log("session", sessionId.slice(0, 8));

const S = (method, params) => rpc(browserWs, method, params, sessionId);

const cleanup = async () => {
  try { await S("Page.navigate", { url: "about:blank" }); } catch {}
};
for (const sig of ["SIGINT", "SIGTERM"]) process.on(sig, async () => { await cleanup(); process.exit(1); });

await S("Page.enable", {});
await S("Runtime.enable", {});
await S("Log.enable", {});
await S("Network.enable", {});

// ---- collect console FrameStats --------------------------------------------
const windows = [];
const rawConsole = [];
browserWs.on("message", (raw) => {
  const m = JSON.parse(raw);
  if (m.sessionId !== sessionId) return;
  if (m.method === "Runtime.consoleAPICalled") {
    const a = m.params.args || [];
    const tag = a[0]?.value;
    if (tag === "[InPhase frame]" && typeof a[1]?.value === "string") {
      try {
        const s = JSON.parse(a[1].value);
        s._wall = Date.now();
        windows.push(s);
      } catch {}
    } else if (typeof tag === "string" && /InPhase|error|warn/i.test(tag)) {
      rawConsole.push({ t: Date.now(), type: m.params.type, text: a.map((x) => x.value ?? x.description ?? "").join(" ") });
    }
  } else if (m.method === "Log.entryAdded") {
    const e = m.params.entry;
    if (e.level === "error" || e.level === "warning") rawConsole.push({ t: Date.now(), type: `log:${e.level}`, text: e.text });
  }
});

// ---- navigate, pair, stream ------------------------------------------------
// `?play` forces the player view even on a loopback origin (main.ts otherwise
// serves the host dashboard to localhost).
const PLAY = `${ORIGIN}/?play`;
if (SETTINGS) {
  await S("Page.navigate", { url: PLAY });
  await sleep(1500);
  await S("Runtime.evaluate", { expression: `localStorage.setItem('inphase.settings', ${JSON.stringify(SETTINGS)})` });
}
await S("Page.navigate", { url: PLAY });
await sleep(2500);

if (PIN) {
  const pair = await S("Runtime.evaluate", {
    expression: `fetch('/api/v1/pair',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({pin:'${PIN}'})}).then(r=>r.status)`,
    awaitPromise: true,
  });
  log("pair status:", pair.result?.value);
}
await sleep(500);
await S("Page.navigate", { url: PLAY });
await S("Page.bringToFront", {}).catch(() => {});
log(`streaming for ${DURATION}s …`);

const startWin = windows.length;
const telem = [];
const deadline = Date.now() + DURATION * 1000;
let lastReport = 0;
while (Date.now() < deadline) {
  await sleep(2000);
  const ev = await S("Runtime.evaluate", {
    expression: `(()=>{const p=window.__inphaseFrameStats;const t=window.__inphaseTelemetry;const v=document.querySelector('video');return JSON.stringify({stats:p||null,telem:t||null,vw:v?.videoWidth||0,vh:v?.videoHeight||0,ct:v?.currentTime||0})})()`,
  }).catch(() => null);
  try {
    const p = JSON.parse(ev?.result?.value || "{}");
    if (p.telem) telem.push({ _wall: Date.now(), ...p.telem });
  } catch {}
  const now = Date.now();
  if (now - lastReport > 20000) {
    lastReport = now;
    const s = windows.at(-1);
    if (s) log(`  n=${windows.length} last: c2g=${s.captureToGlassMs?.p50 ?? "-"}ms jbuf=${s.jbufMs?.p50}ms int.p95=${s.intervalMs?.p95}ms jitter=${s.jitterMs}ms skipped=${s.skippedFrames}`);
    else if (ev) log(`  waiting for frames… ${ev.result?.value}`);
  }
}

const collected = windows.slice(startWin);
log(`done — ${collected.length} stat windows`);

// ---- aggregate ------------------------------------------------------------
const num = (xs) => xs.filter((x) => typeof x === "number" && isFinite(x));
const q = (xs, p) => {
  const s = num(xs).slice().sort((a, b) => a - b);
  return s.length ? +(s[Math.min(s.length - 1, Math.floor(s.length * p))]).toFixed(2) : null;
};
const agg = (path) => {
  const vals = collected.map((w) => path.split(".").reduce((o, k) => o?.[k], w));
  const v = num(vals);
  if (!v.length) return null;
  return { min: q(v, 0), p50: q(v, 0.5), p90: q(v, 0.9), max: q(v, 1), mean: +(v.reduce((a, b) => a + b) / v.length).toFixed(2), n: v.length };
};

const summary = {
  label: LABEL,
  origin: ORIGIN,
  browser: version.Browser,
  duration_s: DURATION,
  stat_windows: collected.length,
  frames_seen: collected.reduce((a, w) => a + (w.n || 0), 0),
  skipped_frames_total: collected.reduce((a, w) => a + (w.skippedFrames || 0), 0),
  display_hz: agg("displayHz")?.p50 ?? null,
  host_stamps: collected.some((w) => w.hostStamps),
  // frame pacing
  interval_p50_ms: agg("intervalMs.p50"),
  interval_p95_ms: agg("intervalMs.p95"),
  interval_max_ms: agg("intervalMs.max"),
  pacing_jitter_ms: agg("jitterMs"),
  // jitter buffer
  jbuf_p50_ms: agg("jbufMs.p50"),
  jbuf_p95_ms: agg("jbufMs.p95"),
  jbuf_max_ms: agg("jbufMs.max"),
  // decode / present
  decode_p50_ms: agg("decodeMs.p50"),
  present_p50_ms: agg("presentMs.p50"),
  // host-merged (need frame_stamps)
  encode_p50_ms: agg("encodeMs.p50"),
  encode_p95_ms: agg("encodeMs.p95"),
  net_p50_ms: agg("netMs.p50"),
  net_p95_ms: agg("netMs.p95"),
  capture_to_glass_p50_ms: agg("captureToGlassMs.p50"),
  capture_to_glass_p95_ms: agg("captureToGlassMs.p95"),
  capture_to_glass_max_ms: agg("captureToGlassMs.max"),
  outlier_windows: collected.filter((w) => (w.outliers?.length || 0) > 0).length,
};

// getStats-derived (from TelemetryReporter): the browser's own view
const tnum = (k) => {
  const v = telem.map((t) => t[k]).filter((x) => typeof x === "number" && isFinite(x));
  return v.length ? { min: q(v, 0), p50: q(v, 0.5), p90: q(v, 0.9), max: q(v, 1) } : null;
};
summary.getstats = {
  samples: telem.length,
  decoded_fps: tnum("decoded_fps"),
  presented_fps: tnum("presented_fps"),
  jitter_buffer_target_ms: tnum("jitter_buffer_target_ms"),
  jitter_buffer_delay_ms: tnum("jitter_buffer_delay_ms"),
  decode_time_ms_p50: tnum("decode_time_ms_p50"),
  rtt_ms: tnum("rtt_ms"),
  inbound_bitrate_kbps: tnum("inbound_bitrate_kbps"),
  packets_lost: tnum("packets_lost"),
  freeze_count: tnum("freeze_count"),
  total_freeze_ms: tnum("total_freeze_ms"),
};

writeFileSync(join(OUT, `${LABEL}.raw.json`), JSON.stringify({ summary, windows: collected, telemetry: telem, console: rawConsole }, null, 2));
writeFileSync(join(OUT, `${LABEL}.summary.json`), JSON.stringify(summary, null, 2));

const row = (k, v, ind = "") => {
  if (v && typeof v === "object" && "p50" in v) console.log(`  ${(ind + k).padEnd(28)} min ${String(v.min).padStart(8)}  p50 ${String(v.p50).padStart(8)}  p90 ${String(v.p90).padStart(8)}  max ${String(v.max).padStart(8)}`);
  else if (v && typeof v === "object") { console.log(`  ${(ind + k)}:`); for (const [k2, v2] of Object.entries(v)) row(k2, v2, ind + "  "); }
  else console.log(`  ${(ind + k).padEnd(28)} ${JSON.stringify(v)}`);
};
console.log("\n==== " + LABEL + " ====");
for (const [k, v] of Object.entries(summary)) row(k, v);

await cleanup();
browserWs.close();
process.exit(0);
