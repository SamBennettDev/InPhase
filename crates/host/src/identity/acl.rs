//! The local controller ACL — the gaming PC is the final authorization
//! authority.
//!
//! One browser profile = one controller = one Ed25519 key. The list lives at
//! `%APPDATA%\InPhase\controllers.json` and is written **only** by the Host —
//! there is no cloud path that adds an entry.

use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Controller {
    /// Lowercase hex of the controller's Ed25519 public key — the durable
    /// identity the operator approved. The browser proves possession of the
    /// matching private key on every signaling connect.
    pub id: String,
    /// Human label (browser / OS, or a name the user gave the device).
    pub name: String,
    pub added_unix: u64,
    #[serde(default)]
    pub last_seen_unix: u64,
    #[serde(default)]
    pub revoked: bool,
}

pub struct ControllerAcl {
    path: PathBuf,
    inner: Mutex<Vec<Controller>>,
}

impl ControllerAcl {
    pub fn load() -> Self {
        let path = Config::config_dir().join("controllers.json");
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Controller>>(&s).ok())
            .unwrap_or_default();
        if !inner.is_empty() {
            tracing::info!(
                controllers = inner.iter().filter(|c| !c.revoked).count(),
                revoked = inner.iter().filter(|c| c.revoked).count(),
                "loaded controller ACL"
            );
        }
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    fn persist(&self, list: &[Controller]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!("could not persist controller ACL: {e:#}");
                }
            }
            Err(e) => tracing::warn!("serialising controller ACL: {e:#}"),
        }
    }

    pub fn list(&self) -> Vec<Controller> {
        self.inner.lock().clone()
    }

    /// No usable (non-revoked) controller — the next pairing is a "first
    /// controller" and (later) will need an explicit local approval.
    pub fn is_first(&self) -> bool {
        self.inner.lock().iter().all(|c| c.revoked)
    }

    pub fn is_authorized(&self, id: &str) -> bool {
        self.inner.lock().iter().any(|c| c.id == id && !c.revoked)
    }

    /// Look up an **active** controller by its Ed25519 id (hex).
    pub fn get(&self, id: &str) -> Option<Controller> {
        self.inner
            .lock()
            .iter()
            .find(|c| !c.revoked && c.id.eq_ignore_ascii_case(id))
            .cloned()
    }

    /// Register (or re-activate) a controller. Idempotent; updates the name.
    pub fn add(&self, id: &str, name: &str) {
        let mut g = self.inner.lock();
        match g.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.name = name.to_string();
                c.revoked = false;
                c.last_seen_unix = now();
            }
            None => g.push(Controller {
                id: id.to_string(),
                name: name.to_string(),
                added_unix: now(),
                last_seen_unix: now(),
                revoked: false,
            }),
        }
        let list = g.clone();
        drop(g);
        self.persist(&list);
        tracing::info!(controller = %short(id), name, "controller added to ACL");
    }

    pub fn touch(&self, id: &str) {
        let mut g = self.inner.lock();
        if let Some(c) = g.iter_mut().find(|c| c.id == id) {
            c.last_seen_unix = now();
            let list = g.clone();
            drop(g);
            self.persist(&list);
        }
    }

    /// Revoke by full id or by a unique short prefix. Returns the id revoked.
    pub fn revoke(&self, id_or_prefix: &str) -> Option<String> {
        let mut g = self.inner.lock();
        let matches: Vec<usize> = g
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.revoked && (c.id == id_or_prefix || c.id.starts_with(id_or_prefix)))
            .map(|(i, _)| i)
            .collect();
        if matches.len() != 1 {
            return None;
        }
        let i = matches[0];
        g[i].revoked = true;
        let id = g[i].id.clone();
        let list = g.clone();
        drop(g);
        self.persist(&list);
        tracing::info!(controller = %short(&id), "controller revoked");
        Some(id)
    }

    /// Revoke every controller (dashboard "un-pair all devices").
    pub fn revoke_all(&self) {
        let mut g = self.inner.lock();
        for c in g.iter_mut() {
            c.revoked = true;
        }
        let list = g.clone();
        drop(g);
        self.persist(&list);
        tracing::info!("all controllers revoked");
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_authorize_revoke() {
        let acl = ControllerAcl {
            path: std::env::temp_dir().join(format!("inphase-acl-test-{}.json", now())),
            inner: Mutex::new(Vec::new()),
        };
        assert!(acl.is_first());
        acl.add("aabbccddeeff00112233", "Test");
        assert!(acl.is_authorized("aabbccddeeff00112233"));
        assert!(!acl.is_first());
        assert!(acl.get("aabbccddeeff00112233").is_some());
        assert!(acl.get("deadbeef").is_none());
        // short-prefix revoke
        assert_eq!(
            acl.revoke("aabbccdd").as_deref(),
            Some("aabbccddeeff00112233")
        );
        assert!(!acl.is_authorized("aabbccddeeff00112233"));
        assert!(acl.get("aabbccddeeff00112233").is_none());
        assert!(acl.is_first());
        // re-add un-revokes
        acl.add("aabbccddeeff00112233", "Test 2");
        assert!(acl.is_authorized("aabbccddeeff00112233"));
        let _ = std::fs::remove_file(&acl.path);
    }
}
