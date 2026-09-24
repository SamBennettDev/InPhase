//! PIN pairing + session cookies (architecture report §14.1 "MVP trusted-LAN
//! mode").
//!
//! * 6-digit cryptographically-random PIN. Default 5-minute TTL + rotate on pair
//!   (§14.1); both are configurable — a single-user setup can set
//!   `pin_ttl_secs = 0` and `rotate_pin_on_pair = false` so an already-paired
//!   browser is the credential and the PIN never changes underfoot.
//! * Successful `/api/v1/pair` mints a 256-bit random session id in an
//!   `HttpOnly; SameSite=Strict` cookie (§14.1) — never a URL bearer token.
//! * The session store is **persisted** to `<config_dir>/sessions.json` so a
//!   host restart does not un-pair every browser; `session_ttl_secs = 0` means
//!   "paired forever on this device".
//! * Global + per-source-IP rate limiting with backoff (§14.1: 5 / 10 min).
//!
//! The report is explicit (§14.1 threat model): on plaintext LAN HTTP this
//! *"deters casual access but does not protect against an active LAN attacker."*
//! We label the mode, we do not oversell the PIN.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{Config, PairingConfig};

/// An opaque authenticated session identifier (the cookie value).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    fn random() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill(&mut bytes);
        Self(b64(&bytes))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// Wrong PIN.
    BadPin,
    /// Too many recent failures — try again later.
    RateLimited { retry_after: Duration },
}

struct Attempts {
    count: u32,
    window_start: Instant,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionFile {
    /// token -> unix-seconds created.
    tokens: HashMap<String, u64>,
    /// The PIN in force, persisted so a restart does not silently invalidate
    /// the number the user is reading off the screen. Optional for backward
    /// compatibility with stores written before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin: Option<String>,
    /// When that PIN was issued, unix seconds - `pin_issued` is an `Instant`
    /// and monotonic clocks do not survive a process, so the age is rebuilt
    /// from wall time on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin_issued_unix: Option<u64>,
}

/// Upper bound on stored session tokens. `session_ttl_secs = 0` means "paired
/// forever on this device", so nothing expires them - without a cap the store
/// grows without limit (the test PC had accumulated 180+ over a week, a ~10 KB file
/// rewritten on every pair). Evicting the oldest keeps the forever-promise for
/// devices actually in use while bounding the file.
const MAX_SESSIONS: usize = 256;

struct Inner {
    cfg: PairingConfig,
    pin: String,
    pin_issued: Instant,
    /// token -> unix-seconds created.
    sessions: HashMap<String, u64>,
    store_path: Option<PathBuf>,
    global: Attempts,
    per_ip: HashMap<IpAddr, Attempts>,
}

/// Thread-safe pairing manager.
pub struct PairingManager {
    inner: Mutex<Inner>,
}

impl PairingManager {
    pub fn new(cfg: PairingConfig) -> Self {
        let now = Instant::now();
        let store_path = cfg
            .persist_sessions
            .then(|| Config::config_dir().join("sessions.json"));
        let stored = store_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<SessionFile>(&s).ok())
            .unwrap_or_default();
        let sessions = stored.tokens;
        if !sessions.is_empty() {
            debug!(count = sessions.len(), "loaded persisted sessions");
        }

        // Restore the PIN across restarts. The exe's memory is the user's
        // visible truth: rotating on restart shows them "wrong PIN" for the PIN
        // the dashboard is displaying, which reads as a broken host rather than
        // as a deploy. Only a PIN that has genuinely outlived its TTL is
        // replaced.
        let age = stored
            .pin_issued_unix
            .map(|t| Duration::from_secs(now_unix().saturating_sub(t)));
        let expired = match (cfg.pin_ttl_secs, age) {
            (0, _) => false, // no expiry configured
            (ttl, Some(a)) => a >= Duration::from_secs(ttl),
            (_, None) => true, // a PIN with no recorded age cannot be trusted
        };
        let (pin, pin_issued, is_fresh) = match (stored.pin, expired) {
            (Some(p), false) => {
                // Rebuild the issue instant so the dashboard's remaining-TTL
                // countdown continues rather than restarting.
                let issued = age.and_then(|a| now.checked_sub(a)).unwrap_or(now);
                debug!("restored the persisted pairing PIN across restart");
                (p, issued, false)
            }
            _ => (random_pin(), now, true),
        };

        let m = Self {
            inner: Mutex::new(Inner {
                pin,
                pin_issued,
                sessions,
                store_path,
                global: Attempts {
                    count: 0,
                    window_start: now,
                },
                per_ip: HashMap::new(),
                cfg,
            }),
        };
        // Write a newly minted PIN straight away. Persisting only on
        // pair/rotate/revoke is not enough: with `rotate_pin_on_pair = false`
        // and no expiry, a host that is simply restarted never triggers any of
        // those, so the fresh PIN is never stored and the next restart mints
        // another one. That is the whole bug - the number on the dashboard
        // changing every deploy while the user is looking at it.
        {
            // Prune on load so a store that grew unbounded under an older build
            // is trimmed once, rather than staying large forever.
            let mut g = m.inner.lock();
            let before = g.sessions.len();
            g.prune();
            if is_fresh || g.sessions.len() != before {
                g.persist();
            }
        }
        m
    }

    /// Current PIN and remaining lifetime (`None` = no expiry) — dashboard only
    /// (§25.1).
    pub fn current_pin(&self) -> (String, Option<Duration>) {
        let mut g = self.inner.lock();
        g.rotate_if_expired();
        let left = (g.cfg.pin_ttl_secs > 0).then(|| {
            Duration::from_secs(g.cfg.pin_ttl_secs).saturating_sub(g.pin_issued.elapsed())
        });
        (g.pin.clone(), left)
    }

    /// Force a new PIN (dashboard "rotate" button, §25.1).
    pub fn rotate_pin(&self) -> String {
        let mut g = self.inner.lock();
        g.pin = random_pin();
        g.pin_issued = Instant::now();
        g.persist();
        g.pin.clone()
    }

    /// Attempt to pair. On success returns a fresh [`SessionId`], persists it,
    /// and (per config) rotates the PIN.
    pub fn try_pair(&self, source: IpAddr, pin_attempt: &str) -> Result<SessionId, PairError> {
        // Count attempts per /64, not per address: over IPv6 a single attacker
        // is handed 2^64 source addresses, so a per-address counter never trips.
        let source = crate::net::rate_limit_key(source);
        let mut g = self.inner.lock();
        g.rotate_if_expired();

        if let Some(retry_after) = g.rate_limited(source) {
            return Err(PairError::RateLimited { retry_after });
        }

        // Length + constant-fold compare. PIN space is 10^6 so rate limiting,
        // not compare timing, is the real defence.
        let ok = pin_attempt.len() == g.pin.len()
            && pin_attempt
                .bytes()
                .zip(g.pin.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;

        if !ok {
            g.record_failure(source);
            return Err(PairError::BadPin);
        }

        g.per_ip.remove(&source);
        if g.cfg.rotate_pin_on_pair {
            g.pin = random_pin();
            g.pin_issued = Instant::now();
        }
        let sid = SessionId::random();
        g.insert_session(&sid);
        g.persist();
        Ok(sid)
    }

    /// Mint an authenticated session directly, bypassing the PIN. Used by the
    /// QR / short-code pairing paths, where holding the invite secret
    /// (+ operator approval for the first controller) *is* the authorization.
    pub fn mint_session(&self) -> SessionId {
        let mut g = self.inner.lock();
        let sid = SessionId::random();
        g.insert_session(&sid);
        if g.cfg.rotate_pin_on_pair {
            g.pin = random_pin();
            g.pin_issued = Instant::now();
        }
        g.persist();
        sid
    }

    /// Validate a cookie value. `session_ttl_secs == 0` => never expires.
    pub fn validate_session(&self, sid: &str) -> Option<SessionId> {
        let mut g = self.inner.lock();
        let created = *g.sessions.get(sid)?;
        let ttl = g.cfg.session_ttl_secs;
        if ttl == 0 || now_unix().saturating_sub(created) < ttl {
            Some(SessionId(sid.to_string()))
        } else {
            g.sessions.remove(sid);
            g.persist();
            None
        }
    }

    /// Revoke one session (clean logout / admin force-disconnect, §16).
    pub fn revoke(&self, sid: &str) {
        let mut g = self.inner.lock();
        if g.sessions.remove(sid).is_some() {
            g.persist();
        }
    }

    /// Revoke every session (dashboard "un-pair all devices").
    pub fn revoke_all(&self) {
        let mut g = self.inner.lock();
        g.sessions.clear();
        g.persist();
    }

    pub fn session_count(&self) -> usize {
        self.inner.lock().sessions.len()
    }
}

impl Inner {
    fn rotate_if_expired(&mut self) {
        if self.cfg.pin_ttl_secs == 0 {
            return;
        }
        if self.pin_issued.elapsed() >= Duration::from_secs(self.cfg.pin_ttl_secs) {
            self.pin = random_pin();
            self.pin_issued = Instant::now();
            self.persist();
        }
    }

    /// Mint-time insert. Makes room *before* inserting, so the token we are
    /// about to hand to a browser can never be the one evicted.
    ///
    /// That ordering is load-bearing, not tidiness: `created` is unix seconds,
    /// so every token minted in the same second compares equal and "evict the
    /// oldest" cannot tell them apart. Insert-then-prune therefore evicted an
    /// arbitrary member of that group - sometimes the brand-new one, which
    /// would log a user out at the moment they paired. (Found by
    /// `the_session_store_is_capped` failing 2 runs in 15.)
    fn insert_session(&mut self, sid: &SessionId) {
        self.prune_to(MAX_SESSIONS.saturating_sub(1));
        self.sessions.insert(sid.0.clone(), now_unix());
    }

    /// Drop expired tokens (when a TTL is configured) and cap the store at
    /// [`MAX_SESSIONS`]. Cheap and idempotent.
    fn prune(&mut self) {
        self.prune_to(MAX_SESSIONS);
    }

    /// Expire, then reduce the store to at most `cap` entries, newest kept.
    fn prune_to(&mut self, cap: usize) {
        let ttl = self.cfg.session_ttl_secs;
        if ttl > 0 {
            let now = now_unix();
            self.sessions
                .retain(|_, created| now.saturating_sub(*created) < ttl);
        }
        if self.sessions.len() <= cap {
            return;
        }
        let mut by_age: Vec<(u64, String)> =
            self.sessions.iter().map(|(k, v)| (*v, k.clone())).collect();
        // Newest first. Ties (same second) resolve by token, only so the choice
        // is deterministic - callers must not rely on which tie-member survives.
        by_age.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let dropped = by_age.len() - cap;
        for (_, k) in by_age.into_iter().skip(cap) {
            self.sessions.remove(&k);
        }
        debug!(dropped, kept = cap, "pruned the session store");
    }

    fn persist(&self) {
        let Some(path) = &self.store_path else { return };
        let file = SessionFile {
            tokens: self.sessions.clone(),
            pin: Some(self.pin.clone()),
            // Convert the monotonic issue instant back to wall time so the age
            // survives the process.
            pin_issued_unix: Some(now_unix().saturating_sub(self.pin_issued.elapsed().as_secs())),
        };
        let Ok(json) = serde_json::to_string(&file) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Best-effort atomic-ish write.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, path) {
                warn!("could not persist session store: {e}");
            }
        }
    }

    fn window(&self) -> Duration {
        Duration::from_secs(self.cfg.rate_window_secs)
    }

    fn rate_limited(&mut self, source: IpAddr) -> Option<Duration> {
        let window = self.window();
        let max = self.cfg.max_attempts;
        for att in [Some(&mut self.global), self.per_ip.get_mut(&source)]
            .into_iter()
            .flatten()
        {
            if att.window_start.elapsed() >= window {
                att.count = 0;
                att.window_start = Instant::now();
            }
            if att.count >= max {
                return Some(window.saturating_sub(att.window_start.elapsed()));
            }
        }
        None
    }

    fn record_failure(&mut self, source: IpAddr) {
        let window = self.window();
        let now = Instant::now();
        for att in [
            &mut self.global,
            self.per_ip.entry(source).or_insert(Attempts {
                count: 0,
                window_start: now,
            }),
        ] {
            if att.window_start.elapsed() >= window {
                att.count = 0;
                att.window_start = now;
            }
            att.count += 1;
        }
    }
}

fn random_pin() -> String {
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn cfg() -> PairingConfig {
        PairingConfig {
            pin_ttl_secs: 300,
            rotate_pin_on_pair: true,
            max_attempts: 3,
            rate_window_secs: 600,
            session_ttl_secs: 3600,
            persist_sessions: false,
        }
    }
    fn mgr() -> PairingManager {
        PairingManager::new(cfg())
    }

    /// `XDG_CONFIG_HOME` is process-global but `cargo test` runs these
    /// concurrently, so any test that points the config dir somewhere must hold
    /// this first - otherwise they clobber each other's store path and fail in
    /// whichever order they happen to interleave.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point the config dir at a private temp dir for the duration of a test.
    /// Returns the guard (hold it) and the directory.
    fn with_temp_config_dir(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "inphase-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        (guard, dir)
    }

    #[test]
    fn pairs_with_correct_pin_and_rotates() {
        let m = mgr();
        let (pin, _) = m.current_pin();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let sid = m.try_pair(ip, &pin).expect("pair ok");
        assert!(m.validate_session(sid.as_str()).is_some());
        assert_ne!(pin, m.current_pin().0);
    }

    #[test]
    fn stable_pin_when_rotate_disabled() {
        let mut c = cfg();
        c.rotate_pin_on_pair = false;
        c.pin_ttl_secs = 0;
        let m = PairingManager::new(c);
        let (pin, ttl) = m.current_pin();
        assert!(ttl.is_none(), "pin_ttl_secs=0 => no expiry");
        m.try_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), &pin).unwrap();
        assert_eq!(pin, m.current_pin().0, "PIN must not change after pair");
    }

    #[test]
    fn session_never_expires_when_ttl_zero() {
        let mut c = cfg();
        c.session_ttl_secs = 0;
        let m = PairingManager::new(c);
        let (pin, _) = m.current_pin();
        let sid = m.try_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), &pin).unwrap();
        // Simulate an ancient session.
        m.inner.lock().sessions.insert(sid.0.clone(), 0);
        assert!(m.validate_session(sid.as_str()).is_some());
    }

    #[test]
    fn rate_limits_after_max_failures() {
        let m = mgr();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        for _ in 0..3 {
            assert_eq!(m.try_pair(ip, "000000000").unwrap_err(), PairError::BadPin);
        }
        assert!(matches!(
            m.try_pair(ip, "123456"),
            Err(PairError::RateLimited { .. })
        ));
    }

    #[test]
    fn revoke_invalidates() {
        let m = mgr();
        let (pin, _) = m.current_pin();
        let sid = m
            .try_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), &pin)
            .expect("pair");
        m.revoke(sid.as_str());
        assert!(m.validate_session(sid.as_str()).is_none());
    }

    #[test]
    fn persists_and_reloads_sessions() {
        let (_env, dir) = with_temp_config_dir("sessions");
        let mut c = cfg();
        c.persist_sessions = true;
        let token = {
            let m = PairingManager::new(c.clone());
            let (pin, _) = m.current_pin();
            m.try_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), &pin)
                .unwrap()
                .as_str()
                .to_string()
        };
        // Fresh manager, same config dir -> session still valid.
        let m2 = PairingManager::new(c);
        assert!(m2.validate_session(&token).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A restart must not invalidate the PIN the user is reading off the
    /// dashboard. Rotating on restart shows "wrong PIN" for the PIN on screen,
    /// which reads as a broken host rather than as a deploy having happened.
    #[test]
    fn pin_survives_a_restart() {
        let (_env, dir) = with_temp_config_dir("pin");
        let mut c = cfg();
        c.persist_sessions = true;
        c.rotate_pin_on_pair = false;

        // Deliberately no pair and no rotate: just start, read the PIN, and
        // restart. This is the path that actually broke in production - an
        // earlier version of this test called rotate_pin() first, which forced
        // a store write and so passed while the real host still rotated on
        // every restart.
        let (first, _) = PairingManager::new(c.clone()).current_pin();
        let (after_restart, left) = PairingManager::new(c.clone()).current_pin();
        assert_eq!(after_restart, first, "the PIN must survive a restart");
        // The countdown continues from the original issue time rather than
        // restarting, so a persisted PIN cannot outlive its TTL by restarting.
        let left = left.expect("a TTL is configured");
        assert!(
            left <= Duration::from_secs(c.pin_ttl_secs),
            "remaining TTL {left:?} exceeds the configured {}s",
            c.pin_ttl_secs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `session_ttl_secs = 0` promises "paired forever", so nothing expires the
    /// tokens; the store must still not grow without bound.
    #[test]
    fn the_session_store_is_capped() {
        let (_env, dir) = with_temp_config_dir("cap");
        let mut c = cfg();
        c.persist_sessions = true;
        c.session_ttl_secs = 0;
        c.rotate_pin_on_pair = false;

        let m = PairingManager::new(c.clone());
        for _ in 0..(MAX_SESSIONS + 40) {
            m.mint_session();
        }
        assert_eq!(
            m.inner.lock().sessions.len(),
            MAX_SESSIONS,
            "the store must be capped"
        );
        // The most recent session still works - eviction takes the oldest.
        let newest = m.mint_session();
        assert!(m.validate_session(newest.as_str()).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A store that already grew past the cap under an older build is trimmed
    /// on load rather than staying large forever.
    #[test]
    fn an_oversized_store_is_pruned_on_load() {
        let (_env, dir) = with_temp_config_dir("prune");
        let mut c = cfg();
        c.persist_sessions = true;
        c.session_ttl_secs = 0;

        let mut tokens = HashMap::new();
        for i in 0..(MAX_SESSIONS + 100) {
            tokens.insert(format!("token-{i}"), 1_700_000_000 + i as u64);
        }
        let path = Config::config_dir().join("sessions.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = SessionFile {
            tokens,
            pin: None,
            pin_issued_unix: None,
        };
        std::fs::write(&path, serde_json::to_string(&f).unwrap()).unwrap();

        let m = PairingManager::new(c);
        assert_eq!(m.inner.lock().sessions.len(), MAX_SESSIONS);
        // Newest survived, oldest did not.
        let newest = format!("token-{}", MAX_SESSIONS + 99);
        assert!(m.validate_session(&newest).is_some(), "newest must survive");
        assert!(
            m.validate_session("token-0").is_none(),
            "oldest must be evicted"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An expired persisted PIN is replaced rather than restored - persistence
    /// must not become a way to keep a stale PIN alive forever.
    #[test]
    fn an_expired_persisted_pin_is_replaced() {
        let (_env, dir) = with_temp_config_dir("pinexp");
        let mut c = cfg();
        c.persist_sessions = true;
        c.pin_ttl_secs = 60;

        let first = PairingManager::new(c.clone()).rotate_pin();
        // Backdate the stored issue time past the TTL.
        let path = Config::config_dir().join("sessions.json");
        let mut f: SessionFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        f.pin_issued_unix = Some(now_unix().saturating_sub(3600));
        std::fs::write(&path, serde_json::to_string(&f).unwrap()).unwrap();

        let (after, _) = PairingManager::new(c).current_pin();
        assert_ne!(after, first, "an expired PIN must not be restored");
        std::fs::remove_dir_all(&dir).ok();
    }
}
