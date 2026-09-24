# Latency gate: queue discipline (report §8.2, §20, §23)

**Pass condition:** under induced bandwidth limits the raw/video queue never
grows beyond 0–1 frame; GCC lowers encoder bitrate before playout delay runs
away; stale frames are dropped, not buffered.

## Procedure
1. Start a session at 1440p120 Balanced.
2. `GET /api/v1/admin/status` once/sec; record `stats.host.raw_queue_frames`,
   `stats.host.encoder_bitrate_kbps`, `stats.transport.outbound_bitrate_kbps`,
   client `jitter_buffer_delay_ms`.
3. Throttle the NIC to 60%, 40%, 25% of the stream's clean-link bitrate for
   60 s each (e.g. `clumsy`, a managed switch, or Windows QoS policy).
4. Restore full bandwidth.

## Expected
- `raw_queue_frames` stays 0–1 throughout, never trends upward.
- `encoder_bitrate_kbps` drops within ~1–2 s of each throttle step.
- `jitter_buffer_delay_ms` returns near the preset target after each step; it
  does **not** monotonically climb.
- On restore, bitrate recovers toward the cap.
