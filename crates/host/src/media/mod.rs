//! The media session: one GStreamer pipeline + its WebRTC consumer
//! (architecture report §3.1 `MediaSession`, §18 "Media pipeline construction").
//!
//! Lifecycle discipline the report requires:
//! * created **only** after an authenticated player asks to start (§3.1);
//! * exactly one pipeline (`SessionManager` guarantees one player, §15);
//! * destroyed **before** the session returns to IDLE (§15);
//! * idle cost near zero — no capture/encode when no player (§29 gate).
//!
//! Non-Windows builds get an inert stub so the rest of the host can be
//! `cargo check`ed on Linux during development.

pub mod bitrate;
pub mod encoder_policy;
pub mod frametrace;
// Route-adaptive resolution. Platform-neutral pure control law, so its tests
// run on the dev machine rather than only on the Windows target.
// WebTransport video transport (ADR-0011 P1): platform-neutral tokio code,
// built everywhere so its loopback tests run on the dev machine too.
pub mod wt;

#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod webrtc;

use std::sync::Arc;

use inphase_protocol::{IceCandidate, SessionConfig, SignalMessage};

use crate::config::Config;
use crate::stats::StatsCollector;

/// Signaling events the media layer emits back toward the client WebSocket.
#[derive(Debug, Clone)]
pub enum MediaSignal {
    Offer(String),
    Answer(String),
    Ice(IceCandidate),
    /// Peer + data channels are up (§15 `session_ready`).
    Ready,
    /// Fatal media error with a code the client can act on.
    Failed(inphase_protocol::SignalError),
}

/// Outbound signaling channel from the media layer back to the client
/// WebSocket ([`crate::http::signal`] owns the receiver).
pub type MediaSignalTx = tokio::sync::mpsc::UnboundedSender<MediaSignal>;
/// Raw bytes from the unreliable `input` data channel (§12.2).
pub type InputBytesTx = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;
/// JSON strings from the reliable `control` data channel (§12.2).
pub type ControlTextTx = tokio::sync::mpsc::UnboundedSender<String>;

impl From<MediaSignal> for Option<SignalMessage> {
    fn from(s: MediaSignal) -> Self {
        Some(match s {
            MediaSignal::Offer(sdp) => SignalMessage::Offer { sdp },
            MediaSignal::Answer(sdp) => SignalMessage::Answer { sdp },
            MediaSignal::Ice(c) => SignalMessage::Ice(c),
            MediaSignal::Ready => SignalMessage::SessionReady,
            MediaSignal::Failed(e) => SignalMessage::Error(e),
        })
    }
}

/// Owns the pipeline for one player.
pub struct MediaSession {
    #[cfg_attr(not(windows), allow(dead_code))]
    cfg: Arc<Config>,
    stats: Arc<StatsCollector>,
    signal_tx: Option<MediaSignalTx>,
    input_tx: Option<InputBytesTx>,
    control_tx: Option<ControlTextTx>,
    /// WebTransport video transport (ADR-0011). `None` = not offered; the
    /// WebRTC path is the only video carrier then.
    #[cfg_attr(not(windows), allow(dead_code))]
    wt: Option<Arc<crate::media::wt::WtVideoTransport>>,
    #[cfg(windows)]
    inner: Option<pipeline::Pipeline>,
    #[cfg(not(windows))]
    started: bool,
}

impl MediaSession {
    pub fn new(cfg: Arc<Config>, stats: Arc<StatsCollector>) -> Self {
        Self {
            cfg,
            stats,
            signal_tx: None,
            input_tx: None,
            control_tx: None,
            wt: None,
            #[cfg(windows)]
            inner: None,
            #[cfg(not(windows))]
            started: false,
        }
    }

    /// Wire the channel the pipeline's WebRTC bridge pushes offer/ice/ready on.
    /// Must be called before [`Self::configure`].
    pub fn set_signal_sink(&mut self, tx: MediaSignalTx) {
        self.signal_tx = Some(tx);
    }

    /// Fire the once-per-session Ready signal because the WT video path is
    /// established (ADR-0011: WT carries the video; WebRTC ICE gates only
    /// audio). The signal layer's `mark_ready` deduplicates, so a later
    /// ICE Connected is a harmless no-op.
    pub fn signal_ready(&self) {
        if let Some(tx) = &self.signal_tx {
            let _ = tx.send(MediaSignal::Ready);
        }
    }

    /// Wire where `input` data-channel bytes go (§12). Before [`Self::configure`].
    pub fn set_input_sink(&mut self, tx: InputBytesTx) {
        self.input_tx = Some(tx);
    }

    /// Wire where `control` data-channel JSON goes (§12). Before [`Self::configure`].
    pub fn set_control_sink(&mut self, tx: ControlTextTx) {
        self.control_tx = Some(tx);
    }

    /// Offer the WebTransport video transport to this session (ADR-0011).
    /// Must be called before [`Self::configure`]. On `None` (or on the WebRTC
    /// fallback) nothing changes — the WT path is additive.
    pub fn set_wt_transport(&mut self, wt: Option<Arc<crate::media::wt::WtVideoTransport>>) {
        self.wt = wt;
    }

    /// Build (but do not start) the pipeline for the negotiated `SessionConfig`
    /// (§18). Called on entry to NEGOTIATING.
    pub fn configure(&mut self, session: &SessionConfig) -> anyhow::Result<()> {
        // Hand the negotiated config to any WT video client (ADR-0011): the
        // client's WebCodecs decoder configures from this right after auth and
        // cannot decode a frame until it has. Cheap even when `wt` is `None`.
        if let Some(wt) = &self.wt {
            wt.set_video_config(
                wt_codec_str(session.codec),
                session.width,
                session.height,
                session.fps,
                session.start_bitrate_kbps,
            );
        }
        #[cfg(windows)]
        {
            let tx = self.signal_tx.clone().ok_or_else(|| {
                anyhow::anyhow!("set_signal_sink() must be called before configure()")
            })?;
            let chans = pipeline::DataChannelSinks {
                input: self.input_tx.clone(),
                control: self.control_tx.clone(),
            };
            let p = pipeline::Pipeline::build(
                &self.cfg,
                self.stats.clone(),
                session,
                tx,
                chans,
                self.wt.clone(),
            )?;
            self.inner = Some(p);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = session;
            anyhow::bail!("media pipeline is Windows-only");
        }
    }

    /// Start capture → encode → WebRTC. Idempotent.
    pub fn start(&mut self) -> anyhow::Result<()> {
        // Anchors the health rules' notion of "how long has this been up",
        // which is what separates "still starting" from "broken".
        self.stats.mark_session_start();
        #[cfg(windows)]
        {
            self.inner
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("configure() not called"))?
                .start()
        }
        #[cfg(not(windows))]
        {
            self.started = true;
            Ok(())
        }
    }

    /// Feed a client SDP answer / trickled ICE candidate into the pipeline's
    /// WebRTC element.
    pub fn on_client_signal(&mut self, msg: &SignalMessage) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            if let Some(p) = self.inner.as_mut() {
                p.on_client_signal(msg)?;
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = msg;
            Ok(())
        }
    }

    /// Send a JSON string on the reliable `control` data channel (§12.2) —
    /// used for `pong` replies so the client can measure data-path RTT.
    pub fn send_control(&self, text: &str) {
        #[cfg(windows)]
        if let Some(p) = self.inner.as_ref() {
            p.send_control(text);
        }
        #[cfg(not(windows))]
        let _ = text;
    }

    /// Ask the encoder for a fresh keyframe (§22 "request fresh keyframe" on
    /// DXGI recovery / codec renegotiation).
    pub fn request_keyframe(&mut self) {
        #[cfg(windows)]
        if let Some(p) = self.inner.as_mut() {
            p.force_keyframe();
        }
    }

    /// Tear down the pipeline. Idempotent — safe to call from `Drop`, STOPPING,
    /// and error paths (§15).
    pub fn stop(&mut self) {
        #[cfg(windows)]
        {
            if let Some(mut p) = self.inner.take() {
                p.stop();
            }
        }
        #[cfg(not(windows))]
        {
            self.started = false;
        }
        self.stats.reset_session();
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// WebCodecs codec string for the WT video path (ADR-0011). The encoder tap
/// carries GStreamer's Annex-B byte stream, so HEVC must be `hev1` (in-band
/// parameter sets allowed) rather than `hvc1` (which requires an out-of-band
/// HVCC description); H.264 baseline is valid description-less.
fn wt_codec_str(codec: inphase_protocol::VideoCodec) -> &'static str {
    match codec {
        // High profile (pinned in the encoder caps), level 5.1. The level in a
        // codec string is a ceiling the decoder must be able to meet, so
        // declaring it generously is safe while declaring it short is not -
        // `avc1.42E01E` claimed Baseline 3.0, which does not cover 1080p60.
        inphase_protocol::VideoCodec::H264 => "avc1.640033",
        inphase_protocol::VideoCodec::H265 => "hev1.1.6.L153.B0",
    }
}
