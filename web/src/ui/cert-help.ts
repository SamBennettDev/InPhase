// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Per-platform "install and trust the InPhase certificate" steps. Shared by the
// http→setup landing page (main.ts) and the dashboard's "other devices" card.

import { escapeHtml, safeHttpUrl } from "./html.js";

export type Platform = "ios" | "macos" | "android" | "windows" | "other";

export function detectPlatform(ua = navigator.userAgent): Platform {
  if (/iPhone|iPad|iPod/.test(ua)) return "ios";
  // iPadOS 13+ reports as Mac; treat a touch Mac as iOS for the cert flow.
  if (/Macintosh/.test(ua) && "ontouchend" in document) return "ios";
  if (/Macintosh|Mac OS X/.test(ua)) return "macos";
  if (/Android/.test(ua)) return "android";
  if (/Windows/.test(ua)) return "windows";
  return "other";
}

const STEPS: Record<Platform, string[]> = {
  ios: [
    "Tap <b>Download the certificate</b> below and choose <b>Allow</b>.",
    "Open <b>Settings</b>. Tap <b>Profile Downloaded</b> near the top (or Settings → General → <b>VPN &amp; Device Management</b>), tap <b>InPhase Local CA</b> → <b>Install</b>, and enter your passcode.",
    "Settings → General → <b>About</b> → scroll to the bottom → <b>Certificate Trust Settings</b> → turn <b>ON</b> the switch for <b>InPhase Local CA</b>. <i>This step is required — the certificate does nothing without it.</i>",
  ],
  macos: [
    "Download the certificate below, then double-click the downloaded file to add it to your login keychain.",
    "Open <b>Keychain Access</b>, find <b>InPhase Local CA</b>, double-click it, expand <b>Trust</b>, and set <b>When using this certificate</b> to <b>Always Trust</b>. Close the window and enter your password.",
  ],
  android: [
    "Download the certificate below.",
    "Settings → <b>Security &amp; privacy</b> → <b>More security settings</b> → <b>Encryption &amp; credentials</b> → <b>Install a certificate</b> → <b>CA certificate</b>, then pick the downloaded file and accept the warning. <i>(The exact path varies by phone — search settings for “CA certificate”.)</i>",
  ],
  windows: [
    "Download the certificate below.",
    "Double-click the file → <b>Install Certificate</b> → <b>Local Machine</b> (or Current User) → <b>Place all certificates in the following store</b> → <b>Browse</b> → <b>Trusted Root Certification Authorities</b> → OK → Finish → Yes.",
    "Restart your browser if it was open.",
  ],
  other: [
    "Download the certificate below and add it to your system's trusted root certificate authorities.",
  ],
};

/** Numbered steps for `platform`, ending with "open the https URL". Returns an
 *  `<ol>` plus a Firefox note. `caHref` is where the .crt is downloaded from. */
export function certInstructionsHtml(
  platform: Platform,
  httpsUrl: string,
  caHref = "/ca.crt",
): string {
  httpsUrl=escapeHtml(safeHttpUrl(httpsUrl));
  caHref=escapeHtml(safeHttpUrl(new URL(caHref,location.origin).href));
  const items = [
    ...STEPS[platform].map((s) =>
      s.includes("Download the certificate")
        ? s.replace(
            "Download the certificate",
            `<a href="${caHref}" download><b>Download the certificate</b></a>`,
          )
        : s,
    ),
    `Open <a href="${httpsUrl}"><b>${httpsUrl}</b></a>.`,
  ];
  return `
    <ol style="margin:.2rem 0;padding-left:1.2rem;line-height:1.65">
      ${items.map((s) => `<li style="margin:.35rem 0">${s}</li>`).join("")}
    </ol>
    <details style="margin-top:.4rem">
      <summary class="k">Using Firefox?</summary>
      <div class="k" style="margin-top:.3rem">
        Firefox keeps its own certificate store. In <b>about:config</b> set
        <b>security.enterprise_roots.enabled</b> to <b>true</b> (it then trusts
        the certificate you just installed in the OS), or import the file under
        Settings → Privacy &amp; Security → Certificates → View Certificates →
        Authorities → Import.
      </div>
    </details>`;
}
