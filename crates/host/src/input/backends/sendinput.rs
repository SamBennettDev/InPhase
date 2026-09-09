//! `SendInput` keyboard/mouse backend (architecture report §13).
//!
//! Default backend. Supported Windows API, no install footprint. Known limits
//! the report calls out and the product surfaces rather than hides (§13, §22):
//!
//! * UIPI/integrity: a non-elevated process cannot inject into a higher-integrity
//!   window. When that happens we report the "Enhanced/Elevated Input" option
//!   instead of a generic failure (§22 "SendInput blocked/elevated target").
//! * `SendInput` does not create a real XInput controller — gamepad goes through
//!   [`super::virtual_hid`] (Phase 5).
//!
//! Wire contract: `physical_code` **is** the Windows set-1 scan code. A high
//! byte of `0xE0` marks an extended key (arrows, right-hand modifiers, nav
//! cluster). The browser client owns the `KeyboardEvent.code` → scan-code table
//! (`web/src/input/keycodes.ts`).

use tracing::warn;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

/// `SendInput` mouse-wheel unit (`WHEEL_DELTA` in the Win32 headers).
const WHEEL_DELTA: i32 = 120;
/// `XBUTTON1` / `XBUTTON2` values for `mouseData` on X-button events.
const XBUTTON1: i32 = 0x0001;
const XBUTTON2: i32 = 0x0002;

use crate::input::backends::InputBackend;
use crate::input::state::{InputAction, InputDiff};

pub struct SendInputBackend {
    /// Set once a `SendInput` call is rejected so we only warn once per session.
    injection_blocked: bool,
}

impl SendInputBackend {
    pub fn new() -> Self {
        Self {
            injection_blocked: false,
        }
    }

    fn send(&mut self, inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        // SAFETY: `inputs` is a valid, correctly-sized slice of `INPUT` for the
        // lifetime of the call; `cbSize` is `size_of::<INPUT>()`.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() && !self.injection_blocked {
            self.injection_blocked = true;
            warn!(
                "SendInput injected {sent}/{} events - likely blocked by UIPI (elevated \
                 target window). Surface the Enhanced/Elevated Input option.",
                inputs.len()
            );
        }
    }
}

impl InputBackend for SendInputBackend {
    fn name(&self) -> &str {
        "SendInput"
    }

    fn apply_diff(&mut self, diff: &InputDiff) {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(diff.actions.len());
        for a in &diff.actions {
            match a {
                InputAction::MouseMove { dx, dy } => {
                    inputs.push(mouse(*dx, *dy, 0, MOUSEEVENTF_MOVE));
                }
                InputAction::Wheel { dx, dy } => {
                    if *dy != 0 {
                        inputs.push(mouse(0, 0, -*dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
                    }
                    if *dx != 0 {
                        inputs.push(mouse(0, 0, *dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
                    }
                }
                InputAction::MouseButton { button, down } => {
                    inputs.push(mouse_button(*button, *down));
                }
                InputAction::Key {
                    physical_code,
                    down,
                } => {
                    inputs.push(key(*physical_code, *down));
                }
                // Gamepad is handled by the virtual-HID backend, not SendInput.
                InputAction::Gamepad(_) | InputAction::GamepadCleared => {}
            }
        }
        self.send(&inputs);
    }

    fn release_all(&mut self) {
        // State-level release_all already produced key-up / button-up actions;
        // nothing device-global to reset for SendInput.
    }
}

fn key(physical_code: u16, down: bool) -> INPUT {
    let extended = (physical_code >> 8) == 0xE0;
    let scan = physical_code & 0xFF;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Button bit → `MOUSEEVENTF_*` (browser `MouseEvent.buttons` order:
/// 0=left, 1=right, 2=middle, 3=x1, 4=x2).
fn mouse_button(button: u8, down: bool) -> INPUT {
    match button {
        0 => mouse(
            0,
            0,
            0,
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
        ),
        1 => mouse(
            0,
            0,
            0,
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
        ),
        2 => mouse(
            0,
            0,
            0,
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
        ),
        3 => mouse(
            0,
            0,
            XBUTTON1,
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ),
        _ => mouse(
            0,
            0,
            XBUTTON2,
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ),
    }
}
