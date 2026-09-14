// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Build identity, and the stale-page guard built on it.
//
// Every "black screen" traced to a client running old code against a new host:
// a cached bundle, a restored tab, a wire format changed under a page nobody
// reloaded, a message tag renamed on one side only. The protocol-version
// constants (`SIGNALING_PROTOCOL_VERSION` and friends) were supposed to catch
// that, but they are bumped by hand and so are bumped late or never — the
// `client_telemetry` / `telemetry` rename shipped without touching one.
//
// `tools/ship.sh` stamps the same `INPHASE_BUILD_ID` into the Vite bundle and
// into the host binary that embeds it. So a page and the host it is talking to
// agree iff they came from the same ship. That holds for *any* change to any
// wire format, with nothing to remember, which is the property the hand-bumped
// constants never had.

declare const __INPHASE_BUILD_ID__: string;

/** The build this bundle was compiled in. `"dev"` outside `tools/ship.sh`. */
export const BUILD_ID: string = __INPHASE_BUILD_ID__;

/** One reload per host build, so a genuine mismatch reports instead of looping. */
const RELOADED_FOR = "inphase_reloaded_for_build";

/**
 * Compare this bundle's build id against the host's and reload once if they
 * differ. Safe to call repeatedly.
 *
 * Returns `"match"`, `"skipped"` (either side is an unshipped `dev` build, so
 * a difference proves nothing), `"unknown"` (status unreachable or from a host
 * too old to report an id), or `"reloading"`.
 */
export async function checkBuildId(): Promise<
  "match" | "skipped" | "unknown" | "reloading"
> {
  if (BUILD_ID === "dev") return "skipped";

  let hostId: string | undefined;
  try {
    const r = await fetch("/api/v1/status", { cache: "no-store" });
    if (!r.ok) return "unknown";
    hostId = (await r.json())?.build_id;
  } catch {
    return "unknown"; // offline / host restarting — not evidence of staleness
  }

  // A host that does not report an id predates this check; nothing to compare.
  if (typeof hostId !== "string" || hostId === "" || hostId === "dev")
    return "skipped";
  if (hostId === BUILD_ID) return "match";

  // Reload at most once per host build. If we already reloaded for this exact
  // host id and still disagree, the reload cannot fix it — the host is serving
  // a bundle that was not built alongside it (a ship that compiled the host
  // without rebuilding the web, say). Say so loudly rather than reload-loop.
  if (sessionStorage.getItem(RELOADED_FOR) === hostId) {
    console.error(
      `inphase: bundle ${BUILD_ID} still does not match host ${hostId} after a reload — ` +
        `the host is serving a bundle it was not built with; re-run tools/ship.sh`,
    );
    return "unknown";
  }

  try {
    sessionStorage.setItem(RELOADED_FOR, hostId);
  } catch {
    // Private mode with storage blocked: reloading without the guard risks a
    // loop, so prefer a stale page that logs over a page that thrashes.
    console.error(
      `inphase: bundle ${BUILD_ID} != host ${hostId}, but cannot record a reload guard`,
    );
    return "unknown";
  }
  console.warn(
    `inphase: bundle ${BUILD_ID} != host ${hostId} — host was updated; reloading`,
  );
  location.reload();
  return "reloading";
}

/**
 * Run {@link checkBuildId} now, and again whenever the tab comes back to the
 * foreground — a phone left in a pocket across a deploy wakes into the check
 * rather than into an unexplained black screen.
 */
export function guardStaleBundle(): void {
  void checkBuildId();
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") void checkBuildId();
  });
}
