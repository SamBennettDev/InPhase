//! Per-frame host-side timestamps (architecture report §20).
//!
//! Three pad probes stamp each frame as it moves through the tail of the graph:
//!
//! * encoder **sink** — `capture_us`: frame handed to the encoder (post-capture)
//! * payloader **sink** — `encode_us`: encoded frame handed to the RTP payloader
//! * payloader **src** (marker packet) — `send_us` + the wire **RTP timestamp**
//!
//! `nvd3d11h264enc` re-bases the buffer PTS between its sink and src (raw side
//! is pipeline running-time, encoded side is a clock-time value), so:
//!
//! * capture → encode is joined **FIFO** — the encoder neither reorders nor
//!   drops frames in low-latency mode, so the Nth frame in is the Nth frame out.
//! * encode → send is joined on the (encoded-domain) buffer PTS, which the
//!   payloader passes straight through.
//!
//! The completed record is keyed by RTP timestamp — the same 90 kHz value the
//! browser sees in `VideoFrameCallbackMetadata.rtpTimestamp`. A background
//! thread drains the completed records onto the `control` data channel as
//! `frame_stamps` messages so the client can reconstruct the full capture→glass
//! budget per frame.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Default, Clone, Copy)]
struct Staging {
    capture_us: u64,
    encode_us: u64,
}

#[derive(Default)]
struct Inner {
    /// Capture wall-clock µs, oldest first — popped FIFO when a frame is encoded.
    capture_times: VecDeque<u64>,
    /// Encoded-domain PTS (ns) -> partial stamps, until the marker packet
    /// completes the frame.
    staging: HashMap<u64, Staging>,
    /// Completed `[rtp_ts, capture_us, encode_us, send_us]`, awaiting send.
    pending: VecDeque<[u64; 4]>,
    last_pts_ns: u64,
}

pub struct FrameTrace {
    inner: Mutex<Inner>,
}

impl FrameTrace {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Raw frame handed to the encoder (probe on the encoder **sink** pad).
    pub fn on_capture(&self, _pts_ns: u64) {
        let mut g = self.inner.lock().unwrap();
        g.capture_times.push_back(now_us());
        // Bound: if the encoder ever stalls/drops, don't grow without limit.
        while g.capture_times.len() > 240 {
            g.capture_times.pop_front();
        }
    }

    /// Encoded frame handed to the RTP payloader (probe on the payloader sink).
    pub fn on_encoded(&self, pts_ns: u64) {
        let mut g = self.inner.lock().unwrap();
        let capture_us = g.capture_times.pop_front().unwrap_or(0);
        g.staging.insert(
            pts_ns,
            Staging {
                capture_us,
                encode_us: now_us(),
            },
        );
    }

    /// Called once per frame, on the RTP packet carrying the marker bit.
    pub fn on_sent(&self, rtp_ts: u32, pts_ns: u64) {
        let send_us = now_us();
        let mut g = self.inner.lock().unwrap();
        let st = g.staging.remove(&pts_ns).unwrap_or_default();
        g.pending
            .push_back([rtp_ts as u64, st.capture_us, st.encode_us, send_us]);
        while g.pending.len() > 600 {
            g.pending.pop_front();
        }
        // Drop staging entries older than ~2s — a frame that never got a marker.
        if pts_ns > g.last_pts_ns {
            g.last_pts_ns = pts_ns;
            let cutoff = pts_ns.saturating_sub(2_000_000_000);
            g.staging.retain(|&k, _| k >= cutoff);
        }
    }

    /// (host wall clock µs now, completed records since the last drain).
    pub fn drain(&self) -> (u64, Vec<[u64; 4]>) {
        let mut g = self.inner.lock().unwrap();
        let out = g.pending.drain(..).collect();
        (now_us(), out)
    }

    /// Compact host-side summary of a drained batch, for the log (temporary,
    /// stutter diagnosis). Records are `[rtp_ts, capture_us, encode_us, send_us]`.
    pub fn summary(frames: &[[u64; 4]]) -> String {
        if frames.is_empty() {
            return "n=0".into();
        }
        let mut enc = Vec::with_capacity(frames.len());
        let mut pay = Vec::with_capacity(frames.len());
        let mut cap2send = Vec::with_capacity(frames.len());
        let mut missing = 0u32;
        for f in frames {
            let (_rtp, cap, encd, send) = (f[0], f[1], f[2], f[3]);
            if cap == 0 || encd == 0 {
                missing += 1;
                continue;
            }
            enc.push(encd.saturating_sub(cap) as f64 / 1000.0);
            pay.push(send.saturating_sub(encd) as f64 / 1000.0);
            cap2send.push(send.saturating_sub(cap) as f64 / 1000.0);
        }
        // Host-side send cadence: gaps between consecutive marker packets.
        let mut sends: Vec<u64> = frames.iter().map(|f| f[3]).filter(|&s| s > 0).collect();
        sends.sort_unstable();
        let mut gaps: Vec<f64> = sends
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / 1000.0)
            .collect();
        let p = |v: &mut Vec<f64>, q: f64| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let i = ((v.len() as f64 - 1.0) * q).round() as usize;
            (v[i] * 10.0).round() / 10.0
        };
        let mut enc_s = enc.clone();
        let mut pay_s = pay.clone();
        let mut c2s_s = cap2send.clone();
        format!(
            "n={} missing={} enc_ms(p50/p95/max)={}/{}/{} pay_ms={}/{}/{} cap2send_ms={}/{}/{} send_gap_ms(p50/p95/max)={}/{}/{}",
            frames.len(),
            missing,
            p(&mut enc_s, 0.5), p(&mut enc_s, 0.95), p(&mut enc_s, 1.0),
            p(&mut pay_s, 0.5), p(&mut pay_s, 0.95), p(&mut pay_s, 1.0),
            p(&mut c2s_s, 0.5), p(&mut c2s_s, 0.95), p(&mut c2s_s, 1.0),
            p(&mut gaps, 0.5), p(&mut gaps, 0.95), p(&mut gaps, 1.0),
        )
    }

    /// (encode_ms_p50, encode_ms_p95, capture→send_ms_p95) across a drained
    /// batch, for the dashboard's host stats. Frames with missing stamps are
    /// skipped; an empty / all-missing batch returns zeros.
    pub fn encode_percentiles(frames: &[[u64; 4]]) -> (f32, f32, f32) {
        let mut enc = Vec::with_capacity(frames.len());
        let mut c2s = Vec::with_capacity(frames.len());
        for f in frames {
            let (_rtp, cap, encd, send) = (f[0], f[1], f[2], f[3]);
            if cap == 0 || encd == 0 {
                continue;
            }
            enc.push(encd.saturating_sub(cap) as f32 / 1000.0);
            c2s.push(send.saturating_sub(cap) as f32 / 1000.0);
        }
        let pct = |v: &mut Vec<f32>, q: f32| -> f32 {
            if v.is_empty() {
                return 0.0;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let i = ((v.len() as f32 - 1.0) * q).round() as usize;
            (v[i] * 10.0).round() / 10.0
        };
        (pct(&mut enc, 0.5), pct(&mut enc, 0.95), pct(&mut c2s, 0.95))
    }

    /// Serialise a drained batch as a `frame_stamps` control message.
    pub fn to_json(host_now_us: u64, frames: &[[u64; 4]]) -> String {
        let mut s = String::with_capacity(32 + frames.len() * 40);
        s.push_str(r#"{"type":"frame_stamps","host_now_us":"#);
        s.push_str(&host_now_us.to_string());
        s.push_str(r#","frames":["#);
        for (i, f) in frames.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("[{},{},{},{}]", f[0], f[1], f[2], f[3]));
        }
        s.push_str("]}");
        s
    }
}

impl Default for FrameTrace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_skip_missing_stamps() {
        // [rtp_ts, capture_us, encode_us, send_us]; three complete frames (5,
        // 10, 15 ms encode) plus one with missing stamps that must be skipped.
        let frames = [
            [1, 1_000, 6_000, 6_100],
            [2, 2_000, 12_000, 12_200],
            [3, 3_000, 18_000, 18_100],
            [4, 0, 9_000, 9_100],
        ];
        let (p50, p95, c2s_p95) = FrameTrace::encode_percentiles(&frames);
        assert_eq!(p50, 10.0);
        assert_eq!(p95, 15.0);
        assert_eq!(c2s_p95, 15.1);
    }

    #[test]
    fn percentiles_empty_and_missing_are_zero() {
        assert_eq!(FrameTrace::encode_percentiles(&[]), (0.0, 0.0, 0.0));
        assert_eq!(
            FrameTrace::encode_percentiles(&[[1, 0, 5_000, 5_100]]),
            (0.0, 0.0, 0.0)
        );
    }
}
