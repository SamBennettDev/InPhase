# Latency gate: reconnect stability (report §23)

**Pass condition:** 100 connect/disconnect cycles with no stuck input, no ghost
virtual controller, no resource leak.

## Procedure
- Script the browser client (Playwright/CDP) to pair → play 5 s → close, x100.
- After each cycle: host `state` returns to "Available" within 2 s.
- After the run:
  - process handle/thread/memory within +5% of the pre-run baseline;
  - no `Xbox 360 Controller` / virtual HID device left in Device Manager;
  - a fresh manual session has full keyboard/mouse control (no wedged keys).
