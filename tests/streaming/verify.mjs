// Assert that a stream actually held its target, using a cdp-probe summary.
// This is the part that decides pass/fail; cdp-probe only reports numbers.
//
//   node verify.mjs --summary out/run.summary.json --target 3840x2160@120 --bitrate 80000
//
// Exits 0 only when every check passes. Tolerances are explicit and printed, so
// a "pass" is auditable rather than a judgement call.

import { readFileSync, writeFileSync } from "node:fs";

const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`);
  return i > -1 ? process.argv[i + 1] : d;
};

const SUMMARY = arg("summary", "");
const TARGET = arg("target", "3840x2160@120");
const BITRATE_KBPS = +arg("bitrate", "80000");
// A stream that is within 2% of nominal bitrate is on target; encoders undershoot
// slightly on easy content and that is not a fault.
const BITRATE_TOL = +arg("bitrate-tol", "0.15");
const FPS_TOL = +arg("fps-tol", "0.02");
const MAX_DROPPED = +arg("max-dropped", "0");
const MAX_FREEZE = +arg("max-freeze", "0");

if (!SUMMARY) { console.error("--summary is required"); process.exit(2); }

const m = /^(\d+)x(\d+)@(\d+)$/.exec(TARGET);
if (!m) { console.error(`--target must look like 3840x2160@120, got '${TARGET}'`); process.exit(2); }
const wantW = +m[1], wantH = +m[2], wantFps = +m[3];

const s = JSON.parse(readFileSync(SUMMARY, "utf8"));
const g = s.getstats || {};

const num = (v) => (typeof v === "number" && isFinite(v) ? v : null);
const p50 = (o) => num(o?.p50);
const mx = (o) => num(o?.max);

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });

// --- geometry ---------------------------------------------------------------
check(
  "negotiated resolution",
  s.stream_width === wantW && s.stream_height === wantH,
  `wanted ${wantW}x${wantH}, stream was ${s.stream_width ?? "?"}x${s.stream_height ?? "?"}`
);
check(
  "resolution stable",
  !s.stream_dim_changes,
  `${s.stream_dim_changes ?? 0} mid-stream dimension change(s)`
);

// --- frame rate -------------------------------------------------------------
// Prefer the browser's own decoded-fps figure; fall back to frames counted from
// the per-window FrameStats when telemetry is absent.
const decoded = p50(g.decoded_fps);
const presented = p50(g.presented_fps);
const measured = decoded ?? presented ?? (s.stat_windows ? s.frames_seen / s.duration_s : null);
check(
  "frame rate",
  measured !== null && measured >= wantFps * (1 - FPS_TOL),
  `wanted ${wantFps} fps, measured ${measured ?? "?"} (tolerance -${(FPS_TOL * 100).toFixed(0)}%)`
);

// --- bitrate ----------------------------------------------------------------
const kbps = p50(g.inbound_bitrate_kbps);
check(
  "bitrate",
  kbps !== null && kbps >= BITRATE_KBPS * (1 - BITRATE_TOL) && kbps <= BITRATE_KBPS * (1 + BITRATE_TOL),
  `wanted ${BITRATE_KBPS} kbps +/-${(BITRATE_TOL * 100).toFixed(0)}%, measured ${kbps ?? "?"} kbps`
);

// --- continuity -------------------------------------------------------------
const dropped = s.skipped_frames_total ?? null;
check("no skipped frames", dropped !== null && dropped <= MAX_DROPPED, `skipped ${dropped ?? "?"}`);

const loss = mx(g.packets_lost);
check("no packet loss", loss !== null && loss <= 0, `max packets_lost ${loss ?? "?"}`);

const freezes = mx(g.freeze_count);
check("no freezes", freezes !== null && freezes <= MAX_FREEZE, `max freeze_count ${freezes ?? "?"}`);

check(
  "no frozen time",
  s.getstats && mx(g.total_freeze_ms) === 0,
  `max total_freeze_ms ${mx(g.total_freeze_ms) ?? "?"}`
);

// --- errors -----------------------------------------------------------------
const errs = (s.console || []).filter((c) => /error/i.test(c.type || ""));
check("no client errors", errs.length === 0, `${errs.length} error-level console/log entries`);

// --- sanity -----------------------------------------------------------------
check("frames were seen", (s.frames_seen ?? 0) > 0, `frames_seen=${s.frames_seen ?? 0}`);
check("run long enough", (s.duration_s ?? 0) >= 10, `duration_s=${s.duration_s ?? 0}`);

const failed = checks.filter((c) => !c.ok);
const report = {
  label: s.label,
  target: TARGET,
  bitrate_target_kbps: BITRATE_KBPS,
  passed: failed.length === 0,
  checks,
  measurements: {
    stream_width: s.stream_width,
    stream_height: s.stream_height,
    decoded_fps_p50: decoded,
    presented_fps_p50: presented,
    inbound_bitrate_kbps_p50: kbps,
    skipped_frames_total: dropped,
    packets_lost_max: loss,
    freeze_count_max: freezes,
    pacing_jitter_ms_p50: p50(s.pacing_jitter_ms),
    display_hz: s.display_hz,
  },
};

console.log(`\n=== verify: ${s.label ?? "(unlabelled run)"} ===`);
for (const c of checks) console.log(`  ${c.ok ? "PASS" : "FAIL"}  ${c.name.padEnd(22)} ${c.detail}`);
console.log(`\n${report.passed ? "PASS" : "FAIL"}: ${checks.length - failed.length}/${checks.length} checks`);

const out = SUMMARY.replace(/\.(summary\.)?json$/, ".verify.json");
writeFileSync(out, JSON.stringify(report, null, 2));
console.log(`wrote ${out}`);

process.exit(report.passed ? 0 : 1);
