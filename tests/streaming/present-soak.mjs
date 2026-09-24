#!/usr/bin/env node
// Presentation soak on the iPhone: how many decoded frames reach the screen
// per route. No InPhase code, no network.
//
//   node streaming/present-soak.mjs <width> <height> <fps> <seconds> <route> <label>
//
// route:
//   canvas  2D canvas drawImage, metered by rAF (the player's route today)
//   webgl   WebGL2 texImage2D of the VideoFrame, metered by rAF
//   video   decode in a worker, VideoTrackGenerator -> <video> (Safari 18+);
//           Safari's compositor presents it, counted by requestVideoFrameCallback
//
// Encodes a moving test pattern with Safari's own VideoEncoder, then loops the
// chunks through a VideoDecoder at <fps>. Reports decoded/s, shown/s and the
// page's refresh rate (rAF callbacks/s, kept armed the whole run).
// Env: INPHASE_ORIGIN (any secure page on the host), SOAK_CODEC (default HEVC).
const [W, H, FPS, SECS, ROUTE, LABEL] = process.argv.slice(2);
const CODEC = process.env.SOAK_CODEC ?? "hvc1.1.6.L153.B0";
const WD = "http://127.0.0.1:4444";
const wd = async (m, p, b) => {
  const r = await fetch(WD + p, {
    method: m,
    headers: { "content-type": "application/json" },
    body: b ? JSON.stringify(b) : undefined,
    signal: AbortSignal.timeout(60000),
  });
  const j = await r.json();
  if (j.value?.error) throw new Error(`${j.value.error}: ${j.value.message}`);
  return j.value;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ORIGIN = (process.env.INPHASE_ORIGIN ?? "").replace(/\/$/, "");
if (!ORIGIN) throw new Error("set INPHASE_ORIGIN=https://<host play URL>");

const s = (await wd("POST", "/session", { capabilities: { alwaysMatch: { browserName: "safari", platformName: "iOS" } } })).sessionId;
const S = `/session/${s}`;
await wd("POST", `${S}/url`, { url: `${ORIGIN}/?present-soak` });

const setup = await wd("POST", `${S}/execute/async`, {
  args: [Number(W), Number(H), Number(FPS), ROUTE, CODEC],
  script: `
  const [W, H, FPS, ROUTE, CODEC, done] = arguments;
  (async () => {
    // Two seconds of motion from Safari's own encoder.
    const src = new OffscreenCanvas(W, H);
    const g = src.getContext('2d');
    const chunks = [];
    let decCfg = null;
    const enc = new VideoEncoder({
      output: (c, meta) => {
        if (meta && meta.decoderConfig && !decCfg) decCfg = meta.decoderConfig;
        const b = new Uint8Array(c.byteLength); c.copyTo(b); chunks.push(b);
      },
      error: () => {},
    });
    const ecfg = { codec: CODEC, width: W, height: H, bitrate: 40e6, framerate: FPS, latencyMode: 'realtime' };
    if (!(await VideoEncoder.isConfigSupported(ecfg)).supported) { done({ error: 'encoder config unsupported' }); return; }
    enc.configure(ecfg);
    for (let i = 0; i < FPS * 2; i++) {
      g.fillStyle = 'hsl(' + (i * 7 % 360) + ',70%,40%)'; g.fillRect(0, 0, W, H);
      for (let k = 0; k < 40; k++) { g.fillStyle = 'hsl(' + ((i * 13 + k * 29) % 360) + ',90%,60%)'; g.fillRect((i * 17 + k * 97) % W, (i * 11 + k * 53) % H, 220, 160); }
      const vf = new VideoFrame(src, { timestamp: i * 1e6 / FPS });
      enc.encode(vf, { keyFrame: i === 0 });
      vf.close();
      if (enc.encodeQueueSize > 8) await new Promise((r) => setTimeout(r, 5));
    }
    await enc.flush(); enc.close();
    if (!decCfg) { done({ error: 'encoder gave no decoderConfig' }); return; }
    const cfg = { ...decCfg, optimizeForLatency: true };
    if (cfg.description) cfg.description = new Uint8Array(cfg.description);

    const st = { route: ROUTE, decoded: 0, shown: 0, skipped: 0, rafs: 0, errors: 0 };
    window.__soak = st;
    const full = 'position:fixed;inset:0;width:100vw;height:100vh;z-index:99999;background:#000;object-fit:contain';

    // The page's refresh rate, independent of the route.
    const tick = () => { st.rafs++; requestAnimationFrame(tick); };
    requestAnimationFrame(tick);

    // Feeds the decoder at FPS in a loop over the clip.
    const feeder = (decode) => {
      let i = 0, next = performance.now();
      const pump = () => {
        const now = performance.now();
        while (next <= now) {
          decode(chunks[i % chunks.length], i % chunks.length === 0, i * 1e6 / FPS);
          i++; next += 1000 / FPS;
        }
        st.fed = i;
        setTimeout(pump, 1);
      };
      pump();
    };

    if (ROUTE === 'video') {
      // Decoder + VideoTrackGenerator live in a worker; the page only
      // hosts the <video>. Chunks go over once, then the worker loops them.
      const worker = new Worker(URL.createObjectURL(new Blob([\`
        onmessage = (e) => {
          const { chunks, cfg, FPS } = e.data;
          const gen = new VideoTrackGenerator();
          const writer = gen.writable.getWriter();
          const st = { decoded: 0, errors: 0, dropped: 0 };
          let writing = 0;
          const dec = new VideoDecoder({
            output: (vf) => {
              st.decoded++;
              // Never queue behind the sink: a frame the sink is not ready
              // for is dropped, like the canvas routes drop superseded frames.
              if (writing > 1) { vf.close(); st.dropped++; return; }
              writing++;
              writer.write(vf).then(() => writing--, () => { writing--; st.errors++; });
            },
            error: (err) => { st.errors++; st.lastError = String(err); },
          });
          dec.configure(cfg);
          let i = 0, next = performance.now();
          const pump = () => {
            const now = performance.now();
            while (next <= now) {
              dec.decode(new EncodedVideoChunk({ type: i % chunks.length === 0 ? 'key' : 'delta', timestamp: i * 1e6 / FPS, data: chunks[i % chunks.length] }));
              i++; next += 1000 / FPS;
            }
            setTimeout(pump, 1);
          };
          postMessage({ track: gen.track }, [gen.track]);
          pump();
          setInterval(() => postMessage({ st }), 500);
        };
      \`])));
      const v = document.createElement('video');
      v.muted = true; v.playsInline = true; v.autoplay = true;
      v.style.cssText = full;
      document.body.appendChild(v);
      worker.onmessage = (e) => {
        if (e.data.track) { v.srcObject = new MediaStream([e.data.track]); v.play().catch((err) => { st.playError = String(err); }); }
        if (e.data.st) { st.decoded = e.data.st.decoded; st.errors = e.data.st.errors; st.skipped = e.data.st.dropped; st.lastError = e.data.st.lastError; }
      };
      worker.onerror = (e) => { st.errors++; st.lastError = 'worker: ' + e.message; };
      let base = null;
      const onVf = (_now, meta) => {
        if (base === null) base = meta.presentedFrames;
        st.shown = meta.presentedFrames - base;
        v.requestVideoFrameCallback(onVf);
      };
      v.requestVideoFrameCallback(onVf);
      worker.postMessage({ chunks, cfg, FPS });
      done({ ok: true, chunks: chunks.length });
      return;
    }

    // Canvas routes: one draw per refresh, two banked (RefreshCoalescer).
    const view = document.createElement('canvas'); view.width = W; view.height = H;
    view.style.cssText = full;
    document.body.appendChild(view);
    let draw;
    if (ROUTE === 'webgl') {
      const gl = view.getContext('webgl2', { alpha: false, antialias: false, depth: false, preserveDrawingBuffer: false });
      const sh = (t, s) => { const o = gl.createShader(t); gl.shaderSource(o, s); gl.compileShader(o); return o; };
      const p = gl.createProgram();
      gl.attachShader(p, sh(gl.VERTEX_SHADER, '#version 300 es\\nout vec2 uv; void main(){ vec2 v = vec2(gl_VertexID & 1, gl_VertexID >> 1); uv = vec2(v.x, 1.0 - v.y); gl_Position = vec4(v * 2.0 - 1.0, 0, 1); }'));
      gl.attachShader(p, sh(gl.FRAGMENT_SHADER, '#version 300 es\\nprecision mediump float; in vec2 uv; uniform sampler2D t; out vec4 o; void main(){ o = texture(t, uv); }'));
      gl.linkProgram(p); gl.useProgram(p);
      const tex = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, tex);
      for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR], [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
      gl.viewport(0, 0, W, H);
      draw = (vf) => { gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, vf); gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4); };
    } else {
      const cx = view.getContext('2d', { alpha: false });
      draw = (vf) => cx.drawImage(vf, 0, 0, W, H);
    }
    let pending = null, armed = false, tokens = 2;
    const present = (vf) => { try { draw(vf); st.shown++; } catch (err) { st.errors++; st.lastError = String(err); } vf.close(); };
    const arm = () => { if (!armed) { armed = true; requestAnimationFrame(onRaf); } };
    const onRaf = () => {
      armed = false; tokens = Math.min(2, tokens + 1);
      if (pending) { present(pending); pending = null; tokens--; arm(); }
    };
    const dec = new VideoDecoder({
      output: (vf) => {
        st.decoded++;
        if (!pending && tokens > 0) { tokens--; present(vf); arm(); return; }
        if (pending) { pending.close(); st.skipped++; }
        pending = vf; arm();
      },
      error: (err) => { st.errors++; st.lastError = String(err); },
    });
    dec.configure(cfg);
    feeder((data, key, ts) => dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data })));
    done({ ok: true, chunks: chunks.length });
  })().catch((e) => done({ error: String(e) }));`,
});
console.log(`[${LABEL}] setup`, JSON.stringify(setup));
if (setup.error) { await wd("DELETE", S).catch(() => {}); process.exit(2); }

await sleep(2000); // let the route settle before the first window
const read = () => wd("POST", `${S}/execute/sync`, { script: "return window.__soak;", args: [] });
const t0 = Date.now();
let last = await read();
let lastT = Date.now();
const rates = [];
let alive = true;
while (Date.now() - t0 < Number(SECS) * 1000) {
  await sleep(5000);
  try {
    const st = await read();
    const dt = (Date.now() - lastT) / 1000;
    const r = (k) => +((st[k] - last[k]) / dt).toFixed(1);
    const row = { decoded: r("decoded"), shown: r("shown"), refresh: r("rafs"), skipped: r("skipped") };
    rates.push(row);
    console.log(`[${LABEL}] t=${((Date.now() - t0) / 1000).toFixed(0)}s decoded/s=${row.decoded} shown/s=${row.shown} refresh/s=${row.refresh} skipped/s=${row.skipped} errors=${st.errors}${st.lastError ? " " + st.lastError : ""}${st.playError ? " play: " + st.playError : ""}`);
    last = st; lastT = Date.now();
  } catch (e) {
    console.log(`[${LABEL}] PAGE GONE at t=${((Date.now() - t0) / 1000).toFixed(0)}s: ${String(e).slice(0, 120)}`);
    alive = false;
    break;
  }
}
const med = (k) => { const v = rates.map((x) => x[k]).sort((a, b) => a - b); return v[v.length >> 1]; };
console.log(`[${LABEL}] RESULT: ${alive ? "SURVIVED" : "PAGE KILLED"} ${ROUTE} ${W}x${H}@${FPS} decoded/s=${med("decoded")} shown/s=${med("shown")} refresh/s=${med("refresh")}`);
await wd("DELETE", S).catch(() => {});
