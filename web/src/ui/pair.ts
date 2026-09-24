// `/pair` — accept a pairing invitation.
//
// Two ways in, both presented over the host's own HTTPS (so the secret itself
// is the proof — no PAKE / HMAC):
//   * QR:   the URL fragment is `#<base64url secret>` (never sent to a server
//           by the browser until this explicit POST).
//   * Code: the operator reads a 9-character code off the PC.
//
// The browser generates its Ed25519 device key and registers it with the host.

import { gate } from "./brand.js";
import { icon } from "./icons.js";

import { getControllerIdentity, deviceLabel } from "../controller-key.js";

export function renderPairPage(root: HTMLElement) {
  let fragment = location.hash.replace(/^#/, "").trim();
  try {
    fragment = decodeURIComponent(fragment);
  } catch {
    /* malformed fragment: allow manual code entry */ fragment = "";
  }
  if (location.hash)
    history.replaceState(null, "", location.pathname + location.search);
  const qrSecret = fragment.length > 20 ? fragment : null;

  root.innerHTML = gate(`
      <h1>Pair this browser</h1>
      <p class="sub">${qrSecret ? "Your PC sent an invitation. Confirm to add this browser to your devices." : "Enter the 9-character code shown with the QR code on your PC."}</p>
      <div class="gate-form">
        ${
          qrSecret
            ? ""
            : `<label for="code">Pairing code</label>
               <input id="code" class="code-input code9" type="text" autocomplete="off" autocapitalize="characters" spellcheck="false"
                      maxlength="11" placeholder="XXXXXXXXX" />`
        }
        <button id="go">${qrSecret ? "Pair this browser" : "Pair"}</button>
        <div class="err" id="err" role="alert"></div>
        <div id="note" role="status"></div>
      </div>`);

  const $ = <T extends HTMLElement>(s: string) => root.querySelector<T>(s)!;
  const err = $<HTMLDivElement>("#err");
  const note = $<HTMLDivElement>("#note");
  const go = $<HTMLButtonElement>("#go");
  const codeEl = root.querySelector<HTMLInputElement>("#code");
  codeEl?.focus();

  let polling = 0;

  const fail = (m: string) => {
    window.clearInterval(polling);
    polling = 0;
    err.textContent = m;
    go.disabled = false;
  };
  const succeed = () => {
    window.clearInterval(polling);
    root.innerHTML = gate(`<div class="gate-state ok">${icon("check")}</div>
      <h1>Paired</h1>
      <p class="sub">This browser can now stream from your PC.</p>
      <div class="gate-actions"><button id="home">Open your library</button></div>`);
    root
      .querySelector("#home")!
      .addEventListener("click", () => (location.href = "/"));
  };

  let inFlight = false;
  window.addEventListener("pagehide", () => window.clearInterval(polling), {
    once: true,
  });
  const submit = async () => {
    if (inFlight) return;
    inFlight = true;
    try {
      go.disabled = true;
      err.textContent = "";
      const invite = qrSecret ?? (codeEl?.value ?? "").trim().toUpperCase();
      if (!invite || (!qrSecret && !/^[A-Z0-9]{9}$/.test(invite)))
        return fail("Enter the full code from the PC.");
      const ident = await getControllerIdentity().catch(() => null);
      if (!ident)
        return fail(
          "This browser can't hold a device key — update it and retry.",
        );

      try {
        const res = await fetch("/api/v1/pair", {
          method: "POST",
          signal: AbortSignal.timeout(8000),
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            invite,
            controller_pubkey: ident.publicKeyHex,
            controller_name: deviceLabel(),
          }),
        });
        const j = (await res.json().catch(() => ({}))) as { error?: string };
        if (!res.ok)
          throw new Error(j.error || `pairing failed (${res.status})`);
        succeed();
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (msg === "waiting for approval on the PC") {
          note.textContent = "Waiting for you to click Approve on the PC…";
          if (!polling) polling = window.setInterval(() => void submit(), 2000);
          return;
        }
        fail(msg);
      }
    } finally {
      inFlight = false;
    }
  };

  go.addEventListener("click", () => void submit());
  codeEl?.addEventListener(
    "keydown",
    (e) => e.key === "Enter" && void submit(),
  );
}
