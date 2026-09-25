//! Can the desktop be captured right now?
//!
//! Desktop duplication is refused while Windows shows a secure desktop (the
//! lock screen, a UAC prompt, Ctrl+Alt+Del), when the session is disconnected,
//! or when the output is already duplicated too often. The d3d11 capture
//! element then fails to prepare, and tearing that pipeline down crashed the
//! host (see `Pipeline::abandon`). Asking first - a duplication opened and
//! released at once - turns that into a clear refusal before any pipeline
//! exists.

use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Device, D3D11_SDK_VERSION};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED,
};

/// `Err` carries the sentence to show the player.
pub fn desktop_capture_available() -> Result<(), String> {
    let result = unsafe { try_duplicate_primary() };
    result.map_err(|e| {
        let code = e.code();
        let why = if code == E_ACCESSDENIED {
            "The PC is locked or showing a Windows security prompt. Unlock it and try again."
        } else if code == DXGI_ERROR_SESSION_DISCONNECTED {
            "The PC's desktop session is disconnected (for example by Remote Desktop). Sign in on the PC and try again."
        } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
            "Too many programs are capturing the PC's screen. Close one (for example a recorder or another streaming app) and try again."
        } else if code == DXGI_ERROR_UNSUPPORTED {
            "This PC's graphics driver does not support desktop capture."
        } else {
            "The PC's screen can't be captured right now. Make sure it is awake and unlocked, then try again."
        };
        tracing::warn!(%e, "desktop capture unavailable - refusing the stream before building a pipeline");
        why.to_string()
    })
}

/// Duplicate the primary output (desktop origin at 0,0) and release it.
unsafe fn try_duplicate_primary() -> windows::core::Result<()> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut i = 0;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        i += 1;
        let mut j = 0;
        while let Ok(output) = adapter.EnumOutputs(j) {
            j += 1;
            let desc = output.GetDesc()?;
            let r = desc.DesktopCoordinates;
            if !desc.AttachedToDesktop.as_bool() || r.left != 0 || r.top != 0 {
                continue;
            }
            let adapter: IDXGIAdapter = adapter.cast()?;
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let Some(device) = device else {
                return Ok(());
            };
            let output1: IDXGIOutput1 = output.cast()?;
            // Dropped at once: this only asks whether a duplication is allowed.
            let _dup = output1.DuplicateOutput(&device)?;
            return Ok(());
        }
    }
    // No primary output found: let the pipeline report what it finds.
    Ok(())
}
