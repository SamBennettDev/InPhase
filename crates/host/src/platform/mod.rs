//! Platform helpers (architecture report §3.1 `PlatformWindows`, §17
//! `platform/windows/`): monitor/GPU enumeration, tray icon, startup-at-login,
//! Windows Firewall rule.
//!
//! Non-Windows builds get inert stubs so the rest of the host `cargo check`s on
//! Linux.

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::*;

use serde::Serialize;

/// One installed title for the play-page library grid.
#[derive(Debug, Clone, Serialize)]
pub struct GameInfo {
    pub id: String,
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steam_app_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_catalog_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub install_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    /// Local cover art discovered during scanning. Served via [`poster_url`].
    #[serde(skip)]
    pub poster_path: Option<std::path::PathBuf>,
    /// Unix seconds the title was last played, when the launcher records it
    /// (Steam appmanifest `LastPlayed`). None = unknown - the library sorts
    /// these after the dated ones, alphabetically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
}

/// One display attached to the host (§29 step 2: *"Dashboard/CLI reports monitor
/// dimensions/refresh"*).
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
    /// DXGI adapter LUID this output hangs off — used to pin capture + encode to
    /// the same GPU.
    pub adapter_luid: i64,
}

/// One GPU / DXGI adapter (§6, §23, §29 step 2).
#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub description: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub dedicated_vram_mb: u64,
    pub luid: i64,
    /// GStreamer encoder element names that exist for this vendor (§6).
    pub encoder_elements: Vec<&'static str>,
}

/// One WASAPI audio endpoint (§11). `id` is `IMMDevice::GetId` — the exact
/// string `wasapi2src device=` wants.
///
/// `capture = false` is a *render* (playback) endpoint captured via WASAPI
/// loopback (the game's speakers). `capture = true` is a *recording* endpoint
/// read directly (a virtual mix like "Elgato Wave Link Stream", or a line-in).
#[derive(Debug, Clone, Serialize)]
pub struct AudioEndpointInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub active: bool,
    #[serde(default)]
    pub capture: bool,
}
