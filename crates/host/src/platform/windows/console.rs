//! Console control-signal handling.
//!
//! The host runs unattended in an interactive-session console (launched at
//! sign-in, §3.1). A stray `CTRL_C` / `CTRL_BREAK` — e.g. an SSH session sharing
//! the session being interrupted — would otherwise kill it with
//! `STATUS_CONTROL_C_EXIT`. Ignore those; still honour a real window-close /
//! logoff / shutdown so the process exits cleanly with the desktop.

use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};

unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        // Swallow: TRUE = "handled, do not terminate".
        CTRL_C_EVENT | CTRL_BREAK_EVENT => BOOL(1),
        // CTRL_CLOSE / CTRL_LOGOFF / CTRL_SHUTDOWN: FALSE = run the default
        // handler so we exit cleanly with the session.
        _ => BOOL(0),
    }
}

/// Install the handler. Best-effort — a failure just leaves the default
/// (terminate on Ctrl+C) behaviour in place.
pub fn ignore_stray_ctrl_c() {
    // SAFETY: `handler` is a valid `PHANDLER_ROUTINE`; the call is idempotent.
    let _ = unsafe { SetConsoleCtrlHandler(Some(handler), BOOL(1)) };
}
