//! WASAPI render-endpoint enumeration for loopback audio capture (§11).
//!
//! `wasapi2src device=…` takes the exact string from `IMMDevice::GetId`, so we
//! ask Windows directly (via Core Audio) rather than scraping GStreamer's device
//! monitor — that gives stable ids, friendly names, and default/active status,
//! and matches how WASAPI loopback is meant to be initialised (obtain the render
//! `IMMDevice`, activate its client in loopback mode).

use windows::core::PWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, DEVICE_STATE_UNPLUGGED,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

use crate::platform::AudioEndpointInfo;

/// Take-ownership convert of a `CoTaskMem`-allocated wide string.
unsafe fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
    s
}

/// All render (playback → loopback) endpoints plus all capture (recording)
/// endpoints. Render endpoints come first and `is_default` marks the default
/// playback device. Capture endpoints carry `capture = true` and the pipeline
/// reads them directly (no loopback) — that is how a virtual mix such as
/// "Elgato Wave Link Stream" is captured.
pub fn enumerate_audio_endpoints() -> anyhow::Result<Vec<AudioEndpointInfo>> {
    unsafe {
        // COM may already be initialised on this thread (S_FALSE) or in a
        // different mode (RPC_E_CHANGED_MODE); either is fine for our use.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

        let default_render = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(|p| take_pwstr(p));

        let states = DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_UNPLUGGED.0);
        let mut out = Vec::new();

        for (flow, is_capture) in [(eRender, false), (eCapture, true)] {
            collect_flow(
                &enumerator,
                flow,
                is_capture,
                default_render.as_deref(),
                states,
                &mut out,
            );
        }

        // Render first, then default, then active, then by name.
        out.sort_by(|a, b| {
            a.capture
                .cmp(&b.capture)
                .then(b.is_default.cmp(&a.is_default))
                .then(b.active.cmp(&a.active))
                .then(a.name.cmp(&b.name))
        });
        Ok(out)
    }
}

unsafe fn collect_flow(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    is_capture: bool,
    default_render_id: Option<&str>,
    states: DEVICE_STATE,
    out: &mut Vec<AudioEndpointInfo>,
) {
    let Ok(collection) = enumerator.EnumAudioEndpoints(flow, states) else {
        return;
    };
    let Ok(count) = collection.GetCount() else {
        return;
    };
    for i in 0..count {
        let Ok(dev) = collection.Item(i) else {
            continue;
        };
        let Ok(id) = dev.GetId() else { continue };
        let id = take_pwstr(id);
        if id.is_empty() {
            continue;
        }
        let name = dev
            .OpenPropertyStore(STGM_READ)
            .ok()
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName).ok())
            .map(|var| var.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());
        let active = dev
            .GetState()
            .map(|s| s.0 & DEVICE_STATE_ACTIVE.0 != 0)
            .unwrap_or(false);
        out.push(AudioEndpointInfo {
            is_default: !is_capture && default_render_id == Some(id.as_str()),
            id,
            name,
            active,
            capture: is_capture,
        });
    }
}
