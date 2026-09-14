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
  return SOURCE_LABEL[s.toLowerCase()] ?? s.replace(/^\w/, (c) => c.toUpperCase());
}

const DESKTOP_POSTER =
  "data:image/svg+xml," +
  encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 450">
    <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#243352"/><stop offset="1" stop-color="#0e1626"/>
    </linearGradient></defs>
    <rect width="300" height="450" fill="url(#g)"/>
    <rect x="54" y="132" width="192" height="120" rx="9" fill="none" stroke="#5aa2ff" stroke-width="4" opacity=".9"/>
    <rect x="66" y="146" width="168" height="92" rx="4" fill="#5aa2ff" opacity=".16"/>
    <rect x="132" y="252" width="36" height="20" fill="#5aa2ff" opacity=".6"/>
    <rect x="104" y="272" width="92" height="7" rx="3.5" fill="#5aa2ff" opacity=".6"/>
  </svg>`);

/** A calm, deterministic cover for a game with no real poster on disk —
 *  a two-tone gradient keyed off the name, with the title set across it. */
function generatedPoster(name: string): string {
  const seed = [...name].reduce((a, c) => (a * 31 + c.charCodeAt(0)) >>> 0, 7);
  const hue = seed % 360;
  const hue2 = (hue + 35) % 360;

  // Wrap the title into up to 4 lines that fit the 300-wide art.
  const words = name.split(/\s+/).filter(Boolean);
  const lines: string[] = [];
  let cur = "";
  for (const w of words) {
    const t = cur ? `${cur} ${w}` : w;
    if (t.length > 11 && cur) {
      lines.push(cur);
      cur = w;
    } else {
      cur = t;
    }
    if (lines.length === 3) break;
  }
  if (cur) lines.push(cur);
  if (lines.length > 4) lines.length = 4;
  // Size to fit the widest line inside ~250px (bold system font ≈ 0.6em/char),
  // then clamp so short and long titles both look deliberate.
  const widest = Math.max(...lines.map((l) => l.length), 1);
  const size = Math.max(24, Math.min(lines.length >= 3 ? 34 : 44, Math.floor(250 / (widest * 0.6))));
  const startY = 225 - ((lines.length - 1) * size * 1.15) / 2;
  const tspans = lines
    .map(
      (l, i) =>
        `<tspan x="150" y="${(startY + i * size * 1.15).toFixed(0)}">${escapeXml(l)}</tspan>`,
    )
    .join("");

  return (
    "data:image/svg+xml," +
    encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 450">
      <defs><linearGradient id="g" x1="0" y1="0" x2="0.7" y2="1">
        <stop offset="0" stop-color="hsl(${hue} 40% 32%)"/>
        <stop offset="1" stop-color="hsl(${hue2} 38% 14%)"/>
      </linearGradient></defs>
      <rect width="300" height="450" fill="url(#g)"/>
      <rect width="300" height="450" fill="#000" opacity="0.05"/>
      <text text-anchor="middle" fill="#fff" fill-opacity="0.94"
        font-family="system-ui,Segoe UI,Roboto,sans-serif" font-size="${size}" font-weight="750">${tspans}</text>
    </svg>`)
  );
}

function escapeXml(s: string): string {
  return s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]!);
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
    const r = await fetch("/api/v1/library", { signal: AbortSignal.timeout(7000) });
    if (!r.ok) return { items: [desktop], artPending: false, error: true };
    const data = (await r.json()) as { items?: LibraryItem[]; art_pending?: boolean };
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
    return { items: [desktop, ...games], artPending: data.art_pending === true };
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
export function targetLabel(t: StreamTarget | null | undefined, items: LibraryItem[]): string {
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
    host.innerHTML='<div class="library-empty"><strong>Your desktop is ready.</strong><p>No games found. Open a launcher on your PC, or stream the desktop to get started.</p></div>';
    return;
  }
  host.innerHTML = `<div class="library-grid" role="group" aria-label="Choose what to stream">
    ${visible
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
            <span class="lib-name">${escapeHtml(item.name)}</span>
          </span>
        </button>`;
      })
      .join("")}
  </div>`;

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
    img.addEventListener("error", () => {
      const name = img.closest(".lib-card")?.querySelector(".lib-name")?.textContent ?? "";
      img.src = generatedPoster(name);
    }, { once: true });
  }
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}
function escapeAttr(s: string): string {
  return escapeHtml(s).replace(/`/g, "&#96;");
}
