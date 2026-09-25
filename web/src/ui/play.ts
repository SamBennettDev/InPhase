// Player page (architecture report §25.2).
//
// Flow: [pair PIN] -> home (pick when to connect) -> capabilities -> negotiate
// -> play. Connect enters fullscreen immediately (the click is the browser
// gesture fullscreen + pointer lock need) and captures input. Ctrl+Shift+Q
// exits back to the home screen; it does not auto-reconnect. Leaving fullscreen
// any other way (Esc — unavoidable without Keyboard Lock, i.e. HTTPS) just
// pauses: a "click to resume" prompt, session still live.

import {
  SignalSocket,
  type SignalMessage,
  SIGNALING_PROTOCOL_VERSION,
} from "../signaling.js";
import { WtVideoClient, type WtVideoInfo } from "../wt.js";
import { WtDecoder } from "../wtdecoder.js";
import { WtRecovery, type RecoveryContext } from "../wtrecovery.js";
import { InputManager } from "../input/manager.js";
import { TouchController } from "../input/touch.js";
import { WtAudio } from "../wtaudio.js";
import { FrameProbe } from "../diag.js";
import { Hud } from "./hud.js";
import { icon } from "./icons.js";
import { escapeHtml } from "./html.js";
import {
  brandLogo,
  browserInputHint,
  gate,
  setSignal,
  signalLine,
} from "./brand.js";
import {
  fetchLibrary,
  mountLibraryGrid,
  posterFor,
  targetLabel,
  type LibraryItem,
} from "./library.js";
import {
  getControllerIdentity,
  forgetControllerIdentity,
  deviceLabel,
} from "../controller-key.js";
import { checkBuildId } from "../buildid.js";
import {
  adoptDisplayRate,
  loadSettings,
  saveSettings,
  type StreamSettings,
  type StreamTarget,
  RESOLUTIONS,
  FPS_CHOICES,
  BITRATE_CHOICES_KBPS,
  bitrateLabel,
  clampFps,
  clampBitrate,
} from "../settings.js";
import {
  detectFeatures,
  receiveAudioCodecs,
  decodeHints,
  usableVideoCodecs,
  fetchHostCapabilities,
  setHostAudioDevice,
  mediaCapabilities,
  playBlocker,
} from "../capabilities.js";
import { nextStepDown, sameDecoderConfig } from "../decoderpolicy.js";

const isTouch =
  matchMedia("(pointer: coarse)").matches || "ontouchstart" in window;

/** Drop `?play` from the address bar without navigating.
 *
 *  `?play` means "auto-connect on load". Any code path that re-renders the page
 *  after a session ends must clear it first, or the render immediately starts
 *  another session — which, when the session cannot succeed, is a reconnect
 *  loop that hammers the host continuously. */
function stripAutoPlayParam(): void {
  const url = new URL(location.href);
  if (!url.searchParams.has("play")) return;
  url.searchParams.delete("play");
  history.replaceState(null, "", url.toString());
}


/** Set once the page starts unloading: sockets it closes are not failures. */
let pageUnloading = false;
window.addEventListener("pagehide", () => {
  pageUnloading = true;
});

export async function renderPlay(root: HTMLElement) {
  root.innerHTML = gate(`<p class="sub">Connecting to your PC…</p>`, "div");
  let paired = false;
  try {
    const r = await fetch("/api/v1/session", {
      signal: AbortSignal.timeout(7000),
      cache: "no-store",
    });
    if (!r.ok && r.status !== 401) throw Error("Host unavailable");
    paired = r.ok;
  } catch {
    root.innerHTML = gate(`<div class="gate-state warn">${icon("wifiOff")}</div>
      <h1>Can’t reach your PC</h1>
      <p class="sub">Check that your gaming PC is awake, InPhase is running in its tray, and this device is on the same network.</p>
      <div class="gate-actions"><button id="retry">${icon("refresh")} Try again</button></div>`);
    root
      .querySelector("#retry")!
      .addEventListener("click", () => void renderPlay(root));
    return;
  }

  // `?play` — auto-connect in this tab, no home screen, no auto-fullscreen
  // (diagnostics / bookmark).
  if (new URLSearchParams(location.search).has("play")) {
    if (paired) new Session(root, loadSettings());
    else showPair(root);
    return;
  }

  if (paired) showHome(root);
  else showPair(root);
}

/** Landing page for a paired device: a searchable poster grid of "Whole
 *  desktop" + every installed game, a live status line, and a sticky Connect
 *  bar. Picking a card just remembers the choice; Connect enters the session. */
function showHome(root: HTMLElement) {
  root.innerHTML = `
    <main class="home app-shell">
      <header class="app-header">${brandLogo("brand-logo brand-logo--home")}<span class="header-divider"></span><span class="header-label">Remote play</span>
        <div class="header-actions"><span class="connection-pill" id="host-status" role="status"><span class="dot"></span>Checking PC</span>
        <button class="icon-btn" id="cfgbtn" aria-label="Stream settings" title="Stream settings">${icon("settings")}</button></div>
      </header>
      ${signalLine("signal")}
      <section aria-labelledby="games-heading" class="library-section">
        <div class="library-toolbar"><div class="section-title"><h2 id="games-heading">Library</h2><span id="game-count" class="count-badge">0</span></div>
          <div class="library-filters"><label class="search-field">${icon("search")}<input id="game-search" type="search" aria-label="Search games" placeholder="Search games" autocomplete="off" /></label>
          <select id="source-filter" aria-label="Game launcher"><option value="">All launchers</option></select></div></div>
        <div class="library" role="group" aria-label="Choose what to stream">
          <button type="button" class="desk-card" id="select-desktop" aria-pressed="false" aria-label="Select Whole desktop">
            <span class="lib-cover desk-cover" id="desktop-preview">${icon("monitor")}<img id="desktop-preview-img" alt="" /><span class="lib-badges" id="desk-badges"></span><span class="lib-check" aria-hidden="true">✓</span></span>
            <span class="lib-name">${icon("monitor", 15)} Whole desktop</span>
          </button>
          <div id="library"><p class="library-empty">Loading your library…</p></div>
        </div>
      </section>
      <footer class="page-footer"><span></span><button class="link" id="forget">${icon("logout")} Unpair this device</button></footer>
      <div class="launch-bar"><div class="launch-inner">
        <span class="launch-thumb" id="pickthumb-box"><img id="pickthumb" alt="" /></span>
        <div class="launch-target"><span id="picklabel">Whole desktop</span><span id="home-help" role="status"></span></div>
        <button type="button" class="stream-summary" id="stream-summary"></button>
        <button id="connect" disabled>Checking PC…</button>
      </div></div>
      <dialog class="settings-dialog" id="settings-dialog" aria-labelledby="settings-title"><div class="dialog-heading"><h2 id="settings-title">Stream settings</h2><button class="icon-btn" id="settings-close" aria-label="Close settings">${icon("close")}</button></div><p class="sub">Saved on this device. Applies to your next stream.</p><div id="panel"></div></dialog>
    </main>`;
  const $ = <T extends HTMLElement>(s: string) => root.querySelector<T>(s)!;
  const connect = $<HTMLButtonElement>("#connect");
  const search = $<HTMLInputElement>("#game-search");
  const source = $<HTMLSelectElement>("#source-filter");
  const dialog = $<HTMLDialogElement>("#settings-dialog");
  let settings = loadSettings(),
    items: LibraryItem[] = [],
    activeStream: StreamTarget | null = null;
  let available = false,
    stopped = false,
    poll = 0,
    libraryPoll = 0,
    previewPoll = 0;
  const stop = () => {
    stopped = true;
    clearTimeout(poll);
    clearTimeout(libraryPoll);
    clearInterval(previewPoll);
  };
  const updateSelection = () => {
    settings = loadSettings();
    $("#picklabel").textContent = targetLabel(settings.streamTarget, items);
    $("#stream-summary").textContent = settingsSummary(settings);
    if (available)
      connect.textContent =
        settings.streamTarget.type === "desktop"
          ? "Stream desktop"
          : "Play now";
    const desk = settings.streamTarget.type === "desktop";
    $("#select-desktop").classList.toggle("sel", desk);
    $("#select-desktop").setAttribute("aria-pressed", String(desk));
    // The launch bar shows what will start: the desktop's live preview, or
    // the game's cover.
    const target = settings.streamTarget;
    const item = desk
      ? null
      : items.find((i) => target.type === "game" && i.id === target.id);
    const thumb = $<HTMLImageElement>("#pickthumb");
    const shot = $<HTMLImageElement>("#desktop-preview-img");
    thumb.src = desk
      ? shot.src ||
        posterFor({ id: "desktop", name: "Whole desktop", kind: "desktop" })
      : posterFor(
          item ?? {
            id: "",
            name: targetLabel(target, items),
            kind: "game",
          },
        );
    $("#pickthumb-box").classList.toggle("wide", desk);
  };
  // A 120 Hz+ display defaults to 120 fps on first visit (half the per-frame
  // waits); the summary refreshes once the measurement lands.
  void adoptDisplayRate().then((changed) => {
    if (changed && !stopped) updateSelection();
  });
  const pick = (target: StreamTarget) => {
    saveSettings({ ...loadSettings(), streamTarget: target });
    updateSelection();
    render();
  };
  const render = () => {
    $("#desk-badges").innerHTML =
      activeStream?.type === "desktop"
        ? '<span class="lib-live">● Live</span>'
        : "";
    mountLibraryGrid(
      $("#library"),
      {
        items: items.filter(
          (i) =>
            i.kind === "game" && (!source.value || i.source === source.value),
        ),
        selected: settings.streamTarget,
        active: activeStream,
        query: search.value,
      },
      pick,
    );
  };
  updateSelection();
  $("#select-desktop").addEventListener("click", () =>
    pick({ type: "desktop" }),
  );
  search.addEventListener("input", render);
  source.addEventListener("change", render);
  const loadLibrary = async (tries = 0) => {
    const result = await fetchLibrary();
    if (stopped || !root.contains(connect)) return;
    items = result.items;
    const games = items.filter((i) => i.kind === "game");
    $("#game-count").textContent = String(games.length);
    const previous = source.value;
    source.innerHTML =
      '<option value="">All launchers</option>' +
      [...new Set(games.flatMap((i) => (i.source ? [i.source] : [])))]
        .sort()
        .map(
          (s) =>
            '<option value="' +
            escapeHtml(s) +
            '">' +
            escapeHtml(s) +
            "</option>",
        )
        .join("");
    source.value = previous;
    if (
      settings.streamTarget.type === "game" &&
      !items.some(
        (i) => i.id === (settings.streamTarget as { id: string }).id,
      ) &&
      !result.error
    )
      saveSettings({ ...loadSettings(), streamTarget: { type: "desktop" } });
    updateSelection();
    render();
    if (result.error) {
      $("#library").innerHTML =
        '<div class="library-empty">Your game library could not be loaded. You can still stream the desktop. <button class="link" id="retry-library">Try again</button></div>';
      $("#retry-library").addEventListener("click", () => void loadLibrary());
    }
    if (result.artPending && tries < 6)
      libraryPoll = window.setTimeout(() => void loadLibrary(tries + 1), 3000);
  };
  void loadLibrary();
  const wallpaper = $("#desktop-preview");
  const preview = $<HTMLImageElement>("#desktop-preview-img");
  const loadPreview = () => {
    if (stopped || !root.contains(connect)) return;
    const probe = new Image();
    probe.onload = () => {
      if (stopped || !root.contains(connect)) return;
      preview.src = probe.src;
      wallpaper.classList.add("has-shot");
      if (loadSettings().streamTarget.type === "desktop")
        $<HTMLImageElement>("#pickthumb").src = probe.src;
    };
    probe.src = `/api/v1/desktop-preview?t=${Date.now()}`;
  };
  loadPreview();
  previewPoll = window.setInterval(loadPreview, 2500);
  const openSettings = () => {
    const panel = $("#panel");
    if (!panel.dataset["built"]) {
      panel.dataset["built"] = "1";
      buildHomeSettings(panel, updateSelection);
    }
    dialog.showModal();
  };
  $("#cfgbtn").addEventListener("click", openSettings);
  $("#stream-summary").addEventListener("click", openSettings);
  $("#settings-close").addEventListener("click", () => dialog.close());
  connect.addEventListener("click", async () => {
    if (connect.disabled) return;
    connect.disabled = true;
    if ((await checkBuildId()) === "reloading") return;
    stop();
    root.innerHTML = "";
    new Session(root, loadSettings(), { immersive: !isTouch });
  });
  $("#forget").addEventListener("click", async () => {
    if (
      !confirm(
        "Unpair this browser? You will need the PIN on your gaming PC to connect again.",
      )
    )
      return;
    const button = $<HTMLButtonElement>("#forget");
    button.disabled = true;
    try {
      const ident = await getControllerIdentity();
      const q = ident ? "?controller=" + ident.publicKeyHex : "";
      const result = await fetch("/api/v1/logout" + q, {
        method: "POST",
        signal: AbortSignal.timeout(7000),
      });
      if (!result.ok)
        throw Error(
          "Could not unpair. Check that your PC is online and try again.",
        );
      await forgetControllerIdentity();
      stop();
      showPair(root);
    } catch (e) {
      $("#home-help").textContent =
        e instanceof Error ? e.message : "Could not unpair this device.";
      button.disabled = false;
    }
  });
  const refresh = async () => {
    if (stopped || !root.contains(connect)) return stop();
    try {
      const r = await fetch("/api/v1/status", {
        signal: AbortSignal.timeout(5000),
        cache: "no-store",
      });
      if (!r.ok) throw Error("offline");
      const st = (await r.json()) as {
        pc_name: string;
        busy: boolean;
        available?: boolean;
        active_stream?: StreamTarget | null;
      };
      if (stopped || !root.contains(connect)) return;
      const next = st.busy ? (st.active_stream ?? null) : null;
      if (JSON.stringify(next) !== JSON.stringify(activeStream)) {
        activeStream = next;
        render();
      }
      available = !st.busy && st.available !== false;
      connect.disabled = !available;
      $("#host-status").className =
        "connection-pill " + (available ? "ok" : st.busy ? "live" : "warn");
      $("#host-status").innerHTML =
        '<span class="dot"></span>' +
        escapeHtml(st.pc_name) +
        " · " +
        (available ? "Online" : st.busy ? "In use" : "Starting");
      setSignal($("#signal"), st.busy ? "live" : available ? "ready" : "idle");
      $("#home-help").textContent = st.busy
        ? "Another device is using this PC. End that session before connecting here."
        : "";
      if (!available)
        connect.textContent = st.busy ? "PC in use" : "PC starting…";
      updateSelection();
    } catch {
      if (stopped || !root.contains(connect)) return;
      available = false;
      connect.disabled = true;
      connect.textContent = "PC offline";
      $("#host-status").className = "connection-pill warn";
      $("#host-status").innerHTML = '<span class="dot"></span>Offline';
      setSignal($("#signal"), "down");
      $("#home-help").textContent =
        "Can’t reach your PC. Retrying…";
    } finally {
      if (!stopped) poll = window.setTimeout(() => void refresh(), 3000);
    }
  };
  window.addEventListener("pagehide", stop, { once: true });
  void refresh();
}

function settingsSummary(s: StreamSettings): string {
  const mbps = (s.maxBitrateKbps / 1000).toFixed(
    s.maxBitrateKbps % 1000 ? 1 : 0,
  );
  return `${s.height}p · ${s.fps} fps · ${mbps} Mbps`;
}

/** Compact pre-connect settings editor, written straight to localStorage
 *  (no live session to reconnect). */
function buildHomeSettings(panel: HTMLElement, onChange: () => void) {
  const s = loadSettings();
  panel.innerHTML = `
    <div class="field-group">
      <p class="eyebrow">Video</p>
      <div class="field-row">
        <label>Resolution
          <select data-s="res" aria-label="Resolution">${RESOLUTIONS.map(
            (r) =>
              `<option value="${r.height}"${r.height === s.height ? " selected" : ""}>${r.label}</option>`,
          ).join("")}</select>
        </label>
        <label>Frame rate
          <select data-s="fps" aria-label="Frame rate">${FPS_CHOICES.map(
            (f) =>
              `<option value="${f}"${f === s.fps ? " selected" : ""}>${f} fps</option>`,
          ).join("")}</select>
        </label>
      </div>
      <label>Max bitrate
        <select data-s="br" aria-label="Max bitrate">${BITRATE_CHOICES_KBPS.map(
          (b) =>
            `<option value="${b}"${b === s.maxBitrateKbps ? " selected" : ""}>${bitrateLabel(b)}</option>`,
        ).join("")}</select>
      </label>
      <p class="field-hint">Now: <span id="cfgsum">${settingsSummary(s)}</span>. Higher settings need a faster network and a device that can decode them.</p>
    </div>
    <div class="field-group">
      <p class="eyebrow">Audio</p>
      <div class="home-audio" id="audiocfg"></div>
    </div>`;
  const pick = (k: string) =>
    panel.querySelector<HTMLSelectElement>(`[data-s="${k}"]`)!;
  const save = () => {
    const height = Number(pick("res").value);
    const width =
      RESOLUTIONS.find((r) => r.height === height)?.width ??
      Math.round((height * 16) / 9);
    saveSettings({
      ...loadSettings(),
      width,
      height,
      fps: clampFps(Number(pick("fps").value)),
      maxBitrateKbps: clampBitrate(Number(pick("br").value)),
    });
    const sum = panel.querySelector("#cfgsum");
    if (sum) sum.textContent = settingsSummary(loadSettings());
    onChange();
  };
  for (const k of ["res", "fps", "br"])
    pick(k).addEventListener("change", save);
  void buildAudioSettings(panel.querySelector<HTMLElement>("#audiocfg")!);
}

/** Audio section of the settings panel: host capture source + client output.
 *  `connected` (a callback that reconnects) is passed only from the in-session
 *  HUD path — on the home screen there's nothing to reconnect. */
async function buildAudioSettings(host: HTMLElement, reconnect?: () => void) {
  const caps = await fetchHostCapabilities();
  const a = caps?.audio;
  if (!a?.enabled) {
    host.innerHTML = `<p class="sub">Audio is turned off on the PC.</p>`;
    return;
  }
  const cur = a.capture.device_id;
  const eps = a.endpoints ?? [];
  const def = eps.find((e) => e.is_default);
  const captureOpts =
    `<option value=""${cur === "" ? " selected" : ""}>System default${def ? ` — ${escapeHtml(def.name)}` : ""}</option>` +
    eps
      .filter((e) => !e.is_default)
      .map(
        (e) =>
          `<option value="${escapeHtml(e.id)}"${e.id === cur ? " selected" : ""}>${escapeHtml(e.name)}${
            e.active ? "" : " (inactive)"
          }</option>`,
      )
      .join("");

  host.innerHTML = `
    <label>Capture source (on the PC)
      <select data-a="capture">${captureOpts}</select>
    </label>
    <label>Output (this device)
      <select data-a="output" disabled><option>Browser / OS default</option></select>
    </label>
    <p class="sub" id="audionote"></p>`;

  const cap = host.querySelector<HTMLSelectElement>('[data-a="capture"]')!;
  const note = host.querySelector<HTMLElement>("#audionote")!;

  // Health / signal readout for the current source.
  const health = a.capture.health;
  if (health === "failed")
    note.textContent =
      "Audio branch failed on the host — reconnect to recover.";
  else if (health === "degraded")
    note.textContent = "Capture device error — audio may be silent.";
  else if (a.capture.signal_detected === false)
    note.textContent = `No audio detected from “${a.capture.name}”. Try another source.`;

  cap.addEventListener("change", async () => {
    const ok = await setHostAudioDevice(cap.value || null);
    if (!ok) {
      note.textContent = "Could not reach the host to save that.";
      return;
    }
    if (reconnect) {
      note.textContent = `Changed to “${cap.selectedOptions[0]!.textContent}”. `;
      const b = document.createElement("button");
      b.className = "secondary";
      b.type = "button";
      b.textContent = "Reconnect now";
      b.addEventListener("click", reconnect);
      note.append(b);
    } else {
      note.textContent = "Saved — applies on your next connection.";
    }
  });

  // ---- output device: needs selectAudioOutput + setSinkId (secure context) ---
  const mc = mediaCapabilities();
  const out = host.querySelector<HTMLSelectElement>('[data-a="output"]')!;
  if (!mc.audioOutputSelection) {
    note.textContent ||= mc.secureContext
      ? "This browser can't switch the audio output device."
      : "Output-device switching needs an HTTPS connection — playing to your device's default.";
    return;
  }
  const md = navigator.mediaDevices as MediaDevices & {
    selectAudioOutput?: () => Promise<MediaDeviceInfo>;
  };
  const fill = async () => {
    const devs = (await md.enumerateDevices()).filter(
      (d) => d.kind === "audiooutput",
    );
    const s = loadSettings();
    out.disabled = false;
    out.innerHTML =
      `<option value="">Default</option>` +
      devs
        .map(
          (d) =>
            `<option value="${escapeHtml(d.deviceId)}"${d.deviceId === s.audioOutputId ? " selected" : ""}>${escapeHtml(
              d.label || "Output device",
            )}</option>`,
        )
        .join("");
  };
  try {
    await fill();
    const changed = () => {
      if (!host.isConnected) {
        md.removeEventListener("devicechange", changed);
        return;
      }
      void fill().catch(() => {});
    };
    md.addEventListener?.("devicechange", changed);
    out.addEventListener("change", () => {
      saveSettings({ ...loadSettings(), audioOutputId: out.value });
      note.textContent = "Output device saved — applies on connect.";
    });
    // Blank labels ⇒ no permission yet; add a picker button.
    if (
      ![...out.options].some(
        (o) => o.value && o.textContent !== "Output device",
      )
    ) {
      const pick = document.createElement("button");
      pick.className = "secondary";
      pick.type = "button";
      pick.textContent = "Choose output device…";
      pick.addEventListener("click", async () => {
        try {
          const d = await md.selectAudioOutput!();
          saveSettings({ ...loadSettings(), audioOutputId: d.deviceId });
          await fill();
          note.textContent = "Output device saved — applies on connect.";
        } catch {
          /* cancelled */
        }
      });
      host.append(pick);
    }
  } catch {
    /* enumerate failed */
  }
}

function showPair(root: HTMLElement) {
  root.innerHTML = gate(`
      <h1>Pair this browser</h1>
      <p class="sub">Enter the six-digit PIN shown in InPhase on your gaming PC.</p>
      <div class="gate-form">
        <label for="pin">Pairing PIN</label>
        <input id="pin" class="code-input" type="tel" inputmode="numeric" maxlength="6" autocomplete="one-time-code" placeholder="000000" />
        <button id="go">Pair this device</button>
        <div class="err" id="err" role="alert"></div>
      </div>
      <p class="gate-note">The PIN is on the InPhase dashboard: open it from the tray icon on your PC.</p>`);
  const pin = root.querySelector<HTMLInputElement>("#pin")!;
  const err = root.querySelector<HTMLDivElement>("#err")!;
  const go = root.querySelector<HTMLButtonElement>("#go")!;
  pin.focus();
  const submit = async () => {
    if (go.disabled) return;
    if (!/^\d{6}$/.test(pin.value.trim())) {
      err.textContent = "Enter all six digits from your gaming PC.";
      pin.focus();
      return;
    }
    go.disabled = true;
    err.textContent = "";
    try {
      // This browser's durable device identity — registered in the Host's ACL
      // on a successful pair.
      const ident = await getControllerIdentity();
      if (!ident)
        throw new Error(
          "This browser can't hold a device key — update it and retry.",
        );
      const res = await fetch("/api/v1/pair", {
        method: "POST",
        signal: AbortSignal.timeout(8000),
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          pin: pin.value.trim(),
          controller_pubkey: ident.publicKeyHex,
          controller_name: deviceLabel(),
        }),
      });
      if (res.status === 429)
        throw new Error("Too many attempts — wait a few minutes.");
      if (res.status === 423)
        throw new Error(
          "PIN pairing is locked after too many wrong PINs. On your PC, open InPhase and choose New PIN.",
        );
      if (res.status === 401)
        throw new Error(
          "That PIN did not match. Check the current PIN on your PC.",
        );
      if (res.status === 403)
        throw new Error(
          "Pair on the same local network using the secure play address from your PC.",
        );
      if (!res.ok)
        throw new Error("The PC could not complete pairing. Please try again.");
      showHome(root);
    } catch (e) {
      err.textContent = e instanceof Error ? e.message : String(e);
      go.disabled = false;
    }
  };
  go.addEventListener("click", submit);
  pin.addEventListener("keydown", (e) => e.key === "Enter" && submit());
}

/** One connection attempt. Recreated on Apply (settings change). */
export class Session {
  private stage = div("stage");
  private status = div("status-line");
  private sock!: SignalSocket;
  private input: InputManager | TouchController | null = null;
  private probe: FrameProbe | null = null;
  private hud: Hud;
  private pingTimer = 0;
  private lastInputRtt = 0;
  private closed = false;
  // WebTransport video path (ADR-0011). Both stay null until the host
  // advertises `wt_video_info` AND the browser supports WebTransport; any
  // failure reverts to the WebRTC <video>, which never stops flowing.
  private wtClient: WtVideoClient | null = null;
  /** §15: the single recovery state machine owns the reset/redial ladder. */
  private wtRecovery = new WtRecovery();
  /** The recovery watchdog's own clock. It rode hudTick, whose only steady
   *  driver is the 2 s pong, so a 3 s reset fired 3-5 s into a stall and the
   *  8 s silent-connection check was just as coarse (audit §2.5). */
  private wtWatchdogTimer = 0;
  /** §13: when the WT path is live it takes input datagrams; the WebRTC
   *  data channel only serves while WT is down. Exactly one path per packet. */
  private wtInput: ((b: Uint8Array) => Promise<boolean>) | null = null;
  private wtDecoder: WtDecoder | null = null;
  private wtSyncError: (() => number | null) | null = null;
  private wtClose: (() => void) | null = null;
  private wtInfo: WtVideoInfo | null = null;
  private wtRedialTimer = 0;
  /** performance.now() when the current dial started (stuck-dial watchdog). */
  private wtDialAt = 0;
  /** onConnected mounted the glass (HUD, input, telemetry) exactly once. */
  private glassMounted = false;
  /** Latest host route warning (HUD), cleared when the glass closes. */
  private routeWarning: string | null = null;
  /** WT audio (§11-on-WT): Opus datagrams -> WebCodecs -> AudioContext. */
  // One WtAudio for the page: start() after stop() recreates its context.
  private readonly wtAudio = new WtAudio();
  /** The codec the WT session actually negotiated (HUD label). */
  private wtCodecLabel: string | null = null;
  private wtActive = false;
  private readonly immersive: boolean;

  /** Where "go home" / teardown returns to. Defaults to `renderPlay`. */
  private readonly onExit?: () => void;
  private notice: string | null = null;

  constructor(
    private readonly root: HTMLElement,
    private settings: StreamSettings,
    opts: {
      immersive?: boolean;
      onExit?: () => void;
      /** Shown once the video is back (e.g. why the mode was stepped down). */
      notice?: string;
    } = {},
  ) {
    this.immersive = opts.immersive ?? false;
    this.onExit = opts.onExit;
    this.notice = opts.notice ?? null;
    saveSettings(settings);
    // Sound from the first frame: creating/resuming the AudioContext here
    // lands inside the Connect click's user gesture - iOS Safari suspends
    // contexts made outside one, which read as "starts muted".
    this.wtAudio.prime();
    document.body.append(this.stage, this.status);
    this.status.textContent = "Connecting…";
    if (this.immersive) {
      document.title = "InPhase — playing";
      window.addEventListener("pagehide", this.onPageHide);
    }

    this.hud = new Hud(
      settings,
      (s) => this.reconnect(s),
      () => this.teardown(),
      (sc) => this.input?.tapKey(sc),
      (vol, muted) => this.setAudio(vol, muted),
      (el) => void buildAudioSettings(el, () => this.reconnect(this.settings)),
      (v) => {
        this.settings.showMetrics = v;
        saveSettings(this.settings);
      },
    );

    // No WebRTC session: video, audio and input all ride WebTransport, and
    // the host's signaling offer is ignored below.
    window.addEventListener("resize", () => this.layoutHud());
    document.addEventListener("fullscreenchange", () => this.layoutHud());

    if (!isTouch) {
      const im = new InputManager(this.stage, {
        sendInput: (b) => this.routeInput(b),
      });
      if (this.immersive) {
        im.onQuitHotkey = () => this.goHome(); // Ctrl+Shift+Q → back to home
        document.addEventListener("fullscreenchange", this.onImmersiveFs);
        document.addEventListener("pointerlockchange", this.onPointerLock);
        // The Connect click is a live browser gesture right now — take the
        // screen + pointer lock before the async negotiation burns it.
        void im.capture();
      } else {
        im.onReleaseHotkey = () =>
          this.toast(
            "Capture released — click the video to go fullscreen again",
          );
      }
      this.input = im;
      this.input.attach();
    }

    window.addEventListener("gamepadconnected", this.onGamepad);

    void this.negotiate();
  }

  private onGamepad = (e: GamepadEvent) => {
    const g = e.gamepad;
    this.toast(`${g.id.split("(")[0]!.trim() || "Controller"} connected`);
    console.info(
      "[InPhase] gamepad",
      g.id,
      g.mapping || "(non-standard)",
      `${g.buttons.length}btn/${g.axes.length}ax`,
    );
  };

  private toast(text: string) {
    const el = div("status-line");
    el.textContent = text;
    document.body.append(el);
    setTimeout(() => el.remove(), 2500);
  }

  // ---- immersive (fullscreen) play --------------------------------------

  private enterOverlay: HTMLElement | null = null;

  /** "Paused" prompt shown when fullscreen drops but the session is still up
   *  (typically Esc — the browser exits fullscreen and we can't stop it without
   *  Keyboard Lock, which needs a secure/HTTPS origin). Click resumes.
   *  Idempotent. */
  private showEnterPrompt() {
    if (this.closed || document.fullscreenElement) return;
    if (!this.enterOverlay) {
      const ov = div("overlay enter-prompt");
      ov.innerHTML = `<div class="enter-cta">
        <div class="enter-glyph">${icon("play", 30)}</div>
        <div class="enter-title">Paused</div>
        <div class="enter-hint">Click to go back to fullscreen and take the mouse. This browser leaves fullscreen when you press Esc.</div>
        <div class="enter-keys"><kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Q</kbd> ends the stream</div>
      </div>`;
      const resume = async () => {
        await (this.input as InputManager | null)?.capture();
        if (document.fullscreenElement) ov.remove();
      };
      ov.addEventListener("click", resume);
      // any keypress also resumes (a keydown is a valid fullscreen gesture)
      ov.addEventListener("keydown", resume);
      ov.tabIndex = 0;
      this.enterOverlay = ov;
    }
    if (!this.enterOverlay.isConnected) this.stage.append(this.enterOverlay);
    this.enterOverlay.focus();
  }

  private onImmersiveFs = () => {
    if (this.closed) return;
    this.updateCaptureUi();
  };

  private onPointerLock = () => {
    if (this.closed || !this.immersive) return;
    this.updateCaptureUi();
  };

  private updateCaptureUi() {
    if (!this.immersive || this.closed) return;
    const fs = !!document.fullscreenElement;
    const locked = document.pointerLockElement === this.stage;
    // Full-screen "Paused" overlay only when we've actually left fullscreen.
    // In fullscreen without pointer lock, the small stage-paused banner is enough
    // — a dark overlay on top of live video looks like a black screen.
    this.stage.classList.toggle("stage-paused", fs && !locked);
    if (!fs) this.showEnterPrompt();
    else this.enterOverlay?.remove();
  }

  private onPageHide = () => {
    // Tab closed / navigated away — free the host session promptly.
    navigator.sendBeacon?.("/api/v1/session/stop");
  };

  private async negotiate() {
    // Before signalling, so an unusable browser never claims the host session.
    const blocker = playBlocker();
    if (blocker !== null) {
      this.fail(blocker);
      return;
    }
    const features = detectFeatures();
    // Probe both codecs at the requested mode. HEVC is preferred when the
    // browser can decode it, but H.264 is the interoperability floor
    // (ADR-0005): advertising HEVC alone stranded every browser without it —
    // Chrome on Linux has no HEVC at all, so the list came back empty and the
    // client refused to start a stream its decoder could handle.
    const modes = (["h265", "h264"] as const).map((codec) => ({
      codec,
      width: this.settings.width,
      height: this.settings.height,
      framerate: this.settings.fps,
    }));
    const hints = await decodeHints(modes);
    const videoCodecs = usableVideoCodecs(modes, hints);
    if (videoCodecs.length === 0) {
      // The host picks between them; the client's job is to report honestly
      // what it can decode. Only a browser that can decode neither is refused,
      // and it is refused by name rather than by guessing at the cause.
      this.fail(
        "This browser can't decode H.265 or H.264 video. Try a current Safari, Chrome, or Edge.",
      );
      return;
    }

    this.sock = new SignalSocket(
      (m) => this.onSignal(m),
      () => this.onDrop(),
    );
    let authOk = true;
    await this.sock.ready.catch((e: unknown) => {
      authOk = false;
      this.fail(e instanceof Error ? e.message : "signaling channel failed");
    });
    if (!authOk || this.closed) return;

    this.sock.send({
      type: "client_hello",
      protocol_version: SIGNALING_PROTOCOL_VERSION,
      browser: navigator.userAgent,
      requested_mode: {
        width: this.settings.width,
        height: this.settings.height,
        fps: this.settings.fps,
        preset: this.settings.preset,
        // Ask for the best codec this browser can actually decode. Hardcoding
        // HEVC here only produces a stream that dies at `configure()` on a
        // browser whose HEVC probe answered `true` and then failed (§30).
        codec_preference: hints[0]?.supported ? "h265" : "h264",
        max_bitrate_kbps: this.settings.maxBitrateKbps,
        stream_target:
          this.settings.streamTarget.type === "desktop"
            ? { type: "desktop" }
            : { type: "game", id: this.settings.streamTarget.id },
      },
    });
    this.sock.send({
      type: "client_capabilities",
      rtp_video_codecs: videoCodecs,
      rtp_audio_codecs: receiveAudioCodecs(),
      decode_hints: hints,
      features,
    });
  }

  private async onSignal(m: SignalMessage) {
    switch (m.type) {
      case "session_config":
        break; // no WebRTC configuration to apply
      case "wt_video_info":
        this.maybeDialWt(m);
        break;
      case "offer":
      case "ice":
        break; // WebRTC is removed (user directive); WT dials via wt_video_info
      case "session_ready":
        this.status.remove();
        this.root.innerHTML = "";
        break;
      case "error":
        if (m.code === "unauthorized") {
          // iOS Home Screen apps share Safari's session cookie and keep their
          // own device key. Logging out here also unpaired Safari. Do not
          // clear the cookie; send this app to PIN pairing.
          stripAutoPlayParam();
          this.teardownInternals();
          this.status.remove();
          document
            .querySelectorAll(".overlay, .status-line")
            .forEach((el) => el.remove());
          showPair(this.root);
          return;
        }
        this.fail(m.message);
        break;
    }
  }

  /**
   * Dial the host's WebTransport video path (ADR-0011). The WebRTC video
   * keeps flowing underneath; the first decoded WT frame swaps the display
   * to the canvas, and any close swaps back. Nothing here can fail the
   * session — worst case we simply never switch away from WebRTC.
   */
  private maybeDialWt(info: WtVideoInfo): void {
    if (this.closed) return;
    if (this.wtClient !== null) {
      // The host rotates its certificate on every restart (self-signed,
      // freshly generated). A dial pinned to the old hash can never
      // complete - and a WebTransport stuck in `connecting` never fires
      // close() - so a wedged client object would block every fresh push
      // forever (seen live 2026-09-08: three host restarts, the page never
      // re-dialed, "stuck on connecting"). A push whose pin differs from
      // the pinned one is authoritative: recycle and dial with it.
      if (info.cert_sha256 === this.wtInfo?.cert_sha256) return;
      console.info("wt: host certificate rotated - recycling the dead dial");
      this.wtTeardown();
    }
    if (!("WebTransport" in globalThis)) {
      // negotiate() refuses this browser up front; this is the backstop. There
      // is no other video path, so a silent return is a blank stage forever.
      this.fail(playBlocker() ?? "This browser doesn't support WebTransport.");
      return;
    }
    if (this.wtWatchdogTimer === 0) {
      this.wtWatchdogTimer = window.setInterval(() => this.wtWatchdog(), 250);
    }
    this.wtInfo = info;
    const client = new WtVideoClient();
    const decoder = new WtDecoder(
      () => {
        if (this.wtActive || this.closed) return;
        this.wtActive = true;
        // The never-decoded ladder has been observe(0)-ing since dial. The
        // first decoded frame calls here *and* hudTick(); on the bitmap path
        // framesPresented is still 0 (present is async). Without forgetting
        // that stall clock, a first picture that took >3 s — encoder warmup,
        // or the 2 s stream wedge cancelling the startup IDR — is classified
        // as "glass frozen" and the decoder is reset on the frame that just
        // arrived.
        this.wtRecovery.reset();
        this.wtAudio?.start();
        // The WT video is presenting - the session is visibly up. Mount the
        // full glass (HUD, touch input, telemetry) here too: on a cellular
        // path WebRTC ICE may never connect, and onConnected - the only
        // other mount site - keys off ICE (2026-09-08 Safari: video
        // streamed while the HUD, input buttons and touch capture were
        // missing entirely, and the page rubber-banded under touches).
        this.onConnected();
        this.stage.append(decoder.root);
        this.toast(this.notice ?? "WebTransport video path active");
        this.notice = null;
        this.hudTick();
      },
      () => client.requestKeyframe(),
      // Pong-synced host↔client clock offset → true capture→glass aging.
      () => client.clockOffsetUs(),
      // The decoder has given up rebuilding. Say so - the alternative was an
      // endless error/configure loop that froze the tab with no explanation.
      (why) => this.fail(why),
      // The decoder worked, then could not keep up: a smaller mode, frame
      // rate first (nextStepDown). Saved, so this device starts there next
      // time; the settings dialog can raise it again.
      {
        available: () => nextStepDown(this.settings) !== null,
        apply: () => {
          const from = this.settings;
          const to = nextStepDown(from);
          if (to === null) return;
          const note =
            `This device couldn't keep up with ${from.height}p at ${from.fps} fps - ` +
            `switched to ${to.height}p at ${to.fps} fps.`;
          console.warn(`wt: ${note}`);
          this.reconnect({ ...from, ...to }, note);
        },
      },
    );
    this.wtSyncError = () => client.syncErrorMs();
    // Lip sync: audio aims at the age the video is shown at, on the same
    // host clock (WtAudio.syncedStartUs).
    this.wtAudio?.setSync(
      () => client.clockOffsetUs(),
      () => decoder.presentAgeMs(),
    );
    this.wtClose = () => client.close();
    const wtInput = (b: Uint8Array) => client.sendInput(b);
    this.wtInput = wtInput;
    client
      .dial(info, {
        onVideoConfig: (cfg) => {
          // The HUD's codec label: what THIS session negotiated, not a
          // stale WebRTC-era telemetry field with an "H.264" fallback.
          this.wtCodecLabel = /hvc1|hev1|265/i.test(cfg.codec)
            ? "H265"
            : cfg.codec.replace(/^video\//i, "").toUpperCase();
          // Probe the exact configuration before applying it (review §5):
          // isConfigSupported is the browser's authoritative answer about
          // this codec/description/size combination.
          void VideoDecoder.isConfigSupported({
            codec: cfg.codec,
            codedWidth: cfg.width,
            codedHeight: cfg.height,
            description: cfg.description ?? undefined,
          })
            .then((probe) => {
              if (!probe.supported)
                throw new Error("config unsupported by this browser");
              // The WT path now races the WebRTC answer at session start, so
              // TWO video_config messages arrive (epochs 3 and 4 in the
              // 18:34 session). Reconfiguring on the identical second one
              // flushes the decoder right after the startup IDR passed - the
              // session then decoded nothing forever. Only a REAL change
              // (codec/description/size) reconfigures; duplicates just ack.
              // (The check used to be computed and then ignored: configure()
              // ran regardless - orderer reset, codec flushed, IDR forced.)
              const same = sameDecoderConfig(decoder.currentConfig(), cfg);
              if (!same) decoder.configure(cfg);
              void client.send({ type: "config_ack", epoch: cfg.epoch });
              if (same)
                console.info(
                  "wt: duplicate video_config (epoch",
                  cfg.epoch,
                  ") - kept the running decoder",
                );
            })
            .catch((e) => {
              // Unsupported codec/description combo: without this the client
              // would sit connected-but-forever-unconfigured. Close so the
              // watchdog redials a fresh WT path.
              console.warn("wt: decoder config rejected:", String(e));
              client.close();
            });
        },
        onFrame: (f) => decoder.onFrame(f),
        onClosed: (why) => {
          console.info("wt video path closed:", why);
          this.wtTeardown();
          if (!this.closed) {
            // No WebRTC video exists to fall back to — the canvas holds the
            // last frame while the auto-redial (3 s) restores the glass.
            this.toast("WebTransport video lost — redialing");
            this.hudTick();
          }
        },
        onRtt: (rtt) => {
          decoder.setRttMs(rtt);
          this.hudTick();
        },
        onRouteWarning: (detail) => {
          // §"respect user inputs": the resolution stays; the user is told.
          console.warn("wt route warning:", detail);
          this.toast(detail);
          this.routeWarning = detail;
        },
        onAudio: (opus, ptsUs) => this.wtAudio?.push(opus, ptsUs),
        onControlMessage: (data) => this.onControl(data),
      })
      .then(() => {
        this.wtClient = client;
        this.wtDialAt = performance.now();
        this.wtDecoder = decoder;
        // Once-per-second WT telemetry reports decoder + playout stats to the
        // host's congestion controller (media/bitrate.rs), same shape as the
        // WebRTC path's ClientTelemetry.
        client.setStatsProvider(() => {
          const s = decoder.stats();
          return {
            codec: this.wtCodecLabel ?? null,
            framesDecoded: s.framesDecoded,
            framesPresented: s.framesPresented,
            presentedFps: s.presentedFps,
            freezeCount: s.freezeCount,
            totalFreezeMs: s.totalFreezeMs,
            framesDropped: s.framesDropped,
            held: s.held,
            queueSize: s.queueSize,
            behindEvents: s.behindEvents,
          };
        });
      })
      .catch((e) => {
        console.info("wt dial failed (watchdog will redial):", String(e));
        if (this.wtInput === wtInput) this.wtInput = null;
        if (this.wtClient === client) this.wtClient = null;
        client.close();
        decoder.stop();
      });
  }

  private wtTeardown(): void {
    this.wtActive = false;
    this.wtAudio?.stop();
    this.wtInput = null;
    this.wtDecoder?.stop();
    this.wtDecoder = null;
    this.wtClient?.close();
    this.wtClient = null;
    // No WebRTC video exists to reveal — the canvas holds the last frame and
    // the auto-redial below restores the glass.
    if (!this.closed && this.wtInfo && this.wtRedialTimer === 0) {
      this.wtRedialTimer = window.setTimeout(() => {
        this.wtRedialTimer = 0;
        if (!this.closed && this.wtClient === null) {
          // The old dial token was consumed by the connection that just
          // died — ask the host for a fresh one over the still-alive
          // signaling channel; the `wt_video_info` handler dials with it.
          console.info("wt: requesting a fresh dial token");
          this.sock.send({ type: "wt_video_info_request" });
        }
      }, 3000);
    }
  }

  // Host frame stamps + host_now_us ride the control stream; consumed in maybeDialWt.
  private onControl(data: string) {
    let m: {
      type?: string;
      at_us?: number;
      host_now_us?: number;
      frames?: [number, number, number, number][];
    };
    try {
      m = JSON.parse(data);
    } catch {
      return; // not json
    }
    if (m.type === "pong" && typeof m.at_us === "number") {
      this.lastInputRtt = performance.now() - m.at_us / 1000;
    } else if (m.type === "frame_stamps" && m.frames) {
      this.probe?.ingestHostStamps(
        m.frames.map(([rtp, capture_us, encode_us, send_us]) => ({
          rtp,
          capture_us,
          encode_us,
          send_us,
        })),
        m.host_now_us ?? 0,
        this.lastInputRtt / 2, // control-channel one-way, from ping/pong
      );
    }
  }

  private onConnected() {
    if (this.glassMounted) return;
    this.glassMounted = true;
    // Video is flowing — clear the "Connecting…" overlay even if `session_ready`
    // is slow (belt-and-suspenders; the host also sends it).
    this.status.remove();
    this.root.innerHTML = "";
    // Must live inside `.stage` — Chrome only paints descendants of the
    // fullscreen element, and we request fullscreen on the stage.
    this.stage.append(this.hud.root);
    // Belt-and-suspenders: autoplay can still stall after a long negotiate or
    // when fullscreen/pointer-lock churns during connect.
    if (isTouch) {
      const tc = new TouchController(this.stage, {
        sendInput: (b) => this.routeInput(b),
      });
      tc.onDisconnect = () => this.teardown();
      tc.attach();
      this.input = tc;
    } else if (this.immersive) {
      // Fullscreen was requested on the Connect gesture. If it didn't stick
      // (denied, or dismissed while connecting), show the resume prompt.
      if (!document.fullscreenElement) this.showEnterPrompt();
      const hint = browserInputHint();
      if (hint) this.toast(hint);
      this.updateCaptureUi();
    } else {
      this.toast(
        "Click the video for fullscreen — input is captured while fullscreen · Esc to release",
      );
    }
    this.probe = new FrameProbe();
    // Label the glass in the frame log: while WT is showing, its measured
    // latency rides along — otherwise the WebRTC numbers below are the hidden
    // fallback stream's, not what the user is watching.
    this.probe.wtGlass = () => {
      const s = this.wtActive ? this.wtDecoder?.stats() : null;
      return s && s.e2eMs > 0
        ? {
            e2eMs: s.e2eMs,
            presentedFps: s.presentedFps,
            syncErrMs: this.wtSyncError?.() ?? null,
            framesDecoded: s.framesDecoded,
            framesDropped: s.framesDropped,
            rendering: s.rendering,
          }
        : null;
    };
    this.probe.start();
    // Slow-poll the host audio-branch health (rare failure; not on the hot path).
    if (this.settings.volume > 0) {
      this.audioHealthPoll = window.setInterval(async () => {
        const c = await fetchHostCapabilities();
        this.audioHostFailed = c?.audio?.capture?.health === "failed";
      }, 8000);
    }
    // Audio autoplays muted; apply the saved volume/mute on the first input
    // (a real user gesture — otherwise Chrome keeps it silent).
    document.addEventListener("pointerdown", this.applyAudioOnce);
    document.addEventListener("keydown", this.applyAudioOnce);
    this.hudTick();
  }

  private applyAudioOnce = () => {
    document.removeEventListener("pointerdown", this.applyAudioOnce);
    document.removeEventListener("keydown", this.applyAudioOnce);
    if (this.closed) return;
    // iOS Safari only creates/resumes an AudioContext inside a user gesture -
    // this handler IS that gesture. Without it the context stays suspended
    // and every decoded sample plays into the void.
    this.wtAudio.prime();
    this.setAudio(this.settings.volume, this.settings.muted);
  };

  private setAudio(volume: number, muted: boolean) {
    this.settings.volume = volume;
    this.settings.muted = muted;
    saveSettings(this.settings);
    this.wtAudio.setVolume(volume, muted);
  }

  private audioHealthPoll = 0;
  private audioHostFailed = false;

  /** Letterbox geometry → HUD placement. When the video's aspect leaves
   *  ≥96px bars on the sides (a phone in landscape), the controls rail moves
   *  into the right gutter and the stats overlay tucks into the left one —
   *  the overlays stop covering the picture. */
  private layoutHud() {
    const hud = this.hud?.root;
    if (!hud?.isConnected) return;
    const sw = this.stage.clientWidth;
    const sh = this.stage.clientHeight;
    if (!sw || !sh) return;
    const vw = this.settings.width || 16;
    const vh = this.settings.height || 9;
    const contentW = sw / sh > vw / vh ? sh * (vw / vh) : sw;
    const gutter = (sw - contentW) / 2;
    if (gutter >= 96) {
      hud.style.setProperty("--gutter-w", `${Math.round(gutter)}px`);
      hud.classList.add("hud-gutter");
    } else {
      hud.classList.remove("hud-gutter");
      hud.style.removeProperty("--gutter-w");
    }
  }

  private hudTick() {
    try {
      this.hudTickInner();
    } catch (e) {
      // The watchdog MUST run even if a DOM probe is null (e.g. an element
      // torn down mid-session) — a dead watchdog is a frozen glass.
      console.warn("hud tick failed:", String(e));
    }
  }

  /** The recovery ladder, on its own 250 ms clock (and from hudTick). Reads
   *  cheap counters only: `stats()` rolls the fps window and must stay on the
   *  1 s cadence. */
  private wtWatchdog() {
    if (this.closed) return;
    // Watchdog: a network stall can break the decode reference chain (or starve
    // the reassembler) while the transport stays healthy — the host keeps
    // pinging fine while presentedFps sits at 0 forever. Recover in stages:
    // reset the decoder and demand an IDR after 3 s frozen; redial the whole
    // WT path after 10 s (the fresh session re-gates on a keyframe anyway).
    // Arm on a live WT path, not only after a first successful decode: the
    // 18:34 shape was a session that NEVER decoded (a mid-startup reconfigure
    // swallowed the IDR) - rendering was false forever, so the ladder never
    // armed and the glass sat black until the user closed it. A never-decoded
    // session IS a freeze from t=0; reset+IDR at 3 s and redial at 10 s apply.
    const dec = this.wtDecoder;
    // A hidden tab stops presenting on purpose, and a freeze that is waiting
    // on a requested IDR is already being repaired: resetting the decoder
    // then flushes that IDR (audit §2.5, §2.6).
    const ctx: RecoveryContext = {
      hidden: document.hidden,
      keyframeRequestedAtMs: dec?.awaitingKeyframeSince() ?? null,
    };
    // Safari never fires onClosed when a WT connection dies silently - the
    // read promises hang and the glass freezes with no recovery path. Watch
    // the connection's own pulse: no pong AND no datagram for 8 s means it
    // is dead regardless of what the transport claims; force the redial.
    if (this.wtActive && this.wtClient && this.wtClient.staleMs() > 8000) {
      console.warn("wt connection silent >8s - forcing teardown + redial");
      this.toast("WebTransport lost — reconnecting");
      this.wtTeardown();
    } else if (this.wtActive && dec) {
      const action = this.wtRecovery.observe(dec.presentedCount, ctx);
      if (action === "reset") {
        console.warn(
          "wt watchdog: glass frozen — resetting decoder, requesting IDR",
        );
        dec.reset();
      } else if (action === "redial") {
        console.warn(
          "wt watchdog: still frozen after reset — redialing WT path",
        );
        this.wtClose?.();
      }
    } else if (dec && !this.wtActive) {
      // Dialed but never presented. (03:55 Chrome: the wedged stream ate the
      // startup IDR and every later forced IDR lacked in-band SPS/PPS, so
      // decode() ate chunks and produced nothing.) wtActive flips true only
      // on first presentation, so the branch above never armed and this is
      // the only ladder a never-decoded session gets: observe(0) resets +
      // demands an IDR at 3 s, redials at 10 s.
      const action = this.wtRecovery.observe(0, ctx);
      if (action === "reset") {
        console.warn(
          "wt watchdog: no decode — resetting decoder, requesting IDR",
        );
        dec.reset();
      } else if (action === "redial") {
        console.warn(
          "wt watchdog: still not decoding after reset — redialing WT path",
        );
        this.wtClose?.();
      }
    } else {
      this.wtRecovery.reset();
    }
  }

  private hudTickInner() {
    // A dial that never completes (host restarted mid-handshake, filtered
    // UDP path) leaves a WebTransport object in `connecting` forever with no
    // close event - it blocks every fresh dial. Recycle it and let the
    // recovery ladder request a fresh token.
    if (
      this.wtClient !== null &&
      !this.wtActive &&
      this.wtDialAt > 0 &&
      performance.now() - this.wtDialAt > 12_000
    ) {
      console.info("wt: dial never completed in 12s - recycling");
      this.wtDialAt = 0;
      this.wtTeardown();
    }

    this.layoutHud();
    this.hud.setHasAudio(this.wtActive);
    const measuredE2e = this.probe?.measuredE2eMs() ?? 0;

    const wtStats = this.wtActive ? this.wtDecoder?.stats() : null;
    this.wtWatchdog();
    const syncErr = this.wtSyncError?.() ?? null;
    const inputPath =
      this.wtInput === null
        ? "input: rtc"
        : this.wtInputFailures > 0
          ? "input: wt failed→rtc"
          : null;
    const wtDetail = wtStats
      ? `wt · dec ${wtStats.decodeMs.toFixed(1)} ms · dropped ${wtStats.framesDropped}` +
        (syncErr !== null ? ` · sync ±${syncErr.toFixed(1)} ms` : "") +
        (inputPath ? ` · ${inputPath}` : "")
      : null;
    // While the WT glass is up, its own pong-synced capture→glass EMA is the
    // truth — the WebRTC probe below measures a stream the user isn't watching.
    const wtE2e = wtStats && wtStats.e2eMs > 0 ? wtStats.e2eMs : 0;
    this.hud.update({
      decodedFps: wtStats?.decodedFps ?? 0,
      presentedFps: wtStats?.presentedFps ?? 0,
      rttMs: this.wtClient?.rttMs() ?? 0,
      inputRttMs: this.lastInputRtt,
      e2eEstMs: wtE2e > 0 ? wtE2e : measuredE2e,
      width: this.wtDecoder?.root.width ?? 0,
      height: this.wtDecoder?.root.height ?? 0,
      inboundKbps: this.wtClient?.inboundKbps() ?? 0,
      codec: this.wtCodecLabel ?? "--",
      jitterBufferMs: undefined,
      decodeMs: wtStats?.decodeMs,
      packetsLost: undefined,
      audioState: null,
      audioHostFailed: this.audioHostFailed,
      // No ICE to classify - the path IS the WebTransport connection.
      path: "wt",
      pathDetail: this.routeWarning
        ? `⚠ ${this.routeWarning}` + (wtDetail ? ` · ${wtDetail}` : "")
        : (wtDetail ?? null),
    });
  }

  private reconnect(s: StreamSettings, note?: string) {
    this.settings = s;
    saveSettings(s);
    this.teardownInternals();
    this.status = div("status-line");
    this.status.textContent = note ?? "Reconnecting…";
    document.body.append(this.status);
    // fresh session object keeps this simple
    new Session(this.root, s, {
      immersive: this.immersive,
      onExit: this.onExit,
      notice: note,
    });
  }

  /** Signalling socket closed on its own (host gone, network drop) — not our
   *  own teardown. */
  private onDrop() {
    // A socket closed by the page itself going away (reload, navigation) is
    // not a failure; reporting one flashed "not paired" over every reload of a
    // page that was still connecting.
    if (this.closed || pageUnloading) return;
    // Touch / PWA used to bounce home with no message. A Home Screen app
    // whose device key was not in the ACL died on the challenge in <100 ms
    // and looked like Stream desktop "just failed".
    if (!this.glassMounted) {
      // Before the first picture the likeliest cause is still an unpaired
      // device key (a Home Screen app dies on the challenge in <100 ms), but a
      // host restart or a network drop mid-connect ends the same way, and
      // telling those users to re-pair sent them the wrong way.
      this.fail(
        "The PC closed the connection before video started. If this device isn't paired yet, " +
          "open it in Safari or enter the PIN shown on the host; otherwise try again.",
      );
      return;
    }
    if (this.immersive) this.fail("Connection lost");
    else this.goHome();
  }

  /** Hard error (unsupported codec, host `error` message, signalling / network
   *  failure). Show why, and offer an explicit retry — never auto-reconnect. */
  private fail(reason: string) {
    if (this.closed) return;
    // Console mirror: the harness and remote debugging read console.log;
    // the overlay alone left E2E verdicts blind to named failures.
    console.error("session failed:", reason);
    this.teardownInternals();
    this.status.remove();
    const ov = div("overlay");
    ov.innerHTML = `<div class="card">
      <div class="gate-state warn">${icon("alert")}</div>
      <h1>Disconnected</h1>
      <p class="sub"></p>
      <div class="card-actions">
        <button data-a="retry">${icon("refresh")} Reconnect</button>
        <button class="secondary" data-a="home" type="button">Back to library</button>
      </div>
    </div>`;
    ov.querySelector(".sub")!.textContent = reason;
    ov.querySelector<HTMLButtonElement>('[data-a="retry"]')!.addEventListener(
      "click",
      () => {
        ov.remove();
        new Session(this.root, this.settings, {
          immersive: this.immersive,
          onExit: this.onExit,
        });
      },
    );
    ov.querySelector<HTMLButtonElement>('[data-a="home"]')!.addEventListener(
      "click",
      () => {
        ov.remove();
        void renderPlay(this.root);
      },
    );
    document.body.append(ov);
  }

  /** Disconnect button (HUD) — end the session and return to the home screen. */
  private teardown() {
    if (this.closed) return;
    this.goHome();
  }

  /** §13: WT datagrams carry replaceable input state while the WT path is
   *  live; the WebRTC data channel is the fallback only. Never both. */
  /** Input packets the WT datagram path rejected (HUD: fallback visibility). */
  private wtInputFailures = 0;

  private routeInput(b: Uint8Array): void {
    const wt = this.wtInput;
    if (!wt) return; // still dialing - snapshots before the transport exists are dropped
    void wt(b).then((took) => {
      if (!took && this.wtClient?.inputReady()) {
        // A write failed on an otherwise-live transport - count it (HUD).
        if (this.wtInputFailures === 0) {
          console.warn(
            "wt input write failed - input is dropped until the WT path recovers",
          );
        }
        this.wtInputFailures++;
      }
      // !took with no live writers = pre-dial snapshot; skip silently.
    });
  }

  private goHome() {
    this.teardownInternals();
    this.status.remove();
    document
      .querySelectorAll(".overlay, .status-line")
      .forEach((el) => el.remove());
    // Drop `?play` first: it means "auto-connect on load", so re-rendering with
    // it still set sends us straight back into a session. When the session is
    // failing (an unpaired device, say) that is an unbounded reconnect loop
    // hammering the host every few milliseconds.
    stripAutoPlayParam();
    if (this.onExit) this.onExit();
    else void renderPlay(this.root);
  }

  private teardownInternals() {
    this.closed = true;
    window.removeEventListener("gamepadconnected", this.onGamepad);
    window.removeEventListener("pagehide", this.onPageHide);
    document.removeEventListener("fullscreenchange", this.onImmersiveFs);
    document.removeEventListener("pointerlockchange", this.onPointerLock);
    document.removeEventListener("pointerdown", this.applyAudioOnce);
    document.removeEventListener("keydown", this.applyAudioOnce);
    clearInterval(this.pingTimer);
    clearInterval(this.audioHealthPoll);
    clearInterval(this.wtWatchdogTimer);
    this.wtWatchdogTimer = 0;
    this.probe?.stop();
    this.wtTeardown();
    this.input?.detach();
    try {
      this.sock?.close();
    } catch {
      /* */
    }
    this.hud.root.remove();
    this.stage.remove();
    this.enterOverlay?.remove();
    if (document.fullscreenElement)
      void document.exitFullscreen().catch(() => {});
  }
}

function div(cls: string): HTMLElement {
  const e = document.createElement("div");
  e.className = cls;
  return e;
}
