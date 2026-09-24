// The play-page picker: "Whole desktop" plus every installed game the host
// found, as a searchable poster grid. Desktop is always first and is injected
// here (the host's `/api/v1/library` only returns games).

import type { StreamTarget } from "../settings.js";

export interface LibraryItem {
  id: string;
  name: string;
  kind: "desktop" | "game";
  poster_url?: string | null;
  source?: string | null;
  last_played?: number | null;
}

const SOURCE_LABEL: Record<string, string> = {
  steam: "Steam",
  epic: "Epic",
  gog: "GOG",
  xbox: "Xbox",
  battlenet: "Battle.net",
  ea: "EA",
  ubisoft: "Ubisoft",
  riot: "Riot",
  amazon: "Prime",
  registry: "PC",
};

function sourceLabel(s?: string | null): string {
  if (!s) return "";
  return (
    SOURCE_LABEL[s.toLowerCase()] ?? s.replace(/^\w/, (c) => c.toUpperCase())
  );
}

const DESKTOP_POSTER =
  "data:image/svg+xml," +
  encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 180">
    <rect width="320" height="180" fill="#0e1220"/>
    <rect x="112" y="46" width="96" height="62" rx="9" fill="none" stroke="#f4f6fb" stroke-width="6" opacity=".85"/>
    <rect x="152" y="108" width="16" height="16" fill="#f4f6fb" opacity=".85"/>
    <rect x="134" y="124" width="52" height="7" rx="3.5" fill="#f4f6fb" opacity=".85"/>
    <defs><linearGradient id="w" x1="0" x2="1"><stop offset="0" stop-color="#22d3ee"/><stop offset="1" stop-color="#8b5cf6"/></linearGradient></defs>
    <path d="M98 78c20-18 40-18 62 0s42 18 62 0" fill="none" stroke="url(#w)" stroke-width="8" stroke-linecap="round"/>
  </svg>`);

/** A deterministic cover for a game with no real poster on disk: a gradient
 *  keyed off the name with its initials set large. The card prints the full
 *  title under the cover, so the art does not repeat it. */
function generatedPoster(name: string): string {
  const seed = [...name].reduce((a, c) => (a * 31 + c.charCodeAt(0)) >>> 0, 7);
  const hue = seed % 360;
  const hue2 = (hue + 40) % 360;
  const words = name
    .replace(/[^\p{L}\p{N}\s]/gu, " ")
    .split(/\s+/)
    .filter(Boolean);
  const initials = (
    words.length > 1
      ? (words[0]![0] ?? "") + (words[1]![0] ?? "")
      : (words[0] ?? "?").slice(0, 2)
  ).toUpperCase();
  return (
    "data:image/svg+xml," +
    encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 450">
      <defs><linearGradient id="g" x1="0" y1="0" x2="0.6" y2="1">
        <stop offset="0" stop-color="hsl(${hue} 34% 30%)"/>
        <stop offset="1" stop-color="hsl(${hue2} 30% 12%)"/>
      </linearGradient></defs>
      <rect width="300" height="450" fill="url(#g)"/>
      <text x="150" y="262" text-anchor="middle" fill="#fff" fill-opacity="0.9"
        font-family="system-ui,Segoe UI,Roboto,sans-serif" font-size="112" font-weight="700" letter-spacing="-4">${escapeXml(initials)}</text>
    </svg>`)
  );
}

function escapeXml(s: string): string {
  return s.replace(
    /[&<>]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]!,
  );
}

export interface Library {
  items: LibraryItem[];
  /** Host is still fetching cover art; caller should poll again. */
  artPending: boolean;
  error?: boolean;
}

export async function fetchLibrary(): Promise<Library> {
  const desktop: LibraryItem = {
    id: "desktop",
    name: "Whole desktop",
    kind: "desktop",
    poster_url: DESKTOP_POSTER,
  };
  try {
    const r = await fetch("/api/v1/library", {
      signal: AbortSignal.timeout(7000),
    });
    if (!r.ok) return { items: [desktop], artPending: false, error: true };
    const data = (await r.json()) as {
      items?: LibraryItem[];
      art_pending?: boolean;
    };
    // Most recently played first; titles whose launcher records no recency
    // sort after the dated ones, alphabetically.
    const games = (data.items ?? [])
      .filter((i) => i.kind === "game" && i.name.trim())
      .sort((a, b) => {
        const at = a.last_played ?? 0;
        const bt = b.last_played ?? 0;
        if (at !== bt) return bt - at;
        return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
      });
    return {
      items: [desktop, ...games],
      artPending: data.art_pending === true,
    };
  } catch {
    return { items: [desktop], artPending: false, error: true };
  }
}

export function posterFor(item: LibraryItem): string {
  if (item.poster_url) return item.poster_url;
  if (item.kind === "desktop") return DESKTOP_POSTER;
  return generatedPoster(item.name);
}

export function targetFor(item: LibraryItem): StreamTarget {
  if (item.kind === "desktop") return { type: "desktop" };
  return { type: "game", id: item.id, name: item.name };
}

export function sameTarget(a: StreamTarget, b: StreamTarget): boolean {
  if (a.type === "desktop") return b.type === "desktop";
  return b.type === "game" && a.id === b.id;
}

/** Resolve a target to something worth showing the user. */
export function targetLabel(
  t: StreamTarget | null | undefined,
  items: LibraryItem[],
): string {
  if (!t) return "";
  if (t.type === "desktop") return "Whole desktop";
  return items.find((i) => i.id === t.id)?.name ?? t.name ?? "a game";
}

export interface GridState {
  items: LibraryItem[];
  selected: StreamTarget;
  /** What the host is currently streaming to some device, if anything. */
  active: StreamTarget | null;
  query: string;
}

export function mountLibraryGrid(
  host: HTMLElement,
  state: GridState,
  onPick: (t: StreamTarget) => void,
): void {
  const q = state.query.trim().toLowerCase();
  const visible = state.items.filter(
    (i) => i.kind === "desktop" || !q || i.name.toLowerCase().includes(q),
  );

  if (visible.length === 0 && q) {
    host.innerHTML = `<p class="library-empty">No games match “${escapeHtml(state.query)}”.</p>`;
    return;
  }

  if (!visible.length) {
    host.innerHTML =
      '<div class="library-empty"><strong>No games found</strong>InPhase looks in Steam, Epic, GOG, Xbox and other launchers on your PC. You can always stream the desktop.</div>';
    return;
  }
  // Cards go straight into the host: it is `display: contents` inside the
  // page's grid, next to the desktop tile.
  host.innerHTML = `    ${visible
      .map((item) => {
        const t = targetFor(item);
        const isSel = sameTarget(t, state.selected);
        const isLive = state.active ? sameTarget(t, state.active) : false;
        const hasSrc = item.kind === "game" && item.source;
        return `<button type="button"
          class="lib-card${isSel ? " sel" : ""}${item.kind === "desktop" ? " desktop" : ""}"
          data-id="${escapeAttr(item.id)}" aria-pressed="${isSel}" aria-label="Select ${escapeAttr(item.name)}">
          <span class="lib-cover">
            <img src="${escapeAttr(posterFor(item))}" alt="" loading="lazy" decoding="async" />
            ${
              isLive || hasSrc
                ? `<span class="lib-badges">
                    ${isLive ? `<span class="lib-live">● Live</span>` : ""}
                    ${hasSrc ? `<span class="lib-src">${escapeHtml(sourceLabel(item.source))}</span>` : ""}
                  </span>`
                : ""
            }
            <span class="lib-check" aria-hidden="true">✓</span>
          </span>
          <span class="lib-name">${escapeHtml(item.name)}</span>
        </button>`;
      })
      .join("")}`;

  for (const btn of host.querySelectorAll<HTMLButtonElement>(".lib-card")) {
    btn.addEventListener("click", () => {
      const item = state.items.find((i) => i.id === btn.dataset["id"]);
      if (!item) return;
      onPick(targetFor(item));
      for (const b of host.querySelectorAll(".lib-card")) {
        b.classList.remove("sel");
        b.setAttribute("aria-pressed", "false");
      }
      btn.classList.add("sel");
      btn.setAttribute("aria-pressed", "true");
    });
  }

  for (const img of host.querySelectorAll<HTMLImageElement>(".lib-cover img")) {
    img.addEventListener(
      "error",
      () => {
        const name =
          img.closest(".lib-card")?.querySelector(".lib-name")?.textContent ??
          "";
        img.src = generatedPoster(name);
      },
      { once: true },
    );
  }
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ]!,
  );
}
function escapeAttr(s: string): string {
  return escapeHtml(s).replace(/`/g, "&#96;");
}
