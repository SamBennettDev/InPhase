//! Application-level congestion control for the WebTransport video path.
//!
//! Nothing underneath estimates bandwidth, so without a feedback loop the
//! encoder runs at a fixed bitrate and floods any constrained path — cellular,
//! or a relay — until no frame fully assembles.
//!
//! This is a deliberately crude AIMD controller driven by the once-a-second
//! [`ClientTelemetry`](inphase_protocol::ClientTelemetry) the browser already
//! sends (fragments lost, inbound bitrate, decoded fps, RTT). It cuts only on
//! evidence that frames are failing to reach the decoder, and crawls back up
//! while things are clean — aggressively on a low-RTT LAN, gently on a relay.
//!
//! Kept as a pure struct so the control law can be unit-tested against recorded
//! telemetry traces without a live pipeline.

/// One tick of feedback (all "since the previous tick", ~1 s apart).
#[derive(Debug, Clone, Copy, Default)]
pub struct Feedback {
    /// RTP packets the client reported lost since the last tick.
    pub lost_delta: u64,
    /// Client's inbound video bitrate estimate (kbps).
    pub recv_kbps: f32,
    /// Rate the client's decoder is producing frames.
    pub decoded_fps: f32,
    /// The session's target fps.
    pub target_fps: u32,
    /// Client round-trip time (ms). 0 = unknown.
    pub rtt_ms: f32,
    /// Host RTP send-queue depth (ms). Fills when `webrtcbin` can't push.
    pub rtp_backlog_ms: f32,
    /// Whether fresh client telemetry backed this tick.
    pub have_client: bool,
    /// Client-measured p95 fragment age (capture→assemble, ms). The one
    /// signal that sees radio-buffer saturation before the loss it causes:
    /// 00:19 the client received a 12 Mbps flood while assembling ~2
    /// frames/s, lat_p95 33 -> 129 -> 293 ms, and the host's own queue
    /// never stalled - the AIMD climbed on. 0 = unknown.
    pub client_lat_p95_ms: f32,
    /// The v4 datagram carrier is live, so fragment injection is paced at
    /// `WT_PACE_PPS` and the target must stay under what that pace can carry
    /// — above it, frames expire unsent and reopen sequence holes.
    pub v4_datagram_carrier: bool,
    /// The connection's v4 injection pace (datagrams/s), from client
    /// telemetry: worker-drain clients pace higher, so their ceiling is
    /// higher. 0 = unknown (legacy client) → `WT_PACED_CEILING_KBPS`.
    pub v4_pace_pps: u32,
    /// Inferred datagram loss: the fraction of datagrams the host injected
    /// that the client never saw. `None` = no window measured yet.
    ///
    /// The transport's other loss input is structurally zero here — QUIC
    /// datagrams are unacknowledged, so a datagram the browser's queue
    /// destroyed leaves no sender-side trace at all — and the host's fallback
    /// substitutes frames it reset, scaled by a fixed bits-per-packet estimate
    /// that understates a lost delta and overstates a lost IDR by an order of
    /// magnitude. This is measured instead of modelled.
    pub inferred_loss_frac: Option<f32>,
}

/// Crude AIMD bitrate controller. `kbps` is the current encoder target.
#[derive(Debug, Clone)]
pub struct BitrateController {
    pub kbps: u32,
    floor: u32,
    ceiling: u32,
    /// Ticks to wait before allowing an increase (set after every decrease).
    cooldown: u8,
    /// Ticks left before any *decrease* is allowed. The client's inbound-rate
    /// estimate and its first keyframe both take a second or two to arrive, so
    /// early telemetry is not trustworthy enough to cut on.
    warmup: u8,
    /// Consecutive clean ticks.
    clean_streak: u8,
    /// Consecutive ticks at >10 % measured loss. A single lossy second is a
    /// burst, not a verdict: the 80 Mbps session produced 63 cuts in eleven
    /// minutes and almost every one was a 50 % halving (`31446->15723`,
    /// `35117->17558`) driven by one second of loss, each costing ~10 s to
    /// climb back. The halving now requires the loss to persist.
    lossy_streak: u8,
    /// The rate that most recently drew loss. Probing won't push past ~90 % of
    /// it; it relaxes slowly while the path stays clean so a genuinely improved
    /// path can still recover the full ceiling.
    soft_ceiling: Option<f32>,
    /// Whether that ceiling came from tail delay rather than loss: a queued
    /// route's bandwidth, which must not be re-probed at the fast loss rate.
    ceiling_from_delay: bool,
}

impl BitrateController {
    pub fn new(start_kbps: u32, floor_kbps: u32, ceiling_kbps: u32) -> Self {
        let ceiling = ceiling_kbps.max(floor_kbps);
        Self {
            kbps: start_kbps.clamp(floor_kbps, ceiling),
            floor: floor_kbps,
            ceiling,
            cooldown: 0,
            warmup: 4,
            clean_streak: 0,
            lossy_streak: 0,
            soft_ceiling: None,
            ceiling_from_delay: false,
        }
    }

    /// Advance one tick; returns the new encoder target (kbps).
    pub fn step(&mut self, fb: &Feedback) -> u32 {
        // Ticks down on every report, not only on clean ones: the cooldown
        // gates *cuts* as well as climbs now (see the tail-delay branch), and a
        // branch that returns early used to freeze it, which would have made a
        // persistently congested route un-cuttable after the first cut.
        if self.cooldown > 0 {
            self.cooldown -= 1;
        }
        // --- fast local signal: the host's own send queue is backing up, so
        //     webrtcbin is pacing slower than the encoder. Cut immediately;
        //     don't wait for the client report a second later.
        if fb.rtp_backlog_ms > 250.0 {
            self.decrease_to((self.kbps as f32 * 0.6) as u32);
            self.cooldown = 4;
            self.clean_streak = 0;
            return self.kbps;
        }

        self.warmup = self.warmup.saturating_sub(1);

        if !fb.have_client {
            // No telemetry yet (startup) — hold, don't guess.
            return self.kbps;
        }

        // --- delay-based congestion: the client's p95 fragment age is the
        //     earliest signal of radio-buffer saturation. The 00:19 death
        //     never filled the host queue (QUIC buffered instead) and the
        //     client received every datagram - as a flood too late to
        //     assemble (p95 33 -> 129 -> 293 ms, ~2 frames/s assembled at
        //     12 Mbps inbound). Rising tail delay means the encoder is
        //     outrunning the path: cut before the loss it causes.
        // One elevation, one cut. `client_lat_p95_ms` is a percentile over the
        // client's whole sample buffer (~8.5 s at 60 fps), so a single spike
        // keeps it above the threshold for seconds after the tail has cleared —
        // and this branch used to cut on every tick, seven times in seven
        // seconds, straight to the floor. Live (2026-09-20 04:2x): each cut
        // line read `decoded_fps=60 rtt_ms=7 client_p95_ms=135 held=0` — the
        // client was decoding 60 fps on a healthy route while the encoder was
        // being pushed to 600 kbps on stale evidence. The cooldown lets the
        // window catch up before the next cut - and it applies to severe tails
        // too. It used to be bypassed above 250 ms on the theory that a big
        // spike is proof rather than memory, but Wi-Fi airtime contention throws
        // 267-481 ms excursions that say nothing about the host's send rate, and
        // each one cut 0.6x per tick, four ticks from 4000 kbps to the 600 kbps
        // floor: `DOWN 4000->2400 lost=0 ... p95=267`, then 1440, 864, 600 in the
        // same episode while the client decoded 59-60 fps the whole way
        // (2026-09-20 04:38). A severe spike still cuts harder (0.6x) and holds
        // longer (4 ticks); it just cannot empty the stream on its own.
        //
        // A negative percentile is the client's clock sync saying "not yet", and
        // `pipeline.rs` used to clamp it to 0.0 - the perfect signal, which
        // passed every guard below including the climb gate.
        let lat_known = fb.client_lat_p95_ms >= 0.0;
        if lat_known && fb.client_lat_p95_ms > 120.0 {
            if self.cooldown > 0 {
                // Over the threshold but still inside the cooldown that the
                // last cut set: hold the rate and let the percentile window
                // turn over. This must return here rather than fall through to
                // the climb branch — a gated tick is not a *clean* tick. Falling
                // through relaxed the remembered ceiling and counted towards
                // `clean_streak`, so every second cut looked like a fresh
                // episode and re-marked the bar at the newly lower rate:
                // 50000 -> 5882 in fourteen cuts, with the client decoding
                // 60 fps on a 7 ms RTT the whole way down (2026-09-20 04:2x).
                return self.kbps;
            }
            let factor = if fb.client_lat_p95_ms > 250.0 {
                0.6
            } else {
                0.7
            };
            // Remember the rate that queued the route, the way loss does.
            // Without this the delay branch is the one cut that forgets, and a
            // controller that forgets ramps straight back into the same wall:
            // 2026-09-20 03:57, a 1440p60 session on a ~6 Mbps uplink, ran
            // 600 -> 6354 kbps and crashed back to 600 every ~17 s for the
            // whole session - ten ramp steps up, seven cuts down, `lost=0`
            // throughout, the tail delay (25 -> 155 ms) the only signal.
            self.mark_delay_ceiling();
            self.decrease_to((self.kbps as f32 * factor) as u32);
            self.cooldown = if fb.client_lat_p95_ms > 250.0 { 4 } else { 2 };
            self.clean_streak = 0;
            return self.kbps;
        }

        // The only trustworthy congestion signal we have is **packet loss**
        // (plus the send-queue backlog above). We deliberately do NOT treat
        // "inbound bitrate < encoder target" as starvation: with VBR encoding
        // and a low-motion screen the encoder legitimately emits far less than
        // its cap, and reading that as congestion collapses the bitrate to the
        // floor and never lets it back up.
        let recv_pps = (fb.recv_kbps * 1000.0 / 8.0 / 1100.0).max(0.0);
        let denom = fb.lost_delta as f32 + recv_pps;
        // The measured figure wins when there is one: `lost_delta` is a
        // model (frames the host reset, scaled by a fixed bits-per-packet
        // estimate) and on the datagram carrier the client's own loss counter
        // is hardcoded to zero, so without this the law is blind to the one
        // loss it can actually measure.
        let loss_frac = match fb.inferred_loss_frac {
            Some(measured) => measured,
            None if denom > 1.0 => fb.lost_delta as f32 / denom,
            None => 0.0,
        };

        // Distinguish real congestion from wireless erasure. Cellular paths
        // erase 10-25 % of datagrams at a base rate while parity and NACK still
        // deliver every frame, so loss alone is not a reason to cut.
        //
        // A falling *receive rate* is not a reason either, and treating it as
        // one is what collapsed the 2026-09-08 phone session:
        //
        //   * it is what a cut *causes*. Lower the encoder and recv falls by
        //     construction, so every cut manufactured the evidence for the
        //     next one - 4000 -> 2000 -> 1000 -> 600 in five seconds, each step
        //     "justified" by the previous step's own effect.
        //   * with VBR it is also what a static desktop looks like. That
        //     session went 900 -> 490 -> 244 datagrams/s at a steady 60 fps
        //     *before* any cut: the screen stopped moving and the P-frames got
        //     small. Nothing was wrong with the route at all.
        //
        // This is the same mistake the comment above warns about ("inbound
        // bitrate < encoder target is not starvation"), wearing a different
        // hat: a *derivative* of the same untrustworthy number.
        //
        // What survives is the signal that answers the actual question - are
        // frames reaching the decoder? Erasure that parity and NACK repair is
        // not congestion, however much of it there is.
        let decode_collapsed = fb.target_fps > 0 && fb.decoded_fps < fb.target_fps as f32 * 0.7;
        let route_failing = decode_collapsed;

        // Both cut branches require `route_failing`. A bare `loss_frac > 0.30`
        // used to cut regardless, and that is what pinned the phone session at
        // the floor once it got there: loss_frac is loss over *received*
        // throughput, so as recv falls the same absolute erasure becomes a
        // larger fraction. At 600 kbps a cellular route's ordinary erasure
        // reads as >30 % forever, so the controller cut, could not recover past
        // its own soft ceiling, and stayed at the floor with the picture in
        // pieces. A fraction measured against a shrinking denominator is not
        // evidence of anything.
        //
        // If the decoder is keeping up, the route is delivering - whatever the
        // ratio says. That is this module's own stated principle; it just was
        // not applied to the severe branch.
        if loss_frac > 0.10 {
            self.lossy_streak = self.lossy_streak.saturating_add(1);
        } else {
            self.lossy_streak = 0;
        }
        if self.warmup == 0 && route_failing && loss_frac > 0.10 && self.lossy_streak >= 2 {
            // Severe loss or genuine congestion — collapse toward what's
            // actually landing, at least halve.
            let delivered = (fb.recv_kbps * 0.85) as u32;
            let target = (self.kbps / 2).min(delivered.max(self.floor));
            self.mark_loss_ceiling();
            self.decrease_to(target);
            self.cooldown = 5;
            self.clean_streak = 0;
        } else if self.warmup == 0 && loss_frac > 0.03 && route_failing {
            // Congestion onset — back off a quarter.
            self.mark_loss_ceiling();
            self.decrease_to((self.kbps as f32 * 0.75) as u32);
            self.cooldown = 3;
            self.clean_streak = 0;
        } else if self.warmup == 0 && fb.rtp_backlog_ms > 100.0 {
            // The send queue is growing: the peer stopped consuming at the
            // current rate. Loss and decoded fps say nothing here - WebKit
            // buffers happily right up until connection flow control freezes
            // (2026-09-09 19:53) - but the writer's own stall time sees the
            // queue directly. Back off gently while it is still moving.
            self.decrease_to((self.kbps as f32 * 0.85) as u32);
            self.cooldown = 2;
            self.clean_streak = 0;
        } else {
            // Clean-ish tick.
            //
            // Nothing decoding is not a clean tick to climb on. The 19:28
            // session ramped 6000 -> 20000 kbps across five seconds while
            // decoded_fps sat at 0 the whole way: loss was 0 because the client
            // was receiving everything, it just was not decoding any of it.
            // Raising the rate there makes every keyframe bigger for a client
            // that has never managed to decode one, and the 20 Mbps peak then
            // drew the real loss that triggered the cuts.
            //
            // Frames reaching the decoder is the precondition for believing
            // more bitrate will help.
            //
            // "Reaching the decoder" means most of the target rate, OR the
            // client is receiving almost nothing we send (low-motion screen:
            // the encoder emits a trickle, decode is slow by nature, not by
            // congestion). The 02:37 Chrome session decoded 1 fps for 10 s
            // (the 80 Mbps IDR could not reassemble) while receiving the
            // full flood at 51-68 Mbps - decoded_fps=1.0 passed the old
            // `> 0` gate and the controller CLIMBED 45 -> 66 Mbps through
            // the black screen. A collapsed route must never climb.
            let decoding = fb.decoded_fps > 0.0
                && (fb.decoded_fps >= fb.target_fps as f32 * 0.5
                    || fb.recv_kbps < self.kbps as f32 * 0.2);
            self.clean_streak = self.clean_streak.saturating_add(1);
            // Let a remembered loss ceiling drift back up so a genuinely
            // improved path can recover — but slowly (~3 %/s), so the rate
            // settles below a level that draws loss instead of charging back
            // to it every few seconds (webrtcbin has no send pacing, so the
            // top of the range briefly overruns the RTP queue).
            if let Some(sc) = self.soft_ceiling {
                // A ceiling that came from tail delay is a bandwidth estimate:
                // creeping past it re-queues the route and freezes the picture
                // again, so it lifts at 0.2 %/s and only while the tail is
                // quiet enough to show the queue actually drained. Recovery
                // from a cut is fast anyway — the rate climbs at 3 %/s up to
                // the cap, and only probing *past* it is slow.
                // 2 %/s, not 0.2 %/s. A delay-marked ceiling is a real
                // estimate of where the route queued, but at 0.2 %/s it was
                // functionally permanent: a session that had been cut to
                // 22 Mbps needed ln(80/22)/0.002 ~ 640 s of *continuously*
                // sub-50 ms tail to be allowed anywhere near an 80 Mbps
                // negotiation, and any single tick above 50 ms reset the
                // clock. That is why raising `max_kbps` did nothing - the
                // ceiling, not the cap, was binding. The climb still stops at
                // the bar (probe cap is 0.9 x it), and re-queuing re-marks it,
                // so the loop is unchanged; only the approach is quicker.
                let creep = if self.ceiling_from_delay {
                    if fb.client_lat_p95_ms <= 50.0 {
                        1.02
                    } else {
                        1.0
                    }
                } else {
                    1.03
                };
                let relaxed = sc * creep;
                self.soft_ceiling = if relaxed >= self.ceiling as f32 {
                    None
                } else {
                    Some(relaxed)
                };
                // Forty-five consecutive clean seconds is stronger evidence
                // than one episode from minutes ago: forget the estimate and
                // let the rate probe the paced ceiling again. Without this the
                // remembered bar outlives the conditions that produced it.
                if self.clean_streak >= 45 && self.ceiling_from_delay {
                    self.soft_ceiling = None;
                    self.ceiling_from_delay = false;
                }
                // A loss ceiling is a transient event, and in practice the
                // event was often the stream's own recovery: an IDR burst
                // stalls decode for a second, that tick reads as a failing
                // route, and the cut marks the bar at whatever rate the
                // session had reached (8640 kbps on 2026-09-22 against an
                // 80 Mbps cap). Creeping 3 %/s from there to the cap takes
                // ~75 s of uninterrupted clean ticks, and any blip restarts
                // it - so a single cut held the rate below the user's mark
                // for the rest of the session. Thirty clean seconds is enough
                // to call it over; a route that really cannot carry the rate
                // re-marks the bar on the next probe.
                if self.clean_streak >= 30 && !self.ceiling_from_delay {
                    self.soft_ceiling = None;
                }
            }
            // Losing datagrams is not a reason to cut on its own - parity and
            // NACKs deliver frames through 10-25% erasure - but it is a
            // reason not to *raise* the rate: more rate means bigger frames,
            // more fragments per FEC group, and loss that parity can no longer
            // cover. The 80 Mbps session ramped through 6 -> 22 Mbps while the
            // client was failing to assemble frames, which is the loop this
            // closes. Measured loss only: `lost_delta` is a model.
            // 2 %, over the transport's multi-second window: 1 % per tick sat
            // inside the measurement's own sampling skew (+/-2 %), so noise
            // alone vetoed most climbs, and Wi-Fi's ordinary 1-2 % erasure is
            // what row parity and NACK exist to absorb.
            let loss_blocks_climb = fb.inferred_loss_frac.is_some_and(|l| l >= 0.02);
            if self.cooldown == 0
                && decoding
                && self.kbps < self.ceiling
                && fb.rtp_backlog_ms < 25.0
                && lat_known
                && fb.client_lat_p95_ms <= 100.0
                && !loss_blocks_climb
            {
                // Climb only while the send queue is empty: a peer that reads
                // slower than we write buffers the difference somewhere, and
                // the stall signal is the only place that shows (see the
                // backlog branch above).
                let probe_cap = self
                    .soft_ceiling
                    .map(|sc| (sc * 0.9) as u32)
                    .unwrap_or(self.ceiling)
                    .clamp(self.floor, self.ceiling);
                // The paced v4 carrier cannot carry more than the pace allows; probing
                // above it just queues frames into expiry (05:08 Chrome: 53 Mbps ≈
                // 5.5k datagrams/s overflowed the browser's incoming-datagram
                // queue → permanent 1 fps re-key loop). The ceiling derives from
                // the connection's pace: worker-drain clients pace higher and
                // climb higher; legacy telemetry (pace unknown) falls back to
                // the in-page constant.
                let pace_ceiling =
                    |pps: u32| ((pps as f32) * crate::media::wt::WT_PACE_TO_CEILING) as u32;
                let paced_ceiling = if fb.v4_pace_pps > 0 {
                    pace_ceiling(fb.v4_pace_pps)
                } else {
                    crate::media::wt::WT_PACED_CEILING_KBPS
                };
                let probe_cap = if fb.v4_datagram_carrier {
                    probe_cap.min(paced_ceiling.max(self.floor))
                } else {
                    probe_cap
                };
                // Raise on the absence of loss alone — NOT gated on decoded fps.
                // A low fps with no loss means capture/encode is the bottleneck,
                // and keeping the bitrate pinned low only makes that worse.
                let low_rtt = fb.rtt_ms > 1.0 && fb.rtt_ms < 40.0;
                if self.kbps < probe_cap {
                    if low_rtt && self.clean_streak >= 2 {
                        // LAN: x1.3 per tick. Doubling (the old x2) hit a
                        // 2560x1440 encoder with 62 Mbps inside 4 s on the
                        // 02:36 Chrome session - IDRs turned into
                        // megabyte-scale fragment floods the client could
                        // not reassemble (held=8, decode 1 fps) - and the
                        // rate cut SLOWER than the collapse it caused.
                        // Fast cut, slow climb.
                        self.kbps = self
                            .kbps
                            .saturating_mul(13)
                            .saturating_div(10)
                            .min(probe_cap);
                    } else if self.clean_streak >= 3 {
                        // WAN / relay: crawl, proportional to the current rate.
                        let stepk = (self.kbps / 5).max(200);
                        self.kbps = (self.kbps + stepk).min(probe_cap);
                    }
                }
            }
        }
        // A carrier switched on mid-session (or a rate carried over from a
        // stream-carrier session) can leave kbps above the paced ceiling;
        // walk it under immediately rather than feeding the pacer more than
        // it can inject.
        if fb.v4_datagram_carrier {
            let paced_ceiling = if fb.v4_pace_pps > 0 {
                ((fb.v4_pace_pps as f32) * crate::media::wt::WT_PACE_TO_CEILING) as u32
            } else {
                crate::media::wt::WT_PACED_CEILING_KBPS
            };
            self.kbps = self.kbps.min(paced_ceiling.max(self.floor));
        }
        self.kbps
    }

    /// True when this objection is the START of an episode: the controller was
    /// climbing (or has nothing remembered yet). Successive cuts inside one bad
    /// stretch are the consequence of the objection already recorded, and
    /// re-marking on each of them ratchets the ceiling down with the kbps the
    /// cut itself produced - ten ticks of high tail delay drove 50000 -> 1412
    /// and the ceiling with it, leaving a probe cap below the rate the route
    /// had already been carrying, so it could never be re-tried.
    fn first_objection_of_episode(&self) -> bool {
        self.soft_ceiling.is_none() || self.clean_streak > 0
    }

    fn mark_loss_ceiling(&mut self) {
        // Remember the rate that just failed - the rate the route *was*
        // carrying, which is what says where its limit is.
        if self.first_objection_of_episode() {
            self.soft_ceiling = Some(self.kbps as f32);
        }
        // A cut ends the episode. Without this, a *series* of cuts off one
        // elevation — the tail percentile is a rolling window, so it stays
        // above the threshold for seconds after the route cleared — each
        // re-marked the bar at the newly lower rate and ratcheted the ceiling
        // down with it: 50000 -> 1412 across seven cuts in seven seconds, with
        // the client decoding 60 fps on a 7 ms RTT the whole way down.
        self.clean_streak = 0;
        // Loss is a transient event; a ceiling remembered from it is expected to
        // lift once the burst is over.
        self.ceiling_from_delay = false;
    }

    /// A ceiling remembered from tail delay, not loss: this route *queues* at
    /// this rate, so it is a bandwidth estimate rather than a transient event.
    fn mark_delay_ceiling(&mut self) {
        let episode_start = self.first_objection_of_episode();
        self.mark_loss_ceiling();
        if episode_start {
            self.ceiling_from_delay = true;
        }
    }

    fn decrease_to(&mut self, target: u32) {
        self.kbps = target.max(self.floor).min(self.kbps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wan(lost: u64, recv: f32, fps: f32) -> Feedback {
        Feedback {
            client_lat_p95_ms: 0.0,
            v4_datagram_carrier: false,
            v4_pace_pps: 0,
            inferred_loss_frac: None,
            lost_delta: lost,
            recv_kbps: recv,
            decoded_fps: fps,
            target_fps: 60,
            rtt_ms: 130.0,
            rtp_backlog_ms: 0.0,
            have_client: true,
        }
    }

    /// Drive the controller against a modelled path of `capacity` kbps with a
    /// one-tick reporting lag, for `ticks` seconds. Returns the final rate.
    fn simulate(c: &mut BitrateController, capacity: f32, rtt_ms: f32, ticks: usize) -> u32 {
        let mut sent_prev = c.kbps as f32;
        for _ in 0..ticks {
            let delivered = sent_prev.min(capacity);
            let over = (sent_prev - capacity).max(0.0);
            // ~1100 B/pkt → packets/s lost is over_kbps*1000/8/1100.
            let lost = (over * 1000.0 / 8.0 / 1100.0) as u64;
            let fps = if delivered >= sent_prev * 0.85 {
                58.0
            } else {
                0.0
            };
            let fb = Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: lost,
                recv_kbps: delivered,
                decoded_fps: fps,
                target_fps: 60,
                rtt_ms,
                rtp_backlog_ms: 0.0,
                have_client: true,
            };
            sent_prev = c.step(&fb) as f32;
        }
        c.kbps
    }

    /// The 2026-09-08 phone session, replayed from its own log.
    ///
    /// A static desktop makes the encoder emit less (900 -> 490 -> 244
    /// datagrams/s at a steady 60 fps, before anything was cut), on a cellular
    /// route with ~20% base erasure that parity and NACK were repairing. The
    /// client was decoding 58 fps and receiving 2.7 Mbps - the stream was
    /// working. The old law read the falling receive rate as a failing route
    /// and cut 4000 -> 2000 -> 1000 -> 600 in five seconds, and each cut made
    /// recv fall further, which "proved" the next one.
    #[test]
    fn a_static_desktop_on_a_lossy_route_is_not_congestion() {
        let mut c = BitrateController::new(4_000, 600, 50_000);
        // Warm up past the startup guard on a healthy route.
        for _ in 0..6 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 4_000.0,
                decoded_fps: 60.0,
                target_fps: 60,
                rtt_ms: 50.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            });
        }
        let before = c.kbps;

        // Replayed from the log: (lost_delta, recv_kbps) as the screen goes
        // static, with the decoder keeping up the whole way. The first sample
        // is the tick that started the real collapse.
        let replay: [(u64, f32); 8] = [
            (84, 2_761.0),
            (27, 1_756.0),
            (29, 1_677.0),
            (30, 900.0),
            (25, 700.0),
            (25, 700.0),
            (25, 700.0),
            (25, 700.0),
        ];
        for (lost, recv) in replay {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: lost,
                recv_kbps: recv,
                decoded_fps: 58.0,
                target_fps: 60,
                rtt_ms: 54.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            });
        }
        assert!(
            c.kbps >= before,
            "a decoding client on a static screen must not be cut: {before} -> {}",
            c.kbps
        );
    }

    /// The erasure has to be catastrophic, or the decoder has to actually be
    /// failing, before anything is cut.
    #[test]
    fn a_collapsing_decoder_still_cuts() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for _ in 0..6 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 6_000.0,
                decoded_fps: 60.0,
                target_fps: 60,
                rtt_ms: 50.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            });
        }
        let before = c.kbps;
        for _ in 0..3 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 400,
                recv_kbps: 2_000.0,
                decoded_fps: 4.0, // frames are not reaching the decoder
                target_fps: 60,
                rtt_ms: 200.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            });
        }
        assert!(c.kbps < before, "genuine failure must still back off");
    }

    /// The 19:28 session: a client receiving everything and decoding nothing.
    /// Loss is genuinely 0, so every tick looks clean - and the old law read
    /// that as room to grow, ramping 6000 -> 20000 kbps in five seconds. The
    /// bigger keyframes then drew the loss that triggered the cuts.
    #[test]
    fn never_climbs_while_nothing_is_decoding() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for _ in 0..30 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 8_200.0, // arriving fine
                decoded_fps: 0.0,   // and decoding none of it
                target_fps: 60,
                rtt_ms: 56.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            });
        }
        assert_eq!(
            c.kbps, 6_000,
            "more bitrate cannot help a decoder that is producing nothing"
        );
    }

    #[test]
    fn holds_until_client_telemetry_arrives() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for _ in 0..5 {
            let fb = Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                have_client: false,
                ..Default::default()
            };
            assert_eq!(c.step(&fb), 6_000);
        }
    }

    /// Replays the shape of the 2026-09-02 iPhone-over-DERP session: encoder at
    /// 4 Mbps, the relay delivers ~300 kbps-3 Mbps with ~100 lost/s. The
    /// controller must collapse toward the floor within a few seconds so the
    /// session can actually run (the raw log showed 58-72 fps decoding the
    /// instant the rate briefly hit ~300 kbps).
    #[test]
    fn collapses_hard_on_derp_relay_loss() {
        // 4 Mbps encoder onto a ~900 kbps relay (the 2026-09-02 iPhone case).
        let mut c = BitrateController::new(4_000, 600, 50_000);
        let end = simulate(&mut c, 900.0, 250.0, 8);
        assert!(
            end <= 1_100,
            "expected collapse near the relay capacity, got {end}"
        );
        assert!(end >= 600, "must not go below the floor");
    }

    #[test]
    fn converges_and_stays_up_on_a_relay() {
        // Long run on a 1 Mbps relay: must settle around capacity, not oscillate
        // between a flood and the floor.
        let mut c = BitrateController::new(6_000, 600, 50_000);
        simulate(&mut c, 1_000.0, 250.0, 8);
        let mut lo = u32::MAX;
        let mut hi = 0;
        for _ in 0..40 {
            let k = simulate(&mut c, 1_000.0, 250.0, 1);
            lo = lo.min(k);
            hi = hi.max(k);
        }
        assert!(
            lo >= 600 && hi <= 2_600,
            "steady-state band {lo}..{hi} kbps too wide"
        );
    }

    #[test]
    fn paced_carrier_never_probes_past_the_pace_ceiling() {
        // 05:08 Chrome session: the AIMD climbed to 53 Mbps ≈ 5.5k datagrams/s
        // and the browser's incoming-datagram queue silently dropped from the
        // head — every large frame became a permanent hole and decode fell
        // into a 1 fps re-key loop at 0% host-side loss. On the paced v4
        // carrier the target must stop at what WT_PACE_PPS can inject.
        let mut c = BitrateController::new(6_000, 600, 80_000);
        for tick in 0..30 {
            let fb = Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: true,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: c.kbps as f32 * 0.95,
                decoded_fps: 60.0,
                target_fps: 60,
                rtt_ms: 4.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            };
            c.step(&fb);
            assert!(
                c.kbps <= crate::media::wt::WT_PACED_CEILING_KBPS,
                "tick {tick}: climbed to {} on the paced carrier",
                c.kbps
            );
        }
        assert_eq!(c.kbps, crate::media::wt::WT_PACED_CEILING_KBPS);
        // A rate carried over from an unpaced session is walked under.
        let mut c = BitrateController::new(80_000, 600, 80_000);
        let fb = Feedback {
            client_lat_p95_ms: 0.0,
            v4_datagram_carrier: true,
            v4_pace_pps: 0,
            inferred_loss_frac: None,
            lost_delta: 0,
            recv_kbps: 70_000.0,
            decoded_fps: 60.0,
            target_fps: 60,
            rtt_ms: 4.0,
            rtp_backlog_ms: 0.0,
            have_client: true,
        };
        c.step(&fb);
        assert!(c.kbps <= crate::media::wt::WT_PACED_CEILING_KBPS);
    }

    #[test]
    fn low_motion_lan_ramps_to_the_ceiling_and_stays() {
        // The regression: a clean LAN where the encoder emits far less than its
        // cap (static screen, or capture starved) — inbound is a trickle but
        // there is zero loss. The controller must NOT read that as starvation.
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for tick in 0..30 {
            let fb = Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 120.0, // near-static screen: tiny inbound...
                decoded_fps: 8.0, // ...and low fps, but not because of bandwidth
                target_fps: 60,
                rtt_ms: 4.0,
                rtp_backlog_ms: 0.0,
                have_client: true,
            };
            c.step(&fb);
            assert!(
                c.kbps >= 6_000,
                "tick {tick}: bitrate dropped to {} on a lossless LAN",
                c.kbps
            );
        }
        assert_eq!(c.kbps, 50_000, "a lossless LAN must reach the ceiling");
    }

    #[test]
    fn recovers_when_a_relay_clears() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        simulate(&mut c, 900.0, 250.0, 8);
        let bottom = c.kbps;
        let mid = simulate(&mut c, 40_000.0, 250.0, 20);
        assert!(
            mid > bottom * 2,
            "should be climbing once the relay clears ({bottom} -> {mid})"
        );
        let end = simulate(&mut c, 40_000.0, 250.0, 60);
        assert!(
            end > 20_000,
            "should recover most of the ceiling on a cleared path ({end})"
        );
    }

    #[test]
    fn ramps_fast_on_a_clean_lan() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        let end = simulate(&mut c, 200_000.0, 8.0, 12);
        assert_eq!(
            end, 50_000,
            "LAN x1.3 climb reaches the ceiling in ~10 ticks"
        );
    }

    /// One lossy second is a burst, not a verdict.
    ///
    /// The severe branch halves the rate, and with the measured loss input a
    /// single Wi-Fi burst could trigger it: the 80 Mbps session cut 63 times in
    /// eleven minutes, nearly all of them halvings, each costing ~10 s to climb
    /// back. A quarter cut on the first lossy second, a halving only once it
    /// persists.
    #[test]
    fn a_single_lossy_second_does_not_halve_the_rate() {
        let mut c = BitrateController::new(30_000, 600, 80_000);
        let spike = Feedback {
            client_lat_p95_ms: 30.0,
            decoded_fps: 20.0,
            target_fps: 60,
            have_client: true,
            recv_kbps: 30_000.0,
            inferred_loss_frac: Some(0.15),
            v4_datagram_carrier: true,
            v4_pace_pps: 11_000,
            ..Default::default()
        };
        c.step(&spike);
        assert!(
            c.kbps > 15_000,
            "one lossy second must not halve the rate ({})",
            c.kbps
        );
        for _ in 0..6 {
            c.step(&spike);
        }
        assert!(
            c.kbps <= 15_000,
            "sustained loss still collapses the rate ({})",
            c.kbps
        );
    }

    /// Measured loss stops the ramp without cutting the rate.
    ///
    /// Loss alone is not a verdict — parity and NACKs deliver frames through
    /// 10-25 % erasure, and the cellular session below relies on that — but
    /// raising the rate makes frames bigger, and a bigger frame puts more
    /// fragments in each FEC group until one burst takes out more than parity
    /// can rebuild. The 80 Mbps session climbed 6 -> 22 Mbps while the client
    /// was failing to assemble frames at all, which is the loop this closes.
    #[test]
    fn measured_loss_holds_the_rate_without_cutting_it() {
        let mut c = BitrateController::new(6_000, 600, 80_000);
        let clean = Feedback {
            client_lat_p95_ms: 20.0,
            rtt_ms: 8.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            recv_kbps: 6_000.0,
            v4_datagram_carrier: true,
            v4_pace_pps: 11_000,
            inferred_loss_frac: Some(0.0),
            ..Default::default()
        };
        for _ in 0..6 {
            c.step(&clean);
        }
        let climbed = c.kbps;
        assert!(climbed > 6_000, "a clean route climbs ({climbed})");

        let lossy = Feedback {
            inferred_loss_frac: Some(0.03),
            ..clean
        };
        for _ in 0..6 {
            c.step(&lossy);
        }
        assert_eq!(c.kbps, climbed, "3% measured loss holds the rate, no cut");
    }

    /// One recovery burst must not cap the session for good. The 2026-09-22
    /// session cut once at 8640 kbps (an IDR stall read as a failing route)
    /// and then crept 3 %/s under the remembered bar, never approaching the
    /// 80 Mbps the user set.
    #[test]
    fn one_loss_cut_does_not_hold_the_rate_below_the_set_mark() {
        let mut c = BitrateController::new(6_000, 600, 80_000);
        let clean = Feedback {
            client_lat_p95_ms: 20.0,
            rtt_ms: 8.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            recv_kbps: 6_000.0,
            v4_datagram_carrier: true,
            v4_pace_pps: 11_000,
            inferred_loss_frac: Some(0.0),
            ..Default::default()
        };
        for _ in 0..8 {
            c.step(&clean);
        }
        let stall = Feedback {
            decoded_fps: 20.0,
            inferred_loss_frac: Some(0.2),
            ..clean
        };
        c.step(&stall);
        let cut = c.kbps;
        for _ in 0..60 {
            c.step(&clean);
        }
        assert_eq!(c.kbps, 80_000, "cut to {cut}, then 60 clean seconds");
    }

    /// Wi-Fi's ordinary erasure, repaired by parity, must not veto the climb.
    #[test]
    fn low_measured_loss_does_not_block_the_climb() {
        let mut c = BitrateController::new(6_000, 600, 80_000);
        let fb = Feedback {
            client_lat_p95_ms: 20.0,
            rtt_ms: 8.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            recv_kbps: 6_000.0,
            v4_datagram_carrier: true,
            v4_pace_pps: 11_000,
            inferred_loss_frac: Some(0.012),
            ..Default::default()
        };
        for _ in 0..20 {
            c.step(&fb);
        }
        assert_eq!(c.kbps, 80_000);
    }

    /// The measured figure replaces the model when one exists: `lost_delta` is
    /// frames the host reset scaled by a fixed bits-per-packet estimate.
    #[test]
    fn measured_loss_overrides_the_modelled_loss_input() {
        let mut c = BitrateController::new(20_000, 600, 80_000);
        // A model that screams (all frames lost) with a measurement that says
        // the route is fine: the measured number must win.
        let fb = Feedback {
            client_lat_p95_ms: 0.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            recv_kbps: 20_000.0,
            lost_delta: 10_000,
            inferred_loss_frac: Some(0.0),
            ..Default::default()
        };
        c.step(&fb);
        assert_eq!(
            c.kbps, 20_000,
            "no cut from a model the measurement contradicts"
        );
    }

    /// A Wi-Fi excursion is not a verdict on the host's send rate.
    ///
    /// The cooldown gated the 120-250 ms band but exempted anything above it,
    /// so a single 267-481 ms spike - exactly what airtime contention throws -
    /// cut 0.6x on every tick: 4000 -> 2400 -> 1440 -> 864 -> 600 kbps inside
    /// five seconds, while the client decoded 59-60 fps throughout (live log
    /// 2026-09-20 04:38). One spike is one cut now; a *sustained* severe tail
    /// still ends at the floor.
    #[test]
    fn a_severe_tail_spike_cannot_empty_the_stream_by_itself() {
        let mut c = BitrateController::new(4_000, 600, 4_000);
        let severe = Feedback {
            client_lat_p95_ms: 400.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            ..Default::default()
        };
        for _ in 0..4 {
            c.step(&severe);
        }
        assert!(
            c.kbps >= 2_400,
            "one spike must not cascade to the floor ({})",
            c.kbps
        );
        for _ in 0..40 {
            c.step(&severe);
        }
        assert_eq!(
            c.kbps, 600,
            "a sustained severe tail still reaches the floor"
        );
    }

    /// `lat_p95_ms` is negative until the client's clock sync settles, and the
    /// host used to clamp that to 0.0 - the perfect signal, which passed every
    /// delay guard and the climb gate. Rate ramps on a route nobody measured.
    #[test]
    fn an_unknown_tail_percentile_does_not_authorise_a_climb() {
        let mut c = BitrateController::new(4_000, 600, 20_000);
        let unknown = Feedback {
            client_lat_p95_ms: -1.0,
            decoded_fps: 60.0,
            target_fps: 60,
            have_client: true,
            ..Default::default()
        };
        for _ in 0..20 {
            c.step(&unknown);
        }
        assert_eq!(
            c.kbps, 4_000,
            "no climb without a measured tail ({})",
            c.kbps
        );
    }

    #[test]
    fn rising_client_tail_delay_cuts_before_the_loss_it_causes() {
        // The 00:19 signature: the client received the full flood but
        // assembled ~2 frames/s while p95 fragment age climbed 33 -> 129
        // -> 293 ms. The host queue never stalled, loss_frac stayed ~0 -
        // only the client's tail delay saw the collapse.
        let mut c = BitrateController::new(6_000, 600, 50_000);
        simulate(&mut c, 200_000.0, 8.0, 12);
        assert_eq!(c.kbps, 50_000);
        // One cut per cooldown window (the tail percentile is a rolling window
        // and must not be able to drive a cut on every single tick), so a
        // sustained 160 ms tail takes a few seconds to reach the same place.
        for _ in 0..8 {
            c.step(&Feedback {
                client_lat_p95_ms: 160.0,
                decoded_fps: 0.0,
                target_fps: 60,
                have_client: true,
                ..Default::default()
            });
        }
        assert!(c.kbps < 20_000, "p95 > 120 ms must cut hard ({})", c.kbps);
        // And it must not charge back up while the tail stays elevated.
        for _ in 0..6 {
            c.step(&Feedback {
                client_lat_p95_ms: 160.0,
                decoded_fps: 60.0,
                target_fps: 60,
                have_client: true,
                ..Default::default()
            });
        }
        assert!(c.kbps < 20_000, "no climb while p95 > 100 ms ({})", c.kbps);
        // Tail settles -> climbing resumes (slow ~3 %/s, so give it ticks).
        for _ in 0..45 {
            c.step(&Feedback {
                client_lat_p95_ms: 40.0,
                decoded_fps: 60.0,
                target_fps: 60,
                have_client: true,
                ..Default::default()
            });
        }
        assert!(
            c.kbps > 20_000,
            "recovers once the tail clears ({})",
            c.kbps
        );
    }

    /// The 2026-09-20 03:57 sawtooth, replayed from the live log.
    ///
    /// A 1440p60 session over the internet on a ~6 Mbps uplink with a deep
    /// buffer. The ramp reached ~6.3 Mbps, the client's p95 fragment age
    /// exploded 25 -> 155 ms, decode collapsed to 0, and the delay branch cut
    /// 0.7x a tick - seven cuts to the floor. Then it forgot, ramped
    /// 600 -> 6354 again, and did it again every ~17 s for the whole session.
    /// `lost_delta` was 0 throughout: nothing dropped, the path just queued.
    ///
    /// A route like this has a *bandwidth*, so the controller must settle below
    /// it instead of charging back into the queue every ramp.
    #[test]
    fn a_queuing_route_settles_instead_of_sawtoothing() {
        // Latency stays flat until the rate approaches capacity, then a deep
        // buffer inflates the tail (25 ms base, 1.3x -> 125 ms).
        fn queuing_route(c: &mut BitrateController, capacity: f32, ticks: usize) -> Vec<u32> {
            let mut history = Vec::with_capacity(ticks);
            let mut sent_prev = c.kbps as f32;
            for _ in 0..ticks {
                let delivered = sent_prev.min(capacity);
                let q = sent_prev / capacity;
                // The live shape: 25 ms flat, then a cliff. 4.9 Mbps was
                // comfortable on this link and 6.3 Mbps read 155 ms.
                let p95 = 25.0 + 4000.0 * (q - 0.85).max(0.0).powi(3);
                let fb = Feedback {
                    client_lat_p95_ms: p95,
                    decoded_fps: if p95 < 100.0 { 58.0 } else { 0.0 },
                    target_fps: 60,
                    lost_delta: 0,
                    recv_kbps: delivered,
                    rtt_ms: 10.0 + (p95 - 25.0).max(0.0),
                    have_client: true,
                    ..Default::default()
                };
                sent_prev = c.step(&fb) as f32;
                history.push(c.kbps);
            }
            history
        }

        let mut c = BitrateController::new(6_000, 600, 80_000);
        let history = queuing_route(&mut c, 6_000.0, 400);

        // It must find the route's ceiling and hold near it: at least 4 Mbps
        // (worth having at 1440p) and never the floor once it has settled.
        let settled = &history[150..];
        let lowest = settled.iter().copied().min().unwrap();
        assert!(
            lowest >= 4_000,
            "the controller collapsed to {lowest} kbps after settling: the              delay ceiling is being forgotten and re-probed"
        );
        // And it must stop re-climbing into the queue: a handful of probes over
        // 250 ticks is a route being re-measured; a sawtooth is every ~15 s.
        let cuts = settled.windows(2).filter(|w| w[1] < w[0]).count();
        assert!(
            cuts <= 12,
            "{cuts} decreases in 250 settled ticks is a sawtooth, not a settled rate"
        );
        let highest = settled.iter().copied().max().unwrap();
        assert!(
            highest <= 7_500,
            "it charged {highest} kbps back into a 6000 kbps route"
        );
    }

    #[test]
    fn never_climbs_while_the_client_receives_but_cannot_decode() {
        // The 02:37 Chrome shape: 45 -> 66 Mbps climbed through a 10 s black
        // screen. The client received the full flood (recv ~= current rate)
        // but assembled nothing (decoded 1 fps) - every climb is gasoline.
        let mut c = BitrateController::new(45_000, 600, 80_000);
        for _ in 0..8 {
            let kbps = c.step(&Feedback {
                client_lat_p95_ms: 20.0,
                recv_kbps: c.kbps as f32 * 1.15, // everything arrives...
                decoded_fps: 1.0,                // ...and none of it decodes
                target_fps: 60,
                rtt_ms: 4.0,
                have_client: true,
                ..Default::default()
            });
            assert!(
                kbps <= 45_000,
                "climbed to {kbps} while the route was collapsed"
            );
        }
        // Low-motion screen: client receives a trickle of an 8 fps stream -
        // different situation, climbing is correct.
        let mut c2 = BitrateController::new(6_000, 600, 50_000);
        for _ in 0..14 {
            c2.step(&Feedback {
                client_lat_p95_ms: 20.0,
                recv_kbps: 120.0,
                decoded_fps: 8.0,
                target_fps: 60,
                rtt_ms: 4.0,
                have_client: true,
                ..Default::default()
            });
        }
        assert_eq!(
            c2.kbps, 50_000,
            "low-motion LAN still climbs to the ceiling"
        );
    }

    #[test]
    fn backlog_cuts_before_client_report() {
        let mut c = BitrateController::new(20_000, 600, 50_000);
        let fb = Feedback {
            client_lat_p95_ms: 0.0,
            v4_datagram_carrier: false,
            v4_pace_pps: 0,
            inferred_loss_frac: None,
            rtp_backlog_ms: 400.0,
            have_client: false,
            ..Default::default()
        };
        c.step(&fb);
        assert!(c.kbps < 20_000 && c.kbps >= 600);
    }

    #[test]
    fn cellular_erasure_with_healthy_decode_does_not_collapse() {
        // The 2026-09-08 Safari session: 8-16 % fragment loss (cellular
        // erasure, repaired by parity) while the phone received 7-9.8 Mbps at
        // 59-61 fps. The old law cut 6000 -> 600 in 5 s; the law must hold,
        // and climb back through the same loss after a real dip.
        let fb = Feedback {
            client_lat_p95_ms: 0.0,
            v4_datagram_carrier: false,
            v4_pace_pps: 0,
            inferred_loss_frac: None,
            lost_delta: 90,
            recv_kbps: 9_000.0,
            decoded_fps: 59.0,
            target_fps: 60,
            rtt_ms: 130.0,
            rtp_backlog_ms: 0.0,
            have_client: true,
        };
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for tick in 0..10 {
            assert!(
                c.step(&fb) >= 6_000,
                "tick {tick}: cut the bitrate on healthy erasure-loss"
            );
        }
        c.kbps = 1_500; // after a real dip (e.g. the one 6 fps second)
        for _ in 0..30 {
            c.step(&fb);
        }
        assert!(
            c.kbps > 4_000,
            "must climb back through erasure loss, got {}",
            c.kbps
        );
    }

    #[test]
    fn never_exceeds_ceiling_or_underflows_floor() {
        let mut c = BitrateController::new(6_000, 600, 8_000);
        for _ in 0..50 {
            c.step(&wan(0, 8_000.0, 60.0));
            assert!(c.kbps <= 8_000);
        }
        for _ in 0..50 {
            c.step(&wan(500, 100.0, 0.0));
            assert!(c.kbps >= 600);
        }
    }

    /// The 2026-09-09 19:53 phone session: every input looked perfect (zero
    /// loss, 60 decoded fps, high recv) while the WT frame writer's queue
    /// grew, because WebKit buffered everything it was sent. The controller
    /// must read the writer's stall time - never climb while it is nonzero,
    /// and cut once the queue is visibly growing.
    #[test]
    fn send_queue_stall_blocks_climb_then_cuts() {
        let mut c = BitrateController::new(6_000, 600, 50_000);
        // Warmup + clean streak, all signals perfect except the stall.
        for _ in 0..6 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 6_000.0,
                decoded_fps: 60.0,
                target_fps: 60,
                rtt_ms: 45.0,
                rtp_backlog_ms: 40.0, // queue building
                have_client: true,
            });
        }
        assert_eq!(
            c.kbps, 6_000,
            "a peer that cannot consume at the current rate must not be sent more"
        );
        // Queue keeps growing: the mid-tier cut engages.
        for _ in 0..4 {
            c.step(&Feedback {
                client_lat_p95_ms: 0.0,
                v4_datagram_carrier: false,
                v4_pace_pps: 0,
                inferred_loss_frac: None,
                lost_delta: 0,
                recv_kbps: 6_000.0,
                decoded_fps: 60.0,
                target_fps: 60,
                rtt_ms: 45.0,
                rtp_backlog_ms: 150.0,
                have_client: true,
            });
        }
        assert!(
            c.kbps < 6_000,
            "a growing send queue must back off before flow control freezes"
        );
    }
}
