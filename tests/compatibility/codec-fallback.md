# Compatibility: codec fallback (report §7, §22, §23)

**Pass condition:** an HEVC-capable host streaming to a non-HEVC client falls
back cleanly to H.264 — no failed connection, no manual step.

## Procedure
1. Host: set `media.allow_hevc = true`, confirm `nvd3d11h265enc` present.
2. Client A (HEVC-capable Chrome): connect → `session_config.codec` = `h265`,
   `inbound-rtp` codec stats confirm H265.
3. Client B (Firefox / HEVC disabled): connect → `session_config.codec` = `h264`
   automatically; stream plays.
4. Kill HEVC mid-negotiation (unsupported profile): host renegotiates H.264
   (§22 "HEVC negotiation fails → retry/renegotiate H.264 automatically").
