//! Encoder selection + low-latency configuration (architecture report §6
//! "Hardware encoding policy" and §9 "Quality presets and starting bitrates").
//!
//! Rules:
//! * hardware-only in the normal path; if no supported HW encoder exists, return
//!   an error — never silently load `x264enc` (§6, §22, ADR-005);
//! * no B-frames, no lookahead, zero-latency rate control everywhere (§6
//!   *"a slightly higher bitrate is preferable to latency hidden inside the
//!   encoder"*);
//! * per-vendor knobs stay **inside this module** — they must not leak into
//!   app-wide code (§30, risk "GPU vendor variance").

use inphase_protocol::{QualityPreset, VideoCodec};

/// GPU vendor families the report enumerates (§6, §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    /// Unknown discrete/integrated GPU — fall back to the generic Media
    /// Foundation D3D11 encoder (§6 "Generic Windows").
    Other,
}

/// A concrete encoder choice for the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderChoice {
    pub vendor: GpuVendor,
    pub codec: VideoCodec,
    /// GStreamer element factory name, e.g. `nvd3d11h264enc` (§6).
    pub element: &'static str,
    /// Properties to set on the element for a low-latency interactive stream.
    /// Names differ per vendor — that is the whole point of centralising here.
    pub properties: Vec<(&'static str, EncProp)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncProp {
    Bool(bool),
    Uint(u32),
    Int(i64),
    Enum(&'static str),
}

impl EncProp {
    /// Render for `Element::set_property_from_str`, which coerces the string into
    /// the property's real GType (gint/gint64/guint/gboolean/enum-nick).
    pub fn to_prop_string(&self) -> String {
        match self {
            EncProp::Bool(b) => b.to_string(),
            EncProp::Uint(u) => u.to_string(),
            EncProp::Int(i) => i.to_string(),
            EncProp::Enum(s) => (*s).to_string(),
        }
    }
}

/// No hardware encoder is available for any acceptable codec (§6, §22).
#[derive(Debug, thiserror::Error)]
#[error(
    "no supported hardware video encoder found for {codec:?} on this GPU \
         (InPhase never falls back to a CPU encoder - see sec 6)"
)]
pub struct NoHardwareEncoder {
    pub codec: VideoCodec,
}

/// Static per-(vendor, codec) element table (§6). Selection at runtime still
/// checks the element actually exists in the registry before committing.
pub fn element_for(vendor: GpuVendor, codec: VideoCodec) -> Option<&'static str> {
    Some(match (vendor, codec) {
        (GpuVendor::Nvidia, VideoCodec::H264) => "nvd3d11h264enc",
        (GpuVendor::Nvidia, VideoCodec::H265) => "nvd3d11h265enc",
        (GpuVendor::Amd, VideoCodec::H264) => "amfh264enc",
        (GpuVendor::Amd, VideoCodec::H265) => "amfh265enc",
        (GpuVendor::Intel, VideoCodec::H264) => "qsvh264enc",
        (GpuVendor::Intel, VideoCodec::H265) => "qsvh265enc",
        (GpuVendor::Other, VideoCodec::H264) => "mfh264enc",
        (GpuVendor::Other, VideoCodec::H265) => "mfh265enc",
    })
}

/// Build the low-latency property set for a chosen encoder (§6 table).
///
/// The property *names* are best-effort per the current GStreamer elements and
/// are verified/adjusted against `gst-inspect` output during Phase 0 (§19).
pub fn low_latency_properties(
    vendor: GpuVendor,
    _codec: VideoCodec,
    bitrate_kbps: u32,
) -> Vec<(&'static str, EncProp)> {
    let br = EncProp::Uint(bitrate_kbps);
    match vendor {
        // NVIDIA `nvd3d11h264enc` / `nvd3d11h265enc` (property names verified
        // against gst-inspect on the deployed machine): zero-latency, no
        // B-frames, no lookahead, no AQ, infinite GOP (keyframes forced on
        // demand), single CBR pass. `preset=p1` + `tune=ultra-low-latency` is
        // the modern lowest-latency combo — legacy presets like
        // `low-latency-hp` ignore `tune` entirely (GstNvEncoder docs).
        GpuVendor::Nvidia => vec![
            ("zerolatency", EncProp::Bool(true)),
            ("bframes", EncProp::Uint(0)),
            ("b-adapt", EncProp::Bool(false)),
            ("rc-lookahead", EncProp::Uint(0)),
            ("spatial-aq", EncProp::Bool(false)),
            ("temporal-aq", EncProp::Bool(false)),
            // Effectively-infinite GOP: automatic IDR every ~1 year of
            // 60 fps frames. Keyframes are FORCED on demand (connect, rung
            // change, client request) - an automatic 1 s keyframe would
            // regularly pound lossy routes with a 20x frame burst (§6).
            // The comment above this vector is the contract.
            ("gop-size", EncProp::Int(0x3FFF_FFFF)),
            ("rc-mode", EncProp::Enum("cbr")),
            ("multi-pass", EncProp::Enum("disabled")),
            ("tune", EncProp::Enum("ultra-low-latency")),
            ("preset", EncProp::Enum("p1")),
            ("aud", EncProp::Bool(false)),
            // Repeat VPS/SPS/PPS before EVERY IDR, forced ones included.
            // Without it only the startup IDR carries parameter sets, so a
            // client that missed that one frame (03:55 Chrome session: the
            // wedged stream ate it) can never bootstrap from a recovery IDR
            // - decode() consumes chunks, produces nothing, logs nothing.
            // Ignored if the negotiated stream-format is "hvc1", which this
            // pipeline never is (Annex-B, see webrtcbin).
            ("repeat-sequence-header", EncProp::Bool(true)),
            ("bitrate", br),
            ("max-bitrate", EncProp::Uint(bitrate_kbps)), // CBR: cap == target
        ],
        // AMD `amfh264enc` / `amfh265enc` (§6).
        GpuVendor::Amd => vec![
            ("usage", EncProp::Enum("ultra-low-latency")),
            ("bframes", EncProp::Uint(0)),
            ("preanalysis", EncProp::Bool(false)),
            ("rate-control", EncProp::Enum("cbr")),
            ("bitrate", br),
            ("max-bitrate", EncProp::Uint(bitrate_kbps)),
        ],
        // Intel QSV `qsvh264enc` / `qsvh265enc` (§6).
        GpuVendor::Intel => vec![
            ("b-frames", EncProp::Uint(0)),
            ("low-latency", EncProp::Bool(true)),
            ("target-usage", EncProp::Uint(7)),
            ("rate-control", EncProp::Enum("cbr")),
            ("bitrate", br),
            ("max-bitrate", EncProp::Uint(bitrate_kbps)),
        ],
        // Media Foundation `mfh264enc` / `mfh265enc` (§6).
        GpuVendor::Other => vec![
            ("low-latency", EncProp::Bool(true)),
            ("rc-mode", EncProp::Enum("cbr")),
            ("bitrate", br),
        ],
    }
}

/// Quality preset → (start, max) bitrate in kbps for a given mode (§9 table).
/// Numbers are engineering starting points that seed GCC and define caps — not
/// codec truths (§9).
pub fn bitrate_kbps(codec: VideoCodec, width: u32, height: u32, fps: u32) -> (u32, u32, u32) {
    // (start, max, floor)
    let (start, max) = match (codec, height, fps) {
        (VideoCodec::H264, 1080, f) if f <= 60 => (20_000, 35_000),
        (VideoCodec::H264, 1080, _) => (30_000, 50_000),
        (VideoCodec::H264, 1440, f) if f <= 60 => (30_000, 50_000),
        (VideoCodec::H264, 1440, _) => (50_000, 85_000),
        (VideoCodec::H265, 1080, f) if f <= 60 => (14_000, 25_000),
        (VideoCodec::H265, 1080, _) => (22_000, 35_000),
        (VideoCodec::H265, 1440, f) if f <= 60 => (22_000, 35_000),
        (VideoCodec::H265, 1440, _) => (35_000, 60_000),
        // Unlisted resolutions: scale from 1080p60 by pixel-rate.
        (c, h, f) => {
            let base = if c == VideoCodec::H264 {
                20_000.0
            } else {
                14_000.0
            };
            let scale = (width as f32 * h as f32 * f.max(1) as f32) / (1920.0 * 1080.0 * 60.0);
            let s = (base * scale) as u32;
            (s.max(4_000), (s as f32 * 1.7) as u32)
        }
    };
    // Floor: avoid catastrophic blockiness but allow app-level step-down (§9).
    let floor = (start / 5).max(2_000);
    (start, max, floor)
}

/// Suggested `RTCRtpReceiver.jitterBufferTarget` in ms (§9/§10).
///
/// Always **0** — the product carries no playout buffer and this is not
/// configurable: every frame is presented the instant it decodes. The client
/// pins `jitterBufferTarget=0` itself and ignores this field; it stays on the
/// wire for protocol compatibility (older clients, external controllers).
/// Browsers may still enforce their own internal floor (Safari clamps to ~2
/// frames; Chrome honours 0 exactly).
pub fn jitter_buffer_target_ms(_preset: QualityPreset) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_h264_is_nvd3d11() {
        assert_eq!(
            element_for(GpuVendor::Nvidia, VideoCodec::H264),
            Some("nvd3d11h264enc")
        );
    }

    #[test]
    fn no_lookahead_or_bframes_for_nvidia() {
        let props = low_latency_properties(GpuVendor::Nvidia, VideoCodec::H264, 30_000);
        assert!(props.contains(&("bframes", EncProp::Uint(0))));
        assert!(props.contains(&("rc-lookahead", EncProp::Uint(0))));
        assert!(props.contains(&("gop-size", EncProp::Int(0x3FFF_FFFF))));
        assert!(props.iter().all(|(k, _)| *k != "rate-control")); // real name is rc-mode
    }

    #[test]
    fn bitrate_table_matches_report() {
        assert_eq!(bitrate_kbps(VideoCodec::H264, 1920, 1080, 60).0, 20_000);
        assert_eq!(bitrate_kbps(VideoCodec::H264, 1920, 1080, 120).1, 50_000);
        assert_eq!(bitrate_kbps(VideoCodec::H264, 2560, 1440, 120).0, 50_000);
        assert_eq!(bitrate_kbps(VideoCodec::H265, 2560, 1440, 60).1, 35_000);
    }

    #[test]
    fn jitter_buffer_is_always_zero() {
        for preset in [
            QualityPreset::LowLatency,
            QualityPreset::Balanced,
            QualityPreset::Quality,
            QualityPreset::Custom,
        ] {
            assert_eq!(jitter_buffer_target_ms(preset), 0);
        }
    }
}
