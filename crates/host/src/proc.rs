//! Child processes that never flash a console window.
//!
//! The host is a windowless (`windows` subsystem) program. When it starts a
//! console program - netsh, reg, certutil, tasklist, cmd - Windows gives that
//! child a console window of its own unless told not to, so every check at
//! startup flashed a terminal on screen. Every command the host runs goes
//! through here.

use std::ffi::OsStr;

/// `CREATE_NO_WINDOW`: the child runs without a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `std::process::Command` for `program` that opens no console window.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// The `tokio::process::Command` counterpart of [`command`].
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut c = tokio::process::Command::new(program);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c
}
