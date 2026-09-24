//! Authoritative input state + transition folding (architecture report §12.2).
//!
//! The host keeps its own picture of what is currently held. Every incoming
//! event produces an [`InputDiff`] — the *minimal* set of Windows actions to
//! reach the new state — which keeps injection idempotent and makes
//! snapshot reconciliation (§12.2) a plain set-difference.

use std::collections::BTreeSet;

use inphase_protocol::{GamepadState, InputEvent, SnapshotState};

/// One absolute mouse-button / key / gamepad delta to hand to the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    MouseMove {
        dx: i32,
        dy: i32,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    Key {
        physical_code: u16,
        down: bool,
    },
    /// Full gamepad snapshot for the virtual-HID backend (§13).
    Gamepad(GamepadState),
    /// Gamepad disconnected / neutralised.
    GamepadCleared,
}

/// A batch of actions produced by one event or reconciliation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputDiff {
    pub actions: Vec<InputAction>,
}

impl InputDiff {
    fn push(&mut self, a: InputAction) {
        self.actions.push(a);
    }
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct InputState {
    held_keys: BTreeSet<u16>,
    mouse_buttons: u16,
    gamepad: Option<GamepadState>,
}

impl InputState {
    /// Fold a non-snapshot event into state, returning the minimal diff.
    pub fn apply_event(&mut self, ev: &InputEvent) -> InputDiff {
        let mut d = InputDiff::default();
        match ev {
            InputEvent::MouseMove {
                dx,
                dy,
                wheel_x,
                wheel_y,
            } => {
                if *dx != 0 || *dy != 0 {
                    d.push(InputAction::MouseMove {
                        dx: *dx as i32,
                        dy: *dy as i32,
                    });
                }
                if *wheel_x != 0 || *wheel_y != 0 {
                    d.push(InputAction::Wheel {
                        dx: *wheel_x as i32,
                        dy: *wheel_y as i32,
                    });
                }
            }
            InputEvent::MouseButtons { buttons } => {
                self.diff_mouse_buttons(*buttons, &mut d);
                self.mouse_buttons = *buttons;
            }
            InputEvent::Key {
                physical_code,
                down,
                ..
            } => {
                let changed = if *down {
                    self.held_keys.insert(*physical_code)
                } else {
                    self.held_keys.remove(physical_code)
                };
                if changed {
                    d.push(InputAction::Key {
                        physical_code: *physical_code,
                        down: *down,
                    });
                }
            }
            InputEvent::Gamepad(g) => {
                self.gamepad = Some(*g);
                d.push(InputAction::Gamepad(*g));
            }
            InputEvent::Touch { .. } => {
                // Phase 6: client maps touch to the canonical protocol before
                // sending, so nothing device-specific lands here yet.
            }
            InputEvent::Snapshot(s) => return self.reconcile_snapshot(s),
        }
        d
    }

    /// Reconcile against an authoritative snapshot (§12.2): press anything newly
    /// held, and crucially **release anything the host holds that the client no
    /// longer does** — this is what unwedges a lost key-up.
    pub fn reconcile_snapshot(&mut self, snap: &SnapshotState) -> InputDiff {
        let mut d = InputDiff::default();

        let want: BTreeSet<u16> = snap.held_keys.iter().copied().collect();
        for &code in want.difference(&self.held_keys) {
            d.push(InputAction::Key {
                physical_code: code,
                down: true,
            });
        }
        for &code in self.held_keys.difference(&want) {
            d.push(InputAction::Key {
                physical_code: code,
                down: false,
            });
        }
        self.held_keys = want;

        self.diff_mouse_buttons(snap.mouse_buttons, &mut d);
        self.mouse_buttons = snap.mouse_buttons;

        match (self.gamepad, snap.gamepad) {
            // Re-assert only on an actual change — a snapshot arrives every
            // ~25 ms and the pad is usually identical frame to frame.
            (prev, Some(g)) if prev != Some(g) => {
                self.gamepad = Some(g);
                d.push(InputAction::Gamepad(g));
            }
            (_, Some(_)) => {}
            (Some(_), None) => {
                self.gamepad = None;
                d.push(InputAction::GamepadCleared);
            }
            (None, None) => {}
        }
        d
    }

    /// Release everything currently held (§22).
    pub fn release_all(&mut self) -> InputDiff {
        let mut d = InputDiff::default();
        for code in std::mem::take(&mut self.held_keys) {
            d.push(InputAction::Key {
                physical_code: code,
                down: false,
            });
        }
        for bit in 0..16 {
            if self.mouse_buttons & (1 << bit) != 0 {
                d.push(InputAction::MouseButton {
                    button: bit,
                    down: false,
                });
            }
        }
        self.mouse_buttons = 0;
        if self.gamepad.take().is_some() {
            d.push(InputAction::GamepadCleared);
        }
        d
    }

    fn diff_mouse_buttons(&self, next: u16, d: &mut InputDiff) {
        let changed = self.mouse_buttons ^ next;
        for bit in 0..16 {
            if changed & (1 << bit) != 0 {
                d.push(InputAction::MouseButton {
                    button: bit,
                    down: next & (1 << bit) != 0,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_down_then_up_is_minimal() {
        let mut s = InputState::default();
        let d = s.apply_event(&InputEvent::Key {
            physical_code: 30,
            down: true,
            modifiers: 0,
        });
        assert_eq!(
            d.actions,
            vec![InputAction::Key {
                physical_code: 30,
                down: true
            }]
        );
        // repeat down = no-op
        let d = s.apply_event(&InputEvent::Key {
            physical_code: 30,
            down: true,
            modifiers: 0,
        });
        assert!(d.is_empty());
        let d = s.apply_event(&InputEvent::Key {
            physical_code: 30,
            down: false,
            modifiers: 0,
        });
        assert_eq!(
            d.actions,
            vec![InputAction::Key {
                physical_code: 30,
                down: false
            }]
        );
    }

    #[test]
    fn snapshot_releases_key_host_still_holds() {
        let mut s = InputState::default();
        s.apply_event(&InputEvent::Key {
            physical_code: 30,
            down: true,
            modifiers: 0,
        });
        s.apply_event(&InputEvent::Key {
            physical_code: 17,
            down: true,
            modifiers: 0,
        });
        // client says only key 17 is held now — 30's key-up was lost
        let d = s.reconcile_snapshot(&SnapshotState {
            held_keys: vec![17],
            ..Default::default()
        });
        assert!(d.actions.contains(&InputAction::Key {
            physical_code: 30,
            down: false
        }));
        assert!(!d.actions.iter().any(|a| matches!(
            a,
            InputAction::Key {
                physical_code: 17,
                ..
            }
        )));
    }

    #[test]
    fn release_all_clears_everything() {
        let mut s = InputState::default();
        s.apply_event(&InputEvent::Key {
            physical_code: 1,
            down: true,
            modifiers: 0,
        });
        s.apply_event(&InputEvent::MouseButtons { buttons: 0b1 });
        let d = s.release_all();
        assert!(d.actions.contains(&InputAction::Key {
            physical_code: 1,
            down: false
        }));
        assert!(d.actions.contains(&InputAction::MouseButton {
            button: 0,
            down: false
        }));
        assert!(s.release_all().is_empty());
    }
}
