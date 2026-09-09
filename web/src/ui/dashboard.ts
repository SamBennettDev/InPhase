// Host dashboard (architecture report §25.1). Shown when the page is opened on
// the host itself (localhost). Read-only status + PIN + live stream facts.
//
// The detailed metrics come from the loopback admin API (§16), which is only
// reachable from the host, so this page only works locally — by design.

import { certInstructionsHtml, type Platform } from "./cert-help.js";
import { brandLogo } from "./brand.js";

interface AdminStatus {
  state: string;
  peer: { browser: string; ip: string } | null;
  uptime_secs: number;
  stats: {
    host: Record<string, unknown>;
    transport: Record<string, unknown>;
    client: Record<string, unknown> | null;
    bottleneck: string | null;
    /** WebTransport video session live right now (ADR-0011). */
    wt_active?: boolean;
  };
}

interface HostStatus {
  pc_name: string;
  version: string;
  state: string;
  https: boolean;
  tls_mode: string;
  play_url: string;
  /** WebTransport video transport offered by the host (ADR-0011). */
  wt?: { port: number; cert_sha256: string } | null;
}

export function renderDashboard(root: HTMLElement) {
  root.innerHTML = `<div class="dash">
    ${brandLogo("brand-logo brand-logo--dash")}
    <div class="status-big" id="state">…</div>
    <div class="row" style="margin-bottom:1rem">
      <div class="card" style="flex:1;min-width:240px">
        <div class="k">Pairing PIN</div>
        <div class="pin" id="pin">— — —</div>
        <div class="k" id="pin-ttl"></div>
        <div class="k" id="paired"></div>
        <div class="row">
          <button class="secondary" id="rotate" style="width:auto">Rotate PIN</button>
          <button class="secondary" id="copy" style="width:auto">Copy play URL</button>
          <button class="secondary" id="unpair" style="width:auto">Un-pair all devices</button>
        </div>
      </div>
      <div class="card"><img class="qr" id="qr" alt="play URL QR" /></div>
    </div>
    <div class="grid" id="metrics"></div>
    <div class="card" style="margin-top:1rem">
      <div class="k" id="wt-line">video path —</div>
    </div>
    <div class="card" style="margin-top:1rem">
      <div class="k">Paired devices (controller ACL)</div>
      <div class="k mono" id="host-id" style="font-size:.7rem;color:var(--muted)"></div>
      <div id="controllers"></div>
      <div class="row" style="margin-top:.6rem">
        <button class="secondary" id="new-device" style="width:auto">Pair a new device</button>
      </div>
      <div id="invite"></div>
    </div>
    <div class="card" style="margin-top:1rem" id="devices-card">
      <div class="k">Play from another device</div>
      <div class="sub" id="devices-url" style="margin:.3rem 0"></div>
      <div id="devices-ca"></div>
    </div>
    <details>
      <summary>Advanced diagnostics</summary>
      <pre class="mono" id="raw" style="white-space:pre-wrap;font-size:.8rem"></pre>
    </details>
  </div>`;

  const $ = <T extends HTMLElement>(s: string) => root.querySelector<T>(s)!;

  let devicesPainted = false;
  const paintDevicesCard = (pub: HostStatus) => {
    if (devicesPainted) return;
    devicesPainted = true;
    const url = pub.play_url || `https://${location.hostname}/`;
    const caUrl = url.replace(/^https:/, "http:").replace(/\/$/, "") + "/ca.crt";
    $("#devices-url").innerHTML = `Open <a href="${url}"><b>${url}</b></a> in a browser on your LAN.`;
    if (pub.tls_mode !== "local-ca") {
      $("#devices-ca").innerHTML = `<div class="k" style="margin-top:.4rem">TLS: ${pub.tls_mode} — no per-device certificate needed.</div>`;
      return;
    }
    const plats: [Platform, string][] = [
      ["ios", "iPhone / iPad"],
      ["android", "Android"],
      ["macos", "Mac"],
      ["windows", "Windows"],
    ];
    $("#devices-ca").innerHTML = `
      <div class="k" style="margin:.5rem 0 .3rem">
        First time on a device: on that device, open <span class="mono">${caUrl}</span>
        and follow the steps for its OS.
      </div>
      <div class="row" id="plat-tabs" style="gap:.3rem;flex-wrap:wrap">
        ${plats
          .map(
            ([p, label]) =>
              `<button class="secondary" data-plat="${p}" style="width:auto;padding:.3rem .6rem;font-size:.85rem">${label}</button>`,
          )
          .join("")}
      </div>
      <div id="plat-steps" style="margin-top:.4rem"></div>`;
    const stepsBox = $("#plat-steps");
    const show = (p: Platform) => {
      stepsBox.innerHTML = certInstructionsHtml(p, url, caUrl);
      for (const b of root.querySelectorAll<HTMLElement>("#plat-tabs [data-plat]"))
        b.style.opacity = b.dataset["plat"] === p ? "1" : ".55";
    };
    for (const b of root.querySelectorAll<HTMLButtonElement>("#plat-tabs [data-plat]"))
      b.addEventListener("click", () => show(b.dataset["plat"] as Platform));
    show("ios");
  };

  $("#rotate").addEventListener("click", () => fetch("/api/v1/admin/rotate-pin", { method: "POST" }).catch(() => {}));
  $("#unpair").addEventListener("click", () => {
    if (confirm("Un-pair every device? They will each need the PIN again.")) {
      fetch("/api/v1/admin/revoke-all", { method: "POST" }).catch(() => {});
    }
  });
  $("#copy").addEventListener("click", async () => {
    const a = await fetch("/api/v1/admin/status").then((r) => (r.ok ? r.json() : null)).catch(() => null);
    const url = (a as AdminStatus & { play_url?: string })?.play_url ?? location.origin + "/";
    await navigator.clipboard.writeText(url).catch(() => {});
  });

  const tick = async () => {
    try {
      const [pub, admin] = await Promise.all([
        fetch("/api/v1/status").then((r) => r.json() as Promise<HostStatus>),
        fetch("/api/v1/admin/status").then((r) => (r.ok ? (r.json() as Promise<AdminStatus & { pin?: string; pin_ttl_secs?: number | null; paired_devices?: number }>) : null)),
      ]);
      $("#state").textContent = pub.state;
      const wtEl = $("#wt-line");
      if (wtEl) {
        wtEl.textContent = pub.wt
          ? `WebTransport video offered (UDP ${pub.wt.port}) — falls back to WebRTC`
          : "WebRTC video only (wt disabled)";
      }
      paintDevicesCard(pub);
      if (admin?.pin) $("#pin").textContent = admin.pin;
      $("#pin-ttl").textContent =
        admin?.pin_ttl_secs == null ? "no expiry" : `expires in ${admin.pin_ttl_secs}s`;
      $("#paired").textContent = `${admin?.paired_devices ?? 0} device(s) paired`;
      renderMetrics($("#metrics"), admin);
      $("#raw").textContent = JSON.stringify(admin ?? pub, null, 2);
    } catch {
      $("#state").textContent = "host unreachable";
    }
  };
  void tick();
  setInterval(tick, 1000);

  const renderControllers = async () => {
    const d = await fetch("/api/v1/admin/controllers").then((r) => (r.ok ? r.json() : null)).catch(() => null);
    const box = $("#controllers");
    if (!d) {
      box.textContent = "—";
      return;
    }
    $("#host-id").textContent = `this host: ${d.host_id}`;
    const list = (d.controllers as {
      short: string;
      name: string;
      revoked: boolean;
      last_seen_unix: number;
    }[]).filter((c) => !c.revoked);
    if (!list.length) {
      box.innerHTML = `<div class="k">no browser has registered a device key yet</div>`;
      return;
    }
    box.innerHTML = list
      .map(
        (c) =>
          `<div class="row" style="justify-content:space-between;padding:.35rem 0;border-top:1px solid var(--line)">
             <span>${escapeHtml(c.name)} <span class="mono" style="color:var(--muted);font-size:.75rem">${c.short}…</span></span>
             <button class="secondary" style="width:auto" data-revoke="${c.short}">Revoke</button>
           </div>`,
      )
      .join("");
    for (const b of box.querySelectorAll<HTMLButtonElement>("[data-revoke]")) {
      b.addEventListener("click", async () => {
        await fetch("/api/v1/admin/controllers/revoke", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ id: b.dataset["revoke"] }),
        }).catch(() => {});
        void renderControllers();
      });
    }
  };
  void renderControllers();
  setInterval(renderControllers, 5000);

  // ---- pair a new device (QR + short code) --------------------------
  const invite = $("#invite");
  let inviteTimer = 0;
  $("#new-device").addEventListener("click", async () => {
    window.clearInterval(inviteTimer);
    const d = await fetch("/api/v1/admin/pair-invite", { method: "POST" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    if (!d) {
      invite.textContent = "could not create an invitation";
      return;
    }
    const render = (approved: boolean, used: boolean, secsLeft: number) => {
      invite.innerHTML = `
        <div class="row" style="align-items:flex-start;gap:1rem;margin-top:.8rem">
          <div style="flex:0 0 auto">${d.qr_svg}</div>
          <div style="flex:1;min-width:180px">
            <div class="k">Scan on the new device, or open</div>
            <div class="mono" style="font-size:.72rem;word-break:break-all">${escapeHtml(d.url)}</div>
            <div class="k" style="margin-top:.5rem">…or type this code at <span class="mono">/pair</span></div>
            <div class="mono" style="font-size:1.4rem;letter-spacing:.18em">${escapeHtml(d.short_code)}</div>
            <div class="k" style="margin-top:.5rem">${
              used
                ? "a device paired"
                : secsLeft <= 0
                  ? "expired — click again for a new one"
                  : `expires in ${secsLeft}s`
            }</div>
            ${
              d.needs_approval && !approved && !used
                ? `<button class="secondary" id="approve" style="width:auto;margin-top:.5rem">Approve this device</button>`
                : d.needs_approval && approved && !used
                  ? `<div class="k">approved — waiting for the device</div>`
                  : ""
            }
          </div>
        </div>`;
      const ap = invite.querySelector("#approve");
      if (ap) {
        ap.addEventListener("click", () => {
          fetch("/api/v1/admin/pair-invite/approve", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ id: d.id }),
          }).catch(() => {});
        });
      }
    };
    render(false, false, Math.max(0, d.not_after_unix - Math.floor(Date.now() / 1000)));
    inviteTimer = window.setInterval(async () => {
      const list = await fetch("/api/v1/admin/pair-invites")
        .then((r) => (r.ok ? r.json() : null))
        .catch(() => null);
      const mine = list?.invites?.find((i: { id: string }) => i.id === d.id);
      const secsLeft = Math.max(0, d.not_after_unix - Math.floor(Date.now() / 1000));
      render(!!mine?.approved, !!mine?.used, secsLeft);
      if (!mine || mine.used || secsLeft <= 0) {
        window.clearInterval(inviteTimer);
        if (mine?.used) void renderControllers();
      }
    }, 1500);
  });
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

function renderMetrics(host: HTMLElement, admin: AdminStatus | null) {
  if (!admin) {
    host.innerHTML = `<div class="metric"><div class="k">note</div><div class="v">open on the host PC for live metrics</div></div>`;
    return;
  }
  const h = admin.stats.host as Record<string, number | string>;
  const t = admin.stats.transport as Record<string, number>;
  const c = (admin.stats.client ?? {}) as Record<string, number | string>;
  const cells: [string, unknown][] = [
    ["state", admin.state],
    ["client", admin.peer ? `${admin.peer.ip}` : "—"],
    // Which carrier is actually on screen: WT canvas over the WebRTC <video>
    // while a WT session is live; WebRTC is the fallback (ADR-0011).
    ["video path", admin.stats.wt_active ? "WebTransport" : "WebRTC"],
    ["codec", h["codec"] || "—"],
    ["encoder", h["encoder_backend"] || "—"],
    ["resolution", h["width"] ? `${h["width"]}×${h["height"]}` : "—"],
    ["target fps", h["target_fps"] ?? "—"],
    ["presented fps", c["presented_fps"] ?? "—"],
    ["raw queue", h["raw_queue_frames"] ?? "—"],
    ["encode p95", fmtMs(h["encode_ms_p95"])],
    ["decode p95", fmtMs(c["decode_time_ms_p95"])],
    ["bitrate", t["outbound_bitrate_kbps"] ? `${Math.round(t["outbound_bitrate_kbps"])} kbps` : "—"],
    ["rtt", fmtMs(t["rtt_ms"])],
    ["loss", t["packet_loss_pct"] != null ? `${t["packet_loss_pct"]}%` : "—"],
    ["jitter buf", fmtMs(c["jitter_buffer_delay_ms"])],
    ["audio buf", fmtMs(c["audio_jitter_buffer_ms"])],
    ["bottleneck", admin.stats.bottleneck ?? "—"],
    ["uptime", `${admin.uptime_secs}s`],
  ];
  host.innerHTML = cells
    .map(([k, v]) => `<div class="metric"><div class="k">${k}</div><div class="v">${v}</div></div>`)
    .join("");
}

function fmtMs(v: unknown): string {
  return typeof v === "number" && v > 0 ? `${v.toFixed(1)} ms` : "—";
}
