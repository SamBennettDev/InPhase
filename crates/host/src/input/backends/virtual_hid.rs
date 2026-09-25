//! Virtual-HID gamepad backend (architecture report §13, Phase 5, ADR-008).
//!
//! Emulates an Xbox 360 controller via **ViGEmBus** (`vigem-client`, pure Rust).
//! The report's caveat stands (§13, §30): ViGEmBus is archived — treat this as a
//! **replaceable adapter** behind [`InputBackend`], and run the compatibility /
//! signing / anti-cheat gate ([`COMPAT_GATE_PASSED`]) before shipping. Newer
//! options (LizardByte `libvirtualhid`, HIDMaestro) slot in here unchanged.
//!
//! Compiled in only with the `virtual-hid` feature; otherwise [`try_new`] is a
//! stub returning `None`.

#[allow(unused_imports)]
use crate::input::backends::InputBackend;

/// Try to construct the gamepad backend. `None` unless the `virtual-hid`
/// feature is built and ViGEmBus is available.
pub fn try_new() -> Option<Box<dyn InputBackend>> {
    #[cfg(all(windows, feature = "virtual-hid"))]
    {
        match vigem::VigemBackend::new() {
            Ok(b) => Some(Box::new(b)),
            Err(e) => {
                tracing::warn!("virtual-HID gamepad unavailable: {e:#}");
                None
            }
        }
    }
    #[cfg(not(all(windows, feature = "virtual-hid")))]
    {
        None
    }
}

/// Whether the ViGEmBus driver answers. Opening a client is a handle open on
/// the bus device; cached briefly because the dashboard polls every 2 s.
pub fn driver_present() -> bool {
    #[cfg(all(windows, feature = "virtual-hid"))]
    {
        use parking_lot::Mutex;
        use std::time::{Duration, Instant};
        static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
        let mut g = CACHE.lock();
        if let Some((at, present)) = *g {
            if at.elapsed() < Duration::from_secs(10) {
                return present;
            }
        }
        let present = vigem_client::Client::connect().is_ok();
        *g = Some((Instant::now(), present));
        present
    }
    #[cfg(not(all(windows, feature = "virtual-hid")))]
    {
        false
    }
}

/// Set to `true` only when the §5 Phase-5 gate has been executed and recorded:
/// XInput identity verified, install/uninstall clean, representative game matrix
/// + anti-cheat behaviour validated (compatibility only, never bypasses).
#[allow(dead_code)]
pub const COMPAT_GATE_PASSED: bool = false;

#[cfg(all(windows, feature = "virtual-hid"))]
mod vigem {
    use tracing::{info, warn};
    use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

    use crate::input::backends::InputBackend;
    use crate::input::state::{InputAction, InputDiff};

    /// Browser Gamepad-API "standard mapping" button index -> XUSB button bit.
    fn xusb_buttons(browser_bits: u32) -> u16 {
        const MAP: [(u32, u16); 15] = [
            (0, 0x1000),  // A
            (1, 0x2000),  // B
            (2, 0x4000),  // X
            (3, 0x8000),  // Y
            (4, 0x0100),  // LB
            (5, 0x0200),  // RB
            (8, 0x0020),  // Back / View
            (9, 0x0010),  // Start / Menu
            (10, 0x0040), // L3
            (11, 0x0080), // R3
            (12, 0x0001), // DPad Up
            (13, 0x0002), // DPad Down
            (14, 0x0004), // DPad Left
            (15, 0x0008), // DPad Right
            (16, 0x0400), // Guide
        ];
        let mut out = 0u16;
        for (idx, bit) in MAP {
            if browser_bits & (1 << idx) != 0 {
                out |= bit;
            }
        }
        out
    }

    fn neg_i16(v: i16) -> i16 {
        v.checked_neg().unwrap_or(i16::MAX)
    }

    pub struct VigemBackend {
        target: Xbox360Wired<Client>,
        state: XGamepad,
    }

    impl VigemBackend {
        pub fn new() -> anyhow::Result<Self> {
            let client = Client::connect().map_err(|e| {
                anyhow::anyhow!("ViGEmBus connect failed ({e:?}) — is the driver installed?")
            })?;
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            target
                .plugin()
                .map_err(|e| anyhow::anyhow!("ViGEm plugin failed: {e:?}"))?;
            let _ = target.wait_ready();
            info!("virtual Xbox 360 controller plugged in (ViGEmBus)");
            Ok(Self {
                target,
                state: XGamepad::default(),
            })
        }

        fn push(&mut self) {
            if let Err(e) = self.target.update(&self.state) {
                warn!("ViGEm update failed: {e:?}");
            }
        }
    }

    impl Drop for VigemBackend {
        fn drop(&mut self) {
            let _ = self.target.unplug();
        }
    }

    impl InputBackend for VigemBackend {
        fn name(&self) -> &str {
            "ViGEm Xbox360"
        }

        fn apply_diff(&mut self, diff: &InputDiff) {
            let mut dirty = false;
            for a in &diff.actions {
                match a {
                    InputAction::Gamepad(g) => {
                        self.state.buttons = XButtons {
                            raw: xusb_buttons(g.buttons),
                        };
                        // Browser Y axis is down-positive; XInput is up-positive.
                        self.state.thumb_lx = g.lx;
                        self.state.thumb_ly = neg_i16(g.ly);
                        self.state.thumb_rx = g.rx;
                        self.state.thumb_ry = neg_i16(g.ry);
                        self.state.left_trigger = (g.lt >> 8) as u8;
                        self.state.right_trigger = (g.rt >> 8) as u8;
                        dirty = true;
                    }
                    InputAction::GamepadCleared => {
                        self.state = XGamepad::default();
                        dirty = true;
                    }
                    _ => {}
                }
            }
            if dirty {
                self.push();
            }
        }

        fn release_all(&mut self) {
            self.state = XGamepad::default();
            self.push();
        }
    }
}
