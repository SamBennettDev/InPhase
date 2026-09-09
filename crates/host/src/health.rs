//! Does the stream actually work? A verdict, not a pile of numbers.
//!
//! Every failure on 2026-09-07 was found by reading logs after the fact: the
//! host died with no console, keyframe requests were rejected on every attempt,
//! iOS drew nothing without erroring, telemetry was refused at the door for
//! hours. In each case the host held enough information to say what was wrong
//! at the time, and never said it. The dashboard showed metrics, but reading
//! "presented_fps: 0" as *"this session is broken right now"* was left to a
//! human who was not looking.
//!
//! So this module names the failure instead. [`assess`] is a pure function from
//! a stats snapshot to a list of [`Symptom`]s, each with a code, a severity and
//! a sentence a person can act on. `/api/v1/health` serves it, and the host
//! logs every transition, so a broken session announces itself in `host.log`
//! at the moment it breaks.
//!
//! Pure and side-effect free so the rules are unit-testable against recorded
//! shapes - including the exact ones from that night.

use serde::Serialize;

/// How bad a symptom is. `Critical` means the user is looking at a broken
/// stream right now; `Warn` means degraded but working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warn,
    Critical,
}

/// One named thing that is wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symptom {
    /// Stable machine-readable identifier, e.g. `decoding-not-presenting`.
    pub code: &'static str,
    pub severity: Severity,
    /// A sentence naming the likely cause, written for whoever is debugging at
    /// 2 a.m. - not a restatement of the metric.
    pub detail: String,
}

/// Overall verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    /// True when no symptom is present.
    pub ok: bool,
    /// Worst severity present, if any.
    pub severity: Option<Severity>,
    pub symptoms: Vec<Symptom>,
}

impl Health {
    /// One-line summary for a log line.
    pub fn summary(&self) -> String {
        if self.ok {
            return "ok".into();
        }
        self.symptoms
            .iter()
            .map(|s| s.code)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Everything the rules need, gathered by the caller so [`assess`] stays pure.
#[derive(Debug, Clone, Default)]
pub struct HealthInput {
    /// A player session is live (negotiating counts as not yet live).
    pub session_active: bool,
    /// Seconds since the session became active. Rules that need the pipeline to
    /// have had a chance to start are gated on this.
    pub session_age_secs: f32,
    /// Frames per second coming out of the capture source.
    pub capture_fps: f32,
    /// Encoded access units per second (post-videorate).
    pub encoded_fps: f32,
    /// Seconds since the last client telemetry message, if any has ever arrived.
    pub telemetry_age_secs: Option<f32>,
    /// Client's decoder output rate.
    pub decoded_fps: f32,
    /// Rate the client actually paints. Zero while decoding is the iOS
    /// render no-op signature.
    pub presented_fps: f32,
    /// Negotiated encode resolution.
    pub width: u32,
    pub height: u32,
    /// Fragment loss the client reports, as a percentage.
    pub route_loss_pct: f32,
    /// Frames that arrived but never assembled, last window.
    pub incomplete_frames: u64,
    pub audio_health: crate::stats::AudioHealth,
}

/// Grace period before a silent client is called broken. Long enough to cover a
/// reconnect and a slow first keyframe, short enough to catch the session while
/// the user is still looking at it.
const TELEMETRY_SILENCE_SECS: f32 = 8.0;
/// The pipeline gets this long to produce its first frames before capture or
/// decode rules fire.
const STARTUP_GRACE_SECS: f32 = 6.0;

/// Turn a snapshot into a verdict. Ordered most- to least-urgent.
pub fn assess(i: &HealthInput) -> Health {
    let mut symptoms = Vec::new();

    if i.session_active {
        // --- the host's own capture ---------------------------------------
        // videorate repeats the last frame at the target fps, so a static
        // desktop legitimately shows capture_fps == 0 while the encoder keeps
        // producing duplicated frames. Call it stalled only when the ENCODER
        // output is dead too - that is the client-visible signal.
        if i.session_age_secs > STARTUP_GRACE_SECS && i.capture_fps <= 0.0 && i.encoded_fps <= 0.0 {
            symptoms.push(Symptom {
                code: "capture-stalled",
                severity: Severity::Critical,
                detail: "A session is live but the capture source is producing no frames. \
                         The desktop-duplication source has stalled; nothing downstream can \
                         recover without restarting the session."
                    .into(),
            });
        }

        // --- is the client talking to us at all? ---------------------------
        match i.telemetry_age_secs {
            None if i.session_age_secs > TELEMETRY_SILENCE_SECS => {
                symptoms.push(Symptom {
                    code: "client-never-reported",
                    severity: Severity::Critical,
                    detail: format!(
                        "A client has been connected {:.0}s and has never sent telemetry. \
                         It is most likely running a bundle built against a different host \
                         (check build_id in /api/v1/status) or its control messages are being \
                         rejected before they are handled.",
                        i.session_age_secs
                    ),
                });
            }
            Some(age) if age > TELEMETRY_SILENCE_SECS => {
                symptoms.push(Symptom {
                    code: "client-silent",
                    severity: Severity::Critical,
                    detail: format!(
                        "No client telemetry for {age:.0}s on a live session. The page is \
                         wedged, backgrounded, or was updated out from under the host."
                    ),
                });
            }
            // Fresh telemetry: the pixel rules below are meaningful.
            Some(_) => {
                // The failure this whole module exists to shout about.
                if i.decoded_fps > 0.0 && i.presented_fps <= 0.0 {
                    symptoms.push(Symptom {
                        code: "decoding-not-presenting",
                        severity: Severity::Critical,
                        detail: format!(
                            "The client is decoding {:.0} fps but presenting none - frames are \
                             arriving and decoding fine, and nothing is reaching the screen. \
                             This is a client-side render failure, not a network problem \
                             (iOS Safari drawing no VideoFrame is the known instance).",
                            i.decoded_fps
                        ),
                    });
                } else if i.decoded_fps <= 0.0
                    && i.capture_fps > 0.0
                    && i.session_age_secs > STARTUP_GRACE_SECS
                {
                    // `incomplete_frames > 0` turns a guess into a diagnosis:
                    // datagrams are arriving and frames are not assembling, so
                    // this is fragment loss rather than a dead transport.
                    let cause = if i.incomplete_frames > 0 {
                        format!(
                            "{} frames arrived in pieces and never assembled in the last window, \
                             at {:.1}% fragment loss - the route cannot complete a keyframe at \
                             this resolution",
                            i.incomplete_frames, i.route_loss_pct
                        )
                    } else {
                        "no fragments are arriving at all - the video datagrams are not reaching \
                         the client (firewall, or the transport is down)"
                            .to_string()
                    };
                    symptoms.push(Symptom {
                        code: "no-decode",
                        severity: Severity::Critical,
                        detail: format!(
                            "The host is capturing {:.0} fps but the client has decoded nothing: \
                             {cause}. This is the permanent-black signature.",
                            i.capture_fps
                        ),
                    });
                }
            }
            None => {} // still inside the startup grace period
        }

        // --- can this route carry what we are sending? ---------------------
        // The resolution no longer auto-drops (user directive): sustained loss
        // at the user's chosen resolution is a warn, not a silent downgrade.
        if i.route_loss_pct > 15.0 {
            symptoms.push(Symptom {
                code: "route-degraded",
                severity: Severity::Warn,
                detail: format!(
                    "Encoding at {}x{} with {:.1}% fragment loss - the route cannot \
                     reliably carry the chosen bitrate. Lower the quality preset if it \
                     persists; the resolution will not change on its own.",
                    i.width, i.height, i.route_loss_pct
                ),
            });
        }
    }

    // --- audio ------------------------------------------------------------
    match i.audio_health {
        crate::stats::AudioHealth::Failed => symptoms.push(Symptom {
            code: "audio-failed",
            severity: Severity::Warn,
            detail: "The audio branch has failed for this session; video is unaffected. \
                     Recovers on reconnect."
                .into(),
        }),
        crate::stats::AudioHealth::Degraded => symptoms.push(Symptom {
            code: "audio-degraded",
            severity: Severity::Warn,
            detail: "The audio capture device reported an error; audio may be silent.".into(),
        }),
        _ => {}
    }

    let severity = symptoms.iter().map(|s| s.severity).max_by_key(|s| match s {
        Severity::Critical => 1,
        Severity::Warn => 0,
    });
    Health {
        ok: symptoms.is_empty(),
        severity,
        symptoms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::AudioHealth;

    /// A healthy live session, as a base for the failure shapes below.
    fn healthy() -> HealthInput {
        HealthInput {
            session_active: true,
            session_age_secs: 60.0,
            capture_fps: 60.0,
            encoded_fps: 60.0,
            telemetry_age_secs: Some(1.0),
            decoded_fps: 60.0,
            presented_fps: 60.0,
            width: 2560,
            height: 1440,
            route_loss_pct: 0.0,
            incomplete_frames: 0,
            audio_health: AudioHealth::Healthy,
        }
    }

    fn codes(h: &Health) -> Vec<&'static str> {
        h.symptoms.iter().map(|s| s.code).collect()
    }

    #[test]
    fn a_healthy_session_is_silent() {
        let h = assess(&healthy());
        assert!(h.ok, "expected ok, got {:?}", codes(&h));
        assert_eq!(h.summary(), "ok");
    }

    #[test]
    fn an_idle_host_is_healthy() {
        let h = assess(&HealthInput {
            session_active: false,
            ..Default::default()
        });
        assert!(h.ok);
    }

    /// The gap the report called out by name: "clients connected but zero
    /// frames presented" must be said out loud.
    #[test]
    fn decoding_but_not_presenting_is_critical() {
        let h = assess(&HealthInput {
            presented_fps: 0.0,
            ..healthy()
        });
        assert_eq!(codes(&h), ["decoding-not-presenting"]);
        assert_eq!(h.severity, Some(Severity::Critical));
    }

    /// The permanent-black signature: host encoding, client decoding nothing.
    #[test]
    fn host_capturing_with_no_client_decode_is_critical() {
        let h = assess(&HealthInput {
            decoded_fps: 0.0,
            presented_fps: 0.0,
            route_loss_pct: 9.5,
            incomplete_frames: 57,
            ..healthy()
        });
        assert!(codes(&h).contains(&"no-decode"));
        assert!(
            h.symptoms[0].detail.contains("9.5%") && h.symptoms[0].detail.contains("57"),
            "the loss figure and the incomplete-frame count belong in the message: {}",
            h.symptoms[0].detail
        );
    }

    /// Nothing arriving at all is a different fault from arriving-but-lossy,
    /// and the message must say which - one is a firewall, the other a bitrate.
    #[test]
    fn no_fragments_at_all_reads_differently_from_lossy_fragments() {
        let dead = assess(&HealthInput {
            decoded_fps: 0.0,
            presented_fps: 0.0,
            route_loss_pct: 0.0,
            incomplete_frames: 0,
            ..healthy()
        });
        assert!(dead.symptoms[0].detail.contains("not reaching the client"));
        let lossy = assess(&HealthInput {
            decoded_fps: 0.0,
            presented_fps: 0.0,
            route_loss_pct: 12.0,
            incomplete_frames: 40,
            ..healthy()
        });
        assert!(lossy.symptoms[0].detail.contains("never assembled"));
    }

    /// Telemetry refused at the door - hours of silence that nothing flagged.
    #[test]
    fn a_client_that_never_reports_is_critical() {
        let h = assess(&HealthInput {
            telemetry_age_secs: None,
            ..healthy()
        });
        assert_eq!(codes(&h), ["client-never-reported"]);
        assert!(
            h.symptoms[0].detail.contains("build_id"),
            "should point at the likeliest cause"
        );
    }

    #[test]
    fn a_client_that_goes_quiet_is_critical() {
        let h = assess(&HealthInput {
            telemetry_age_secs: Some(30.0),
            ..healthy()
        });
        assert_eq!(codes(&h), ["client-silent"]);
    }

    /// Pixel rules must not fire on a client that has not reported yet, or
    /// every session start would look broken.
    #[test]
    fn silence_suppresses_the_pixel_rules() {
        let h = assess(&HealthInput {
            telemetry_age_secs: Some(30.0),
            decoded_fps: 0.0,
            presented_fps: 0.0,
            ..healthy()
        });
        assert_eq!(
            codes(&h),
            ["client-silent"],
            "a silent client's stale fps readings must not add symptoms"
        );
    }

    #[test]
    fn startup_grace_keeps_a_fresh_session_quiet() {
        let h = assess(&HealthInput {
            session_age_secs: 2.0,
            capture_fps: 0.0,
            telemetry_age_secs: None,
            decoded_fps: 0.0,
            presented_fps: 0.0,
            ..healthy()
        });
        assert!(h.ok, "a 2s-old session must not alarm: {:?}", codes(&h));
    }

    #[test]
    fn stalled_capture_is_critical() {
        // The whole pipeline is dead: no capture AND no encoder output.
        let h = assess(&HealthInput {
            capture_fps: 0.0,
            encoded_fps: 0.0,
            ..healthy()
        });
        assert!(codes(&h).contains(&"capture-stalled"));
    }

    #[test]
    fn static_desktop_with_videorate_output_is_not_stalled() {
        // videorate repeats the last frame at the target fps: a static
        // desktop shows capture_fps == 0 while the encoder still produces
        // duplicated frames. That is healthy video, not a stall.
        let h = assess(&HealthInput {
            capture_fps: 0.0,
            encoded_fps: 60.0,
            ..healthy()
        });
        assert!(!codes(&h).contains(&"capture-stalled"));
    }

    #[test]
    fn a_route_that_cannot_carry_the_resolution_warns() {
        let h = assess(&HealthInput {
            route_loss_pct: 20.0,
            ..healthy()
        });
        assert_eq!(codes(&h), ["route-degraded"]);
        assert_eq!(h.severity, Some(Severity::Warn));
        assert!(h.symptoms[0].detail.contains("2560x1440"));
    }

    #[test]
    fn audio_failure_warns_without_masking_video_health() {
        let h = assess(&HealthInput {
            audio_health: AudioHealth::Failed,
            ..healthy()
        });
        assert_eq!(codes(&h), ["audio-failed"]);
        assert_eq!(h.severity, Some(Severity::Warn));
    }

    #[test]
    fn critical_outranks_warn_in_the_overall_severity() {
        let h = assess(&HealthInput {
            presented_fps: 0.0,
            audio_health: AudioHealth::Failed,
            ..healthy()
        });
        assert_eq!(h.severity, Some(Severity::Critical));
        assert_eq!(h.symptoms.len(), 2);
    }
}
