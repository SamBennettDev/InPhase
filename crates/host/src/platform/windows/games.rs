//! Installed-game discovery for the play-page library grid.
//!
//! Store-specific scanners run first (Steam, Epic, GOG, Xbox, Battle.net,
//! Ubisoft, EA, Riot, Amazon). A Windows Uninstall-registry pass fills gaps.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::platform::GameInfo;

use super::posters;

static POSTER_INDEX: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

fn poster_index() -> &'static Mutex<HashMap<String, PathBuf>> {
    POSTER_INDEX.get_or_init(|| Mutex::new(HashMap::new()))
}

fn refresh_poster_index(games: &[GameInfo]) {
    let mut idx = poster_index().lock();
    idx.clear();
    for g in games {
        if let Some(p) = &g.poster_path {
            idx.insert(g.id.clone(), p.clone());
        }
    }
}

/// Launch a scanned library game.
///
/// Returns `Ok(true)` when a launch was actually kicked off, `Ok(false)` when
/// the game already appears to be running (stream it as-is) or there is no
/// known way to start it. Never blocks on the game — `start` returns as soon
/// as the store/exe is handed off.
pub fn launch_game(id: &str) -> anyhow::Result<bool> {
    let games = enumerate_installed_games()?;
    let g = games
        .iter()
        .find(|g| g.id == id)
        .ok_or_else(|| anyhow::anyhow!("`{id}` is not in the game library"))?;

    // For Xbox games `exe_name` is an AUMID, not a process name — skip the
    // "already running" shortcut (re-launching a UWP app just focuses it).
    if g.source != "xbox" {
        if let Some(exe) = g.exe_name.as_deref() {
            if process_running(exe) {
                tracing::info!(game = %g.name, "game already running - streaming as-is");
                return Ok(false);
            }
        }
    }

    let (target, cwd) = match launch_target(g) {
        Some(v) => v,
        None => {
            tracing::warn!(
                game = %g.name, source = %g.source,
                "no known launch method - streaming the desktop instead"
            );
            return Ok(false);
        }
    };

    tracing::info!(game = %g.name, %target, "launching game");
    let mut cmd = if target.starts_with("shell:") {
        // UWP / Store apps activate by shell moniker — `start` silently no-ops
        // on these, `explorer.exe` does it.
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(&target);
        c
    } else {
        // `cmd /C start "" <target>` runs in the host's interactive session,
        // handles both `steam://` URIs and bare exe paths, and detaches at once.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", &target]);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("start `{}`: {e}", g.name))?;
    record_launch(&g.id);
    Ok(true)
}

/// InPhase's own play-recency ledger: game id -> last launch (unix secs).
/// The library sorts on this; launcher-side recency fields are not consulted
/// (user directive: recency is what InPhase observed, not what Steam claims).
fn recent_path() -> std::path::PathBuf {
    crate::config::Config::config_dir().join("recent.json")
}

fn load_recent() -> std::collections::HashMap<String, u64> {
    std::fs::read_to_string(recent_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn record_launch(id: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut m = load_recent();
    m.insert(id.to_string(), now);
    if let Ok(j) = serde_json::to_string(&m) {
        if let Err(e) = std::fs::write(recent_path(), j) {
            tracing::warn!("play-recency ledger write failed: {e}");
        }
    }
}

/// `(what to hand to `start`, working directory for an exe)`.
fn launch_target(g: &GameInfo) -> Option<(String, Option<String>)> {
    if g.source == "steam" {
        if let Some(app) = g.steam_app_id {
            return Some((format!("steam://rungameid/{app}"), None));
        }
    }
    // Xbox / Game Pass: `exe_name` carries the `<family>!<app-id>` AUMID. UWP
    // games have no directly runnable exe — the shell launches them by moniker.
    if g.source == "xbox" {
        let aumid = g.exe_name.as_deref()?;
        return Some((format!(r"shell:AppsFolder\{aumid}"), None));
    }
    // Every other store: the executable we recorded, resolved against its
    // install directory (Epic's LaunchExecutable is relative). A bare exe name
    // with no install dir is not launchable on its own.
    let exe = g.exe_name.as_deref()?;
    if g.install_dir.is_empty() {
        return None;
    }
    let dir = g.install_dir.trim_end_matches(['\\', '/']);
    let rel = exe.trim_start_matches(['\\', '/']).replace('/', "\\");
    Some((format!(r"{dir}\{rel}"), Some(dir.to_string())))
}

/// True if a process with this executable name is currently running.
fn process_running(exe_name: &str) -> bool {
    let base = std::path::Path::new(exe_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(exe_name)
        .to_ascii_lowercase();
    let Ok(out) = std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH", "/FI", &format!("IMAGENAME eq {base}")])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .to_ascii_lowercase()
        .contains(&base)
}

/// Resolve a previously scanned local poster path (populated by [`enumerate_installed_games`]).
pub fn poster_path_for_game_id(id: &str) -> Option<PathBuf> {
    if let Some(p) = poster_index().lock().get(id).cloned() {
        return Some(p);
    }
    // Cold request before `/api/v1/library` — scan once to populate the index.
    let _ = enumerate_installed_games();
    poster_index().lock().get(id).cloned()
}

/// Scan common PC game stores and return unique titles, sorted A→Z.
pub fn enumerate_installed_games() -> anyhow::Result<Vec<GameInfo>> {
    let mut seen_names = HashSet::new();
    let mut out = Vec::new();
    let scanners: &[fn() -> anyhow::Result<Vec<GameInfo>>] = &[
        scan_steam,
        scan_epic,
        scan_gog,
        scan_xbox,
        scan_battlenet,
        scan_ubisoft,
        scan_ea,
        scan_riot,
        scan_amazon,
        scan_registry_games,
    ];
    for scan in scanners {
        for g in scan()? {
            // Store clients, runtimes, unresolved localization keys, and other
            // non-games leak out of nearly every scanner — filter them here,
            // once, rather than in ten places.
            if is_junk_name(&g.name) {
                continue;
            }
            let key = g.name.to_ascii_lowercase();
            if seen_names.insert(key) {
                out.push(g);
            }
        }
    }
    out.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    // Recency is InPhase's own ledger (user directive): the client sorts on
    // this, so only games actually launched through the host carry a date.
    let recent = load_recent();
    for g in &mut out {
        g.last_played = recent.get(&g.id).copied();
    }
    refresh_poster_index(&out);
    Ok(out)
}

fn scan_steam() -> anyhow::Result<Vec<GameInfo>> {
    let mut libs = Vec::new();
    if let Some(p) = windows_registry::steam_install_path() {
        libs.push(PathBuf::from(p));
    }
    libs.extend(steam_library_paths()?);
    let mut out = Vec::new();
    for lib in libs {
        let apps = lib.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&apps) else {
            continue;
        };
        for ent in entries.flatten() {
            let path = ent.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
                continue;
            }
            if let Some(game) = parse_steam_manifest(&lib, &path) {
                out.push(game);
            }
        }
    }
    Ok(out)
}

fn steam_library_paths() -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let steam = match windows_registry::steam_install_path() {
        Some(p) => PathBuf::from(p),
        None => return Ok(out),
    };
    let vdf = steam.join("steamapps").join("libraryfolders.vdf");
    let text = match std::fs::read_to_string(&vdf) {
        Ok(t) => t,
        Err(_) => return Ok(out),
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"path\"") {
            if let Some(path) = parse_vdf_quoted_value(rest) {
                out.push(PathBuf::from(path.replace("\\\\", "\\")));
            }
        }
    }
    Ok(out)
}

fn parse_steam_manifest(steam_lib: &Path, path: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let appid = vdf_field(&text, "appid")?;
    let name = vdf_field(&text, "name")?;
    if name.is_empty() || name == "Steamworks Common Redistributables" {
        return None;
    }
    let installdir = vdf_field(&text, "installdir").unwrap_or_default();
    let id = format!("steam:{appid}");
    let install_path = steam_lib.join("steamapps").join("common").join(&installdir);
    let poster_file = posters::steam_library_poster(steam_lib, &appid)
        .or_else(|| posters::install_dir_poster(&install_path));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name,
        source: "steam".into(),
        steam_app_id: Some(appid.parse().ok()?),
        epic_catalog_id: None,
        install_dir: installdir,
        exe_name: None,
        poster_url,
        poster_path,

        last_played: None,
    })
}

fn scan_epic() -> anyhow::Result<Vec<GameInfo>> {
    let base = PathBuf::from(r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("item") {
            continue;
        }
        if let Some(game) = parse_epic_manifest(&path) {
            out.push(game);
        }
    }
    Ok(out)
}

fn parse_epic_manifest(path: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = v.get("DisplayName")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let catalog = v
        .get("CatalogItemId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let id = if catalog.is_empty() {
        format!("epic:{}", slug(name))
    } else {
        format!("epic:{catalog}")
    };
    let exe = v
        .get("LaunchExecutable")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let install_dir = v
        .get("InstallLocation")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let install = PathBuf::from(install_dir.replace('/', "\\"));
    let poster_file = posters::install_dir_poster(&install);
    let (mut poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    if poster_url.is_none() {
        poster_url = epic_poster(&v);
    }
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "epic".into(),
        steam_app_id: None,
        epic_catalog_id: if catalog.is_empty() {
            None
        } else {
            Some(catalog)
        },
        install_dir,
        exe_name: exe,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn epic_poster(v: &serde_json::Value) -> Option<String> {
    let images = v.get("KeyImages")?.as_array()?;
    for pref in ["DieselGameBoxWide", "DieselGameBox", "Thumbnail"] {
        if let Some(url) = images.iter().find_map(|img| {
            let ty = img.get("Type")?.as_str()?;
            if ty == pref {
                img.get("Url").and_then(|u| u.as_str())
            } else {
                None
            }
        }) {
            return Some(url.to_string());
        }
    }
    images
        .first()
        .and_then(|img| img.get("Url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn scan_gog() -> anyhow::Result<Vec<GameInfo>> {
    let mut roots = Vec::new();
    if let Some(p) = windows_registry::gog_library_paths() {
        roots.extend(p);
    }
    for p in [r"C:\GOG Games", r"D:\GOG Games", r"E:\GOG Games"] {
        roots.push(PathBuf::from(p));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        scan_gog_info_files(&root, &mut out, &mut seen);
        let games = root.join("Games");
        if games.is_dir() {
            scan_gog_info_files(&games, &mut out, &mut seen);
        }
    }
    Ok(out)
}

fn scan_gog_info_files(dir: &Path, out: &mut Vec<GameInfo>, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let path = ent.path();
        if path.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&path) {
                for sub_ent in sub.flatten() {
                    let sub_path = sub_ent.path();
                    if let Some(name) = sub_path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("goggame-") && name.ends_with(".info") {
                            if let Some(g) = parse_gog_info(&sub_path) {
                                if seen.insert(g.id.clone()) {
                                    out.push(g);
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("goggame-") && name.ends_with(".info") {
            if let Some(g) = parse_gog_info(&path) {
                if seen.insert(g.id.clone()) {
                    out.push(g);
                }
            }
        }
    }
}

fn parse_gog_info(path: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = v
        .get("name")
        .or_else(|| v.get("gameName"))
        .and_then(|x| x.as_str())?
        .trim();
    if name.is_empty() {
        return None;
    }
    let game_id = v
        .get("gameId")
        .or_else(|| v.get("rootGameId"))
        .and_then(|x| x.as_str())
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("goggame-"))
        })
        .unwrap_or(name);
    let install_dir = v
        .get("installDirectory")
        .or_else(|| v.get("path"))
        .and_then(|x| x.as_str())
        .map(|s| s.replace("\\\\", "\\"))
        .unwrap_or_else(|| {
            path.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    let exe_name = v
        .get("playTasks")
        .and_then(|t| t.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| t.get("path"))
        .and_then(|p| p.as_str())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let id = format!("gog:{game_id}");
    let install = PathBuf::from(&install_dir);
    let poster_file = posters::gog_webcache_poster(game_id)
        .or_else(|| posters::install_dir_poster(&install))
        .or_else(|| path.parent().and_then(posters::install_dir_poster));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "gog".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir,
        exe_name,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn scan_xbox() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    // The Xbox app installs to any fixed drive the user picks, not just C:/D:.
    // `read_dir` fails fast on a drive letter that isn't mounted, so probing
    // C..Z is cheap.
    for drive in b'C'..=b'Z' {
        let base = PathBuf::from(format!("{}:\\XboxGames", drive as char));
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for ent in entries.flatten() {
            let dir = ent.path();
            if !dir.is_dir() {
                continue;
            }
            let manifest = dir.join("Content").join("appxmanifest.xml");
            if let Some(game) = parse_xbox_manifest(&manifest, &dir) {
                out.push(game);
            }
        }
    }
    Ok(out)
}

fn parse_xbox_manifest(path: &Path, install_dir: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;

    // A real game has a launchable `<Application>`; the DLC / "game stub"
    // packages the Xbox app drops next to it (e.g. "MW3 PC MS DLC01 Game Stub")
    // carry none. Require it — this also gives us the AUMID to launch with.
    let (app_id, _exe) = xbox_application(&text)?;
    let identity = xbox_tag_attr(&text, "<Identity", "Name")?;
    let publisher = xbox_tag_attr(&text, "<Identity", "Publisher");

    let name = xml_element_text(&text, "DisplayName")
        .or_else(|| xbox_tag_attr(&text, "<uap:VisualElements", "DisplayName"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("ms-resource:"))
        .or_else(|| {
            install_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let id = format!("xbox:{identity}");
    // shell:AppsFolder AUMID = <PackageFamilyName>!<AppId>. The family name is
    // <identity>_<hash>, where the hash is derived from the Publisher string
    // (offline, deterministic); fall back to the registered-package list.
    // Without either the game still lists, it just can't be launched.
    let family = publisher
        .as_deref()
        .map(|p| format!("{identity}_{}", package_family_hash(p)))
        .or_else(|| xbox_family_name(&identity));
    let launch = family.map(|fam| format!("{fam}!{app_id}"));

    let poster_file = posters::install_dir_poster(install_dir);
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "xbox".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir: install_dir.to_string_lossy().to_string(),
        exe_name: launch,
        poster_url,
        poster_path,
    
        last_played: None,})
}

/// The first `<Application Id="…" … Executable="…">` in an appxmanifest.
/// Returns `(id, executable)`; `None` when there is no `<Application>` element
/// (`<Applications>` is not a match).
fn xbox_application(xml: &str) -> Option<(String, String)> {
    let mut from = 0;
    loop {
        let start = from + xml[from..].find("<Application")?;
        let after = xml[start + "<Application".len()..].chars().next();
        if matches!(after, Some(c) if c.is_whitespace() || c == '>' || c == '/') {
            let end = xml[start..].find('>')? + start;
            let tag = &xml[start..end];
            return Some((
                xml_attr(tag, "Id")?,
                xml_attr(tag, "Executable").unwrap_or_default(),
            ));
        }
        from = start + "<Application".len();
    }
}

/// Read one attribute from a specific opening tag (`open` = `"<Identity"` etc.),
/// so we don't pick up a same-named attribute on an unrelated element.
fn xbox_tag_attr(xml: &str, open: &str, attr: &str) -> Option<String> {
    let start = xml.find(open)?;
    let end = xml[start..].find('>')? + start;
    xml_attr(&xml[start..end], attr)
}

/// The 13-char publisher hash in an MSIX package family name (`…_<hash>`).
///
/// Documented algorithm: SHA-256 of the UTF-16LE `Publisher` string, first 8
/// bytes, Crockford-style base32 (`0-9 a-z` minus `i l o u`) over 65 bits
/// (64 + one pad bit). `CN=Microsoft Corporation, …` → `8wekyb3d8bbwe`.
fn package_family_hash(publisher: &str) -> String {
    use sha2::{Digest, Sha256};
    const ALPHA: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let utf16: Vec<u8> = publisher
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let digest = Sha256::digest(utf16);
    let bits = (u64::from_be_bytes(digest[..8].try_into().unwrap()) as u128) << 1;
    (0..13)
        .map(|i| ALPHA[((bits >> (60 - 5 * i)) & 0x1f) as usize] as char)
        .collect()
}

/// `<identity>_<hash>` for the AUMID, from the current user's registered MSIX
/// packages (`…\AppModel\Repository\Packages`, whose subkey names are package
/// full names: `Name_Version_Arch_ResourceId_Hash`). Fallback for when the
/// manifest has no `Publisher` attribute.
fn xbox_family_name(identity: &str) -> Option<String> {
    let prefix = format!("{identity}_");
    for full in windows_registry::appx_package_full_names() {
        if full.starts_with(&prefix) {
            let hash = full.rsplit('_').next().filter(|h| !h.is_empty())?;
            return Some(format!("{identity}_{hash}"));
        }
    }
    None
}

fn scan_battlenet() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    for entry in windows_registry::uninstall_entries()? {
        if !is_battlenet_publisher(&entry.publisher) {
            continue;
        }
        if let Some(g) = game_from_uninstall("battlenet", &entry) {
            out.push(g);
        }
    }
    Ok(out)
}

fn is_battlenet_publisher(publisher: &str) -> bool {
    let p = publisher.to_ascii_lowercase();
    p.contains("blizzard") || p.contains("battle.net")
}

fn scan_ubisoft() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    let config_dir = PathBuf::from(
        r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\cache\configuration\configurations",
    );
    if config_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&config_dir) {
            for ent in entries.flatten() {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(g) = parse_ubisoft_config(&path) {
                    out.push(g);
                }
            }
        }
    }
    for entry in windows_registry::uninstall_entries()? {
        if !entry.publisher.to_ascii_lowercase().contains("ubisoft") {
            continue;
        }
        if let Some(g) = game_from_uninstall("ubisoft", &entry) {
            out.push(g);
        }
    }
    Ok(out)
}

fn parse_ubisoft_config(path: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = v
        .get("displayName")
        .or_else(|| v.get("name"))
        .and_then(|x| x.as_str())?
        .trim();
    if name.is_empty() {
        return None;
    }
    let id = v
        .get("spaceId")
        .or_else(|| v.get("gameId"))
        .and_then(|x| x.as_str())
        .map(|s| format!("ubisoft:{s}"))
        .unwrap_or_else(|| format!("ubisoft:{}", slug(name)));
    let install_dir = v
        .get("installDir")
        .or_else(|| v.get("installPath"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let exe_name = v
        .get("executable")
        .and_then(|x| x.as_str())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let space_id = id.strip_prefix("ubisoft:").unwrap_or("");
    let poster_file = posters::ubisoft_asset_poster(space_id)
        .or_else(|| posters::install_dir_poster(&PathBuf::from(&install_dir)));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "ubisoft".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir,
        exe_name,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn scan_ea() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    for path in [
        PathBuf::from(r"C:\ProgramData\Electronic Arts\EA Desktop\Installed Games.json"),
        PathBuf::from(r"C:\ProgramData\Electronic Arts\EA Desktop\InstalledGames.json"),
    ] {
        if path.is_file() {
            out.extend(parse_ea_installed_games(&path)?);
        }
    }
    for entry in windows_registry::uninstall_entries()? {
        let p = entry.publisher.to_ascii_lowercase();
        if !p.contains("electronic arts") && !p.contains("ea ") && !p.contains("origin") {
            continue;
        }
        if let Some(g) = game_from_uninstall("ea", &entry) {
            out.push(g);
        }
    }
    Ok(out)
}

fn parse_ea_installed_games(path: &Path) -> anyhow::Result<Vec<GameInfo>> {
    let text = std::fs::read_to_string(path)?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let mut out = Vec::new();
    if let Some(obj) = v.as_object() {
        for (key, item) in obj {
            if let Some(g) = parse_ea_game_entry(key, item) {
                out.push(g);
            }
        }
    }
    if let Some(title_ids) = v.get("titleIds").and_then(|x| x.as_object()) {
        for (key, item) in title_ids {
            if let Some(g) = parse_ea_game_entry(key, item) {
                out.push(g);
            }
        }
    }
    if let Some(arr) = v.as_array() {
        for item in arr {
            let key = item
                .get("titleId")
                .or_else(|| item.get("id"))
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            if let Some(g) = parse_ea_game_entry(key, item) {
                out.push(g);
            }
        }
    }
    Ok(out)
}

fn parse_ea_game_entry(key: &str, item: &serde_json::Value) -> Option<GameInfo> {
    let name = item
        .get("displayName")
        .or_else(|| item.get("title"))
        .or_else(|| item.get("name"))
        .and_then(|x| x.as_str())?
        .trim();
    if name.is_empty() || is_junk_name(name) {
        return None;
    }
    let install_dir = item
        .get("installPath")
        .or_else(|| item.get("installLocation"))
        .or_else(|| item.get("path"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let exe_name = item
        .get("launchExecutable")
        .or_else(|| item.get("executable"))
        .and_then(|x| x.as_str())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let id = format!("ea:{key}");
    let poster_file = posters::install_dir_poster(&PathBuf::from(&install_dir));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "ea".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir,
        exe_name,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn scan_riot() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    let installs = PathBuf::from(r"C:\ProgramData\Riot Games\RiotClientInstalls.json");
    if installs.is_file() {
        if let Ok(text) = std::fs::read_to_string(&installs) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                out.extend(parse_riot_installs(&v));
            }
        }
    }
    let meta = PathBuf::from(r"C:\ProgramData\Riot Games\Metadata");
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&meta) {
            for ent in entries.flatten() {
                let dir = ent.path();
                if !dir.is_dir() {
                    continue;
                }
                let game_slug = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Ok(files) = std::fs::read_dir(&dir) {
                    for f in files.flatten() {
                        let path = f.path();
                        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if fname.ends_with(".product_settings.yaml")
                            || fname.ends_with(".installed.json")
                        {
                            if let Some(g) = parse_riot_metadata(&path, game_slug) {
                                out.push(g);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn parse_riot_installs(v: &serde_json::Value) -> Vec<GameInfo> {
    let mut out = Vec::new();
    let Some(assoc) = v.get("associated_client").and_then(|x| x.as_object()) else {
        return out;
    };
    for (_client, info) in assoc {
        let Some(patchlines) = info.get("patchlines").and_then(|x| x.as_object()) else {
            continue;
        };
        for (line, pl) in patchlines {
            let path = pl
                .get("path")
                .or_else(|| pl.get("install_path"))
                .and_then(|x| x.as_str());
            let Some(path) = path else { continue };
            let install = PathBuf::from(path.replace('/', "\\"));
            let name = install
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(line)
                .to_string();
            if name.eq_ignore_ascii_case("live") {
                continue;
            }
            let display = title_case_slug(&name);
            let id = format!("riot:{line}:{name}");
            let poster_file = posters::install_dir_poster(&install);
            let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
            out.push(GameInfo {
                id,
                name: display,
                source: "riot".into(),
                steam_app_id: None,
                epic_catalog_id: None,
                install_dir: install.to_string_lossy().to_string(),
                exe_name: None,
                poster_url,
                poster_path,
            
        last_played: None,});
        }
    }
    out
}

fn parse_riot_metadata(path: &Path, fallback_slug: &str) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let name = v
            .get("product_name")
            .or_else(|| v.get("name"))
            .and_then(|x| x.as_str())?;
        let id = v
            .get("product_id")
            .and_then(|x| x.as_str())
            .unwrap_or(fallback_slug);
        let id = format!("riot:{id}");
        let poster_file = path.parent().and_then(posters::install_dir_poster);
        let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
        return Some(GameInfo {
            id,
            name: name.to_string(),
            source: "riot".into(),
            steam_app_id: None,
            epic_catalog_id: None,
            install_dir: String::new(),
            exe_name: None,
            poster_url,
            poster_path,
        
        last_played: None,});
    }
    let name = yaml_field(&text, "product_name")
        .or_else(|| yaml_field(&text, "name"))
        .unwrap_or_else(|| title_case_slug(fallback_slug));
    let id = format!("riot:{fallback_slug}");
    let poster_file = path.parent().and_then(posters::install_dir_poster);
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name,
        source: "riot".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir: String::new(),
        exe_name: None,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn scan_amazon() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    for base in [
        r"C:\ProgramData\Amazon Games\Data\Games",
        r"C:\ProgramData\Amazon Games\AppData\Local\Amazon Games\Data\Games",
    ] {
        let base = PathBuf::from(base);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(g) = parse_amazon_manifest(&path) {
                    out.push(g);
                }
            } else if path.is_dir() {
                let manifest = path.join("manifest.json");
                if let Some(g) = parse_amazon_manifest(&manifest) {
                    out.push(g);
                }
            }
        }
    }
    for entry in windows_registry::uninstall_entries()? {
        if !entry.publisher.to_ascii_lowercase().contains("amazon") {
            continue;
        }
        if let Some(g) = game_from_uninstall("amazon", &entry) {
            out.push(g);
        }
    }
    Ok(out)
}

fn parse_amazon_manifest(path: &Path) -> Option<GameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = v
        .get("displayName")
        .or_else(|| v.get("title"))
        .or_else(|| v.get("name"))
        .and_then(|x| x.as_str())?
        .trim();
    if name.is_empty() {
        return None;
    }
    let id = v
        .get("id")
        .or_else(|| v.get("productId"))
        .and_then(|x| x.as_str())
        .map(|s| format!("amazon:{s}"))
        .unwrap_or_else(|| format!("amazon:{}", slug(name)));
    let install_dir = v
        .get("installDirectory")
        .or_else(|| v.get("installPath"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let poster_file = posters::install_dir_poster(&PathBuf::from(&install_dir));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: "amazon".into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir,
        exe_name: None,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn scan_registry_games() -> anyhow::Result<Vec<GameInfo>> {
    let mut out = Vec::new();
    for entry in windows_registry::uninstall_entries()? {
        if entry.system_component || entry.parent_key.is_some() {
            continue;
        }
        if !looks_like_game(&entry) {
            continue;
        }
        if let Some(g) = game_from_uninstall("registry", &entry) {
            out.push(g);
        }
    }
    Ok(out)
}

fn game_from_uninstall(source: &str, entry: &windows_registry::UninstallEntry) -> Option<GameInfo> {
    let name = entry.display_name.trim();
    if name.is_empty() || is_junk_name(name) {
        return None;
    }
    let install_dir = entry.install_location.clone();
    let exe_name = entry
        .display_icon
        .as_deref()
        .or(entry.uninstall_string.as_deref())
        .and_then(exe_from_path);
    let id = format!("{source}:{}", slug(name));
    let poster_file = posters::install_dir_poster(&PathBuf::from(&install_dir));
    let (poster_url, poster_path) = posters::attach_poster(&id, poster_file);
    Some(GameInfo {
        id,
        name: name.to_string(),
        source: source.into(),
        steam_app_id: None,
        epic_catalog_id: None,
        install_dir,
        exe_name,
        poster_url,
        poster_path,
    
        last_played: None,})
}

fn looks_like_game(entry: &windows_registry::UninstallEntry) -> bool {
    let publisher = entry.publisher.to_ascii_lowercase();
    const PUBLISHERS: &[&str] = &[
        "valve",
        "epic games",
        "gog",
        "blizzard",
        "ubisoft",
        "electronic arts",
        "riot",
        "amazon",
        "bethesda",
        "square enix",
        "rockstar",
        "activision",
        "2k",
        "capcom",
        "sega",
        "bandai",
        "xbox",
        "microsoft games",
        "playstation",
        "paradox",
        "frontier",
        "facepunch",
        "hazelight",
        "studio wildcard",
        "behaviour",
        "crytek",
        "id software",
        "gearbox",
        "bungie",
        "ncsoft",
        "larian",
        "cd projekt",
        "hoyoverse",
        "mihoyo",
        "warner",
        "wb games",
        "focus entertainment",
        "devolver",
        "annapurna",
        "supergiant",
        "klei",
        "obsidian",
        "insomniac",
        "nintendo",
    ];
    if PUBLISHERS.iter().any(|p| publisher.contains(p)) {
        return true;
    }
    let loc = entry.install_location.to_ascii_lowercase();
    if loc.contains("steamapps")
        || loc.contains("epic games")
        || loc.contains("gog games")
        || loc.contains("xboxgames")
        || loc.contains("riot games")
        || loc.contains("battle.net")
        || loc.contains("ubisoft")
        || loc.contains("origin games")
        || loc.contains("electronic arts")
    {
        return true;
    }
    false
}

fn is_junk_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() || n.len() < 2 {
        return true;
    }
    // Unresolved MSIX/UWP localization reference — the Xbox app leaks these.
    if n.starts_with("ms-resource:") || n.starts_with("@{") {
        return true;
    }
    // Store clients / helper apps, matched whole so a game called "Steamworld"
    // or "The Ubisoft Museum" survives.
    const LAUNCHERS: &[&str] = &[
        "steam",
        "steamvr",
        "battle.net",
        "blizzard battle.net",
        "ea",
        "ea app",
        "ea desktop",
        "origin",
        "epic games launcher",
        "epic online services",
        "epic games",
        "gog galaxy",
        "ubisoft connect",
        "uplay",
        "rockstar games launcher",
        "riot client",
        "amazon games",
        "xbox",
        "xbox game bar",
        "xbox game pass",
        "nvidia app",
        "nvidia geforce experience",
        "geforce experience",
        "discord",
        "wallpaper engine",
        "microsoft store",
        "roblox",
    ];
    if LAUNCHERS.iter().any(|j| n == *j) {
        return true;
    }
    // Substring matches for runtimes / build artefacts / non-title entries.
    const JUNK: &[&str] = &[
        "redistributable",
        "visual c++",
        "directx",
        " runtime",
        "sdk",
        "uninstall",
        "hotfix",
        " launcher",
        "steamworks",
        "easy anti-cheat",
        "anticheat",
        "battleye",
        "punkbuster",
        "vcredist",
        "physx",
        "nvidia ",
        "amd software",
        "microsoft edge",
        "webview",
        "dedicated server",
        "social club",
        " demo",
        " beta test",
        "closed beta",
        "public test",
        "early access",
        " dlc",
        "benchmark",
        "soundtrack",
        "art book",
        "artbook",
        "wallpaper",
        "bonus content",
        "season pass",
        " dlc ",
        "crash handler",
        "installer",
    ];
    JUNK.iter().any(|j| n.contains(j))
}

fn exe_from_path(s: &str) -> Option<String> {
    let s = s.trim_matches('"');
    let path = s.split(',').next().unwrap_or(s);
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.to_ascii_lowercase().ends_with(".exe"))
        .map(|s| s.to_string())
}

fn xml_attr(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xml.find(&needle)? + needle.len();
    let rest = &xml[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn xml_element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = &xml[start..];
    let close_open = after.find('>')? + 1;
    let inner = &after[close_open..];
    let close = format!("</{tag}>");
    let end = inner.find(&close)?;
    Some(inner[..end].trim().to_string())
}

fn yaml_field(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn title_case_slug(s: &str) -> String {
    s.split(['-', '_', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => {
                    let mut out = f.to_ascii_uppercase().to_string();
                    out.push_str(chars.as_str());
                    out
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn vdf_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&needle) {
            return parse_vdf_quoted_value(rest);
        }
    }
    None
}

fn parse_vdf_quoted_value(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-'); // collapse any run of separators to one dash
        }
    }
    out.trim_matches('-').to_string()
}

mod windows_registry {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_SZ,
    };

    pub struct UninstallEntry {
        pub display_name: String,
        pub install_location: String,
        pub publisher: String,
        pub display_icon: Option<String>,
        pub uninstall_string: Option<String>,
        pub system_component: bool,
        pub parent_key: Option<String>,
    }

    pub fn steam_install_path() -> Option<String> {
        read_reg_string(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Valve\Steam",
            "InstallPath",
        )
    }

    pub fn gog_library_paths() -> Option<Vec<std::path::PathBuf>> {
        let json = read_reg_string(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\GOG.com\GalaxyClient\paths",
            "json",
        )?;
        let v: serde_json::Value = serde_json::from_str(&json).ok()?;
        let mut out = Vec::new();
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    out.push(std::path::PathBuf::from(s));
                }
            }
        }
        if let Some(obj) = v.as_object() {
            for (_k, item) in obj {
                if let Some(s) = item.as_str() {
                    out.push(std::path::PathBuf::from(s));
                }
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Package full names of every MSIX/Appx package registered for the current
    /// user (`Name_Version_Arch_ResourceId_Hash`). Readable without elevation,
    /// unlike `C:\Program Files\WindowsApps`.
    pub fn appx_package_full_names() -> Vec<String> {
        enum_subkey_names(
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages",
        )
    }

    /// Immediate child key names of `root\subkey` (read-only enumeration).
    fn enum_subkey_names(root: HKEY, subkey: &str) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: opening a registry path for read-only key enumeration.
        unsafe {
            let mut key = HKEY::default();
            let path_w: Vec<u16> = format!("{subkey}\0").encode_utf16().collect();
            if RegOpenKeyExW(root, PCWSTR(path_w.as_ptr()), 0, KEY_READ, &mut key).is_err() {
                return out;
            }
            let mut index = 0u32;
            loop {
                let mut name_buf = [0u16; 512];
                let mut name_len = name_buf.len() as u32;
                if RegEnumKeyExW(
                    key,
                    index,
                    windows::core::PWSTR(name_buf.as_mut_ptr()),
                    &mut name_len,
                    None,
                    windows::core::PWSTR::null(),
                    None,
                    None,
                )
                .is_err()
                {
                    break;
                }
                index += 1;
                out.push(String::from_utf16_lossy(&name_buf[..name_len as usize]));
            }
            let _ = RegCloseKey(key);
        }
        out
    }

    pub fn uninstall_entries() -> anyhow::Result<Vec<UninstallEntry>> {
        let mut out = Vec::new();
        for subkey in [
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ] {
            out.extend(enum_uninstall_key(subkey)?);
        }
        Ok(out)
    }

    fn enum_uninstall_key(subkey: &str) -> anyhow::Result<Vec<UninstallEntry>> {
        let mut out = Vec::new();
        // SAFETY: opening a well-known registry path for read-only enumeration.
        unsafe {
            let mut key = HKEY::default();
            let path = format!("{subkey}\0");
            let path_w: Vec<u16> = path.encode_utf16().collect();
            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path_w.as_ptr()),
                0,
                KEY_READ,
                &mut key,
            )
            .is_err()
            {
                return Ok(out);
            }
            let mut index = 0u32;
            loop {
                let mut name_buf = [0u16; 256];
                let mut name_len = name_buf.len() as u32;
                let err = RegEnumKeyExW(
                    key,
                    index,
                    windows::core::PWSTR(name_buf.as_mut_ptr()),
                    &mut name_len,
                    None,
                    windows::core::PWSTR::null(),
                    None,
                    None,
                );
                if err.is_err() {
                    break;
                }
                index += 1;
                let child_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
                let child_path = format!("{subkey}\\{child_name}");
                if let Some(entry) = read_uninstall_entry(&child_path) {
                    out.push(entry);
                }
            }
            let _ = RegCloseKey(key);
        }
        Ok(out)
    }

    fn read_uninstall_entry(path: &str) -> Option<UninstallEntry> {
        let display_name = read_reg_string(HKEY_LOCAL_MACHINE, path, "DisplayName")?;
        let install_location =
            read_reg_string(HKEY_LOCAL_MACHINE, path, "InstallLocation").unwrap_or_default();
        let publisher = read_reg_string(HKEY_LOCAL_MACHINE, path, "Publisher").unwrap_or_default();
        let display_icon = read_reg_string(HKEY_LOCAL_MACHINE, path, "DisplayIcon");
        let uninstall_string = read_reg_string(HKEY_LOCAL_MACHINE, path, "UninstallString");
        let system_component = read_reg_dword(HKEY_LOCAL_MACHINE, path, "SystemComponent")
            .map(|v| v == 1)
            .unwrap_or(false);
        let parent_key = read_reg_string(HKEY_LOCAL_MACHINE, path, "ParentKeyName");
        Some(UninstallEntry {
            display_name,
            install_location,
            publisher,
            display_icon,
            uninstall_string,
            system_component,
            parent_key,
        })
    }

    fn read_reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
        // SAFETY: standard read-only registry query.
        unsafe {
            let mut key = HKEY::default();
            let sub = format!("{subkey}\0");
            let sub_w: Vec<u16> = sub.encode_utf16().collect();
            if RegOpenKeyExW(root, PCWSTR(sub_w.as_ptr()), 0, KEY_READ, &mut key).is_err() {
                return None;
            }
            let val = format!("{value}\0");
            let val_w: Vec<u16> = val.encode_utf16().collect();
            let mut ty = REG_SZ;
            let mut len = 0u32;
            let _ = RegQueryValueExW(
                key,
                PCWSTR(val_w.as_ptr()),
                None,
                Some(&mut ty as *mut _),
                None,
                Some(&mut len),
            );
            if len == 0 {
                let _ = RegCloseKey(key);
                return None;
            }
            let mut buf = vec![0u8; len as usize];
            let ok = RegQueryValueExW(
                key,
                PCWSTR(val_w.as_ptr()),
                None,
                Some(&mut ty as *mut _),
                Some(buf.as_mut_ptr()),
                Some(&mut len),
            )
            .is_ok();
            let _ = RegCloseKey(key);
            if !ok {
                return None;
            }
            if ty == REG_SZ {
                let wide: Vec<u16> = buf
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                let s = String::from_utf16_lossy(&wide);
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            } else {
                None
            }
        }
    }

    fn read_reg_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
        unsafe {
            let mut key = HKEY::default();
            let sub = format!("{subkey}\0");
            let sub_w: Vec<u16> = sub.encode_utf16().collect();
            if RegOpenKeyExW(root, PCWSTR(sub_w.as_ptr()), 0, KEY_READ, &mut key).is_err() {
                return None;
            }
            let val = format!("{value}\0");
            let val_w: Vec<u16> = val.encode_utf16().collect();
            let mut ty = REG_DWORD;
            let mut data = 0u32;
            let mut len = std::mem::size_of::<u32>() as u32;
            let ok = RegQueryValueExW(
                key,
                PCWSTR(val_w.as_ptr()),
                None,
                Some(&mut ty as *mut _),
                Some(&mut data as *mut u32 as *mut u8),
                Some(&mut len),
            )
            .is_ok();
            let _ = RegCloseKey(key);
            if ok && ty == REG_DWORD {
                Some(data)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vdf_field() {
        let text = r#"
"AppState"
{
    "appid"        "570"
    "name"         "Dota 2"
}
"#;
        assert_eq!(vdf_field(text, "appid").as_deref(), Some("570"));
        assert_eq!(vdf_field(text, "name").as_deref(), Some("Dota 2"));
    }

    #[test]
    fn xbox_manifest_parsing() {
        // A real game: has an <Application> with an Executable.
        let mw3 = r#"<?xml version="1.0"?>
<Package>
  <Identity Name="38985CA0.MWIIIGame" Publisher="CN=07A9AC0F" Version="1.0.13.0" />
  <Properties><DisplayName>Call of Duty: Modern Warfare III</DisplayName></Properties>
  <Applications>
    <Application Id="codShip" Executable="GameLaunchHelper.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="Call of Duty: Modern Warfare III" />
    </Application>
  </Applications>
</Package>"#;
        let (id, exe) = xbox_application(mw3).unwrap();
        assert_eq!(id, "codShip");
        assert_eq!(exe, "GameLaunchHelper.exe");
        assert_eq!(
            xbox_tag_attr(mw3, "<Identity", "Name").as_deref(),
            Some("38985CA0.MWIIIGame")
        );
        assert_eq!(
            xml_element_text(mw3, "DisplayName").as_deref(),
            Some("Call of Duty: Modern Warfare III")
        );

        // A DLC / stub package: no <Application> → not a game.
        let stub = r#"<Package>
  <Identity Name="38985CA0.MW3PCMSDLC01GameStub01" Version="0.0.9.0" />
  <Properties><DisplayName>MW3 PC MS DLC01 Game Stub 01</DisplayName></Properties>
</Package>"#;
        assert!(xbox_application(stub).is_none());
    }

    #[test]
    fn package_family_hash_matches_known_vectors() {
        // The universally-known one.
        assert_eq!(
            package_family_hash(
                "CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US"
            ),
            "8wekyb3d8bbwe"
        );
        // MW3's publisher → the hash Get-AppxPackage reports.
        assert_eq!(
            package_family_hash("CN=07A9AC0F-5502-4D92-BA69-01D5D39D1E92"),
            "5bkah9njm3e9g"
        );
    }

    #[test]
    fn junk_names_filtered() {
        // runtimes / build artefacts
        assert!(is_junk_name("Microsoft Visual C++ 2015 Redistributable"));
        assert!(is_junk_name("DirectX Runtime"));
        // store clients and helpers (the clutter in the screenshot)
        assert!(is_junk_name("Battle.net"));
        assert!(is_junk_name("EA app"));
        assert!(is_junk_name("Epic Online Services"));
        assert!(is_junk_name("Epic Games Launcher"));
        assert!(is_junk_name("Minecraft Launcher"));
        assert!(is_junk_name("Wallpaper Engine"));
        assert!(is_junk_name("ms-resource:AppDisplayName"));
        assert!(is_junk_name("MW4 DLC29 Beta Early Access 01"));
        assert!(is_junk_name("Rockstar Games Social Club"));
        assert!(is_junk_name(""));
        // real games must survive — including ones whose names contain a
        // launcher word as a substring.
        assert!(!is_junk_name("Call of Duty"));
        assert!(!is_junk_name("Counter-Strike 2"));
        assert!(!is_junk_name("SteamWorld Dig"));
        assert!(!is_junk_name("Overwatch"));
        assert!(!is_junk_name("ARC Raiders"));
        assert!(!is_junk_name("Grand Theft Auto V"));
    }

    #[test]
    fn slug_normalizes() {
        assert_eq!(slug("Call of Duty: MW3"), "call-of-duty-mw3");
    }

    fn gi(source: &str, appid: Option<u32>, dir: &str, exe: Option<&str>) -> GameInfo {
        GameInfo {
            id: "x".into(),
            name: "X".into(),
            source: source.into(),
            steam_app_id: appid,
            epic_catalog_id: None,
            install_dir: dir.into(),
            exe_name: exe.map(String::from),
            poster_url: None,
            poster_path: None,
            last_played: None,
        }
    }

    #[test]
    fn launch_target_per_store() {
        // Steam always goes through the client so its overlay / cloud saves work.
        let (t, cwd) = launch_target(&gi("steam", Some(730), "C:\\g\\cs2", None)).unwrap();
        assert_eq!(t, "steam://rungameid/730");
        assert!(cwd.is_none());

        // Everyone else launches the recorded exe from its install dir; Epic's
        // LaunchExecutable is relative and slash-separated.
        let (t, cwd) = launch_target(&gi(
            "epic",
            None,
            "D:/Games/ARC",
            Some("ARC/Binaries/Win64/ARC.exe"),
        ))
        .unwrap();
        assert_eq!(t, r"D:/Games/ARC\ARC\Binaries\Win64\ARC.exe");
        assert_eq!(cwd.as_deref(), Some("D:/Games/ARC"));

        // Nothing to go on → no launch (stream the desktop).
        assert!(launch_target(&gi("gog", None, "", None)).is_none());
        assert!(launch_target(&gi("gog", None, "", Some("game.exe"))).is_none());
        assert!(launch_target(&gi("steam", None, "", None)).is_none());
    }
}
