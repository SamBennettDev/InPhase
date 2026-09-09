// Aggregate host-side frametrace summary lines (inphase_host::frametrace).
// Line shape:
//   ...Z  INFO n=15 missing=0 enc_ms(p50/p95/max)=11.4/11.5/11.5 pay_ms=0.1/0.1/0.1
//         cap2send_ms=11.4/11.6/11.6 send_gap_ms(p50/p95/max)=16.6/17.6/17.7
import { readFileSync } from "node:fs";

const lines = readFileSync(process.argv[2], "utf8").split("\n").filter((l) => l.includes("missing="));
const tri = (s, key) => {
  const m = s.match(new RegExp(`${key}[^=]*=([\\d.]+)/([\\d.]+)/([\\d.]+)`));
  return m ? [+m[1], +m[2], +m[3]] : null;
};
const rows = [];
for (const l of lines) {
  const n = +(l.match(/\bn=(\d+)/)?.[1] ?? 0);
  const missing = +(l.match(/missing=(\d+)/)?.[1] ?? 0);
  const enc = tri(l, "enc_ms");
  const pay = tri(l, "pay_ms");
  const c2s = l.match(/cap2send_ms=([\d.]+)\/([\d.]+)\/([\d.]+)/);
  const gap = tri(l, "send_gap_ms");
  rows.push({ n, missing, enc, pay, c2s: c2s ? [+c2s[1], +c2s[2], +c2s[3]] : null, gap });
}
const q = (xs, p) => {
  const s = xs.filter((x) => isFinite(x)).sort((a, b) => a - b);
  return s.length ? +s[Math.min(s.length - 1, Math.floor(s.length * p))].toFixed(2) : null;
};
const col = (pick) => {
  const v = rows.map(pick).filter((x) => x != null && isFinite(x));
  return v.length ? { min: q(v, 0), p50: q(v, 0.5), p90: q(v, 0.9), max: q(v, 1) } : null;
};
const out = {
  windows: rows.length,
  frames_total: rows.reduce((a, r) => a + r.n, 0),
  missing_total: rows.reduce((a, r) => a + r.missing, 0),
  // host encode latency (capture->encode-submit), from the encoder-sink / payloader-sink probes
  encode_p50_ms: col((r) => r.enc?.[0]),
  encode_p95_ms: col((r) => r.enc?.[1]),
  encode_max_ms: col((r) => r.enc?.[2]),
  payloader_p95_ms: col((r) => r.pay?.[1]),
  // capture -> on the wire
  cap2send_p50_ms: col((r) => r.c2s?.[0]),
  cap2send_p95_ms: col((r) => r.c2s?.[1]),
  cap2send_max_ms: col((r) => r.c2s?.[2]),
  // host send cadence between marker packets — the host-side frame-pacing metric
  send_gap_p50_ms: col((r) => r.gap?.[0]),
  send_gap_p95_ms: col((r) => r.gap?.[1]),
  send_gap_max_ms: col((r) => r.gap?.[2]),
};
console.log(JSON.stringify(out, null, 2));
