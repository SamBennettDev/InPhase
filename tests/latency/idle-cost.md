# Latency gate: idle cost (report §23, §29)

**Pass condition:** with no player connected there is no capture/encoder
pipeline and near-zero GPU encode use.

## Procedure
1. Start `inphase-host.exe`; do not connect a browser.
2. Task Manager → Performance → GPU → "Video Encode": expect ~0%.
3. `GET /api/v1/admin/status`: `state` = "Available", `stats.host` all zero.
4. Connect, play 2 min, disconnect.
5. Re-check GPU encode → back to ~0%; `stats` reset; no leaked threads/handles
   (compare `handle`/`thread` counts before vs. 30 s after disconnect).
