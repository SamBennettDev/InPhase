#!/usr/bin/env node
// Drive the stream in Safari on a real iPhone (or Mac Safari) over WebDriver.
//
//   node streaming/ios.mjs --label iphone-1080p60 --duration 90 \
//     --settings '{"width":1920,"height":1080,"fps":60,"maxBitrateKbps":80000}'
//
// `safaridriver` runs on the Mac the iPhone is paired with (Settings > Apps >
// Safari > Advanced > Remote Automation on the phone; `sudo safaridriver
// --enable` once on the Mac); ios.sh tunnels its port here. The same per-second
// sampling and verdict as xbrowser.mjs, spoken over plain W3C WebDriver HTTP,
// because Playwright cannot drive iOS Safari.
//
// Every automation session is a fresh, isolated Safari - no pairing survives -
// so each run pairs with the host's PIN.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const args = process.argv.slice(2);
const arg = (k, d) => {
  const i = args.indexOf(`--${k}`);
  return i >= 0 ? args[i + 1] : d;
};
const WD = arg("webdriver", "http://127.0.0.1:4444").replace(/\/$/, "");
const PLATFORM = arg("platform", "iOS");
const ORIGIN = arg("origin", process.env.INPHASE_ORIGIN ?? "").replace(/\/$/, "");
if (!ORIGIN) throw new Error("pass --origin https://<host play URL> or set INPHASE_ORIGIN");
const ADMIN = arg("admin", "http://127.0.0.1:47811").replace(/\/$/, "");
const DURATION = Number(arg("duration", "90"));
const WARMUP = Number(arg("warmup", "10"));
const RAMP_S = Number(arg("ramp", "30"));
const LABEL = arg("label", "ios-run");
const OUT = arg("out", new URL("./out/", import.meta.url).pathname);
const settings = {
  width: 1920,
  height: 1080,
  fps: 60,
  maxBitrateKbps: 80000,
  preset: "low_latency",
  streamTarget: { type: "desktop" },
  showMetrics: true,
  ...JSON.parse(arg("settings", "{}")),
};

const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1).padStart(6)}s ${PLATFORM}]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function wd(method, path, body) {
  const r = await fetch(`${WD}${path}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(60000),
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok || j.value?.error) throw new Error(`${method} ${path}: ${j.value?.error ?? r.status} ${j.value?.message ?? ""}`);
  return j.value;
}
async function admin(path, init) {
  const r = await fetch(`${ADMIN}${path}`, { ...init, signal: AbortSignal.timeout(3000) });
  if (!r.ok) throw new Error(`${path}: HTTP ${r.status}`);
  return r.headers.get("content-type")?.includes("json") ? r.json() : r.text();
}

const status0 = await admin("/api/v1/admin/status");
if (status0.state !== "Available") {
  log(`host busy (${status0.state}) - disconnecting the previous player`);
  await admin("/api/v1/admin/disconnect", { method: "POST" }).catch(() => {});
  for (let i = 0; i < 20 && (await admin("/api/v1/admin/status")).state !== "Available"; i++) await sleep(500);
}
const PIN = arg("pin", status0.pin);

const created = await wd("POST", "/session", {
  capabilities: { alwaysMatch: { browserName: "safari", platformName: PLATFORM } },
});
const sid = created.sessionId;
const caps = created.capabilities;
const device = `${caps["safari:deviceName"] ?? caps.platformName} ${caps.browserName} ${caps.browserVersion}`;
log(`session on ${device}`);
const S = `/session/${sid}`;
const js = (script, ...a) => wd("POST", `${S}/execute/sync`, { script, args: a });
const find = async (css) => {
  const v = await wd("POST", `${S}/element`, { using: "css selector", value: css }).catch(() => null);
  return v ? Object.values(v)[0] : null;
};
const until = async (fn, ms, what) => {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn().catch(() => null);
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(300);
  }
};

mkdirSync(OUT, { recursive: true });
let result;
const errors = [];
try {
  await wd("POST", `${S}/url`, { url: `${ORIGIN}/?play` });
  await js("localStorage.setItem('inphase.settings', arguments[0]);", JSON.stringify(settings));
  await wd("POST", `${S}/url`, { url: `${ORIGIN}/?play` });

  const pin = await until(async () => (await find("#pin")) ?? ((await find("#connect")) ? "paired" : null), 20000, "the player");
  // Through the page's own DOM, not WebDriver's input commands: on iOS
  // Safari, element/value never reached the tel input and element/click
  // started a text selection on the button instead of tapping it.
  if (pin !== "paired") {
    log("pairing");
    await js(`const p = document.querySelector('#pin');
      p.value = arguments[0];
      p.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('#go').click();`, PIN);
  }
  await until(
    () => js("const b=document.querySelector('#connect');return b&&!b.disabled?true:null;"),
    30000,
    "Connect",
  );
  log("connect");
  // A real touch tap: iOS only starts audio from a trusted gesture, and a
  // script click is not one (the stream would run silent and lip sync could
  // not be measured). Falls back to the script click if the tap fails.
  const btn = await find("#connect");
  const tapped = btn
    ? await wd("POST", `${S}/actions`, {
        actions: [{
          type: "pointer", id: "finger", parameters: { pointerType: "touch" },
          actions: [
            { type: "pointerMove", duration: 0, origin: { "element-6066-11e4-a52e-4f735466cecf": btn }, x: 0, y: 0 },
            { type: "pointerDown", button: 0 },
            { type: "pause", duration: 60 },
            { type: "pointerUp", button: 0 },
          ],
        }],
      }).then(() => true, (e) => (log(`touch tap failed (${String(e).slice(0, 80)}) - script click`), false))
    : false;
  await sleep(1500);
  const started = await js("return !!window.__inphaseWt || !document.querySelector('#connect');").catch(() => false);
  if (!tapped || !started) await js("const b=document.querySelector('#connect'); if (b) b.click();");

  const timeline = [];
  let prev = null;
  const hist = [];
  const startAt = Date.now();
  let lastLog = 0;
  while (Date.now() - startAt < DURATION * 1000) {
    await sleep(1000);
    const now = Date.now();
    const c = await js(`
      const w = window.__inphaseWt, wire = window.__inphaseWtWire;
      const cv = document.querySelector('canvas.wt-video');
      const err = document.querySelector('.err, [role=alert]');
      return { audio: window.__inphaseAudio ? Object.assign({}, window.__inphaseAudio) : null,
        wt: w ? Object.assign({}, w) : null, wire: wire ? Object.assign({}, wire) : null,
        w: cv ? cv.width : 0, h: cv ? cv.height : 0, vis: document.visibilityState,
        uiError: err && err.textContent ? err.textContent.trim().slice(0, 200) : '' };`).catch((e) => ({ evalError: String(e) }));
    const h = await admin("/api/v1/admin/status").catch((e) => ({ adminError: String(e) }));
    // Rates over the last ~3 s of samples, not one: the page publishes its
    // counters on its own timer, so a 1 s read aliases against it - an
    // iPhone at a steady 60 fps read as 10 / 107 / 11 / 117 in alternate
    // seconds, and the dips failed runs whose stream never faltered.
    hist.push({ t: now, decoded: c.wt?.framesDecoded ?? 0, presented: c.wt?.framesPresented ?? 0 });
    if (hist.length > 4) hist.shift();
    const h0 = hist[0];
    const span = (now - h0.t) / 1000;
    const dt = prev ? (now - prev.t) / 1000 : 1;
    const decoded = c.wt?.framesDecoded ?? 0;
    const presented = c.wt?.framesPresented ?? 0;
    const received = c.wire?.framesReceived ?? 0;
    const row = {
      s: +((now - startAt) / 1000).toFixed(1),
      dec_fps: hist.length > 1 && span > 0 ? +(((c.wt?.framesDecoded ?? 0) - h0.decoded) / span).toFixed(1) : null,
      pres_fps: hist.length > 1 && span > 0 ? +(((c.wt?.framesPresented ?? 0) - h0.presented) / span).toFixed(1) : null,
      redial: prev ? received < prev.received : false,
      freeze_ms: c.wt?.totalFreezeMs ?? null,
      freezes: c.wt?.freezeCount ?? null,
      abandoned: c.wire?.framesAbandoned ?? null,
      v4_expired: c.wire?.v4Expired ?? null,
      stream_aborted: c.wire?.streamAborted ?? null,
      nacks: c.wire?.nacksSent ?? null,
      whole_nacks: c.wire?.wholeFrameNacks ?? null,
      decode_ms: c.wt?.decodeMs ?? null,
      e2e_ms: c.wt?.e2eMs ?? null,
      dims: c.w ? `${c.w}x${c.h}` : null,
      // A hidden page stops presenting by design: a dip with vis=hidden is
      // the phone (screen off, app switch), not the stream.
      vis: c.vis ?? null,
      render: c.wt?.renderMode ?? null,
      heard_ms: c.audio?.heardAgeMs ?? null,
      seen_ms: c.audio?.seenAgeMs ?? null,
      out_lat_ms: c.audio?.outputLatencyMs ?? null,
      ui_error: c.uiError || c.evalError || null,
      enc_kbps: h.stats?.host?.encoder_bitrate_kbps ?? null,
      codec: h.stats?.host?.codec ?? null,
      cli_kbps: h.stats?.client?.inbound_bitrate_kbps ?? null,
      cli_p95_ms: h.stats?.client?.lat_p95_ms ?? null,
    };
    if (row.ui_error && !errors.includes(row.ui_error)) errors.push(row.ui_error);
    timeline.push(row);
    prev = { t: now, decoded, presented, received };
    if (now - lastLog > 10000) {
      lastLog = now;
      log(`t=${row.s}s dec=${row.dec_fps}fps enc=${row.enc_kbps}kbps rx=${row.cli_kbps}kbps e2e=${row.e2e_ms}ms ${row.codec} ${row.dims} freezes=${row.freezes} aband=${row.abandoned}` +
        (row.ui_error ? ` UI:"${row.ui_error}"` : ""));
    }
  }

  const target = settings.fps;
  const maxKbps = settings.maxBitrateKbps;
  const steady = timeline.filter((r) => r.s > WARMUP && r.dec_fps !== null);
  const q = (xs, p) => {
    const v = xs.filter((x) => typeof x === "number" && isFinite(x)).sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.floor(v.length * p))] : null;
  };
  const stall = steady.filter((r) => r.dec_fps === 0).length;
  const low = steady.filter((r) => r.dec_fps < target * 0.8).length;
  const firstAtMark = timeline.find((r) => r.enc_kbps >= maxKbps * 0.9);
  const after = firstAtMark ? timeline.filter((r) => r.s >= firstAtMark.s) : [];
  const atMark = after.length ? after.filter((r) => r.enc_kbps >= maxKbps * 0.9).length / after.length : 0;
  const checks = [
    { name: "frames decoded", ok: steady.some((r) => r.dec_fps > 0), detail: `${steady.length} steady seconds` },
    { name: "no stalled second", ok: stall === 0, detail: `${stall} s with zero decoded frames` },
    { name: "no WT redial", ok: !timeline.some((r) => r.redial), detail: `${timeline.filter((r) => r.redial).length}` },
    { name: "no UI errors", ok: errors.length === 0, detail: errors.join(" | ") || "none" },
    { name: "decode rate held", ok: low <= steady.length * 0.05, detail: `p50 ${q(steady.map((r) => r.dec_fps), 0.5)} of ${target}; ${low}/${steady.length} s under 80 %` },
    { name: "encoder reached the set bitrate", ok: !!firstAtMark && firstAtMark.s <= RAMP_S + WARMUP, detail: firstAtMark ? `at t=${firstAtMark.s}s` : "never" },
    { name: "bitrate held at the mark", ok: atMark >= 0.9, detail: `${(atMark * 100).toFixed(0)} %` },
  ];
  result = {
    label: LABEL,
    device,
    settings,
    passed: checks.every((c) => c.ok),
    checks,
    measurements: {
      dec_fps_p50: q(steady.map((r) => r.dec_fps), 0.5),
      dec_fps_p5: q(steady.map((r) => r.dec_fps), 0.05),
      pres_fps_p50: q(steady.map((r) => r.pres_fps), 0.5),
      enc_kbps_p50: q(steady.map((r) => r.enc_kbps), 0.5),
      rx_kbps_p50: q(steady.map((r) => r.cli_kbps), 0.5),
      decode_ms_p50: q(steady.map((r) => r.decode_ms), 0.5),
      e2e_ms_p50: q(steady.map((r) => r.e2e_ms), 0.5),
      freeze_ms: steady.length > 1 ? (steady.at(-1).freeze_ms ?? 0) - (steady[0].freeze_ms ?? 0) : null,
      codec: timeline.at(-1)?.codec,
      dims: timeline.at(-1)?.dims,
    },
    timeline,
  };
} catch (e) {
  result = { label: LABEL, device, passed: false, fatal: String(e), errors };
} finally {
  await admin("/api/v1/admin/disconnect", { method: "POST" }).catch(() => {});
  await wd("DELETE", S).catch(() => {});
}
writeFileSync(join(OUT, `${LABEL}.json`), JSON.stringify(result, null, 2));
if (result.fatal) log(`FATAL: ${result.fatal}`);
for (const c of result.checks ?? []) log(`${c.ok ? "PASS" : "FAIL"}  ${c.name}: ${c.detail}`);
log(result.passed ? "RESULT: PASS" : "RESULT: FAIL", JSON.stringify(result.measurements ?? {}));
process.exit(result.passed ? 0 : 1);
