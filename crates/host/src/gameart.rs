// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyless game cover art.
//!
//! Steam's 600×900 vertical box art is the preferred cover for the library
//! grid — consistent and the right shape. For any game that is not already
//! showing Steam's own art from the local Steam client cache, resolve its name
//! (or a Steam appid we already scanned) to a Steam appid via the public
//! `storesearch` endpoint and pull `library_600x900` from the Steam CDN. When
//! that fails, the launcher's own poster (or a generated one) is the fallback.
//! These are the same unauthenticated endpoints the Steam website uses; **no
//! API key**.
//!
//! Everything is cached under `<config-dir>/artcache/`:
//! * `<slug>.jpg` — the downloaded cover
//! * `<slug>.miss` — an empty marker; a name with no match is not retried for a
//!   week, so the library page does not re-query Valve on every load.
//!
//! The library handler never blocks on this: a cache hit is one `stat`, a miss
//! returns `None` and schedules one background warm pass.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

const CDN: &str = "https://cdn.cloudflare.steamstatic.com/steam/apps";
const SEARCH: &str = "https://store.steampowered.com/api/storesearch/";
// A day, not a week: most misses are a transient Valve API hiccup, not a
// permanent "this game isn't on Steam" — retry sooner instead of leaving a
// game posterless for a week over a blip.
const MISS_TTL: Duration = Duration::from_secs(24 * 3600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(12);

/// One game the library page wants art for.
#[derive(Clone, Debug)]
pub struct Want {
    /// The library item id — also the cache slug source.
    pub id: String,
    pub name: String,
    /// Set when the game was scanned from Steam; skips the name search.
    pub steam_appid: Option<u32>,
}

pub struct ArtCache {
    dir: PathBuf,
    enabled: bool,
    client: reqwest::Client,
    /// Slugs with a warm pass in flight, so repeated `/api/v1/library` calls do
    /// not stack fetches.
    in_flight: Mutex<HashSet<String>>,
}

impl ArtCache {
    pub fn new(enabled: bool) -> Arc<Self> {
        let dir = crate::config::Config::config_dir().join("artcache");
        let _ = std::fs::create_dir_all(&dir);
        let client = reqwest::Client::builder()
            .user_agent("InPhase")
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            dir,
            enabled,
            client,
            in_flight: Mutex::new(HashSet::new()),
        })
    }

    /// The cached cover for `id`, if one has been downloaded.
    pub fn cached(&self, id: &str) -> Option<PathBuf> {
        let p = self.dir.join(format!("{}.jpg", slug(id)));
        p.is_file().then_some(p)
    }

    /// True while a warm pass still has fetches outstanding — the library page
    /// polls again while this is set, so Steam art replaces the launcher poster
    /// without a reload.
    pub fn busy(&self) -> bool {
        !self.in_flight.lock().is_empty()
    }

    fn missed_recently(&self, slug: &str) -> bool {
        let m = self.dir.join(format!("{slug}.miss"));
        std::fs::metadata(&m)
            .and_then(|md| md.modified())
            .map(|t| t.elapsed().unwrap_or(MISS_TTL) < MISS_TTL)
            .unwrap_or(false)
    }

    /// Fetch covers for every `want` that is not already cached or recently
    /// missed. Fire-and-forget; deduped by slug.
    pub fn warm(self: &Arc<Self>, wants: Vec<Want>) {
        if !self.enabled {
            return;
        }
        let todo: Vec<Want> = {
            let mut guard = self.in_flight.lock();
            wants
                .into_iter()
                .filter(|w| {
                    let s = slug(&w.id);
                    self.cached(&w.id).is_none() && !self.missed_recently(&s) && guard.insert(s)
                })
                .collect()
        };
        if todo.is_empty() {
            return;
        }
        tracing::debug!(count = todo.len(), "fetching keyless cover art");
        let this = self.clone();
        tokio::spawn(async move {
            for w in todo {
                this.fetch_one(&w).await;
                let s = slug(&w.id);
                this.in_flight.lock().remove(&s);
                // Be gentle with Valve's endpoints.
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
        });
    }

    async fn fetch_one(&self, want: &Want) {
        let s = slug(&want.id);
        let appid = match want.steam_appid {
            Some(id) => Some(id),
            None => self.resolve_appid(&want.name).await,
        };
        let Some(appid) = appid else {
            tracing::debug!(game = %want.name, "no Steam match for cover art");
            self.mark_miss(&s);
            return;
        };
        // No `header.jpg` fallback: it's a 460x215 landscape store banner, and
        // stretched into the grid's 2:3 poster tile it just looks smeared.
        // Better to leave the game posterless (the client shows a clean
        // generated cover) than to show that.
        for file in ["library_600x900_2x.jpg", "library_600x900.jpg"] {
            if let Some(bytes) = self.get_image(&format!("{CDN}/{appid}/{file}")).await {
                let out = self.dir.join(format!("{s}.jpg"));
                if std::fs::write(&out, &bytes).is_ok() {
                    let _ = std::fs::remove_file(self.dir.join(format!("{s}.miss")));
                    tracing::debug!(game = %want.name, appid, file, "cached Steam cover art");
                    return;
                }
            }
        }
        tracing::debug!(game = %want.name, appid, "Steam appid resolved but no usable cover");
        self.mark_miss(&s);
    }

    fn mark_miss(&self, slug: &str) {
        let _ = std::fs::write(self.dir.join(format!("{slug}.miss")), []);
    }

    /// Best-effort name → Steam appid via the public store search.
    async fn resolve_appid(&self, name: &str) -> Option<u32> {
        #[derive(serde::Deserialize)]
        struct Resp {
            items: Vec<Item>,
        }
        #[derive(serde::Deserialize)]
        struct Item {
            #[serde(rename = "type")]
            kind: String,
            name: String,
            id: u32,
        }
        let resp: Resp = match self
            .client
            .get(SEARCH)
            .query(&[("term", name), ("cc", "US"), ("l", "en")])
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(%name, "store search parse failed: {e}");
                    return None;
                }
            },
            Err(e) => {
                tracing::debug!(%name, "store search request failed: {e}");
                return None;
            }
        };
        pick_appid(
            name,
            resp.items
                .iter()
                .map(|i| (i.kind.as_str(), i.name.as_str(), i.id)),
        )
    }

    async fn get_image(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !ct.starts_with("image/") {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        (bytes.len() > 512).then(|| bytes.to_vec())
    }
}

/// Choose the appid for the base game from a search result list.
fn pick_appid<'a>(
    query: &str,
    items: impl Iterator<Item = (&'a str, &'a str, u32)>,
) -> Option<u32> {
    let q = norm(query);
    let mut best: Option<(u32, u32)> = None; // (score, appid)
    for (kind, name, id) in items {
        if kind != "app" {
            continue;
        }
        let n = norm(name);
        // Skip editions/bundles/DLC the query did not ask for.
        const EXTRA: &[&str] = &[
            "dlc",
            "bundle",
            "pack",
            "season pass",
            "soundtrack",
            "edition",
            "upgrade",
            "starter pack",
            "battle pass",
        ];
        if EXTRA.iter().any(|w| n.contains(w) && !q.contains(w)) {
            continue;
        }
        let score = if n == q {
            100
        } else if n.starts_with(&q) || q.starts_with(&n) {
            60
        } else if n.contains(&q) || q.contains(&n) {
            30
        } else {
            0
        };
        if score == 0 {
            continue;
        }
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, id));
        }
    }
    best.map(|(_, id)| id)
}

/// Lower-case, strip ®™© and punctuation, collapse whitespace, and fold
/// standalone Roman numerals to digits so "Modern Warfare 3" == "… III".
fn norm(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned
        .split_whitespace()
        .map(|w| match w {
            "i" => "1",
            "ii" => "2",
            "iii" => "3",
            "iv" => "4",
            "v" => "5",
            "vi" => "6",
            "vii" => "7",
            "viii" => "8",
            "ix" => "9",
            "x" => "10",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Filesystem-safe cache key from a library item id.
fn slug(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_safe() {
        assert_eq!(slug("xbox:38985CA0.MWIIIGame"), "xbox_38985ca0_mwiiigame");
        assert_eq!(slug("steam:730"), "steam_730");
    }

    #[test]
    fn picks_base_game_over_editions_and_dlc() {
        let items = [
            (
                "app",
                "Baldur's Gate 3 - Digital Deluxe Edition DLC",
                2378500u32,
            ),
            ("app", "Baldur's Gate 3", 1086940),
            ("bundle", "Baldur's Gate Bundle", 999),
        ];
        assert_eq!(
            pick_appid("Baldur's Gate 3", items.iter().copied()),
            Some(1086940)
        );
    }

    #[test]
    fn matches_despite_trademark_noise() {
        let items = [("app", "Call of Duty®: Modern Warfare® III", 3595270u32)];
        assert_eq!(
            pick_appid("Call of Duty- Modern Warfare 3", items.iter().copied()),
            Some(3595270)
        );
    }

    #[test]
    fn no_match_returns_none() {
        let items = [("app", "Totally Unrelated Game", 1u32)];
        assert_eq!(
            pick_appid("Some Obscure Title", items.iter().copied()),
            None
        );
    }
}
