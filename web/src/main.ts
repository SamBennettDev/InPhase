// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// InPhase web client entry (architecture report §17: vanilla TS, no framework).
//
// One bundle serves two views:
//   * the player page (§25.2) — the default;
//   * the host dashboard (§25.1) — shown when opened on the host itself, since
//     the live metrics come from the loopback-only admin API (§16).

import "./ui/app.css";
import { escapeHtml, safeHttpUrl } from "./ui/html.js";
import { gate } from "./ui/brand.js";

// A restored tab renders the OLD bundle from memory cache (no revalidation),
// which can be a wire-format mismatch with the host. Reload once; after a
// reload the entry type is "reload", so this cannot loop.
const nav = performance.getEntriesByType("navigation")[0] as
  | PerformanceNavigationTiming
  | undefined;
if (nav?.type === "back_forward") {
  location.reload();
}
import { guardStaleBundle } from "./buildid.js";
import { renderPlay } from "./ui/play.js";
import { renderDashboard } from "./ui/dashboard.js";
import { renderPairPage } from "./ui/pair.js";
import { certInstructionsHtml, detectPlatform } from "./ui/cert-help.js";

const root = document.getElementById("app");
if (!root) throw new Error("#app missing");

const params = new URLSearchParams(location.search);

// Reload if this bundle was not built alongside the host it is talking to. The
// back_forward reload above handles a restored tab; this handles the rest of
// the stale-page cases (a deploy under an open tab, a cached bundle) and does
// it on evidence rather than on a heuristic.
guardStaleBundle();

const isLocalhost =
  location.hostname === "localhost" ||
  location.hostname === "127.0.0.1" ||
  location.hostname === "[::1]";

if (location.protocol === "http:" && !isLocalhost) {
  // Reached over plaintext — no secure context, so no WebCrypto / pairing.
  // Point the user at the certificate + the https URL.
  renderCertSetup(root);
} else if (location.pathname === "/pair") {
  renderPairPage(root);
} else {
  const isHost = isLocalhost || params.has("dashboard");
  // `?play` forces the player view even on the host origin.
  if (isHost && !params.has("play")) renderDashboard(root);
  else renderPlay(root);
}

async function renderCertSetup(el: HTMLElement) {
  let httpsUrl = `https://${location.host}/`;
  try {
    const s = await fetch("/api/v1/status").then((r) => r.json());
    if (typeof s.play_url === "string") httpsUrl = safeHttpUrl(s.play_url);
  } catch {
    /* keep the guess */
  }
  el.innerHTML = gate(`
      <h1>Trust your gaming PC</h1>
      <p class="sub">One-time setup for this device: install your PC’s certificate so this browser can connect securely.</p>
      <div class="gate-card">
        ${certInstructionsHtml(detectPlatform(), httpsUrl)}
      </div>
      <div class="gate-actions"><button id="go">Continue to ${escapeHtml(new URL(httpsUrl).hostname)}</button></div>`);
  el.querySelector(".gate-col")?.classList.add("wide");
  el.querySelector("#go")!.addEventListener(
    "click",
    () => (location.href = httpsUrl),
  );
}
