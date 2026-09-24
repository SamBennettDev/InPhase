//! System timer resolution for the host process.
//!
//! Windows rounds every timed wait up to the system timer tick - 15.6 ms by
//! default. The WT sender paces datagrams by sleeping "1 ms" between token
//! refills, and each of those sleeps could last a whole tick: measured on
//! Cin-PC (2026-09-23), a frame's paced send took 6.7 ms at the median and
//! 18.9 ms at p95, and frames queued behind it waited up to 15 ms, for a
//! frame of ~20 datagrams that needs ~2 ms at the configured pace.
//!
//! Two calls, because Windows 11 ignores the first for a process with no
//! visible window - which is exactly this tray app - unless the second opts it
//! out of timer-resolution throttling.

use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};

/// Ask for a 1 ms timer for the life of the process. Best effort: a failure
/// is logged and streaming carries on at the default resolution.
pub fn raise_timer_resolution() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        // Control bit set, state bit clear: "do not throttle my timer".
        StateMask: 0,
    };
    // SAFETY: `state` is a valid PROCESS_POWER_THROTTLING_STATE that outlives
    // the call, and the size passed is its size.
    let unthrottled = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    // SAFETY: plain winmm call; balanced implicitly at process exit.
    let rc = unsafe { timeBeginPeriod(1) };
    tracing::info!(
        timer_resolution_ms = 1,
        ok = rc == 0,
        ignore_throttling = unthrottled.is_ok(),
        "raised the system timer resolution for frame pacing"
    );
}
