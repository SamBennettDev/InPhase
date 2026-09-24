//! Start-at-sign-in via the per-user `Run` key.
//!
//! This is the **single** autostart mechanism. The installer writes the same
//! value; the tray's "Start at sign-in" toggle adds/removes it. `reg.exe` keeps
//! it to one shell-out with no COM/unsafe surface, and the value is easy for a
//! user to inspect or delete (`Settings > Apps > Startup`).

use tracing::info;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "InPhaseHost";

/// The command the Run key launches. The exe is a windowless (`windows`
/// subsystem) binary, so it needs no launcher script or hidden-console wrapper.
fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

pub fn set_start_at_login(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        let cmd = run_command().ok_or_else(|| anyhow::anyhow!("cannot resolve the exe path"))?;
        let status = crate::proc::command("reg")
            .args([
                "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &cmd, "/f",
            ])
            .status()?;
        anyhow::ensure!(status.success(), "reg add failed ({:?})", status.code());
    } else {
        // Ignore "value not found" — deleting an absent value is a no-op.
        let _ = crate::proc::command("reg")
            .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .output();
    }
    info!(enabled, "start-at-sign-in updated");
    Ok(())
}

/// Whether the Run-key value currently exists (autostart is on).
pub fn start_at_login_enabled() -> bool {
    crate::proc::command("reg")
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
