// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

//! InPhase Windows host — library root.
//!
//! The architecture report's one-sentence description (§1):
//!
//! > A per-user Rust Windows application hosts a tiny same-origin web client and
//! > authenticated WebRTC signaller; it lazily creates a GStreamer pipeline that
//! > captures a monitor as D3D11 textures, hardware-encodes into H.264 or HEVC,
//! > sends video plus Opus audio through WebRTC with GCC, receives low-latency
//! > binary input over a data channel, translates it to Windows input, and tears
//! > the media/input pipeline back down when the player disconnects.
//!
//! Module map (mirrors §3.1 "Host process model" and §17 "repository layout"):
//!
//! | Module            | Responsibility                                            |
//! |-------------------|----------------------------------------------------------|
//! | [`app`]           | `HostRuntime`: boot, config, lifecycle, shutdown          |
//! | [`config`]        | Typed configuration + per-user appdata persistence        |
//! | [`http`]          | Static assets, pair/status APIs, authenticated signaling  |
//! | [`pairing`]       | PIN generation, expiry, rate limit, session cookie        |
//! | [`session`]       | Exactly one active `PlayerSession`; state transitions     |
//! | [`media`]         | One GStreamer pipeline + its `webrtcbin` consumer         |
//! | [`input`]         | Decode packets, sequence/state/watchdog, Windows backend  |
//! | [`stats`]         | Normalise host + WebRTC + client telemetry                |
//! | [`platform`]      | Monitor/GPU enumeration, tray/startup/firewall helpers    |
//!
//! The discipline the report insists on (§3.1): these stay **adapters around
//! real primitives** (Windows capture, GPU encoders, GStreamer WebRTC, browser
//! APIs) and never grow into a generic streaming framework.

pub mod acme;
pub mod app;
pub mod config;
pub mod doctor;
pub mod endpoint;
pub mod gameart;
pub mod health;
pub mod http;
pub mod identity;
pub mod input;
pub mod media;
pub mod net;
pub mod pairing;
pub mod platform;
pub mod portmap;
pub mod session;
pub mod stats;
pub mod tls;

pub use app::HostRuntime;
pub use config::Config;

/// Version of the host, surfaced in `/api/v1/status` and logs.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identifies the exact build this binary came from — the git commit
/// `tools/ship.sh` shipped, plus a `-dirty.<digest>` suffix when it shipped an
/// uncommitted tree. `"dev"` for any build not made by `ship.sh`.
///
/// The embedded web bundle is compiled with the *same* value, so a page whose
/// `build_id` differs from the one `/api/v1/status` reports is running against a
/// host it was not built with, and reloads itself. That check is what closes the
/// stale-client class of failure: it fires on any change to any wire format,
/// rather than only when someone remembers to bump a protocol constant.
pub const BUILD_ID: &str = env!("INPHASE_BUILD_ID");
