//! Input decode → state → Windows injection (architecture report §12, §13).
//!
//! Pipeline for each packet arriving on the unreliable `input` data channel:
//!
//! 1. [`inphase_protocol::InputPacket::decode`] — reject truncated / wrong version.
//! 2. [`state::InputState`] — drop packets older than the newest `sequence`
//!    (§12.2: *"a 30 ms-old mouse packet is worse than a dropped packet"*),
//!    fold transitions, reconcile against periodic authoritative snapshots.
//! 3. [`backends::InputBackend`] — translate to Windows input (`SendInput` by
//!    default, optional virtual-HID for gamepad, §13).
//!
//! Safety property the report insists on (§12.2, §22): if the channel closes or
//! the ~250 ms heartbeat expires, **every held key/button/axis is released**.

pub mod backends;
pub mod state;

use std::time::{Duration, Instant};

use tracing::{debug, warn};

use inphase_protocol::{InputCodecError, InputEvent, InputPacket};

use crate::config::InputConfig;
use backends::{new_default_backend, new_gamepad_backend, InputBackend};
use state::{InputAction, InputDiff, InputState};

/// Upper bound on a single input frame. A full keyboard+gamepad snapshot is
/// well under this; anything larger is malformed.
const MAX_PACKET_BYTES: usize = 1024;

/// Per-player input processor.
pub struct InputSession {
    cfg: InputConfig,
    state: InputState,
    /// Keyboard + mouse ([`backends::SendInputBackend`]).
    backend: Box<dyn InputBackend>,
    /// Optional gamepad backend ([`backends::virtual_hid`], Phase 5).
    gamepad: Option<Box<dyn InputBackend>>,
    last_packet_at: Instant,
    /// Highest `sequence` accepted so far (wrapping compare).
    high_water: Option<u32>,
    released: bool,
    /// Token-bucket rate limiter.
    tokens: f64,
    tokens_refilled_at: Instant,
    /// Count of packets dropped by the rate limiter / validation since the last
    /// time we logged about it.
    throttled: u64,
    last_throttle_log: Instant,
}

impl InputSession {
    pub fn new(cfg: InputConfig) -> Self {
        let gamepad = new_gamepad_backend(&cfg);
        let now = Instant::now();
        Self {
            state: InputState::default(),
            backend: new_default_backend(&cfg),
            gamepad,
            last_packet_at: now,
            high_water: None,
            // Start "released": the watchdog stays quiet until the first packet
            // arrives, so it does not fire before the data channel opens.
            released: true,
            tokens: cfg.packet_burst as f64,
            tokens_refilled_at: now,
            throttled: 0,
            last_throttle_log: now,
            cfg,
        }
    }

    /// Token-bucket admission check. Returns false if this packet
    /// should be dropped for exceeding the sustained rate.
    fn rate_ok(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.tokens_refilled_at).as_secs_f64();
        self.tokens_refilled_at = now;
        self.tokens = (self.tokens + elapsed * self.cfg.max_packets_per_sec as f64)
            .min(self.cfg.packet_burst as f64);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn note_throttled(&mut self, reason: &str) {
        self.throttled += 1;
        if self.last_throttle_log.elapsed() >= Duration::from_secs(2) {
            warn!(
                dropped = self.throttled,
                reason, "input packets rejected (rate limit / validation)"
            );
            self.throttled = 0;
            self.last_throttle_log = Instant::now();
        }
    }

    /// Route a diff: gamepad actions to the gamepad backend, the rest to
    /// keyboard/mouse.
    fn apply(&mut self, diff: &InputDiff) {
        let has_pad = diff
            .actions
            .iter()
            .any(|a| matches!(a, InputAction::Gamepad(_) | InputAction::GamepadCleared));
        if has_pad {
            if let Some(gp) = self.gamepad.as_mut() {
                gp.apply_diff(diff);
            }
        }
        self.backend.apply_diff(diff);
    }

    pub fn watchdog_timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.watchdog_ms)
    }

    /// Feed one raw binary frame from the data channel.
    pub fn handle_packet(&mut self, bytes: &[u8]) -> Result<(), InputCodecError> {
        // bound the amount of oversized garbage we even parse.
        if bytes.len() > MAX_PACKET_BYTES {
            self.note_throttled("oversize");
            return Ok(());
        }
        let pkt = InputPacket::decode(bytes)?;
        self.last_packet_at = Instant::now();
        self.released = false;

        if !self.rate_ok() {
            self.note_throttled("rate");
            return Ok(());
        }

        if !self.is_fresh(pkt.header.sequence) {
            debug!(
                seq = pkt.header.sequence,
                "dropping stale/out-of-order input packet"
            );
            return Ok(());
        }
        self.high_water = Some(pkt.header.sequence);

        // reject implausible mouse jumps rather than teleporting the
        // cursor / firing a huge scroll.
        if let InputEvent::MouseMove {
            dx,
            dy,
            wheel_x,
            wheel_y,
        } = &pkt.event
        {
            let lim = self.cfg.max_mouse_delta;
            if (*dx as i32).abs() > lim
                || (*dy as i32).abs() > lim
                || (*wheel_x as i32).abs() > lim
                || (*wheel_y as i32).abs() > lim
            {
                self.note_throttled("mouse-delta");
                return Ok(());
            }
        }

        let diff = match &pkt.event {
            // Authoritative reconciliation — releases anything the host still
            // thinks is held but the client says is not (§12.2).
            InputEvent::Snapshot(snap) => self.state.reconcile_snapshot(snap),
            ev => self.state.apply_event(ev),
        };
        if !diff.is_empty() {
            tracing::debug!(kind = ?pkt.event.kind(), actions = diff.actions.len(), "input");
        }
        self.apply(&diff);
        Ok(())
    }

    /// Wrapping "is this newer than what we've seen" check. Treats a jump of
    /// more than half the u32 space as a wrap, not an old packet.
    fn is_fresh(&self, seq: u32) -> bool {
        match self.high_water {
            None => true,
            Some(hw) => seq.wrapping_sub(hw) < u32::MAX / 2,
        }
    }

    /// Call periodically. Releases all held state if the heartbeat expired
    /// (§12.2, §22 "Input heartbeat lost").
    pub fn tick_watchdog(&mut self) {
        if self.released {
            return;
        }
        if self.last_packet_at.elapsed() >= self.watchdog_timeout() {
            self.release_all();
            self.released = true;
        }
    }

    /// Release every held key / mouse button / gamepad control (§22). Warns only
    /// if something was actually held (a quiet mouse produces nothing to release).
    pub fn release_all(&mut self) {
        let diff = self.state.release_all();
        if !diff.is_empty() {
            warn!(
                ms = self.cfg.watchdog_ms,
                actions = diff.actions.len(),
                "input heartbeat expired - released held keys/buttons"
            );
        }
        self.apply(&diff);
        self.backend.release_all();
        if let Some(gp) = self.gamepad.as_mut() {
            gp.release_all();
        }
        self.released = true;
    }
}

impl Drop for InputSession {
    fn drop(&mut self) {
        if !self.released {
            self.release_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inphase_protocol::{InputEvent, InputPacket};

    fn session() -> InputSession {
        InputSession::new(InputConfig {
            max_packets_per_sec: 1_000,
            packet_burst: 5,
            max_mouse_delta: 4_000,
            ..InputConfig::default()
        })
    }

    fn mv(seq: u32, dx: i16, dy: i16) -> Vec<u8> {
        InputPacket::new(
            seq,
            0,
            0,
            InputEvent::MouseMove {
                dx,
                dy,
                wheel_x: 0,
                wheel_y: 0,
            },
        )
        .encode()
    }

    #[test]
    fn token_bucket_caps_the_burst() {
        let mut s = session();
        // burst = 5 → first 5 admitted, rest throttled (no wall-clock sleep).
        let mut admitted = 0u32;
        for i in 0..20 {
            let before = s.throttled;
            s.handle_packet(&mv(i + 1, 1, 1)).unwrap();
            if s.throttled == before {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 5, "only the burst allowance should get through");
    }

    #[test]
    fn implausible_mouse_jump_is_rejected() {
        let mut s = session();
        let before = s.throttled;
        s.handle_packet(&mv(1, 30_000, 0)).unwrap();
        assert_eq!(s.throttled, before + 1);
        // a normal move still lands
        let before = s.throttled;
        s.handle_packet(&mv(2, 12, -8)).unwrap();
        assert_eq!(s.throttled, before);
    }

    #[test]
    fn oversize_frame_is_rejected_without_parsing() {
        let mut s = session();
        let junk = vec![0u8; MAX_PACKET_BYTES + 1];
        assert!(s.handle_packet(&junk).is_ok());
        assert_eq!(s.throttled, 1);
    }

    #[test]
    fn truncated_frame_still_errors() {
        let mut s = session();
        assert!(s.handle_packet(&[1, 2, 3]).is_err());
    }
}
