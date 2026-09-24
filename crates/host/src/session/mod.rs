//! Session lifecycle and the **one active player** rule (architecture report
//! §15 "Signaling and session state machine").
//!
//! ```text
//! IDLE
//!  └─ pair accepted ─► PAIRED
//!       └─ capabilities received ─► NEGOTIATING
//!            └─ ICE/DTLS connected ─► PLAYING
//!                 ├─ transient ICE loss ─► RECONNECTING ─► PLAYING
//!                 └─ stop / timeout / failure ─► STOPPING ─► IDLE
//! ```
//!
//! Rules the report calls out and this module enforces:
//! * one `PlayerSession` owns the one `MediaSession`;
//! * a second play request gets `BUSY`, **never** a second encoder by accident;
//! * entering `STOPPING` immediately releases all input state (§15, §22);
//! * the media pipeline is destroyed **before** returning to `IDLE`.
//!
//! The signaling *driver* (message handling, negotiation) lives in
//! [`crate::http::signal`]; this module owns only the state and the slot.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::{info, warn};

use inphase_protocol::{SessionConfig, SignalMessage};

use crate::config::Config;
use crate::input::InputSession;

use crate::media::MediaSession;
use crate::stats::StatsCollector;

/// The session state (§15). `Copy` so it is cheap to read for `/api/v1/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Paired,
    Negotiating,
    Playing,
    Reconnecting,
    Stopping,
}

impl SessionState {
    /// The dashboard's large status label (§25.1).
    pub fn label(self) -> &'static str {
        match self {
            SessionState::Idle => "Available",
            SessionState::Paired | SessionState::Negotiating => "Connecting",
            SessionState::Playing => "Playing",
            SessionState::Reconnecting => "Recovering",
            SessionState::Stopping => "Available",
        }
    }

    fn can_transition_to(self, next: SessionState) -> bool {
        use SessionState::*;
        matches!(
            (self, next),
            (Idle, Paired)
                | (Paired, Negotiating)
                | (Paired, Stopping)
                | (Negotiating, Playing)
                | (Negotiating, Stopping)
                | (Playing, Reconnecting)
                | (Playing, Stopping)
                | (Reconnecting, Playing)
                | (Reconnecting, Stopping)
                | (Stopping, Idle)
        )
    }
}

/// Why a play request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRejected {
    /// Another player already owns the session (§15 `BUSY`).
    Busy,
}

/// Safe-to-expose metadata about the current player (§22 "return BUSY with
/// current-session metadata safe to expose").
#[derive(Debug, Clone, Default)]
pub struct PeerInfo {
    pub browser: String,
    pub client_ip: String,
    pub connected_since_unix: u64,
}

/// One connected player. Owns the media + input sessions for its lifetime.
pub struct PlayerSession {
    pub peer: PeerInfo,
    pub media: MediaSession,
    pub input: InputSession,
    pub config: Option<SessionConfig>,
    /// Outbound signaling sink to this player's WebSocket.
    pub to_client: mpsc::UnboundedSender<SignalMessage>,
    /// Hex Ed25519 id of the controller that authorized this session: it
    /// signed the connect challenge and is active in the ACL.
    pub controller_id: Option<String>,
    /// Input injection is disarmed until the media path is up. Packets received
    /// while disarmed are dropped.
    pub input_armed: bool,
    pub media_ready: bool,
}

impl PlayerSession {
    /// §15/§22: entering STOPPING releases all input immediately, then the
    /// media pipeline is torn down.
    fn shutdown(&mut self) {
        self.input.release_all();
        self.media.stop();
    }
}

struct Inner {
    /// Bumped on every claim. The signaling link that created the current
    /// session holds this ticket; a DISPLACED link's late close must not
    /// tear the replacement down (review §11's essential regression).
    ticket: u64,
    state: SessionState,
    player: Option<PlayerSession>,
}

/// Global session coordinator. Exactly one [`PlayerSession`] at a time.
/// Identifies the session a signaling link created (review §11): every
/// teardown decision asks "is this still my session?" with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTicket(u64);

pub struct SessionManager {
    cfg: Arc<Config>,
    stats: Arc<StatsCollector>,
    inner: Mutex<Inner>,
    /// Fired whenever the session walks down to Idle (or the player slot
    /// frees) so an async claim can await settlement instead of polling.
    settle: std::sync::Arc<tokio::sync::Notify>,
    /// Runtime override for `media.audio_capture_device` — applied to the next
    /// session so a client can pick the loopback source without a host restart.
    audio_device: Mutex<Option<String>>,
    /// WebTransport video transport (ADR-0011), if the host enabled it. Attached
    /// to every claimed session. A slot, not a plain `Arc`: the certificate
    /// behind it is rotated on a timer (`app::spawn_wt_rotation`), so a claim
    /// must read the transport that is current *now*.
    wt: Mutex<crate::media::wt::WtSlot>,
}

impl SessionManager {
    pub fn new(cfg: Arc<Config>, stats: Arc<StatsCollector>) -> Self {
        let audio_device = Mutex::new(cfg.media.audio_capture_device.clone());
        Self {
            settle: std::sync::Arc::new(tokio::sync::Notify::new()),
            cfg,
            stats,
            inner: Mutex::new(Inner {
                state: SessionState::Idle,
                player: None,
                ticket: 0,
            }),
            audio_device,
            wt: Mutex::new(crate::media::wt::WtSlot::new()),
        }
    }

    /// Install the WebTransport video-transport slot (ADR-0011). Called once at
    /// boot when `media.wt_enabled`; the slot's *contents* change on certificate
    /// rotation, so the manager never caches the transport itself.
    pub fn set_wt_slot(&self, wt: crate::media::wt::WtSlot) {
        *self.wt.lock() = wt;
    }

    /// The loopback capture endpoint id the next session will use (empty/unset
    /// = Windows default).
    pub fn audio_device(&self) -> Option<String> {
        self.audio_device.lock().clone().filter(|s| !s.is_empty())
    }

    /// Set the loopback capture endpoint for future sessions.
    pub fn set_audio_device(&self, id: Option<String>) {
        *self.audio_device.lock() = id.filter(|s| !s.is_empty());
    }

    pub fn state(&self) -> SessionState {
        self.inner.lock().state
    }

    pub fn is_busy(&self) -> bool {
        !matches!(
            self.inner.lock().state,
            SessionState::Idle | SessionState::Stopping
        )
    }

    pub fn peer_info(&self) -> Option<PeerInfo> {
        self.inner.lock().player.as_ref().map(|p| p.peer.clone())
    }

    pub fn active_config(&self) -> Option<SessionConfig> {
        self.inner
            .lock()
            .player
            .as_ref()
            .and_then(|p| p.config.clone())
    }

    /// Claim the single session for a newly-paired player. Returns
    /// [`StartRejected::Busy`] if one already exists (§15).
    pub async fn claim(
        &self,
        peer: PeerInfo,
        to_client: mpsc::UnboundedSender<SignalMessage>,
    ) -> Result<SessionTicket, StartRejected> {
        // Newest player wins (§15): an abrupt client death (browser quit,
        // network drop) can leave the previous session unreaped for a while.
        // Rather than rejecting the newcomer with BUSY, walk the stale
        // session down and wait for its resources (GPU encoder, capture) to
        // actually settle before handing the slot over.
        // The busy check runs in its OWN scope: a guard whose binding scope
        // reaches an await stays in the generator state (rustc is
        // scope-based here, not flow-based - drop() does not help).
        let reclaim_needed = {
            let busy = self.inner.lock();
            let stale_state = busy.state;
            (busy.player.is_some() || busy.state != SessionState::Idle).then_some(stale_state)
        };
        if let Some(stale_state) = reclaim_needed {
            tracing::info!(state = ?stale_state, "session busy - reclaiming for the new player");
            self.force_disconnect();
            // Async settlement: the walk-down path fires `settle` when the
            // player slot frees. No worker thread sleeps here (§11).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                {
                    let g = self.inner.lock();
                    if g.player.is_none() && g.state == SessionState::Idle {
                        break;
                    }
                }
                if std::time::Instant::now() > deadline {
                    tracing::error!("stale session never settled - rejecting new player");
                    return Err(StartRejected::Busy);
                }
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    self.settle.notified(),
                )
                .await;
            }
            tracing::info!("stale session settled - new player taking over");
        }
        // Fresh binding AFTER the settlement await: no lock guard is ever
        // live across an await point (the future must stay Send).
        let mut g = self.inner.lock();
        // Overlay the runtime-chosen loopback capture endpoint onto this
        // session's config (the rest is immutable app config).
        let mut session_cfg = (*self.cfg).clone();
        session_cfg.media.audio_capture_device = self.audio_device.lock().clone();
        let mut media = MediaSession::new(Arc::new(session_cfg), self.stats.clone());
        // Read the slot in its own statement: the guard must not reach the
        // session's lifetime (a parking_lot guard held across an await is not
        // `Send`, and the claim future is awaited inside a spawned handler).
        let wt = self.wt.lock().get();
        media.set_wt_transport(wt);
        let input = InputSession::new(self.cfg.input.clone());
        g.player = Some(PlayerSession {
            peer,
            media,
            input,
            config: None,
            to_client,
            controller_id: None,
            input_armed: false,
            media_ready: false,
        });
        self.force_state(&mut g, SessionState::Paired);
        g.ticket += 1;
        Ok(SessionTicket(g.ticket))
    }

    /// A signaling link closed. Tears the session down ONLY if this link is
    /// still the one that created it — a displaced link's late close (TCP FIN
    /// after its successor claimed) must leave the replacement untouched.
    pub fn end_link(&self, ticket: SessionTicket) {
        let g = self.inner.lock();
        if g.ticket != ticket.0 {
            debug!(
                link = ticket.0,
                current = g.ticket,
                "displaced signaling link closed; current session untouched"
            );
            return;
        }
        drop(g);
        self.transition(SessionState::Stopping);
    }

    /// Whether `ticket`'s link still owns a live player session. A link that
    /// a newer player displaced must not act for the session any more.
    pub fn owns(&self, ticket: SessionTicket) -> bool {
        let g = self.inner.lock();
        g.ticket == ticket.0 && g.player.is_some()
    }

    /// Run `f` against the active player, if any.
    pub fn with_player<R>(&self, f: impl FnOnce(&mut PlayerSession) -> R) -> Option<R> {
        let mut g = self.inner.lock();
        g.player.as_mut().map(f)
    }

    /// Attempt a state transition, logging + rejecting illegal ones (§15).
    pub fn transition(&self, next: SessionState) -> bool {
        let settled = next == SessionState::Idle;
        let mut g = self.inner.lock();
        if !g.state.can_transition_to(next) {
            warn!(from = ?g.state, to = ?next, "rejected illegal session transition");
            return false;
        }
        if next == SessionState::Stopping {
            if let Some(p) = g.player.as_mut() {
                p.shutdown();
            }
        }
        self.force_state(&mut g, next);
        if next == SessionState::Stopping {
            // media/input already released; drop the player and settle to IDLE.
            g.player = None;
            self.force_state(&mut g, SessionState::Idle);
        }
        if settled && g.state == SessionState::Idle {
            self.settle.notify_waiters();
        }
        true
    }

    /// Force-disconnect the active player (loopback admin, §16).
    pub fn force_disconnect(&self) {
        if let Some(p) = self.with_player(|p| p.to_client.clone()) {
            let _ = p.send(SignalMessage::Bye);
        }
        // best-effort walk to STOPPING regardless of current state
        for s in [
            SessionState::Playing,
            SessionState::Negotiating,
            SessionState::Paired,
            SessionState::Reconnecting,
        ] {
            if self.state() == s {
                self.transition(SessionState::Stopping);
                return;
            }
        }
    }

    /// Record which controller key authorized this session.
    pub fn set_controller(&self, controller_id: String) {
        if let Some(p) = self.inner.lock().player.as_mut() {
            p.controller_id = Some(controller_id);
        }
    }

    pub fn input_armed(&self) -> bool {
        self.inner
            .lock()
            .player
            .as_ref()
            .map(|p| p.input_armed)
            .unwrap_or(false)
    }

    /// The media path is up. Arms input and returns `true` exactly once so the
    /// caller sends `session_ready`.
    pub fn mark_ready(&self) -> bool {
        let mut g = self.inner.lock();
        let Some(p) = g.player.as_mut() else {
            return false;
        };
        if p.media_ready {
            return false;
        }
        p.media_ready = true;
        p.input_armed = true;
        info!("session ready — media path up, input armed");
        true
    }

    fn force_state(&self, g: &mut Inner, next: SessionState) {
        if g.state != next {
            info!(from = ?g.state, to = ?next, "session state");
            g.state = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_matches_report() {
        use SessionState::*;
        assert!(Idle.can_transition_to(Paired));
        assert!(Paired.can_transition_to(Negotiating));
        assert!(Negotiating.can_transition_to(Playing));
        assert!(Playing.can_transition_to(Reconnecting));
        assert!(Reconnecting.can_transition_to(Playing));
        assert!(Playing.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Idle));
        // illegal
        assert!(!Idle.can_transition_to(Playing));
        assert!(!Playing.can_transition_to(Paired));
        assert!(!Idle.can_transition_to(Negotiating));
    }

    #[tokio::test]
    async fn second_claim_reclaims_the_session() {
        // Newest player wins (§15): a second claim reclaims the slot from a
        // stale player (abrupt client death) instead of returning BUSY, and
        // the stale player is told goodbye.
        let cfg = Arc::new(Config::default());
        let stats = Arc::new(StatsCollector::new());
        let sm = SessionManager::new(cfg, stats);
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let t1 = sm.claim(PeerInfo::default(), tx1).await.ok();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let t2 = sm
            .claim(PeerInfo::default(), tx2)
            .await
            .expect("a new claim reclaims a stale session instead of BUSY");
        assert!(matches!(rx1.try_recv(), Ok(SignalMessage::Bye)));
        // A's late close must not touch B's session; B's own close does.
        sm.transition(SessionState::Negotiating);
        sm.transition(SessionState::Playing);
        sm.end_link(t1.expect("first ticket"));
        assert_eq!(sm.state(), SessionState::Playing, "replacement unaffected");
        sm.end_link(t2);
        assert_eq!(sm.state(), SessionState::Idle);
    }

    #[tokio::test]
    async fn stopping_returns_to_idle_and_frees_slot() {
        let sm = SessionManager::new(Arc::new(Config::default()), Arc::new(StatsCollector::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        sm.claim(PeerInfo::default(), tx).await.unwrap();
        sm.transition(SessionState::Negotiating);
        sm.transition(SessionState::Playing);
        assert!(sm.transition(SessionState::Stopping));
        assert_eq!(sm.state(), SessionState::Idle);
        assert!(!sm.is_busy());
    }
}
