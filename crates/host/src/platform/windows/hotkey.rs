//! Emergency local disconnect hotkey (Phase 3 Input:
//! *"Complete … emergency local disconnect"*).
//!
//! `Ctrl+Alt+Shift+F12`, registered process-wide. It works even while a
//! full-screen game has the keyboard, and it is the operator's guaranteed way
//! to cut a remote session from the physical machine — independent of the
//! network, the browser, and the media pipeline.
//!
//! A dedicated thread owns a message queue; `RegisterHotKey(None, …)` posts
//! `WM_HOTKEY` straight to that queue, so no window is needed.

use std::sync::Arc;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_F12,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

const HOTKEY_ID: i32 = 0x0B1F;

/// Keeps the hotkey thread alive for the process lifetime.
pub struct EmergencyHotkey {
    _keepalive: Arc<()>,
}

impl EmergencyHotkey {
    /// Register the hotkey. `on_trigger` runs on the hotkey thread each time it
    /// is pressed. Returns `None` if registration failed (e.g. another process
    /// owns the combo) — the host still runs.
    pub fn spawn<F>(on_trigger: F) -> Option<Self>
    where
        F: Fn() + Send + 'static,
    {
        let keepalive = Arc::new(());
        let _hold = keepalive.clone();
        std::thread::Builder::new()
            .name("inphase-emergency-hotkey".into())
            .spawn(move || {
                let _hold = _hold;
                unsafe {
                    let mods = MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT;
                    if RegisterHotKey(None, HOTKEY_ID, mods, VK_F12.0 as u32).is_err() {
                        tracing::warn!(
                            "emergency-disconnect hotkey (Ctrl+Alt+Shift+F12) could not be \
                             registered — another app may own it"
                        );
                        return;
                    }
                    tracing::info!(
                        "emergency local disconnect armed — press Ctrl+Alt+Shift+F12 on the host"
                    );
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                            tracing::warn!(
                                "EMERGENCY LOCAL DISCONNECT — host keyboard triggered a teardown"
                            );
                            on_trigger();
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            _keepalive: keepalive,
        })
    }
}
