//! Monitor enumeration via GDI (§29 step 2). Simpler than a second DXGI walk and
//! gives us name + resolution + refresh, which is all the dashboard needs.

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, ENUM_CURRENT_SETTINGS,
};

use crate::platform::MonitorInfo;

fn wstr(bytes: &[u16]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf16_lossy(&bytes[..end])
}

pub fn enumerate_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    let mut idx = 0u32;
    loop {
        let mut dev = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dev.cb` is set; PCWSTR::null() enumerates adapters.
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dev, 0).as_bool() };
        if !ok {
            break;
        }
        idx += 1;

        if dev.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0 {
            continue;
        }
        let primary = dev.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0;
        let device_name = dev.DeviceName;

        // Friendly monitor name from the child device.
        let mut mon = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let friendly = unsafe {
            if EnumDisplayDevicesW(PCWSTR(device_name.as_ptr()), 0, &mut mon, 0).as_bool() {
                wstr(&mon.DeviceString)
            } else {
                wstr(&dev.DeviceString)
            }
        };

        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: dmSize set; ENUM_CURRENT_SETTINGS reads the active mode.
        let has_mode = unsafe {
            EnumDisplaySettingsW(PCWSTR(device_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
                .as_bool()
        };
        let (w, h, hz) = if has_mode {
            (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
        } else {
            (0, 0, 0)
        };

        out.push(MonitorInfo {
            index: out.len() as u32,
            name: friendly,
            width: w,
            height: h,
            refresh_hz: hz,
            primary,
            adapter_luid: 0, // paired with DXGI output LUID in a later pass
        });
    }

    // Primary first, then by index — matches how `d3d11screencapturesrc`
    // `monitor-index` is ordered closely enough for v1.
    out.sort_by_key(|m| (!m.primary, m.index));
    for (i, m) in out.iter_mut().enumerate() {
        m.index = i as u32;
    }
    Ok(out)
}
