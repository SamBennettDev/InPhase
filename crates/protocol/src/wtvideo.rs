//! WebTransport video wire format — **v3, one self-delimiting frame per
//! unidirectional stream**.
//!
//! Each encoded access unit is written to its own QUIC unidirectional stream and
//! the stream is closed. That single decision is the whole protocol, and it is
//! what QUIC is for: fragmentation, retransmission, ordering and flow control
//! all belong to the transport, and streams are independent so a stall in one
//! cannot block another.
//!
//! v1 put frames in datagrams and rebuilt all of that by hand — an
//! application-layer fragmenter, a reassembler, interleaved XOR parity, a
//! re-send cache, a NACK protocol, a fragment-loss estimator and a pacer.
//! About 1,500 lines, and the source of essentially every defect the project
//! hit: fragments sized past the path MTU and silently black-holed, parity
//! recovery that never worked in the browser, loss accounting wrong three
//! separate ways, NACK bursts that panicked the QUIC stack, and a pacer that
//! raced the NACK sweep it shared a frame with. None of those failures are
//! expressible here.
//!
//! Stream payload, header **little-endian**, 18 bytes, then the access unit:
//!
//! ```text
//!   version:    u8   // == WT_VIDEO_PROTOCOL_VERSION (3)
//!   flags:      u8   // WT_FRAME_KEY
//!   frame_no:   u32  // per-connection monotonic, wraps
//!   capture_us: u64  // host capture monotonic clock, µs (FrameTrace semantics)
//!   payload_len: u32 // access-unit length; the frame needs no stream EOF
//! ```
//!
//! v3 added `payload_len` (v2's header was 14 bytes). v2 delimiters were
//! EOF-only: the client read each stream to the end, which made every frame
//! wait on the peer's FIN. WebKit on iOS intermittently never completes a
//! stream's reads, and an unread stream holds connection flow-control credit
//! forever — the iPhone Safari freeze ~10 s into every session. A
//! self-delimiting frame lets the client read exactly the frame's bytes and
//! release the stream without caring whether a FIN ever arrives.
//!
//! The payload is the encoder's Annex-B byte stream verbatim, parameter sets
//! in-band — which is what WebCodecs expects from a config with no
//! `description`, and what Safari decodes natively.
//!
//! Latency is bounded by the *reader*, not by retransmission: a frame that has
//! not fully arrived by its playout deadline is abandoned and its stream reset.
//! QUIC will keep trying to deliver a stream forever; the application decides
//! when a frame has stopped being worth having.
//!
//! Control traffic (auth, keyframe requests, telemetry, input) rides the
//! reliable bidirectional stream as newline-delimited JSON — see
//! [`WtClientMessage`] and [`WtHostMessage`].

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Wire version. v1 was the datagram fragmenter; v2 is one frame per stream.
pub const WT_VIDEO_PROTOCOL_VERSION: u8 = 3;

/// Fixed header length in bytes.
pub const WT_VIDEO_HEADER_LEN: usize = 18;

/// The frame is a keyframe (IDR/CRA) — a clean rejoin point for a client.
pub const WT_FRAME_KEY: u8 = 0x01;

/// Header flag bits this parser knows. Unknown bits are ignored for forward
/// compatibility; the version byte is strict.
pub const WT_FRAME_KNOWN: u8 = WT_FRAME_KEY;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WtVideoError {
    #[error("stream shorter than the {WT_VIDEO_HEADER_LEN}-byte frame header")]
    Truncated,
    #[error("unknown video wire version {0} (this host speaks v{WT_VIDEO_PROTOCOL_VERSION})")]
    BadVersion(u8),
}

/// One encoded frame, as it goes on the wire and as it comes off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WtFrame {
    pub frame_no: u32,
    pub capture_us: u64,
    pub key: bool,
    pub payload: Vec<u8>,
}

impl WtFrame {
    /// Header + payload, ready to write to a fresh unidirectional stream.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(WT_VIDEO_HEADER_LEN + self.payload.len());
        out.push(WT_VIDEO_PROTOCOL_VERSION);
        out.push(if self.key { WT_FRAME_KEY } else { 0 });
        out.extend_from_slice(&self.frame_no.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse a complete frame body (header + exactly `payload_len` payload
    /// bytes).
    pub fn decode(buf: &[u8]) -> Result<Self, WtVideoError> {
        if buf.len() < WT_VIDEO_HEADER_LEN {
            return Err(WtVideoError::Truncated);
        }
        let payload_len =
            u32::from_le_bytes([buf[14], buf[15], buf[16], buf[17]]) as usize;
        if buf.len() < WT_VIDEO_HEADER_LEN + payload_len {
            return Err(WtVideoError::Truncated);
        }
        if buf[0] != WT_VIDEO_PROTOCOL_VERSION {
            return Err(WtVideoError::BadVersion(buf[0]));
        }
        Ok(Self {
            key: buf[1] & WT_FRAME_KEY != 0,
            frame_no: u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]),
            capture_us: u64::from_le_bytes([
                buf[6], buf[7], buf[8], buf[9], buf[10], buf[11], buf[12], buf[13],
            ]),
            payload: buf[WT_VIDEO_HEADER_LEN..WT_VIDEO_HEADER_LEN + payload_len].to_vec(),
        })
    }
}

// ---- control stream (JSON, reliable) -----------------------------------------

/// Client → host messages on the WT control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WtClientMessage {
    /// First message on the stream. The host drops datagrams and refuses media
    /// until the one-time token issued over the authenticated signaling
    /// WebSocket validates (ADR-0011: a QUIC dial carries no credentials).
    Auth {
        token: String,
    },
    /// Lost a keyframe / gap too large to wait out.
    KeyframeRequest,
    /// The client applied a configuration change (resolution/codec epoch).
    ConfigAck {
        epoch: u32,
    },
    /// The same per-second telemetry the WebRTC path sends (§20 dashboard).
    /// Wire tag matches stats.ts exactly - the WebRTC control channel parses
    /// `client_telemetry`, so the WT control stream must too.
    #[serde(rename = "client_telemetry")]
    Telemetry(crate::signaling::ClientTelemetry),
    Ping {
        at_us: u64,
    },
    /// One input packet (§12 wire format, base64 of the exact datagram bytes).
    /// The datagram path is faster but iOS Safari never delivers client →
    /// host datagrams (2026-09-08 evidence: zero Input events from the phone
    /// across every session, while control-stream messages flowed). Input
    /// rides BOTH: datagrams where they work, the control stream always.
    /// The host's sequence-freshness gate dedupes the double delivery.
    Input {
        data_b64: String,
    },
    /// Switch the video carrier to v4 datagram fragments (client must have
    /// proof that host→client datagrams arrive: audio datagrams flowing).
    /// The client can switch back with `disable_datagram_video` — the
    /// reliable stream stays installed as the fallback carrier.
    EnableDatagramVideo,
    DisableDatagramVideo,
    /// One v4 fragment of `frame` did not arrive; the host re-sends it from
    /// its resend cache as a datagram. Reliable, ordered control channel, so
    /// a NACK is never lost — the same property that makes the pong sync
    /// work on iOS. Repairing one fragment costs one RTT; the alternative is
    /// holding decode until an IDR reassembles through burst loss.
    Nack {
        frame: u32,
        idx: u16,
    },
}

/// Host → client messages on the WT control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WtHostMessage {
    /// The route cannot carry the bitrate the user chose (§"respect user
    /// inputs"): shown in the client's HUD instead of silently degrading
    /// their resolution. Pushed at most once per 30 s while it holds.
    RouteWarning {
        detail: String,
    },
    /// Media description for the datagram stream: what was negotiated, and any
    /// out-of-band codec description. Annex-B CSD travels in-band with the
    /// keyframe, so `description_b64` is usually `None`.
    VideoConfig {
        codec: String,
        width: u32,
        height: u32,
        fps: u32,
        start_bitrate_kbps: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description_b64: Option<String>,
        /// Bumped on every config push; the client acknowledges it so the
        /// host can see when a resolution/codec change has been applied
        /// (review §5: acknowledge configuration changes).
        #[serde(default)]
        epoch: u32,
    },
    /// Echoes the client's ping. `host_us` is the host's receive time
    /// expressed on the capture (PTS) clock, letting the client estimate the
    /// host↔client clock offset NTP-style and age each frame truthfully
    /// (`0` = no capture clock yet — the client ignores the offset).
    Pong {
        at_us: u64,
        #[serde(default)]
        host_us: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    fn frame(key: bool, payload: &[u8]) -> WtFrame {
        WtFrame {
            frame_no: 77,
            capture_us: 1_234_567_890,
            key,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn round_trips() {
        for key in [true, false] {
            let f = frame(key, &[9u8; 5000]);
            assert_eq!(WtFrame::decode(&f.encode()).unwrap(), f);
        }
    }

    #[test]
    fn a_frame_of_any_size_is_one_stream() {
        // The whole point of v2: no chunking, no MTU to get wrong. A 2 MB
        // keyframe is one write, and the 300-datagram keyframes that could not
        // be delivered on a lossy route are not a thing that can happen.
        let f = frame(true, &vec![7u8; 2 * 1024 * 1024]);
        let wire = f.encode();
        assert_eq!(wire.len(), WT_VIDEO_HEADER_LEN + 2 * 1024 * 1024);
        assert_eq!(
            WtFrame::decode(&wire).unwrap().payload.len(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn an_empty_payload_is_legal() {
        let f = frame(false, &[]);
        assert_eq!(WtFrame::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn rejects_truncated_and_wrong_version() {
        assert_eq!(WtFrame::decode(&[]), Err(WtVideoError::Truncated));
        assert_eq!(
            WtFrame::decode(&[0u8; WT_VIDEO_HEADER_LEN - 1]),
            Err(WtVideoError::Truncated)
        );
        let mut wire = frame(true, &[1, 2, 3]).encode();
        wire[0] = 1; // the v1 datagram format
        assert_eq!(WtFrame::decode(&wire), Err(WtVideoError::BadVersion(1)));
    }

    #[test]
    fn unknown_flag_bits_are_ignored() {
        let mut wire = frame(true, &[4, 5]).encode();
        wire[1] |= 0x80;
        assert!(WtFrame::decode(&wire).unwrap().key);
    }
}

/// Chrome (macOS at minimum) refuses description-less HEVC WebCodecs configs,
/// so the client needs an `HEVCDecoderConfigurationRecord`. The encoder tap
/// carries GStreamer's Annex-B byte stream, which carries the parameter sets
/// in-band; this harvests one each of VPS/SPS/PPS from an access unit and
/// builds the record. Assumes 4:2:0 8-bit output — the encoder's fixed
/// configuration.
///
/// Returns `None` until all three parameter sets have been seen (e.g. before
/// the first IDR).
pub fn hvcc_description(au: &[u8]) -> Option<Vec<u8>> {
    let mut vps: Option<&[u8]> = None;
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for nal in annexb_nals(au) {
        if nal.len() < 2 {
            continue;
        }
        match (nal[0] >> 1) & 0x3F {
            32 if vps.is_none() => vps = Some(nal),
            33 if sps.is_none() => sps = Some(nal),
            34 if pps.is_none() => pps = Some(nal),
            _ => {}
        }
    }
    let (vps, sps, pps) = (vps?, sps?, pps?);

    // The record copies `profile_tier_level` out of the SPS: 12 bytes covering
    // profile_space/tier/idc, 4 compatibility flags, 6 constraint flags, and
    // general_level_idc.
    //
    // Two things make that harder than indexing the NAL directly, and getting
    // either wrong ships a description a decoder will not produce frames from:
    //
    //  * The bytes must come from the **RBSP**. HEVC escapes any `00 00 00|01|
    //    02|03` in the payload as `00 00 03 xx`, and the constraint flags are
    //    almost entirely zeros - so a real SPS reliably carries emulation
    //    prevention bytes *inside* the profile block. This is the normal case,
    //    not an edge case.
    //  * `profile_tier_level` starts one byte into the RBSP, after
    //    sps_video_parameter_set_id / sps_max_sub_layers_minus1 /
    //    sps_temporal_id_nesting_flag.
    //
    // Reading the raw NAL at `sps[2..14]` did both wrong. On a real 1080p Main
    // SPS it produced general_level_idc = 0 - not a valid level - while landing
    // on profile_idc = 1 by coincidence, which is why it looked almost right.
    // Safari accepted the config and decoded nothing: 5 Mbps arriving at 1%
    // loss, zero frames out, keyframe requests every four seconds
    // (2026-09-08 phone session).
    let rbsp = rbsp_unescape(sps.get(2..)?);
    let ptl = rbsp.get(1..13)?;

    let mut out = Vec::with_capacity(23 + 3 * 5 + vps.len() + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.extend_from_slice(ptl);
    out.extend_from_slice(&[0xF0, 0x00]); // min_spatial_segmentation_idc = 0
    out.push(0xFC); // parallelismType = 0 (mono)
    out.push(0xFC | 1); // chromaFormat = 1 (4:2:0)
    out.push(0xFC); // bitDepthLumaMinus8 = 0
    out.push(0xFC); // bitDepthChromaMinus8 = 0
    out.extend_from_slice(&[0, 0]); // avgFrameRate = 0 (unknown)
    out.push(0x1F); // cfr=0, numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3
    out.push(3); // numOfArrays
    for (nal_type, nal) in [(32u8, vps), (33u8, sps), (34u8, pps)] {
        out.push(0x80); // array_completeness = 1
        out.push(nal_type);
        out.extend_from_slice(&[0, 1]); // numNalus = 1
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Some(out)
}

/// Remove HEVC/H.264 emulation prevention bytes: `00 00 03 xx` -> `00 00 xx`.
///
/// Anything parsed *out* of a NAL payload has to go through this first; NALs
/// stored whole (the parameter-set arrays in an HVCC record) keep their escapes.
fn rbsp_unescape(nal_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut i = 0;
    while i < nal_payload.len() {
        if i + 2 < nal_payload.len()
            && nal_payload[i] == 0
            && nal_payload[i + 1] == 0
            && nal_payload[i + 2] == 3
        {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(nal_payload[i]);
            i += 1;
        }
    }
    out
}

/// Split an Annex-B byte stream into NAL units (without start codes, trailing
/// zero bytes trimmed). Handles both 3- and 4-byte start codes.
fn annexb_nals(au: &[u8]) -> Vec<&[u8]> {
    let mut triplets: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            triplets.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(triplets.len());
    for (k, &t) in triplets.iter().enumerate() {
        let start = t + 3;
        let end = triplets.get(k + 1).copied().unwrap_or(au.len());
        let mut slice = &au[start..end];
        while let Some((&0, rest)) = slice.split_last() {
            slice = rest;
        }
        nals.push(slice);
    }
    nals
}

/// Rewrite an Annex-B access unit into the length-prefixed sample format an
/// `hvc1`/`avc1` WebCodecs config with a `description` requires: each start
/// code becomes a 4-byte big-endian NAL length. The tap applies this when it
/// synthesizes a description for a byte-stream encoder (ADR-0011) — without
/// it the decoder accepts the config and then never produces a frame.
pub fn annexb_to_length_prefixed(au: &[u8]) -> Vec<u8> {
    let mut triplets: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            triplets.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(au.len() + 4 * triplets.len().max(1));
    for (k, &t) in triplets.iter().enumerate() {
        let start = t + 3;
        let end = triplets.get(k + 1).copied().unwrap_or(au.len());
        let mut nal_end = end;
        while nal_end > start && au[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        out.extend_from_slice(&((nal_end - start) as u32).to_be_bytes());
        out.extend_from_slice(&au[start..nal_end]);
    }
    out
}

#[cfg(test)]
mod length_prefix_tests {
    use super::annexb_to_length_prefixed;

    #[test]
    fn rewrites_start_codes_as_lengths() {
        let au = [
            0, 0, 0, 1, 0x40, 0x01, 0xAA, 0, 0, 1, 0x42, 0x01, 0xBB, 0xCC,
        ];
        let out = annexb_to_length_prefixed(&au);
        // First NAL: 4-byte BE length 3, then its bytes.
        assert_eq!(&out[0..7], &[0, 0, 0, 3, 0x40, 0x01, 0xAA]);
        // Second NAL runs to the end: length 4.
        assert_eq!(&out[7..], &[0, 0, 0, 4, 0x42, 0x01, 0xBB, 0xCC]);
    }

    #[test]
    fn handles_4byte_start_codes_and_trailing_zero_pad() {
        let au = [
            0, 0, 0, 1, 0x40, 0x01, 0x0C, 0, 0, 0, 0, 1, 0x42, 0x01, 5, 6, 7, 8,
        ];
        let out = annexb_to_length_prefixed(&au);
        // NAL 1 is trimmed of the pad zeros before the second start code.
        assert_eq!(&out[0..7], &[0, 0, 0, 3, 0x40, 0x01, 0x0C]);
        assert_eq!(&out[7..], &[0, 0, 0, 6, 0x42, 0x01, 5, 6, 7, 8]);
    }

    #[test]
    fn passthrough_without_start_codes() {
        assert!(annexb_to_length_prefixed(&[1, 2, 3]).is_empty());
    }
}

#[cfg(test)]
mod hvcc_tests {
    use super::{hvcc_description, rbsp_unescape};

    // Real parameter sets from libx265, 1920x1080 Main profile - the shape the
    // host's NVENC encoder also emits. Note the `00 00 03` escapes *inside* the
    // SPS profile block: the constraint flags are mostly zeros, so emulation
    // prevention bytes there are the normal case, and any code reading the
    // profile straight out of the raw NAL gets the wrong bytes.
    const VPS: &[u8] = &[
        0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x7B, 0x95, 0x98, 0x09,
    ];
    const SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x7B, 0xA0, 0x03, 0xC0, 0x80, 0x10, 0xE5, 0x96, 0x56, 0x69, 0x24, 0xCA, 0xF0,
        0x16, 0x80, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1E, 0x04,
    ];
    const PPS: &[u8] = &[0x44, 0x01, 0xC1, 0x72, 0xB4, 0x62, 0x40];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut au = Vec::new();
        for n in nals {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(n);
        }
        au
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(rbsp_unescape(&[0, 0, 3, 1, 2]), vec![0, 0, 1, 2]);
        assert_eq!(rbsp_unescape(&[0, 0, 3, 0, 0, 3, 5]), vec![0, 0, 0, 0, 5]);
        assert_eq!(rbsp_unescape(&[1, 2, 3]), vec![1, 2, 3], "a bare 3 is data");
    }

    /// The record must describe the stream a decoder is about to be handed.
    ///
    /// The previous implementation copied the raw NAL at `sps[2..14]`: one byte
    /// early, and without removing emulation prevention. On this exact SPS that
    /// yields general_level_idc = 0, which is not a valid HEVC level, while
    /// landing on profile_idc = 1 by coincidence. Safari accepted the config and
    /// produced no frames at all.
    #[test]
    fn profile_and_level_match_the_sps() {
        let hvcc = hvcc_description(&annexb(&[VPS, SPS, PPS])).expect("parameter sets present");
        assert_eq!(hvcc[0], 1, "configurationVersion");

        let ptl = &hvcc[1..13];
        assert_eq!((ptl[0] >> 6) & 0x3, 0, "general_profile_space");
        assert_eq!((ptl[0] >> 5) & 0x1, 0, "general_tier_flag");
        assert_eq!(ptl[0] & 0x1F, 1, "general_profile_idc = Main");
        assert_eq!(ptl[11], 123, "general_level_idc = 4.1 (1080p60)");
        assert_ne!(ptl[11], 0, "level 0 is not a level a decoder will accept");

        assert_eq!(hvcc[22], 3, "numOfArrays");
        // Parameter sets are stored as whole NALs, escapes intact.
        assert_eq!(&hvcc[23..27], &[0x80, 32, 0, 1]);
        assert_eq!(&hvcc[29..29 + VPS.len()], VPS);
    }

    #[test]
    fn returns_none_without_all_parameter_sets() {
        assert_eq!(hvcc_description(&annexb(&[SPS])), None, "no VPS/PPS");
        assert_eq!(hvcc_description(&[1, 2, 3, 4]), None, "no start codes");
    }

    #[test]
    fn returns_none_when_the_sps_is_too_short_to_describe() {
        let stub: &[u8] = &[0x42, 0x01, 0x01, 0x60];
        assert_eq!(hvcc_description(&annexb(&[VPS, stub, PPS])), None);
    }
}

/// ---- v4: deadline-aware datagram fragments (docs/research/performance-latency-2026-09-09.md P0) ----

pub const WT_VIDEO_PROTOCOL_V4: u8 = 4;

/// One fragment of one frame, carried in a single QUIC datagram: header +
/// payload, where payload must fit the path's datagram budget. No
/// retransmission, no ordering: a fragment that misses its playout window is
/// worthless, and its successor (or the next recovery frame) proceeds
/// regardless — the property a reliable stream cannot offer.
///
/// Wire layout (little-endian throughout):
/// `[0]` version = 4, `[1]` flags (bit0 = keyframe, bit1 = parity row 1,
/// bit2 = parity row 2), `[2..6)` frame_no, `[6..8)` frag_idx,
/// `[8..10)` frag_cnt, `[10..18)` capture_us, `[18..]` payload.
///
/// Parity fragments (FEC, research doc §target transport): data fragments
/// are grouped 8 at a time; row 1 carries the XOR of each group's
/// payloads, so ONE lost fragment in a group is rebuilt locally — no NACK,
/// no RTT. Keyframes add row 2 (XOR of each group's odd-offset members),
/// recovering a second loss when the two fall in different row halves.
/// Re-sends (client NACKs) remain the bounded fallback for the rest.
pub const WT_FRAGMENT_HEADER_LEN: usize = 18;

/// First payload byte of a server→client audio datagram (transport.rs): a
/// fragment can never collide with it, so the client's datagram reader can
/// demultiplex by the first byte alone.
pub const WT_AUDIO_DATAGRAM_TAG: u8 = 0x41;

/// Flags byte: keyframe (bit 0), parity row 1 (bit 1), parity row 2 (bit 2).
/// `WT_FRAME_KEY` (bit 0) is shared with the v3 stream header.
pub const WT_FRAGMENT_PARITY1: u8 = 0x02;
pub const WT_FRAGMENT_PARITY2: u8 = 0x04;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WtFragment {
    pub frame_no: u32,
    pub frag_idx: u16,
    pub frag_cnt: u16,
    pub capture_us: u64,
    pub key: bool,
    /// 0 = data fragment, 1 = parity row 1 (XOR of the group), 2 = parity
    /// row 2 (XOR of the group's odd-offset members, keyframes only).
    pub parity: u8,
    pub payload: Vec<u8>,
}

impl WtFragment {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(WT_FRAGMENT_HEADER_LEN + self.payload.len());
        out.push(WT_VIDEO_PROTOCOL_V4);
        let mut flags = 0u8;
        if self.key {
            flags |= WT_FRAME_KEY;
        }
        flags |= match self.parity {
            1 => WT_FRAGMENT_PARITY1,
            2 => WT_FRAGMENT_PARITY2,
            _ => 0,
        };
        out.push(flags);
        out.extend_from_slice(&self.frame_no.to_le_bytes());
        out.extend_from_slice(&self.frag_idx.to_le_bytes());
        out.extend_from_slice(&self.frag_cnt.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, WtVideoError> {
        if buf.len() < WT_FRAGMENT_HEADER_LEN {
            return Err(WtVideoError::Truncated);
        }
        if buf[0] != WT_VIDEO_PROTOCOL_V4 {
            return Err(WtVideoError::BadVersion(buf[0]));
        }
        let key = buf[1] & WT_FRAME_KEY != 0;
        let parity = if buf[1] & WT_FRAGMENT_PARITY2 != 0 {
            2
        } else if buf[1] & WT_FRAGMENT_PARITY1 != 0 {
            1
        } else {
            0
        };
        let frame_no = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
        let frag_idx = u16::from_le_bytes([buf[6], buf[7]]);
        let frag_cnt = u16::from_le_bytes([buf[8], buf[9]]);
        let capture_us = u64::from_le_bytes([
            buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16], buf[17],
        ]);
        Ok(Self {
            frame_no,
            frag_idx,
            frag_cnt,
            capture_us,
            key,
            parity,
            payload: buf[WT_FRAGMENT_HEADER_LEN..].to_vec(),
        })
    }
}

#[test]
fn fragment_roundtrips_and_rejects_truncation() {
    let f = WtFragment {
        frame_no: 0x0102_0304,
        frag_idx: 2,
        frag_cnt: 7,
        capture_us: 1_234_567_890,
        key: true,
        parity: 0,
        payload: vec![7; 600],
    };
    let wire = f.encode();
    assert_eq!(wire.len(), WT_FRAGMENT_HEADER_LEN + 600);
    assert_eq!(wire[0], 4);
    assert_eq!(WtFragment::decode(&wire).unwrap(), f);
    // A datagram is atomic - decode can never see a partial one - but a
    // short buffer yields a short payload rather than panicking, and a wrong
    // version is refused outright.
    assert_eq!(
        WtFragment::decode(&wire[..WT_FRAGMENT_HEADER_LEN + 599])
            .unwrap()
            .payload
            .len(),
        599
    );
    let mut bad = wire.clone();
    bad[0] = 3;
    assert_eq!(WtFragment::decode(&bad), Err(WtVideoError::BadVersion(3)));
}

#[test]
fn fragment_tag_never_collides_with_audio() {
    let f = WtFragment {
        frame_no: 1,
        frag_idx: 0,
        frag_cnt: 1,
        capture_us: 0,
        key: false,
        parity: 0,
        payload: vec![],
    };
    assert_ne!(f.encode()[0], WT_AUDIO_DATAGRAM_TAG);
}

/// Parity reconstruction, at the protocol layer so host and client agree on
/// the exact scheme: row 1 = XOR of each group of 8 payloads (zero-padded to
/// the group's longest member); row 2 (keyframes) = XOR of the group's
/// odd-offset members. A single lost fragment in a group is rebuilt from row
/// 1; a second loss is rebuilt from rows 1+2 when the losses split across
/// even/odd offsets.
pub const FRAGMENT_GROUP: usize = 8;

/// Compute the parity payloads for one frame's fragments. `frags` are the
/// data-fragment payloads in order. Returns one `(parity, payload)` per
/// group: every frame gets row 1; keyframes additionally get row 2.
pub fn fragment_parity(frags: &[Vec<u8>], key: bool) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    for g in 0..frags.len().div_ceil(FRAGMENT_GROUP) {
        let lo = g * FRAGMENT_GROUP;
        let hi = ((g + 1) * FRAGMENT_GROUP).min(frags.len());
        let width = frags[lo..hi].iter().map(|p| p.len()).max().unwrap_or(0);
        let mut row1 = vec![0u8; width];
        let mut row2 = vec![0u8; width];
        for (off, p) in frags[lo..hi].iter().enumerate() {
            for (j, b) in p.iter().enumerate() {
                row1[j] ^= b;
                if off % 2 == 1 {
                    row2[j] ^= b;
                }
            }
        }
        out.push((1, row1));
        if key {
            out.push((2, row2));
        }
    }
    out
}

#[test]
fn parity_rebuilds_a_single_lost_fragment() {
    // On the wire every non-final fragment is exactly the datagram budget,
    // so equal lengths are the realistic shape.
    let frags: Vec<Vec<u8>> = (0..FRAGMENT_GROUP).map(|i| vec![i as u8 + 1; 24]).collect();
    let parity = fragment_parity(&frags, false);
    assert_eq!(parity.len(), 1, "one group of 8 -> one row-1 payload");
    assert_eq!(parity[0].0, 1);
    // Rebuild fragment 3 from the parity and the other seven.
    let mut acc = parity[0].1.clone();
    for (i, p) in frags.iter().enumerate() {
        if i != 3 {
            for (j, b) in p.iter().enumerate() {
                acc[j] ^= b;
            }
        }
    }
    assert_eq!(acc, frags[3]);
}

#[test]
fn keyframe_parity_has_two_rows() {
    let frags: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8 + 3; 20]).collect();
    let parity = fragment_parity(&frags, true);
    assert_eq!(parity.len(), 2, "row 1 + row 2 for a keyframe");
    assert_eq!(parity[1].0, 2);
}
