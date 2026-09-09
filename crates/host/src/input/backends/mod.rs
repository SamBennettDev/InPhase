//! Windows input injection backends (architecture report §13 "Windows input
//! injection: the unavoidable compatibility decision").
//!
//! | Backend               | Purpose                              | Install impact                    |
//! |-----------------------|--------------------------------------|-----------------------------------|
//! | [`SendInputBackend`]  | Default mouse/keyboard, desktop ctrl  | none — ships in core              |
//! | `ElevatedInputBroker` | Inject into elevated games (§13)      | optional small elevated process   |
//! | [`VirtualHidBackend`] | XInput gamepad / raw-input compat     | installs an established virtual-HID|
//!
//! The report's wording (§13): *"InPhase does not develop its own kernel driver.
//! Core keyboard/mouse works with supported Windows APIs. Enhanced
//! controller/raw-input compatibility may install a separately maintained
//! virtual-HID component."* Backends therefore stay **replaceable adapters**.

use crate::config::InputConfig;
use crate::input::state::InputDiff;

#[cfg(windows)]
mod sendinput;
mod virtual_hid;

#[cfg(not(windows))]
mod noop;

/// A translation target for [`InputDiff`]. Implementations must be safe to call
/// from the async input task and must fully neutralise device state on
/// [`InputBackend::release_all`] (§22).
pub trait InputBackend: Send {
    fn name(&self) -> &str;
    /// Apply a batch of absolute/relative input actions.
    fn apply_diff(&mut self, diff: &InputDiff);
    /// Release every key/button/axis this backend can hold.
    fn release_all(&mut self);
}

/// Pick the default backend for keyboard/mouse (§13: `SendInput` by default).
pub fn new_default_backend(cfg: &InputConfig) -> Box<dyn InputBackend> {
    #[cfg(windows)]
    {
        let _ = cfg;
        Box::new(sendinput::SendInputBackend::new())
    }
    #[cfg(not(windows))]
    {
        let _ = cfg;
        Box::new(noop::NoopBackend::default())
    }
}

/// Pick the gamepad backend (Phase 5, §13). Only returns a real backend when
/// the `virtual-hid` feature is built **and** config opts in **and** the
/// compatibility gate has been recorded as passed.
pub fn new_gamepad_backend(cfg: &InputConfig) -> Option<Box<dyn InputBackend>> {
    if !cfg.enable_virtual_hid {
        return None;
    }
    virtual_hid::try_new()
}
