// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

//! InPhase shared wire protocol.
//!
//! This crate is deliberately platform-neutral so it can be:
//!   * linked into the Windows `host` crate, and
//!   * mirrored, byte-for-byte, by the browser client in `web/src/input/protocol.ts`.
//!
//! Two protocols live here, and per the architecture report (§16) **both are
//! versioned from day one** so a stale cached web asset fails cleanly instead of
//! silently misbehaving:
//!
//! * [`input`] — the high-frequency **binary** input packet (§12.2). Sent on the
//!   unreliable/unordered `input` data channel.
//! * [`signaling`] — the JSON control/signaling messages (§15). Sent on the
//!   reliable `control` data channel and the `/api/v1/signal` WebSocket.
//! * [`wtvideo`] — the WebTransport video datagram + control-stream format
//!   (ADR-0011). Sent as QUIC datagrams plus a reliable bidirectional stream.
//!
//! The golden Rust <-> TypeScript test vectors (§19 Phase 3) live in
//! `tests/vectors.rs` and `web/src/input/protocol.vectors.ts`; keep them in sync.

#![forbid(unsafe_code)]

pub mod input;
pub mod signaling;
pub mod vectors;
pub mod wtvideo;

pub use input::{
    GamepadState, InputCodecError, InputEvent, InputHeader, InputKind, InputPacket, SnapshotState,
    INPUT_HEADER_LEN, INPUT_PROTOCOL_VERSION,
};
pub use signaling::{
    ClientFeatures, ClientTelemetry, DecodeHint, IceCandidate, InputCapabilities, QualityPreset,
    RequestedMode, RtpCodecCapability, SessionConfig, SignalError, SignalErrorCode, SignalMessage,
    StreamTarget, VideoCodec, SIGNALING_PROTOCOL_VERSION,
};
pub use wtvideo::{
    annexb_to_length_prefixed, fragment_parity, hvcc_description, WtClientMessage, WtFragment,
    WtFrame, WtHostMessage, WtVideoError, WT_AUDIO_DATAGRAM_TAG, WT_FRAGMENT_HEADER_LEN,
    WT_FRAME_KEY, WT_VIDEO_HEADER_LEN, WT_VIDEO_PROTOCOL_VERSION,
};
