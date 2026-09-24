//! DXGI adapter enumeration (§6, §23, §29 step 2).

use gstreamer as gst;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};

/// `DXGI_ADAPTER_FLAG_SOFTWARE` — the WARP / software rasteriser adapter.
const ADAPTER_FLAG_SOFTWARE: u32 = 2;

use crate::media::encoder_policy::{element_for, GpuVendor};
use crate::platform::GpuInfo;
use inphase_protocol::VideoCodec;

pub fn vendor_of(vendor_id: u16) -> GpuVendor {
    match vendor_id {
        0x10DE => GpuVendor::Nvidia,
        0x1002 | 0x1022 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        _ => GpuVendor::Other,
    }
}

pub fn enumerate_gpus() -> anyhow::Result<Vec<GpuInfo>> {
    let mut out = Vec::new();
    // SAFETY: standard DXGI factory + adapter enumeration; every COM pointer is
    // checked before use and released by `windows` RAII wrappers.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            if let Ok(desc) = adapter.GetDesc1() {
                let is_software = (desc.Flags & ADAPTER_FLAG_SOFTWARE) != 0;
                if !is_software {
                    let description = String::from_utf16_lossy(
                        &desc.Description[..desc
                            .Description
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.Description.len())],
                    );
                    let vendor_id = desc.VendorId as u16;
                    let vendor = vendor_of(vendor_id);
                    let luid = ((desc.AdapterLuid.HighPart as i64) << 32)
                        | (desc.AdapterLuid.LowPart as i64 & 0xFFFF_FFFF);
                    out.push(GpuInfo {
                        index: i,
                        description,
                        vendor_id,
                        device_id: desc.DeviceId as u16,
                        dedicated_vram_mb: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
                        luid,
                        encoder_elements: available_encoders(vendor),
                    });
                }
            }
            i += 1;
        }
    }
    Ok(out)
}

/// Encoder element names that both (a) the report maps to this vendor and
/// (b) exist in the current GStreamer registry (§6).
fn available_encoders(vendor: GpuVendor) -> Vec<&'static str> {
    let _ = gst::init();
    [VideoCodec::H264, VideoCodec::H265]
        .into_iter()
        .filter_map(|c| element_for(vendor, c))
        .filter(|name| gst::ElementFactory::find(name).is_some())
        .collect()
}

/// Vendor of the adapter with the most dedicated VRAM (the gaming GPU).
pub fn primary_gpu_vendor() -> Option<GpuVendor> {
    enumerate_gpus()
        .ok()?
        .into_iter()
        .max_by_key(|g| g.dedicated_vram_mb)
        .map(|g| vendor_of(g.vendor_id))
}
