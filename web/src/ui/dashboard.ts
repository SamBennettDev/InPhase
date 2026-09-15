import {
  certInstructionsHtml,
  detectPlatform,
  type Platform,
} from "./cert-help.js";
import { brandLogo } from "./brand.js";
import { icon } from "./icons.js";
import { escapeHtml as esc, safeHttpUrl } from "./html.js";

interface Admin {
  state: string;
  pin: string;
  pin_ttl_secs: number | null;
  peer: { browser: string; ip: string } | null;
  uptime_secs: number;
  stats: {
    host: Record<string, unknown>;
    transport: Record<string, unknown>;
    client: Record<string, unknown> | null;
    wt_active?: boolean;
  };
}
interface Status {
  pc_name: string;
  version: string;
  state: string;
  busy: boolean;
  available: boolean;
  https: boolean;
  tls_mode: string;
  play_url: string;
  remote_mapping?: unknown;
}
interface Controller {
  id: string;
  name: string;
  revoked: boolean;
  last_seen_unix: number;
}
interface Invite {
  id: string;
  short_code: string;
  qr_svg: string;
  not_after_unix: number;
  needs_approval: boolean;
}

async function api<T>(
  path: string,
  body?: Record<string, unknown>,
): Promise<T> {
  const r = await fetch("/api/v1/" + path, {
    cache: "no-store",
    signal: AbortSignal.timeout(7000),
    ...(body
      ? {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        }
      : {}),
  });
  if (!r.ok)
    throw Error(
      r.status === 403
        ? "Open the dashboard on your gaming PC using the InPhase tray icon."
        : "The host could not complete this action (" +
            r.status +
            "). Try again.",
    );
  return r.json() as Promise<T>;
}

export function renderDashboard(root: HTMLElement) {
  root.innerHTML = `
    <main class="dash app-shell">
      <header class="app-header">${brandLogo("brand-logo brand-logo--home")}<span class="header-divider"></span><span class="header-label">Host</span>
        <span class="connection-pill" id="connection" role="status"><span class="dot"></span>Connecting</span>
        <a class="icon-btn" href="https://github.com/SamBennettDev/InPhase#readme" target="_blank" rel="noreferrer" aria-label="Open InPhase help">${icon("help")}</a>
      </header>
      <div class="page-heading"><div><h1>Welcome to InPhase</h1><p class="sub">A little setup. A lot more freedom to play.</p></div><span class="quiet-tag">${icon("shield")} Only devices you pair</span></div>
      <p class="action-message" id="message" role="status" hidden></p>
      <section class="host-overview" aria-label="Host status"><div class="host-symbol">${icon("monitor")}</div><div class="host-overview-copy"><p class="eyebrow">HOST STATUS</p><h2 id="state">Checking your PC…</h2><p id="state-detail">Fetching the latest host status.</p></div><div class="host-overview-meta"><span class="quiet-tag" id="access">${icon("wifi")} Checking access</span><span id="version" class="muted"></span></div></section>
      <div class="dashboard-columns"><div class="dashboard-primary">
        <section class="surface" aria-labelledby="pairing-heading"><div class="section-heading"><span class="section-icon">${icon("link")}</span><div><h2 id="pairing-heading">Connect another device</h2><p>Open this address on the device you want to play from.</p></div></div>
          <div class="address-field"><span id="play-url" class="mono">Loading address…</span><button id="copy" class="icon-btn" aria-label="Copy play address" disabled>${icon("copy")}</button></div>
          <div class="pairing-row"><div><p class="eyebrow">YOUR PAIRING PIN</p><div class="pin" id="pin">••• •••</div><span class="muted" id="pin-ttl">Available only on this PC</span></div><button id="rotate" class="secondary compact" disabled>${icon("refresh")} New PIN</button></div>
          <div class="pairing-actions"><button id="new-device" class="compact" disabled>${icon("device")} Pair with a QR code</button><span class="muted">Pair once. Play whenever.</span></div><div id="invite" aria-live="polite"></div>
          <details class="setup-details"><summary>${icon("shield")} First time on this device? <span>Certificate setup</span>${icon("chevron")}</summary><div id="certificate-setup"></div></details>
        </section>
        <section class="surface" aria-labelledby="devices-heading"><div class="section-heading"><span class="section-icon">${icon("device")}</span><div><h2 id="devices-heading">Your devices <span id="device-count" class="count-badge">0</span></h2><p>You control who can connect.</p></div></div><div id="controllers"><p class="muted">Loading paired devices…</p></div>
          <details class="device-management"><summary>Manage all devices</summary><p class="muted">Unpairing removes access and ends the active stream.</p><button id="unpair" class="secondary compact danger" disabled>Unpair all devices</button></details>
        </section>
      </div><aside class="dashboard-secondary">
        <section class="surface"><div class="section-heading"><span class="section-icon">${icon("activity")}</span><div><h2>Stream status</h2><p id="session-detail">No active session</p></div></div><div class="metric-list" id="metrics"></div><div id="health" class="health-note" role="status"></div><button id="disconnect" class="secondary compact danger" hidden>End stream</button></section>
        <section class="surface setup-guide"><p class="eyebrow">FROM HERE TO PLAY</p><ol><li><span>1</span><div><strong>Keep this PC awake</strong><p>InPhase stays in your system tray.</p></div></li><li><span>2</span><div><strong>Pair your browser</strong><p>Open the address above and enter your PIN.</p></div></li><li><span>3</span><div><strong>Choose a game</strong><p>Your library and desktop are ready on your device.</p></div></li></ol></section>
      </aside></div>
      <details class="diagnostics"><summary>${icon("activity")} Advanced diagnostics</summary><p class="muted">Live technical details. Pairing PINs are excluded.</p><pre id="raw"></pre></details>
      <footer class="page-footer"><span>Local first. Open source. Built for your PC.</span><a href="https://github.com/SamBennettDev/InPhase" target="_blank" rel="noreferrer">InPhase on GitHub ${icon("arrow")}</a></footer>
    </main>`;
  const $ = <T extends HTMLElement>(s: string) => root.querySelector<T>(s)!;
  const shell = root.firstElementChild;
  let playUrl = "",
    stopped = false,
    timer = 0,
    controllerTimer = 0,
    inviteTimer = 0,
    inviteGeneration = 0,
    setupKey = "",
    controllerKey = "";
  const pending = new Set<HTMLButtonElement>();
  const stop = () => {
    stopped = true;
    clearTimeout(timer);
    clearTimeout(controllerTimer);
    clearTimeout(inviteTimer);
  };
  const active = () => !stopped && root.firstElementChild === shell;
  window.addEventListener("pagehide", stop, { once: true });
  const message = (text: string, error = false) => {
    if (!active()) return;
    const el = $("#message");
    el.hidden = !text;
    el.textContent = text;
    el.className = "action-message" + (error ? " error" : "");
  };
  const action = (id: string, task: () => Promise<void>) => {
    const b = $<HTMLButtonElement>(id);
    b.addEventListener("click", async () => {
      if (b.disabled || pending.has(b)) return;
      pending.add(b);
      b.disabled = true;
      message("");
      try {
        await task();
      } catch (e) {
        message(
          e instanceof Error
            ? e.message
            : "Could not reach your PC. Try again.",
          true,
        );
      } finally {
        pending.delete(b);
        if (active()) b.disabled = false;
      }
    });
  };
  action("#copy", async () => {
    if (playUrl) {
      await navigator.clipboard.writeText(playUrl);
      message("Play address copied. Open it on your other device.");
    }
  });
  action("#rotate", async () => {
    const d = await api<{ pin: string }>("admin/rotate-pin", {});
    $("#pin").textContent = d.pin;
    message("New PIN ready. Your paired devices keep their access.");
  });
  action("#unpair", async () => {
    if (
      !confirm(
        "Unpair all devices and end the current stream? Each device will need to pair again.",
      )
    )
      return;
    await api("admin/revoke-all", {});
    await controllers();
    message("All devices have been unpaired.");
  });
  action("#disconnect", async () => {
    if (
      !confirm("End the active stream? The player will return to the library.")
    )
      return;
    await api("admin/disconnect", {});
    message("The stream has ended.");
  });

  const paintSetup = (pub: Status) => {
    const key = pub.play_url + ":" + pub.tls_mode;
    if (key === setupKey) return;
    setupKey = key;
    const box = $("#certificate-setup");
    if (pub.tls_mode !== "local-ca") {
      box.textContent = pub.https
        ? "This host already uses a trusted certificate. Open the play address to continue."
        : "HTTPS is unavailable. Restart InPhase and check diagnostics before pairing.";
      return;
    }
    const platforms: [Platform, string][] = [
      ["windows", "Windows"],
      ["macos", "macOS"],
      ["ios", "iPhone / iPad"],
      ["android", "Android"],
      ["other", "Other"],
    ];
    box.innerHTML =
      '<p class="muted">Install this PC’s certificate on your other device, then open the play address again. Only trust certificates from a PC you own.</p><div class="platform-tabs" role="group" aria-label="Device operating system">' +
      platforms
        .map(
          ([p, label]) =>
            '<button class="secondary compact" data-platform="' +
            p +
            '">' +
            label +
            "</button>",
        )
        .join("") +
      '</div><div id="platform-steps"></div>';
    const show = (platform: Platform) => {
      $("#platform-steps").innerHTML = certInstructionsHtml(
        platform,
        playUrl,
        new URL("/ca.crt", playUrl).href,
      );
      box
        .querySelectorAll<HTMLButtonElement>("[data-platform]")
        .forEach((b) =>
          b.setAttribute(
            "aria-pressed",
            String(b.dataset["platform"] === platform),
          ),
        );
    };
    box
      .querySelectorAll<HTMLButtonElement>("[data-platform]")
      .forEach((b) =>
        b.addEventListener("click", () =>
          show(b.dataset["platform"] as Platform),
        ),
      );
    show(detectPlatform());
  };
  const controllers = async () => {
    try {
      const d = await api<{ controllers: Controller[] }>("admin/controllers");
      if (!active()) return;
      const list = d.controllers.filter((c) => !c.revoked);
      $("#device-count").textContent = String(list.length);
      const key = JSON.stringify(list);
      if (key === controllerKey) return;
      controllerKey = key;
      $("#controllers").innerHTML = list.length
        ? list
            .map(
              (c) =>
                '<div class="device-row"><span class="device-symbol">' +
                icon("device") +
                "</span><div><strong>" +
                esc(c.name) +
                '</strong><span class="muted">' +
                (c.last_seen_unix
                  ? "Last connected " +
                    esc(new Date(c.last_seen_unix * 1000).toLocaleDateString())
                  : "Paired · ready to connect") +
                '</span></div><button class="link danger" data-revoke="' +
                esc(c.id) +
                '" aria-label="Unpair ' +
                esc(c.name) +
                '">Unpair</button></div>',
            )
            .join("")
        : '<div class="empty-devices">' +
          icon("device") +
          "<strong>Your next screen starts here.</strong><p>Pair a device above and it will appear in this list.</p></div>";
      $("#controllers")
        .querySelectorAll<HTMLButtonElement>("[data-revoke]")
        .forEach((b) =>
          b.addEventListener("click", async () => {
            if (
              b.disabled ||
              !confirm(
                "Remove this device’s access? Any active stream will end.",
              )
            )
              return;
            b.disabled = true;
            try {
              await api("admin/controllers/revoke", {
                id: b.dataset["revoke"],
              });
              await controllers();
              message("Device unpaired.");
            } catch (e) {
              message(
                e instanceof Error ? e.message : "Could not unpair device.",
                true,
              );
              b.disabled = false;
            }
          }),
        );
    } catch (e) {
      if (active() && !controllerKey)
        $("#controllers").textContent =
          e instanceof Error ? e.message : "Could not load devices.";
    }
  };
  const pollControllers = async () => {
    if (!active()) return stop();
    await controllers();
    if (active())
      controllerTimer = window.setTimeout(() => void pollControllers(), 5000);
  };
  void pollControllers();

  action("#new-device", async () => {
    clearTimeout(inviteTimer);
    const generation = ++inviteGeneration;
    const d = await api<Invite>("admin/pair-invite", {});
    if (!active() || generation !== inviteGeneration) return;
    $("#invite").innerHTML =
      '<div class="invite-card"><img class="qr" src="data:image/svg+xml,' +
      encodeURIComponent(d.qr_svg) +
      '" alt="Scan to pair this device" /><div><strong>Scan with your other device</strong><p class="muted">Or enter this code on the pairing page.</p><div class="invite-code">' +
      esc(d.short_code) +
      '</div><p id="invite-status" class="muted"></p><button id="approve" class="secondary compact" ' +
      (d.needs_approval ? "" : "hidden") +
      ">Approve this device</button></div></div>";
    action("#approve", async () => {
      await api("admin/pair-invite/approve", { id: d.id });
      $("#approve").hidden = true;
      message("Device approved. Finish pairing on your other device.");
    });
    const pollInvite = async () => {
      if (!active() || generation !== inviteGeneration) return;
      const seconds = Math.max(
        0,
        d.not_after_unix - Math.floor(Date.now() / 1000),
      );
      $("#invite-status").textContent = seconds
        ? "Code expires in " + seconds + "s"
        : "Code expired. Create a new QR code to try again.";
      if (!seconds) {
        $("#approve").hidden = true;
        return;
      }
      try {
        const data = await api<{
          invites: { id: string; used: boolean; approved: boolean }[];
        }>("admin/pair-invites");
        if (!active() || generation !== inviteGeneration) return;
        const mine = data.invites.find((i) => i.id === d.id);
        if (mine?.approved) $("#approve").hidden = true;
        if (mine?.used) {
          $("#invite-status").textContent =
            "Device paired. You’re ready to play.";
          $("#approve").hidden = true;
          await controllers();
          return;
        }
        if (!mine) {
          $("#invite-status").textContent =
            "Invitation ended. Create another QR code to continue.";
          $("#approve").hidden = true;
          return;
        }
      } catch {
        if (active())
          $("#invite-status").textContent = "Cannot reach the host. Retrying…";
      }
      if (active() && generation === inviteGeneration)
        inviteTimer = window.setTimeout(() => void pollInvite(), 1500);
    };
    void pollInvite();
  });

  const tick = async () => {
    if (!active()) return stop();
    try {
      const [pub, admin, health] = await Promise.all([
        api<Status>("status"),
        api<Admin>("admin/status"),
        api<{ ok: boolean; symptoms: { detail: string }[] }>("health"),
      ]);
      if (!active()) return;
      playUrl = safeHttpUrl(pub.play_url);
      $("#play-url").textContent = playUrl;
      for (const id of ["#copy", "#rotate", "#unpair", "#new-device"]) {
        const b = $<HTMLButtonElement>(id);
        if (!pending.has(b)) b.disabled = false;
      }
      $("#version").textContent = "InPhase " + pub.version;
      $("#pin").textContent = admin.pin;
      $("#pin-ttl").textContent =
        admin.pin_ttl_secs == null
          ? "Regenerate this PIN whenever you need to."
          : "Refreshes in " + admin.pin_ttl_secs + "s";
      $("#connection").className = "connection-pill ok";
      $("#connection").innerHTML = '<span class="dot"></span>Host online';
      $("#state").textContent = pub.busy
        ? "A game is in motion."
        : pub.https && pub.available
          ? "Ready to play."
          : !pub.https
            ? "Secure setup needs attention."
            : "Getting ready…";
      $("#state-detail").textContent = admin.peer
        ? "Streaming to " + admin.peer.browser
        : pub.https
          ? "Your PC is waiting for a paired device."
          : "Check diagnostics and restart InPhase to restore HTTPS.";
      $("#access").innerHTML =
        icon("wifi") +
        " " +
        (pub.remote_mapping != null
          ? "Remote access enabled"
          : "Local network only");
      $("#session-detail").textContent =
        admin.peer?.browser ?? "No active session";
      $("#disconnect").hidden = !pub.busy;
      renderMetrics($("#metrics"), admin, pub.busy);
      $("#health").textContent = health.ok
        ? pub.busy
          ? "Stream health looks good."
          : "Performance appears here when a stream starts."
        : health.symptoms.map((s) => s.detail).join(" ");
      $("#health").classList.toggle("warning", !health.ok);
      const { pin: _pin, ...diagnostics } = admin;
      $("#raw").textContent = JSON.stringify(diagnostics, null, 2);
      paintSetup(pub);
    } catch (e) {
      if (!active()) return;
      $("#connection").className = "connection-pill warn";
      $("#connection").innerHTML = '<span class="dot"></span>Connection lost';
      $("#state").textContent = "Let’s reconnect your PC.";
      $("#state-detail").textContent =
        e instanceof Error
          ? e.message
          : "Check the InPhase tray icon. Retrying automatically…";
      $("#pin").textContent = "••• •••";
      $("#pin-ttl").textContent = "Reconnect to see the current PIN.";
      for (const id of ["#copy", "#rotate", "#unpair", "#new-device"])
        $<HTMLButtonElement>(id).disabled = true;
    } finally {
      if (active()) timer = window.setTimeout(() => void tick(), 2000);
    }
  };
  void tick();
}
function renderMetrics(box: HTMLElement, admin: Admin, live: boolean) {
  const h = admin.stats.host,
    t = admin.stats.transport,
    c = admin.stats.client ?? {};
  const num = (v: unknown, unit: string, divisor = 1) =>
    typeof v === "number" && Number.isFinite(v)
      ? (v / divisor).toFixed(1) + " " + unit
      : "—";
  const cells = [
    ["Resolution", live && h["width"] ? h["width"] + " × " + h["height"] : "—"],
    ["Frame rate", live ? num(c["presented_fps"], "fps") : "—"],
    ["Bandwidth", live ? num(t["outbound_bitrate_kbps"], "Mbps", 1000) : "—"],
    ["Round-trip time", live ? num(t["rtt_ms"], "ms") : "—"],
    [
      "Video",
      live
        ? (h["codec"] || "Negotiating") +
          " · " +
          (admin.stats.wt_active ? "WebTransport" : "WebRTC")
        : "Waiting for a player",
    ],
  ];
  box.innerHTML = cells
    .map(
      ([k, v]) =>
        '<div class="metric-row"><span>' +
        esc(k) +
        "</span><strong>" +
        esc(v) +
        "</strong></div>",
    )
    .join("");
}
