// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sam Bennett

// Browser mirror of the WT video wire format — **v3, self-delimiting frames**.
//
// Byte-for-byte parity with `crates/protocol/src/wtvideo.rs` is asserted by
// `wtvideo.test.ts` against generated golden vectors.
//
// v1 mirrored a fragmenter, a reassembler with interleaved XOR parity groups,
// and a NACK planner — about 700 lines across this file and its neighbours.
// None of it survives: a frame is a QUIC unidirectional stream, so the receive
// path is "read the stream to EOF, parse 14 bytes of header". Fragmentation,
// retransmission and ordering are the transport's job and it was always doing
// them better than we were.

/** Wire version. v1 was the datagram fragmenter; v2 was one frame per stream
 *  read to EOF; v3 makes each frame self-delimiting (payload length in the
 *  header, checked against the body). */
export const WT_VIDEO_PROTOCOL_VERSION = 3;
/** Fixed header length in bytes. */
export const WT_VIDEO_HEADER_LEN = 18;
/** The frame is a keyframe (IDR/CRA) — a clean rejoin point. */
export const WT_FRAME_KEY = 0x01;

export interface WtFrame {
  frame_no: number;
  capture_us: number;
  key: boolean;
  payload: Uint8Array;
}

/**
 * Parse a complete stream body. The caller reads the stream to EOF first — a
 * short body is a truncated frame, not a fragment to be held and reassembled.
 *
 * Returns `null` for anything unparseable (truncated, or a version this build
 * does not speak), which the caller counts so a page talking to a host it was
 * not built with can notice and reload.
 */
export function parseFrame(buf: Uint8Array): WtFrame | null {
  if (buf.byteLength < WT_VIDEO_HEADER_LEN) return null;
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  if (dv.getUint8(0) !== WT_VIDEO_PROTOCOL_VERSION) return null;
  const flags = dv.getUint8(1);
  const payloadLen = dv.getUint32(14, true);
  // v3 frames are self-delimiting; a body that disagrees with its own header
  // is a parse failure, not a fragment to reassemble.
  if (buf.byteLength !== WT_VIDEO_HEADER_LEN + payloadLen) return null;
  return {
    key: (flags & WT_FRAME_KEY) !== 0,
    frame_no: dv.getUint32(2, true),
    // capture_us is a u64; Number is exact to 2^53 µs (~285 years of uptime).
    capture_us: Number(dv.getBigUint64(6, true)),
    payload: buf.subarray(WT_VIDEO_HEADER_LEN),
  };
}
