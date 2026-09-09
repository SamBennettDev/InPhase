//! `WebRtcBridge` — the seam between the GStreamer graph and WebRTC
//! (architecture report §18: *"Keep this code contained in `WebRtcBridge` so
//! replacing the backend later does not infect the rest of the product."*).
//!
//! One backend: [`webrtcbin`], driven directly, with InPhase owning the encoder
//! chain, SDP and ICE. Congestion control lives in [`crate::media::bitrate`],
//! driven by client telemetry.

mod webrtcbin;

use std::sync::Arc;

use gstreamer as gst;

use inphase_protocol::{SessionConfig, SignalMessage};

use crate::config::Config;
use crate::stats::StatsCollector;

/// Outbound signaling channel back to the client WebSocket. The bridge pushes
/// [`crate::media::MediaSignal`]s; `http::signal` converts + forwards them.
pub type SignalTx = crate::media::MediaSignalTx;

pub struct WebRtcBridge {
    inner: webrtcbin::BinBridge,
}

impl WebRtcBridge {
    pub fn new(
        cfg: &Config,
        session: &SessionConfig,
        stats: Arc<StatsCollector>,
        signal_tx: SignalTx,
        chans: crate::media::pipeline::DataChannelSinks,
    ) -> anyhow::Result<Self> {
        let mut inner = webrtcbin::BinBridge::new(cfg, session, stats, chans)?;
        inner.set_signal_tx(signal_tx);
        Ok(Self { inner })
    }

    pub fn attach_video(
        &self,
        pipeline: &gst::Pipeline,
        upstream: &gst::Element,
    ) -> anyhow::Result<()> {
        self.inner.attach_video(pipeline, upstream)
    }

    pub fn attach_audio(
        &self,
        pipeline: &gst::Pipeline,
        upstream: &gst::Element,
    ) -> anyhow::Result<()> {
        self.inner.attach_audio(pipeline, upstream)
    }

    /// Handle an inbound `answer` / `ice` from the client (§15).
    pub fn on_client_signal(&mut self, msg: &SignalMessage) -> anyhow::Result<()> {
        self.inner.on_client_signal(msg)
    }

    pub fn force_keyframe(&mut self) {
        self.inner.force_keyframe()
    }

    pub fn send_control(&self, text: &str) {
        self.inner.send_control(text)
    }

    /// Start streaming per-frame host stamps onto the control channel (§20).
    pub fn spawn_frame_trace(&self, trace: Arc<crate::media::frametrace::FrameTrace>) {
        self.inner.spawn_frame_trace(trace)
    }
}
