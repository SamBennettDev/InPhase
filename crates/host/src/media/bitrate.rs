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
    /// The rate that most recently drew loss. Probing won't push past ~90 % of
    /// it; it relaxes slowly while the path stays clean so a genuinely improved
    /// path can still recover the full ceiling.
    soft_ceiling: Option<f32>,
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
            soft_ceiling: None,
        }
    }

    /// Advance one tick; returns the new encoder target (kbps).
    pub fn step(&mut self, fb: &Feedback) -> u32 {
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

        // The only trustworthy congestion signal we have is **packet loss**
        // (plus the send-queue backlog above). We deliberately do NOT treat
        // "inbound bitrate < encoder target" as starvation: with VBR encoding
        // and a low-motion screen the encoder legitimately emits far less than
        // its cap, and reading that as congestion collapses the bitrate to the
        // floor and never lets it back up.
        let recv_pps = (fb.recv_kbps * 1000.0 / 8.0 / 1100.0).max(0.0);
        let denom = fb.lost_delta as f32 + recv_pps;
        let loss_frac = if denom > 1.0 {
            fb.lost_delta as f32 / denom
        } else {
            0.0
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
        let decode_collapsed =
            fb.target_fps > 0 && (fb.decoded_fps as f32) < fb.target_fps as f32 * 0.7;
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
        if self.warmup == 0 && route_failing && loss_frac > 0.10 {
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
            let decoding = fb.decoded_fps > 0.0;
            self.clean_streak = self.clean_streak.saturating_add(1);
            // Let a remembered loss ceiling drift back up so a genuinely
            // improved path can recover — but slowly (~3 %/s), so the rate
            // settles below a level that draws loss instead of charging back
            // to it every few seconds (webrtcbin has no send pacing, so the
            // top of the range briefly overruns the RTP queue).
            if let Some(sc) = self.soft_ceiling {
                let relaxed = sc * 1.03;
                self.soft_ceiling = if relaxed >= self.ceiling as f32 {
                    None
                } else {
                    Some(relaxed)
                };
            }
            if self.cooldown > 0 {
                self.cooldown -= 1;
            } else if decoding
                && self.kbps < self.ceiling
                && fb.rtp_backlog_ms < 25.0
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
                // Raise on the absence of loss alone — NOT gated on decoded fps.
                // A low fps with no loss means capture/encode is the bottleneck,
                // and keeping the bitrate pinned low only makes that worse.
                let low_rtt = fb.rtt_ms > 1.0 && fb.rtt_ms < 40.0;
                if self.kbps < probe_cap {
                    if low_rtt && self.clean_streak >= 2 {
                        // LAN: double each tick — reach a 50 Mbps ceiling in ~4 s.
                        self.kbps = self.kbps.saturating_mul(2).min(probe_cap);
                    } else if self.clean_streak >= 3 {
                        // WAN / relay: crawl, proportional to the current rate.
                        let stepk = (self.kbps / 5).max(200);
                        self.kbps = (self.kbps + stepk).min(probe_cap);
                    }
                }
            }
        }
        self.kbps
    }

    fn mark_loss_ceiling(&mut self) {
        // Remember the rate that just failed (unless we already have a lower one).
        let here = self.kbps as f32;
        self.soft_ceiling = Some(match self.soft_ceiling {
            Some(sc) => sc.min(here),
            None => here,
        });
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
    fn low_motion_lan_ramps_to_the_ceiling_and_stays() {
        // The regression: a clean LAN where the encoder emits far less than its
        // cap (static screen, or capture starved) — inbound is a trickle but
        // there is zero loss. The controller must NOT read that as starvation.
        let mut c = BitrateController::new(6_000, 600, 50_000);
        for tick in 0..30 {
            let fb = Feedback {
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
        let end = simulate(&mut c, 200_000.0, 8.0, 6);
        assert_eq!(end, 50_000, "LAN should reach the ceiling within ~5 ticks");
    }

    #[test]
    fn backlog_cuts_before_client_report() {
        let mut c = BitrateController::new(20_000, 600, 50_000);
        let fb = Feedback {
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
