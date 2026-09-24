#!/usr/bin/env node
// Pure-Safari decode soak on the iPhone: no InPhase code, no network.
//
//   node streaming/decode-soak.mjs <width> <height> <fps> <seconds> <draw 0|1> <label>
//
// Injects a script into the (secure) player origin that encodes a moving test
// pattern with Safari's own VideoEncoder (HEVC, hardware), then loops those
// chunks through a VideoDecoder at <fps> for <seconds>, optionally drawing
// each frame to a full-size canvas. Polls progress over WebDriver; a session
// that stops answering means Safari killed the page.
//
// Isolates "the iPhone cannot sustain this decode" from "our player does
// something wrong at this rate": if this dies too, it is not our code.

const [W, H, FPS, SECS, DRAW, LABEL] = process.argv.slice(2);
const WD = "http://127.0.0.1:4444";
const wd = async (m, p, b) => {
  const r = await fetch(WD + p, {
    method: m,
    headers: { "content-type": "application/json" },
    body: b ? JSON.stringify(b) : undefined,
    signal: AbortSignal.timeout(20000),
  });
  const j = await r.json();
  if (j.value?.error) throw new Error(`${j.value.error}: ${j.value.message}`);
  return j.value;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const s = (await wd("POST", "/session", { capabilities: { alwaysMatch: { browserName: "safari", platformName: "iOS" } } })).sessionId;
const S = `/session/${s}`;
const ORIGIN = (process.env.INPHASE_ORIGIN ?? "").replace(/\/$/, "");
if (!ORIGIN) throw new Error("set INPHASE_ORIGIN=https://<host play URL> (any secure page on the host works)");
await wd("POST", `${S}/url`, { url: `${ORIGIN}/?soak` });

const setup = await wd("POST", `${S}/execute/async`, {
  args: [Number(W), Number(H), Number(FPS), Number(DRAW)],
  script: `
  const [W, H, FPS, DRAW, done] = arguments;
  (async () => {
    const codec = 'hvc1.1.6.L153.B0';
    const src = new OffscreenCanvas(W, H);
    const g = src.getContext('2d');
    const chunks = [];
    let decCfg = null;
    const enc = new VideoEncoder({ output: (c, meta) => { if (meta && meta.decoderConfig && !decCfg) decCfg = meta.decoderConfig; const b = new Uint8Array(c.byteLength); c.copyTo(b); chunks.push({ key: c.type === 'key', data: b }); }, error: (e) => { window.__soak = { error: 'enc ' + e }; } });
    const ecfg = { codec, width: W, height: H, bitrate: 40e6, framerate: FPS, latencyMode: 'realtime' };
    const es = await VideoEncoder.isConfigSupported(ecfg);
    if (!es.supported) { done({ error: 'encoder config unsupported' }); return; }
    enc.configure(ecfg);
    const N = FPS * 2; // two seconds of motion, looped
    for (let i = 0; i < N; i++) {
      g.fillStyle = 'hsl(' + (i * 7 % 360) + ',70%,40%)'; g.fillRect(0, 0, W, H);
      for (let k = 0; k < 40; k++) { g.fillStyle = 'hsl(' + ((i * 13 + k * 29) % 360) + ',90%,60%)'; g.fillRect((i * 17 + k * 97) % W, (i * 11 + k * 53) % H, 220, 160); }
      const vf = new VideoFrame(src, { timestamp: i * 1e6 / FPS });
      enc.encode(vf, { keyFrame: i === 0 });
      vf.close();
      if (enc.encodeQueueSize > 8) await new Promise((r) => setTimeout(r, 5));
    }
    await enc.flush(); enc.close();
    const view = document.createElement('canvas'); view.width = W; view.height = H;
    view.style.cssText = 'position:fixed;inset:0;width:100vw;height:100vh;z-index:99999;background:#000';
    document.body.appendChild(view);
    const vx = view.getContext('2d', { alpha: false, desynchronized: true });
    const st = { decoded: 0, errors: 0, queueMax: 0, chunks: chunks.length, bytes: chunks.reduce((a, c) => a + c.data.length, 0) };
    window.__soak = st;
    const dec = new VideoDecoder({ output: (vf) => { if (DRAW) vx.drawImage(vf, 0, 0, W, H); vf.close(); st.decoded++; }, error: (e) => { st.errors++; st.lastError = String(e); } });
    if (!decCfg) { done({ error: 'encoder gave no decoderConfig' }); return; }
    dec.configure({ ...decCfg, optimizeForLatency: true });
    st.decCodec = decCfg.codec;
    let i = 0, ts = 0;
    const period = 1000 / FPS;
    let next = performance.now();
    const pump = () => {
      const now = performance.now();
      while (next <= now) {
        const c = chunks[i % chunks.length];
        if (i > 0 && i % chunks.length === 0) { /* loop: restart at the keyframe */ }
        dec.decode(new EncodedVideoChunk({ type: (i % chunks.length === 0) ? 'key' : 'delta', timestamp: ts, data: c.data }));
        ts += 1e6 / FPS; i++; next += period;
        st.queueMax = Math.max(st.queueMax, dec.decodeQueueSize);
      }
      st.fed = i; st.queue = dec.decodeQueueSize;
      setTimeout(pump, 1);
    };
    pump();
    done({ ok: true, chunks: chunks.length, bytes: st.bytes });
  })().catch((e) => done({ error: String(e) }));`,
});
console.log(`[${LABEL}] setup`, JSON.stringify(setup));
if (setup.error) { await wd("DELETE", S).catch(() => {}); process.exit(2); }

const t0 = Date.now();
let last = 0;
let alive = true;
while (Date.now() - t0 < Number(SECS) * 1000) {
  await sleep(5000);
  try {
    const st = await wd("POST", `${S}/execute/sync`, { script: "return window.__soak;", args: [] });
    const secs = (Date.now() - t0) / 1000;
    console.log(`[${LABEL}] t=${secs.toFixed(0)}s decoded=${st.decoded} (+${((st.decoded - last) / 5).toFixed(0)}/s) fed=${st.fed} queue=${st.queue} queueMax=${st.queueMax} errors=${st.errors}${st.lastError ? " " + st.lastError : ""}`);
    last = st.decoded;
  } catch (e) {
    console.log(`[${LABEL}] PAGE GONE at t=${((Date.now() - t0) / 1000).toFixed(0)}s: ${String(e).slice(0, 120)}`);
    alive = false;
    break;
  }
}
console.log(`[${LABEL}] RESULT: ${alive ? "SURVIVED" : "PAGE KILLED"}`);
await wd("DELETE", S).catch(() => {});
