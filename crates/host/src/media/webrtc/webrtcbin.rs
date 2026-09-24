//! Direct `webrtcbin` backend: InPhase owns the encoder chain, SDP
//! negotiation and ICE.
//!
//! Video carries no RTP since ADR-0011: the encoder tail is capped by a
//! capsfilter (hvc1/avc, au-aligned) into fakesink, and the encoder feeds
//! the WebTransport tap directly (crates/host/src/media/pipeline.rs).
//! webrtcbin carries audio + the `input`/`control` data channels only.
//! ```
//!
//! ICE policy (§8.1): `turn-server` is always cleared (InPhase runs no
//! relay). `stun-server` is cleared in strict-LAN mode (host + VPN candidates
//! only) and set to InPhase's own STUN URI otherwise. The host is the
//! **offerer**.
//!
//! Closures capture only cheap `Send + Sync` handles (the [`SignalTx`] mpsc
//! sender), never a back-reference to the bridge.

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{anyhow, Context};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp::SDPMessage;
use gstreamer_webrtc::{
    WebRTCDataChannel, WebRTCICEConnectionState, WebRTCSDPType, WebRTCSessionDescription,
};
use tracing::{debug, info, warn};

use inphase_protocol::{
    IceCandidate, SessionConfig, SignalError, SignalErrorCode, SignalMessage, VideoCodec,
};

use crate::config::Config;
use crate::media::encoder_policy::{self, GpuVendor};
use crate::media::pipeline::DataChannelSinks;
use crate::media::webrtc::SignalTx;
use crate::media::MediaSignal;
use crate::stats::StatsCollector;

pub struct BinBridge {
    session: SessionConfig,
    vendor: GpuVendor,
    stats: Arc<StatsCollector>,
    strict_lan: bool,
    /// STUN URI for server-reflexive candidates when not strict-LAN.
    stun_server: Option<String>,
    chans: DataChannelSinks,
    webrtcbin: Mutex<Option<gst::Element>>,
    encoder: Mutex<Option<gst::Element>>,
    signal_tx: Mutex<Option<SignalTx>>,
    /// Keep the data-channel wrappers alive for the session's lifetime.
    data_channels: Arc<Mutex<Vec<WebRTCDataChannel>>>,
    /// The `control` channel, for host-initiated sends (pong replies).
    control_channel: Arc<Mutex<Option<WebRTCDataChannel>>>,
    /// Set on drop to stop the frame-trace sender thread.
    frametrace_stop: Arc<std::sync::atomic::AtomicBool>,
}

impl BinBridge {
    pub fn new(
        cfg: &Config,
        session: &SessionConfig,
        stats: Arc<StatsCollector>,
        chans: DataChannelSinks,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session: session.clone(),
            vendor: crate::platform::primary_gpu_vendor().unwrap_or(GpuVendor::Nvidia),
            stats,
            strict_lan: cfg.network.strict_lan,
            stun_server: (!cfg.network.strict_lan)
                .then(|| cfg.network.stun_servers.first().cloned())
                .flatten(),
            chans,
            webrtcbin: Mutex::new(None),
            encoder: Mutex::new(None),
            signal_tx: Mutex::new(None),
            data_channels: Arc::new(Mutex::new(Vec::new())),
            control_channel: Arc::new(Mutex::new(None)),
            frametrace_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn set_signal_tx(&mut self, tx: SignalTx) {
        *self.signal_tx.lock().unwrap() = Some(tx);
    }

    fn tx(&self) -> Option<SignalTx> {
        self.signal_tx.lock().unwrap().clone()
    }

    pub fn attach_video(
        &self,
        pipeline: &gst::Pipeline,
        upstream: &gst::Element,
    ) -> anyhow::Result<()> {
        let codec = self.session.codec;
        // `webrtcbin` has no built-in bandwidth estimation. The encoder
        // bitrate starts at whatever the client asked for (clamped to
        // 2–120 Mbps in `build_config`); `crate::media::bitrate` adapts it
        // from client telemetry after that.
        let bitrate_kbps = self.session.start_bitrate_kbps.clamp(2_000, 120_000);

        let enc_name = encoder_policy::element_for(self.vendor, codec)
            .ok_or_else(|| anyhow!("no encoder element for {:?}/{:?}", self.vendor, codec))?;
        if gst::ElementFactory::find(enc_name).is_none() {
            return Err(anyhow!(
                "hardware encoder `{enc_name}` not in the GStreamer registry - \
                 InPhase does not fall back to a CPU encoder (sec 6)"
            ));
        }
        let encoder = gst::ElementFactory::make(enc_name)
            .name("venc")
            .build()
            .with_context(|| format!("create {enc_name}"))?;
        crate::media::pipeline::configure_encoder(&encoder, self.vendor, codec, bitrate_kbps);

        // Annex-B byte stream, AU-aligned: parameter sets in-band, no
        // out-of-band description, nothing for the tap to synthesize.
        //
        // This was `hvc1`/`avc` - length-prefixed samples with `codec_data` in
        // the caps - which forced the client's decoder config to carry a
        // matching HVCC description, and coupled two things that then drifted
        // apart repeatedly. The description was built by hand from the SPS and
        // got the profile block wrong (it declared level 0, and Safari accepts
        // such a config and silently never emits a frame); deleting the
        // description then left the decoder in Annex-B mode while the encoder
        // still emitted length-prefixed samples, which is worse. A probe
        // capture confirmed the mismatch: 8.6 MB with one start code in it and
        // NAL lengths that chain perfectly.
        //
        // WebCodecs reads an absent `description` as "Annex-B, parameter sets
        // in-band" (the `hev1.*` codec string this host advertises), so making
        // the encoder emit exactly that removes the coupling instead of
        // maintaining it. It is also what the architecture note prescribes (§5).
        // The profile is pinned rather than left to the encoder's default: the
        // codec string the client is told (media::wt_codec_str) has to describe
        // this bitstream, and an unpinned profile is exactly the drift that
        // produced the level-0 HVCC bug above. Pin it here, declare it there.
        let (media, stream_format, profile) = match codec {
            VideoCodec::H264 => ("video/x-h264", "byte-stream", "high"),
            VideoCodec::H265 => ("video/x-h265", "byte-stream", "main"),
        };
        let venc_caps = gst::ElementFactory::make("capsfilter")
            .name("venc-caps")
            .property(
                "caps",
                gst::Caps::builder(media)
                    .field("stream-format", stream_format)
                    .field("alignment", "au")
                    .field("profile", profile)
                    .build(),
            )
            .build()?;
        // ADR-0011, final form: WebTransport is the ONLY video path. The
        // encoder feeds the WT tap directly; there is no RTP payloader, no
        // video transceiver, no duplicate stream. `webrtcbin` stays purely as
        // the audio + input-datachannel + signaling carrier. The fakesink
        // keeps the encoder's src pad linked (an unlinked encoder pad returns
        // NOT_LINKED and stalls the graph); sync=false drains it instantly.
        let video_sink = gst::ElementFactory::make("fakesink")
            .name("venc-sink")
            .property("sync", false)
            .property("async", false)
            .build()?;
        let webrtcbin = self.make_webrtcbin()?;

        pipeline.add_many([&encoder, &venc_caps, &video_sink, &webrtcbin])?;
        gst::Element::link_many([upstream, &encoder, &venc_caps, &video_sink])?;

        // Data channels are created lazily on the first `on-negotiation-needed`
        // (see `make_webrtcbin`), by which point the bin is live and
        // `create-data-channel` yields a real object (§12.2).

        *self.encoder.lock().unwrap() = Some(encoder);
        *self.webrtcbin.lock().unwrap() = Some(webrtcbin);
        Ok(())
    }

    pub fn attach_audio(
        &self,
        pipeline: &gst::Pipeline,
        upstream: &gst::Element,
    ) -> anyhow::Result<()> {
        let webrtcbin = self
            .webrtcbin
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("attach_video must run before attach_audio"))?;
        let pay = gst::ElementFactory::make("rtpopuspay")
            .property("pt", 111u32)
            .build()?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                // Chrome requires OPUS/48000/2 in the SDP (RFC 7587).
                "application/x-rtp,media=audio,encoding-name=OPUS,payload=111,clock-rate=48000,channels=2"
                    .parse::<gst::Caps>()?,
            )
            .build()?;
        pipeline.add_many([&pay, &caps])?;
        gst::Element::link_many([upstream, &pay, &caps])?;
        let sinkpad = webrtcbin
            .request_pad_simple("sink_%u")
            .ok_or_else(|| anyhow!("webrtcbin audio pad"))?;
        caps.static_pad("src").expect("caps src").link(&sinkpad)?;
        // Chrome rejects audio offers that include RTX/NACK SSRCs — video only.
        set_transceiver_sendonly(&sinkpad);
        Ok(())
    }

    fn make_webrtcbin(&self) -> anyhow::Result<gst::Element> {
        let wb = gst::ElementFactory::make("webrtcbin")
            .name("inphase-webrtc")
            .property_from_str("bundle-policy", "max-bundle")
            .build()
            .context("create webrtcbin (is the gstwebrtc plugin present?)")?;

        // §8.1 / §10 network policy. TURN is never configured (no InPhase relay).
        // strict-LAN: clear the library's default public STUN too — host + VPN
        // candidates only. Otherwise use InPhase's own STUN for
        // server-reflexive candidates; never fall back to a third-party STUN.
        wb.set_property("turn-server", None::<String>);
        match (self.strict_lan, self.stun_server.as_deref()) {
            (false, Some(uri)) => {
                wb.set_property("stun-server", uri);
                tracing::info!(stun = %uri, "webrtcbin: STUN enabled");
            }
            _ => wb.set_property("stun-server", None::<String>),
        }

        let tx = self.tx();

        // on-negotiation-needed -> (first time) create the data channels while
        // the bin is live so `create-data-channel` yields a real object and the
        // SCTP m-line lands in the offer, then create + send the offer.
        {
            let tx = tx.clone();
            let chans = self.chans.clone();
            let held = self.data_channels.clone();
            let control_slot = self.control_channel.clone();
            wb.connect_closure(
                "on-negotiation-needed",
                false,
                glib::closure!(move |wb: &gst::Element| {
                    if held.lock().unwrap().is_empty() {
                        match build_data_channels(wb, &chans) {
                            Ok(dcs) => {
                                // build_data_channels yields [input, control].
                                *control_slot.lock().unwrap() = dcs.get(1).cloned();
                                *held.lock().unwrap() = dcs;
                            }
                            Err(e) => warn!("data channels: {e:#}"),
                        }
                    }
                    negotiate(wb, tx.clone());
                }),
            );
        }

        // on-ice-candidate -> forward to client.
        {
            let tx = tx.clone();
            wb.connect_closure(
                "on-ice-candidate",
                false,
                glib::closure!(move |_wb: &gst::Element, mline: u32, cand: String| {
                    if let Some(tx) = &tx {
                        let _ = tx.send(MediaSignal::Ice(IceCandidate {
                            candidate: cand,
                            sdp_mid: None,
                            sdp_mline_index: Some(mline),
                        }));
                    }
                }),
            );
        }

        // ICE connection state -> session readiness / failure.
        {
            let tx = tx.clone();
            wb.connect_notify(Some("ice-connection-state"), move |wb, _| {
                let state = wb.property::<WebRTCICEConnectionState>("ice-connection-state");
                debug!(?state, "ice-connection-state");
                let Some(tx) = &tx else { return };
                match state {
                    WebRTCICEConnectionState::Connected | WebRTCICEConnectionState::Completed => {
                        let _ = tx.send(MediaSignal::Ready);
                    }
                    WebRTCICEConnectionState::Failed => {
                        let _ = tx.send(MediaSignal::Failed(SignalError::new(
                            SignalErrorCode::NegotiationFailed,
                            "ICE failed",
                        )));
                    }
                    _ => {}
                }
            });
        }

        Ok(wb)
    }

    pub fn on_client_signal(&mut self, msg: &SignalMessage) -> anyhow::Result<()> {
        let wb = self
            .webrtcbin
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("webrtcbin not created yet"))?;
        match msg {
            SignalMessage::Answer { sdp } => {
                let sdp = SDPMessage::parse_buffer(sdp.as_bytes())
                    .map_err(|_| anyhow!("invalid answer SDP"))?;
                let answer = WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp);
                wb.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
                info!("applied client answer");
            }
            SignalMessage::Ice(c) if !c.candidate.is_empty() => {
                wb.emit_by_name::<()>(
                    "add-ice-candidate",
                    &[&c.sdp_mline_index.unwrap_or(0), &c.candidate],
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub fn force_keyframe(&mut self) {
        if let Some(enc) = self.encoder.lock().unwrap().as_ref() {
            if let Some(pad) = enc.static_pad("sink") {
                let ev = gst::event::CustomUpstream::new(
                    gst::Structure::builder("GstForceKeyUnit")
                        .field("all-headers", true)
                        .build(),
                );
                pad.push_event(ev);
            }
        }
    }

    pub fn send_control(&self, text: &str) {
        if let Some(ch) = self.control_channel.lock().unwrap().as_ref() {
            let state = ch.property::<gstreamer_webrtc::WebRTCDataChannelState>("ready-state");
            if state == gstreamer_webrtc::WebRTCDataChannelState::Open {
                ch.emit_by_name::<()>("send-string", &[&text]);
            }
        }
    }

    /// Drain [`FrameTrace`] onto the control channel ~4×/s (architecture §20).
    /// Also folds the batch's encode timings into the host stats, so the
    /// dashboard's `encode_ms_p50/p95` reflects measured frames, not zeroes.
    pub fn spawn_frame_trace(&self, trace: Arc<crate::media::frametrace::FrameTrace>) {
        use std::sync::atomic::Ordering;
        let ch = self.control_channel.clone();
        let stats = self.stats.clone();
        let stop = self.frametrace_stop.clone();
        std::thread::Builder::new()
            .name("inphase-frametrace".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let (host_now_us, frames) = trace.drain();
                    if frames.is_empty() {
                        continue;
                    }
                    tracing::info!(
                        target: "inphase_host::frametrace",
                        "{}",
                        crate::media::frametrace::FrameTrace::summary(&frames)
                    );
                    let (enc_p50, enc_p95, _c2s_p95) =
                        crate::media::frametrace::FrameTrace::encode_percentiles(&frames);
                    stats.update_host(|h| {
                        h.encode_ms_p50 = enc_p50;
                        h.encode_ms_p95 = enc_p95;
                    });
                    let json = crate::media::frametrace::FrameTrace::to_json(host_now_us, &frames);
                    if let Some(c) = ch.lock().unwrap().as_ref() {
                        let st =
                            c.property::<gstreamer_webrtc::WebRTCDataChannelState>("ready-state");
                        if st == gstreamer_webrtc::WebRTCDataChannelState::Open {
                            c.emit_by_name::<()>("send-string", &[&json]);
                        }
                    }
                }
            })
            .ok();
    }
}

impl Drop for BinBridge {
    fn drop(&mut self) {
        self.frametrace_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// RTP payload size (bytes). Kept well under a WireGuard tunnel's 1280-byte MTU:
fn set_transceiver_sendonly(sinkpad: &gst::Pad) {
    // The `transceiver` property is populated once the pad is requested; it may
    // be null very early, so tolerate that.
    if let Some(t) = sinkpad.property::<Option<glib::Object>>("transceiver") {
        t.set_property_from_str("direction", "sendonly");
        // Ask for generic NACK + RTX retransmission on *video* only. Chrome
        // rejects audio offers that include an RTX SSRC (gstwebrtc #540).
        // No NACK/RTX request: video left webrtcbin with ADR-0011, and
        // Chrome rejects audio offers that carry an RTX SSRC (gstwebrtc
        // #540) - so for audio-only this must stay off.
    }
}

/// Host-side `create-offer` → `set-local-description` → emit offer to client.
fn negotiate(wb: &gst::Element, tx: Option<SignalTx>) {
    let wb = wb.clone();
    let wb_for_promise = wb.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let desc = match reply {
            Ok(Some(s)) => s
                .value("offer")
                .ok()
                .and_then(|v| v.get::<WebRTCSessionDescription>().ok()),
            _ => None,
        };
        let Some(offer) = desc else {
            warn!("create-offer produced no description");
            return;
        };
        wb_for_promise
            .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
        let sdp = offer.sdp().as_text().unwrap_or_default();
        {
            let fb: Vec<&str> = sdp
                .lines()
                .filter(|l| {
                    l.starts_with("a=rtcp-fb")
                        || l.starts_with("a=rtpmap")
                        || l.contains("rtx")
                        || l.contains("transport-cc")
                })
                .collect();
            info!(target: "inphase_host::media", "offer feedback lines: {}", fb.join(" | "));
        }
        if let Some(tx) = &tx {
            let _ = tx.send(MediaSignal::Offer(sdp));
        }
    });
    wb.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
}

/// Create the two data channels the report specifies (§12.2) and wire their
/// inbound traffic:
/// * `input`   — `ordered=false, maxRetransmits=0`; binary -> [`InputBytesTx`];
/// * `control` — reliable/ordered; string JSON -> [`ControlTextTx`].
///
/// Called from the first `on-negotiation-needed`, when the bin is live.
fn build_data_channels(
    wb: &gst::Element,
    chans: &DataChannelSinks,
) -> anyhow::Result<Vec<WebRTCDataChannel>> {
    let input_opts = gst::Structure::builder("config")
        .field("ordered", false)
        .field("max-retransmits", 0i32)
        .build();
    let input_ch = emit_data_channel(wb, "input", Some(&input_opts))?;
    let control_ch = emit_data_channel(wb, "control", None)?;

    if let Some(tx) = chans.input.clone() {
        input_ch.connect_on_message_data(move |_ch, data| {
            if let Some(bytes) = data {
                let _ = tx.send(bytes.to_vec());
            }
        });
    }
    input_ch.connect_on_open(|_| info!("data channel `input` open"));

    if let Some(tx) = chans.control.clone() {
        control_ch.connect_on_message_string(move |_ch, msg| {
            if let Some(s) = msg {
                let _ = tx.send(s.to_string());
            }
        });
    }
    control_ch.connect_on_open(|_| info!("data channel `control` open"));

    Ok(vec![input_ch, control_ch])
}

/// `webrtcbin`'s `create-data-channel` action returns a `GstWebRTCDataChannel`
/// whose GType handle does not line up with `emit_by_name`'s strict
/// closure-return check (it panics with "expected X, got X", even for
/// `glib::Object`). Use `emit_by_name_with_values`, which skips that check, and
/// pull the object out of the raw `Value` — the approach gst-plugins-rs uses.
fn emit_data_channel(
    wb: &gst::Element,
    label: &str,
    opts: Option<&gst::Structure>,
) -> anyhow::Result<WebRTCDataChannel> {
    let opts_value = match opts {
        Some(s) => s.to_value(),
        None => glib::Value::from_type(gst::Structure::static_type()),
    };
    let ret = wb
        .emit_by_name_with_values("create-data-channel", &[label.to_value(), opts_value])
        .ok_or_else(|| anyhow!("create-data-channel `{label}` returned no value"))?;
    ret.get::<WebRTCDataChannel>()
        .map_err(|e| anyhow!("create-data-channel `{label}`: {e}"))
}
