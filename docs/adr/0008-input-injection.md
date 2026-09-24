# ADR 0008 — `SendInput` default + optional established virtual-HID backend

**Status:** accepted (virtual-HID deferred to Phase 5) · **Report:** §13, §30, §27

## Decision
- Default keyboard/mouse backend: `SendInput` (`input/backends/sendinput.rs`).
  Supported Windows API, no install footprint. Surface the UIPI/integrity limit
  (can't inject into a higher-integrity window) as the "Enhanced/Elevated Input"
  option rather than a generic failure.
- Enhanced controller / raw-input compatibility: an **optional** component
  behind `VirtualHidBackend`, treated as a replaceable adapter.
- InPhase does **not** author or require its own kernel driver.

## Virtual-HID gate (before shipping Phase 5)
Do not make ViGEmBus the strategic dependency — archived/retired. Candidates:
LizardByte `libvirtualhid` (Windows licensing needs commercial review) and
HIDMaestro (UMDF2). Choose only after an explicit gate:
XInput identity verified · clean install/uninstall · representative game matrix ·
anti-cheat behaviour validated (compatibility only, never bypasses) · signing +
update lifecycle understood. `virtual_hid::COMPAT_GATE_PASSED` tracks this.

## Revisit trigger
Windows adds a supported user-mode gamepad-injection API, or the chosen backend
fails the product gates.
