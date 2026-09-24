# Compatibility: host GPU matrix (report §6, §20, §23)

For each of NVIDIA (Turing/Ampere/Ada+), AMD (RDNA2/3), Intel (Arc+):

| Check | How | Pass |
|-------|-----|------|
| Intended encoder element selected | host log `gpu ... encoders=[...]`; session `encoder_backend` | matches §6 table for the vendor |
| No B-frames / no lookahead | `gst-inspect` the element + confirm `media/encoder_policy.rs` props applied | b-frames=0, lookahead=0 |
| Encode latency | `stats.host.encode_ms_p95` at each target mode | within frame budget (8.3 ms @120, 16.7 ms @60) |
| No CPU fallback | disable HW encoder, start session | fails with `no_hardware_encoder`, **never** loads x264 |
| All four modes | 1080p60/120, 1440p60/120 × H.264 | FPS + latency stable on reference HW |

Reference machine on file: **NVIDIA RTX 3070** — capture + HEVC/H.264 encode +
end-to-end stream verified at 1080p60 and 1440p60 under GPU load.
