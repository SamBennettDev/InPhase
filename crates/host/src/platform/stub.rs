//! Non-Windows stubs (dev/CI only). InPhase only runs on Windows.

use super::{AudioEndpointInfo, GpuInfo, MonitorInfo};
use crate::media::encoder_policy::GpuVendor;

pub fn enumerate_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    Ok(vec![MonitorInfo {
        index: 0,
        name: "stub-display".into(),
        width: 1920,
        height: 1080,
        refresh_hz: 60,
        primary: true,
        adapter_luid: 0,
    }])
}

pub fn enumerate_gpus() -> anyhow::Result<Vec<GpuInfo>> {
    Ok(Vec::new())
}

pub fn enumerate_audio_endpoints() -> anyhow::Result<Vec<AudioEndpointInfo>> {
    Ok(Vec::new())
}

pub fn enumerate_installed_games() -> anyhow::Result<Vec<super::GameInfo>> {
    Ok(Vec::new())
}

pub fn poster_path_for_game_id(_id: &str) -> Option<std::path::PathBuf> {
    None
}

pub fn launch_game(_id: &str) -> anyhow::Result<bool> {
    Ok(false)
}

pub fn primary_gpu_vendor() -> Option<GpuVendor> {
    None
}

pub fn ensure_firewall_rule(
    _ports: &[u16],
    _allowed_remote: &[String],
    _recreate: bool,
) -> anyhow::Result<()> {
    Ok(())
}

pub fn ignore_stray_ctrl_c() {}

pub fn raise_timer_resolution() {}

pub fn set_start_at_login(_enabled: bool) -> anyhow::Result<()> {
    Ok(())
}

pub fn start_at_login_enabled() -> bool {
    false
}

#[derive(Default)]
pub struct TrayModel {
    pub streaming: std::sync::atomic::AtomicBool,
    pub remote_access: std::sync::atomic::AtomicBool,
    pub start_at_login: std::sync::atomic::AtomicBool,
    pub status: std::sync::Mutex<String>,
    pub pin_line: std::sync::Mutex<String>,
}
impl TrayModel {
    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }

    pub fn set_pin_line(&self, s: impl Into<String>) {
        *self.pin_line.lock().unwrap() = s.into();
    }
}

pub struct TrayActions {
    pub open_dashboard: Box<dyn Fn() + Send + Sync>,
    pub set_remote_access: Box<dyn Fn(bool) + Send + Sync>,
    pub set_start_at_login: Box<dyn Fn(bool) + Send + Sync>,
    pub quit: Box<dyn Fn() + Send + Sync>,
}

pub struct Tray;
impl Tray {
    pub fn spawn(_model: std::sync::Arc<TrayModel>, _actions: TrayActions) -> Option<Self> {
        None
    }
    pub fn handle(&self) -> TrayHandle {
        TrayHandle
    }
    pub fn refresh(&self) {}
}

#[derive(Clone)]
pub struct TrayHandle;
impl TrayHandle {
    pub fn refresh(&self) {}
}

pub struct EmergencyHotkey;
impl EmergencyHotkey {
    pub fn spawn<F: Fn() + Send + 'static>(_on_trigger: F) -> Option<Self> {
        None
    }
}
