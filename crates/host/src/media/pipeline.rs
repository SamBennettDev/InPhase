//! GStreamer pipeline construction (architecture report §5, §6, §11, §18).
//!
//! Implementation-shape (not a `gst-launch` string — §18):
//!
//! ```text
//! Video: d3d11screencapturesrc(dxgi|wgc, monitor N)
//!          -> queue(max-buffers=3, leaky=downstream)      // drop stale frames
//!          -> d3d11convert                                // GPU BGRA->NV12 §5.1
//!          -> video/x-raw(memory:D3D11Memory),NV12,W x H @ fps
//!          -> encoder + WT tap (see `media::webrtc`)
//! Audio:   wasapi2src(loopback, low-latency)
//!          -> audioconvert -> audioresample -> 48k stereo
//!          -> opusenc(10ms, ~160k) -> webrtcbin
//! ```
//!
//! Encoder bitrate is adapted from client telemetry — see [`crate::media::bitrate`].

use std::sync::Arc;

use anyhow::Context;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use tracing::{debug, error, info, warn};

use inphase_protocol::{SessionConfig, SignalMessage, VideoCodec};

use crate::config::{CaptureApi, Config};
use crate::media::encoder_policy::{self, GpuVendor};
use crate::media::webrtc::{SignalTx, WebRtcBridge};
use crate::media::{ControlTextTx, InputBytesTx};
use crate::stats::StatsCollector;

/// Tap the encoder output for the WT video path (ADR-0011): a pad probe on the
/// `venc` src pad copies every encoded access unit out of the pipeline —
/// *before* the RTP payloader — and hands it to the transport. Frame numbers
/// are assigned here in send order; `capture_us` comes from the buffer PTS so
/// the client can compute capture→glass like the WebRTC stats do. A missing
/// `venc` is a silent no-op — the WebRTC path keeps working.
///
/// Returns the encoder's PTS offset in µs (output PTS minus input PTS, `i64::MIN`
/// until the first frame): the encoder stamps its output a constant hour
/// ahead of the running time its input was captured at, so a clock that
/// answers in running time must add it to speak the client's `capture_us`.
fn attach_gst_tap(
    pipeline: &gst::Pipeline,
    t: &Arc<crate::media::wt::WtVideoTransport>,
    pts_offset: Arc<std::sync::atomic::AtomicI64>,
) -> Arc<std::sync::atomic::AtomicI64> {
    use std::sync::atomic::{AtomicU32, Ordering};

    let Some(enc) = pipeline.by_name("venc") else {
        debug!("wt: no `venc` in the pipeline — video tap not attached");
        return pts_offset;
    };
    let Some(src) = enc.static_pad("src") else {
        debug!("wt: `venc` has no src pad — video tap not attached");
        return pts_offset;
    };
    // Input PTS in arrival order. Zero-latency NVENC emits one frame per input
    // in order (no B-frames), so the front of this queue is the input of the
    // frame leaving the encoder.
    let inputs = Arc::new(parking_lot::Mutex::new(
        std::collections::VecDeque::<u64>::new(),
    ));
    if let Some(sink) = enc.static_pad("sink") {
        let inputs = inputs.clone();
        sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                if let Some(p) = b.pts() {
                    let mut q = inputs.lock();
                    q.push_back(p.useconds());
                    while q.len() > 16 {
                        q.pop_front();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    let offset_out = pts_offset.clone();
    let t = t.clone();
    src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
            if let (Some(out), Some(inp)) = (b.pts(), inputs.lock().pop_front()) {
                let off = out.useconds() as i64 - inp as i64;
                if offset_out.swap(off, Ordering::Relaxed) == i64::MIN {
                    info!(offset_us = off, "wt: encoder PTS offset measured");
                }
            }
            // Numbering belongs to the transport: a pipeline restart must not
            // reset it, or the previous pipeline's in-flight frames arrive on the
            // new session looking like the future.
            let frame_no = t.next_frame_no();
            let capture_us = b.pts().map(|p| p.nseconds() / 1000).unwrap_or(0);
            // DELTA_UNIT set = predicted frame; unset = keyframe.
            let key = !b.flags().contains(gst::BufferFlags::DELTA_UNIT);
            if let Ok(map) = b.map_readable() {
                // One codec config per session, and it never changes.
                //
                // This used to harvest an HVCC/AVCC description off the first
                // frames and push a *second* config upgrading the client from
                // hev1 + Annex-B to hvc1 + length-prefixed samples. Every H.265
                // session in the log died exactly there: epoch 1 acked, epoch 2
                // acked ~200 ms later, decoded_fps 0 from that moment on,
                // keyframe requests every four seconds until the user gave up.
                //
                // The upgrade existed for a note about Chrome refusing
                // description-less HEVC. WebCodecs treats an absent
                // `description` as "Annex-B, parameter sets in-band", which is
                // what this encoder emits and what Safari - the actual client -
                // decodes natively. So the upgrade bought nothing and cost the
                // stream, along with a synthesized HVCC record, an
                // Annex-B-to-length-prefix rewrite of every payload, a second
                // config epoch, and a mid-stream decoder reset on every session.
                //
                // All of that is gone. The payload goes out exactly as the
                // encoder produced it.
                let payload = map.as_slice().to_vec();
                // The client half of this conversation is already logged
                // ("client requested a keyframe"). Without the encoder half, a
                // session that begs for IDRs and stays black cannot be told
                // apart from one whose IDRs never leave the host: an encoder
                // that quietly ignores ForceKeyUnit looks identical from every
                // other angle - frames flow, loss is zero, the client decodes
                // nothing.
                if key {
                    info!(
                        frame_no,
                        bytes = payload.len(),
                        "wt: encoder produced a keyframe"
                    );
                }
                // The transport never blocks the streaming thread: a full
                // queue drops the frame (leaky bucket, see send_frame).
                let _ = t.send_frame(crate::media::wt::OutboundFrame {
                    frame_no,
                    capture_us,
                    key,
                    payload,
                    captured_at: std::time::Instant::now(),
                    enq_us: 0,
                    capture_host_us: 0,
                });
            }
        }
        gst::PadProbeReturn::Ok
    });
    pts_offset
}

/// Where the two WebRTC data channels' inbound traffic is forwarded (§12.2).
#[derive(Clone, Default)]
pub struct DataChannelSinks {
    pub input: Option<InputBytesTx>,
    pub control: Option<ControlTextTx>,
}

pub struct Pipeline {
    pipeline: gst::Pipeline,
    bridge: WebRtcBridge,
    stats: Arc<StatsCollector>,
    session: SessionConfig,
}

impl Pipeline {
    pub fn build(
        cfg: &Config,
        stats: Arc<StatsCollector>,
        session: &SessionConfig,
        signal_tx: SignalTx,
        chans: DataChannelSinks,
        wt: Option<Arc<crate::media::wt::WtVideoTransport>>,
    ) -> anyhow::Result<Self> {
        gst::init().context("gst::init")?;

        let pipeline = gst::Pipeline::builder().name("inphase-media").build();

        // ---- capture source (§5.1) -------------------------------------------
        let capture_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let src = build_capture_src(cfg, capture_frames.clone())?;

        // ---- leaky queue: decouples the capture thread from encode, and drops
        //      STALE frames rather than stalling the source (§2.1, §5.1, §8.2).
        //      `leaky=upstream` keeps the freshest buffers and discards the
        //      oldest when full — `downstream` would do the opposite (hold old
        //      frames, discard fresh ones), letting latency grow to the cap.
        //      Leak DOWNSTREAM with a single buffer: when the encoder
        //      stalls, the OLDEST frame is dropped and the encoder always
        //      wakes on the newest capture. (leaky=upstream kept 3 stale
        //      frames and starved the live edge — the "raw queue retains
        //      older frames" defect from the review.)
        let queue = gst::ElementFactory::make("queue")
            .name("raw-leaky")
            .property("max-size-buffers", 1u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()?;

        // ---- GPU colour convert, stays in D3D11 memory (§5.1) ----------------
        let convert = gst::ElementFactory::make("d3d11convert").build()?;

        let caps = gst::Caps::builder("video/x-raw")
            .features(["memory:D3D11Memory"])
            .field("format", "NV12")
            .field("width", session.width as i32)
            .field("height", session.height as i32)
            .field("framerate", gst::Fraction::new(session.fps as i32, 1))
            .build();
        // Named so the stats poller can find it and retune the encode
        // resolution if the operator ever re-introduces that. d3d11convert
        // upstream does the scaling when these caps change.
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("vcaps")
            .property("caps", &caps)
            .build()?;

        // ---- WebRTC sink / bin bridge (§4.1, §18) ---------------------------
        let bridge = WebRtcBridge::new(cfg, session, stats.clone(), signal_tx, chans)?;

        // No videorate. It was added (2026-09-08) so a static desktop still
        // produced buffers for ForceKeyUnit to land on, but it cannot emit a
        // frame until the NEXT one arrives - it has to see where the following
        // timestamp falls - so every captured frame waited up to a frame
        // interval inside it, and on a truly idle screen the last change sat
        // there until something else moved. d3d11screencapturesrc already
        // repeats the last frame at the negotiated rate: measured on Cin-PC
        // (GStreamer 1.28.6, 2026-09-23) at 60.0 fps with nothing moving, the
        // rate coming from the framerate in `vcaps` below.
        pipeline.add_many([&src, &queue, &convert, &capsfilter])?;
        gst::Element::link_many([&src, &queue, &convert, &capsfilter])
            .context("link capture → caps")?;
        bridge.attach_video(&pipeline, &capsfilter)?;

        // ---- audio branch (Phase 2, §11 — now WT-only, ADR-0011) ------------
        // opusenc emits raw Opus packets; a src-pad probe forwards each as a
        // single WT audio datagram (header tag 0x41, see transport.rs). The
        // WebRTC audio leg is gone: no rtpopuspay, no webrtcbin audio sink.
        // The encoder's PTS offset (see attach_gst_tap), shared with the audio
        // tap: video `capture_us` is running time + this offset, audio PTS is
        // plain running time, and the client can only line the two up if
        // they arrive on one clock.
        let pts_offset = Arc::new(std::sync::atomic::AtomicI64::new(i64::MIN));
        if cfg.media.enable_audio {
            match build_audio_branch(cfg) {
                Ok(elems) => {
                    let refs: Vec<&gst::Element> = elems.iter().collect();
                    pipeline.add_many(refs.as_slice())?;
                    gst::Element::link_many(refs.as_slice())?;
                    let mut wired = false;
                    if let Some(wt_a) = &wt {
                        // Probe the opus output pad (the fakesink after it
                        // merely drains the chain).
                        let opus_el = elems.iter().find(|e| e.name() == "wt-audio-enc").cloned();
                        if let Some(opus_el) = opus_el {
                            if let Some(src_pad) = opus_el.static_pad("src") {
                                let wt_probe = wt_a.clone();
                                let audio_offset = pts_offset.clone();
                                let audio_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
                                let audio_el = opus_el.downgrade();
                                src_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                                    if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                                        let raw_us =
                                            b.pts().map(|p| p.nseconds() / 1_000).unwrap_or(0);
                                        // On the video's capture clock, so the
                                        // client can play each packet when its
                                        // frame is on screen (lip sync). Held
                                        // back until the first video frame has
                                        // measured the offset: a browser's
                                        // AudioDecoder takes its timestamp base
                                        // from the FIRST packet and counts on
                                        // from it, so raw-clock packets at the
                                        // start left every later one reported
                                        // 1000 h early and lip sync never
                                        // engaged (2026-09-24, Mac Chrome).
                                        let off =
                                            audio_offset.load(std::sync::atomic::Ordering::Relaxed);
                                        if off == i64::MIN {
                                            return gst::PadProbeReturn::Ok;
                                        }
                                        let ts_us = (raw_us as i64 + off).max(0) as u64;
                                        // How long audio spent inside this
                                        // pipeline (capture PTS -> Opus out),
                                        // every ~5 s: the host's share of any
                                        // lip-sync lag, which the client can
                                        // only chase, never undo.
                                        let n = audio_count
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        if n % 500 == 0 {
                                            if let Some(el) = audio_el.upgrade() {
                                                if let Some(now) = el
                                                    .clock()
                                                    .zip(el.base_time())
                                                    .and_then(|(c, b)| c.time().checked_sub(b))
                                                {
                                                    tracing::debug!(
                                                        in_pipeline_ms = (now.useconds() as i64
                                                            - raw_us as i64)
                                                            / 1000,
                                                        "wt audio: capture-to-encoded latency"
                                                    );
                                                }
                                            }
                                        }
                                        if let Ok(m) = b.map_readable() {
                                            wt_probe.send_audio_datagram(m.as_slice(), ts_us);
                                        }
                                    }
                                    gst::PadProbeReturn::Ok
                                });
                                wired = true;
                            }
                        }
                    }
                    if wired {
                        stats.set_audio_health(crate::stats::AudioHealth::Healthy);
                    } else {
                        warn!("audio captured but no WT transport — audio disabled this session");
                        stats.set_audio_health(crate::stats::AudioHealth::Failed);
                    }
                }
                Err(e) => {
                    warn!("audio branch could not be built: {e:#} — continuing without audio");
                    stats.set_audio_health(crate::stats::AudioHealth::Failed);
                }
            }
        }

        // ---- bus watch: DXGI loss, encoder errors, EOS (§22) ---------------
        install_bus_watch(&pipeline, stats.clone());

        // ---- host-side pipeline telemetry (§20) ---------------------------
        // Count encoded frames via a probe on the payloader sink pad.
        let encoded_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let trace = Arc::new(crate::media::frametrace::FrameTrace::new());

        if let Some(enc) = pipeline.by_name("venc") {
            if let Some(sink) = enc.static_pad("sink") {
                let counter = encoded_frames.clone();
                let ft = trace.clone();
                sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                        if let Some(pts) = b.pts() {
                            ft.on_capture(pts.nseconds());
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        bridge.spawn_frame_trace(trace);

        // ---- WebTransport video path (ADR-0011 P1) --------------------------
        // Second consumer of the encoded AUs: datagrams to the client's
        // VideoDecoder, bypassing the RTP/jitter-buffer path entirely. The
        // probe is installed pre-start like the frametrace ones above.
        if let Some(t) = &wt {
            let pts_offset = attach_gst_tap(&pipeline, t, pts_offset.clone());
            // Clock-sync pongs answer from the clock `capture_us` is stamped
            // on - running time plus the encoder's PTS offset - so client-side
            // ages include this host's capture, encode, queue and pacing. A
            // weak ref: a torn-down pipeline answers None and the transport
            // falls back, as it does until the offset is first measured.
            let weak = pipeline.downgrade();
            t.set_capture_clock(Some(Arc::new(move || {
                let off = pts_offset.load(std::sync::atomic::Ordering::Relaxed);
                if off == i64::MIN {
                    return None;
                }
                let p = weak.upgrade()?;
                let now = p.clock()?.time();
                let base = p.base_time()?;
                let running = now.checked_sub(base)?.useconds() as i64;
                u64::try_from(running + off).ok()
            })));
        }

        spawn_stats_poller(
            &pipeline,
            stats.clone(),
            encoded_frames,
            capture_frames,
            session.clone(),
            wt.clone(),
        );

        Ok(Self {
            pipeline,
            bridge,
            stats,
            session: session.clone(),
        })
    }

    pub fn start(&mut self) -> anyhow::Result<()> {
        info!(
            codec = ?self.session.codec, w = self.session.width, h = self.session.height,
            fps = self.session.fps, "starting media pipeline"
        );
        self.pipeline
            .set_state(gst::State::Playing)
            .context("pipeline -> PLAYING")?;
        Ok(())
    }

    pub fn on_client_signal(&mut self, msg: &SignalMessage) -> anyhow::Result<()> {
        self.bridge.on_client_signal(msg)
    }

    pub fn force_keyframe(&mut self) {
        if let Some(enc) = self.pipeline.by_name("venc") {
            force_keyframe_on(&enc);
        }
    }

    pub fn send_control(&self, text: &str) {
        self.bridge.send_control(text);
    }

    pub fn stop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.stats.reset_session();
        info!("media pipeline stopped");
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn build_audio_branch(cfg: &Config) -> anyhow::Result<Vec<gst::Element>> {
    let endpoints = crate::platform::enumerate_audio_endpoints().unwrap_or_default();
    for ep in &endpoints {
        info!(
            target: "inphase_host::media",
            "audio endpoint{}{}{}: {} id=\"{}\"",
            if ep.capture { " [recording]" } else { "" },
            if ep.is_default { " [default]" } else { "" },
            if ep.active { "" } else { " [inactive]" },
            ep.name,
            ep.id
        );
    }

    // A pinned `media.audio_capture_device` (or INPHASE_AUDIO_DEVICE) is the
    // exact WASAPI endpoint id (`IMMDevice::GetId`). If it names a *recording*
    // endpoint (e.g. an Elgato Wave Link mix) we read it directly; otherwise
    // wasapi2src does loopback on the *playback* endpoint's mix. With no
    // `device` it follows the Windows default playback device — often not where
    // the game plays on a machine with virtual-audio routing.
    let pinned = cfg
        .media
        .audio_capture_device
        .as_deref()
        .filter(|s| !s.is_empty());
    let is_recording = pinned
        .and_then(|id| endpoints.iter().find(|e| e.id == id))
        .map(|e| e.capture)
        .unwrap_or(false);

    let src = gst::ElementFactory::make("wasapi2src")
        .property("loopback", !is_recording)
        .property("low-latency", true)
        .build()
        .context("wasapi2src")?;

    match pinned {
        Some(id) => {
            info!(
                device = id,
                mode = if is_recording {
                    "recording"
                } else {
                    "loopback"
                },
                "audio: capturing pinned endpoint"
            );
            src.set_property("device", id);
        }
        None => info!("audio: loopback-capturing the default playback device"),
    }

    // gst 1.28: if the endpoint can't be opened / disappears, post a warning and
    // keep producing (silence) instead of erroring — audio must never take the
    // video path down with it.
    if src.find_property("continue-on-error").is_some() {
        src.set_property("continue-on-error", true);
    } else {
        warn!("wasapi2src has no `continue-on-error` — an audio-device failure can stall the pipeline");
    }

    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", 48_000i32)
                .field("channels", 2i32)
                .build(),
        )
        .build()?;
    // opusenc: 10 ms frames, ~160 kbps stereo, generic audio-type (§11)
    let opus = gst::ElementFactory::make("opusenc")
        .name("wt-audio-enc")
        .property("bitrate", cfg.media.audio_bitrate_bps as i32)
        .property_from_str("audio-type", "generic")
        .build()
        .context("opusenc")?;
    // frame-size is an enum-ish "10"/"20"/... ms property
    opus.set_property_from_str("frame-size", &cfg.media.audio_frame_ms.to_string());
    // The WT audio tap reads opusenc's output through a src-pad probe; a
    // probe observes but does not drain. The chain needs a real consumer -
    // a clock-less fakesink - or the first push errors out (§11-on-WT).
    let drain = gst::ElementFactory::make("fakesink")
        .name("wt-audio-drain")
        .property("sync", false)
        .property("async", false)
        .build()
        .context("wt audio fakesink")?;
    Ok(vec![src, convert, resample, caps, opus, drain])
}

/// Poll `queue` levels + the encoder + an encoded-frame counter into
/// [`StatsCollector`] once a second, so the dashboard and acceptance gates
/// (§20) are not blind on the host side.
fn spawn_stats_poller(
    pipeline: &gst::Pipeline,
    stats: Arc<StatsCollector>,
    encoded_frames: Arc<std::sync::atomic::AtomicU64>,
    capture_frames: Arc<std::sync::atomic::AtomicU64>,
    session: SessionConfig,
    wt: Option<Arc<crate::media::wt::WtVideoTransport>>,
) {
    let raw_q = pipeline.by_name("raw-leaky");
    let rtp_q = pipeline.by_name("rtp-leaky");
    let venc = pipeline.by_name("venc");
    // Resolved here, not inside the thread: the closure is `move` and cannot
    // borrow `pipeline`.
    let vcaps = pipeline.by_name("vcaps");
    let wt_stats = wt.clone();
    std::thread::Builder::new()
        .name("inphase-gst-stats".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let mut last_frames = encoded_frames.load(Ordering::Relaxed);
            let mut last_capture = capture_frames.load(Ordering::Relaxed);
            let mut last = std::time::Instant::now();
            // Application-level congestion control for the webrtcbin path (it has
            // no built-in bandwidth estimation). Driven by the client's once-a-second telemetry
            // (packets lost, inbound bitrate, decoded fps, RTT) plus the local
            // RTP send-queue depth. See `crate::media::bitrate`.
            let ceiling = session.max_bitrate_kbps.max(session.start_bitrate_kbps);
            let floor = session.min_bitrate_kbps.clamp(500, ceiling).max(500);
            let mut ctl = crate::media::bitrate::BitrateController::new(
                session.start_bitrate_kbps,
                floor,
                ceiling,
            );
            let mut adapted_kbps = ctl.kbps;
            let mut last_lost: Option<u64> = None;
            let mut last_incomplete_ctl: Option<u64> = None;
            let mut last_client_move = std::time::Instant::now();
            let mut last_decoded: u64 = 0;
            // Route-adaptive resolution (crate::media::quality). A keyframe is
            let mut last_incomplete: u64 = 0;
            let mut last_route_warn =
                std::time::Instant::now() - std::time::Duration::from_secs(60);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let now = std::time::Instant::now();
                let dt = now.duration_since(last).as_secs_f32().max(0.001);
                last = now;

                let frames = encoded_frames.load(Ordering::Relaxed);
                let enc_fps = (frames.saturating_sub(last_frames)) as f32 / dt;
                last_frames = frames;
                let cap = capture_frames.load(Ordering::Relaxed);
                let capture_fps = (cap.saturating_sub(last_capture)) as f32 / dt;
                last_capture = cap;

                let raw_level = raw_q
                    .as_ref()
                    .map(|q| q.property::<u32>("current-level-buffers"))
                    .unwrap_or(0);
                let rtp_backlog_ms = rtp_q
                    .as_ref()
                    .map(|q| q.property::<u64>("current-level-time") as f32 / 1e6)
                    .unwrap_or(0.0);
                let enc_bitrate = venc
                    .as_ref()
                    .filter(|e| e.find_property("bitrate").is_some())
                    .map(|e| e.property::<u32>("bitrate"))
                    .unwrap_or(0);

                // --- gather client feedback -------------------------------------
                let client = stats.client_snapshot();
                let (lost_delta, recv_kbps, decoded_fps, rtt_ms, have_client, client_lat_p95) =
                    match &client {
                        Some(c) => {
                            let ld = match last_lost {
                                Some(prev) => c.packets_lost.saturating_sub(prev),
                                None => 0,
                            };
                            last_lost = Some(c.packets_lost);
                            // Wire v2: per-frame QUIC streams hide packet loss from
                            // the client entirely (packets_lost is structurally 0),
                            // so the law's loss input must come from frames the
                            // host reset mid-stream. Scaled to packet-equivalents
                            // (a dropped frame costs ~its own size in packets) so
                            // loss_frac keeps its scale - a 20 KB frame at 1100 B
                            // per packet is ~18 packets of loss, not 1.
                            let dropped = match last_incomplete_ctl {
                                Some(prev) => c.frames_dropped_incomplete.saturating_sub(prev),
                                None => 0,
                            };
                            last_incomplete_ctl = Some(c.frames_dropped_incomplete);
                            // "fresh" = the client's counters actually moved recently.
                            if c.frames_decoded != last_decoded || ld > 0 {
                                last_client_move = now;
                                last_decoded = c.frames_decoded;
                            }
                            let fresh = now.duration_since(last_client_move).as_secs_f32() < 4.0;
                            let bpf = if session.fps > 0 {
                                (enc_bitrate as f32 * 1000.0 / 8.0 / session.fps as f32) as u64
                            } else {
                                0
                            };
                            let pkts_per_frame = (bpf / 1100).max(1);
                            let ld = ld + dropped.saturating_mul(pkts_per_frame);
                            (
                                ld,
                                c.inbound_bitrate_kbps,
                                c.decoded_fps,
                                c.rtt_ms,
                                fresh,
                                // Not coerced: a negative percentile is the
                                // client reporting "clock not synced yet", and
                                // `.max(0.0)` turned that into the *perfect*
                                // signal - it passed every delay guard and the
                                // climb gate, so the controller ramped a route
                                // nobody had measured. The law treats < 0 as
                                // unknown: no delay cut, no climb.
                                c.lat_p95_ms,
                            )
                        }
                        None => (0, 0.0, 0.0, 0.0, false, 0.0),
                    };

                if let Some(enc) = venc
                    .as_ref()
                    .filter(|e| e.find_property("bitrate").is_some())
                {
                    let prev = adapted_kbps;
                    // The WT path has no RTP queue; its send-queue depth is the
                    // frame writer's own stall time (`Shared::write_stall_ms`).
                    // Without it the controller saw zero loss and a client
                    // "draining" whatever WebKit buffered, and pushed a
                    // cellular link to 20 Mbps until flow control froze
                    // (2026-09-09 19:53).
                    let backlog_ms = rtp_backlog_ms
                        .max(wt_stats.as_ref().map(|t| t.send_stall_ms()).unwrap_or(0) as f32);
                    adapted_kbps = ctl.step(&crate::media::bitrate::Feedback {
                        lost_delta,
                        recv_kbps,
                        decoded_fps,
                        target_fps: session.fps,
                        rtt_ms,
                        rtp_backlog_ms: backlog_ms,
                        have_client,
                        client_lat_p95_ms: client_lat_p95,
                        v4_datagram_carrier: wt_stats
                            .as_ref()
                            .map(|t| t.datagram_video_enabled())
                            .unwrap_or(false),
                        v4_pace_pps: wt_stats.as_ref().map(|t| t.pace_pps()).unwrap_or(0),
                        inferred_loss_frac: wt_stats.as_ref().and_then(|t| t.inferred_loss_frac()),
                    });
                    if adapted_kbps != prev {
                        enc.set_property("bitrate", adapted_kbps);
                        // Every input the step actually used, so a cut explains
                        // itself instead of being reconstructed from the one
                        // line above it - `rtp_backlog_ms` here is the value fed
                        // to the law (send-queue stall included), not the raw RTP
                        // queue the WT path never fills, and on this carrier
                        // `packets_lost` is structurally 0 (the client reports
                        // no WT loss), so the tail delay and the NACK/keyframe
                        // counters are what actually drove the decision.
                        let (held, nacks, keys, aband) = client
                            .as_ref()
                            .map(|c| {
                                (
                                    c.decode_held,
                                    c.nacks_sent,
                                    c.keys_received,
                                    c.frames_dropped_incomplete,
                                )
                            })
                            .unwrap_or((0, 0, 0, 0));
                        info!(
                            from_kbps = prev,
                            to_kbps = adapted_kbps,
                            lost = lost_delta,
                            recv_kbps = recv_kbps as u32,
                            decoded_fps = decoded_fps as u32,
                            rtt_ms = rtt_ms as u32,
                            rtp_backlog_ms = backlog_ms as u32,
                            client_p95_ms = client_lat_p95 as u32,
                            held,
                            nacks,
                            keys,
                            aband,
                            "encoder bitrate {}",
                            if adapted_kbps < prev { "DOWN" } else { "up" }
                        );
                    }
                } else if enc_fps > 0.0
                    && enc_fps + 15.0 < session.fps as f32
                    && rtp_backlog_ms > 50.0
                {
                    warn!(
                        enc_fps = enc_fps as u32,
                        target = session.fps,
                        rtp_backlog_ms = rtp_backlog_ms as u32,
                        "encoder throttled by webrtcbin back-pressure (the encoder has no \
                         runtime `bitrate` property on this element)"
                    );
                }

                // --- route telemetry: measure, warn, NEVER change resolution --
                // The resolution is the user's choice (client-requested mode,
                // clamped to the capture's limits). When the route cannot carry
                // it we say so - to the log and to the client's HUD - instead
                // of silently degrading their picture (user directive).
                if let Some(t) = wt.as_ref() {
                    if have_client {
                        let chunk = crate::media::wt::DEFAULT_DATAGRAM_BUDGET as f32
                            - inphase_protocol::WT_VIDEO_HEADER_LEN as f32;
                        let recv_frags = (recv_kbps * 1000.0 / 8.0 / chunk.max(1.0)).max(0.0);
                        let denom = lost_delta as f32 + recv_frags;
                        let loss_frac = if denom > 1.0 {
                            lost_delta as f32 / denom
                        } else {
                            0.0
                        };
                        // Frames that arrived and never assembled, this tick.
                        // Reported cumulatively, like packets_lost, so it is
                        // differenced here rather than on the client.
                        let inc_now = client
                            .as_ref()
                            .map(|c| c.frames_dropped_incomplete)
                            .unwrap_or(0);
                        let inc_delta = inc_now.saturating_sub(last_incomplete);
                        last_incomplete = inc_now;

                        // §"respect user inputs": warn when the user's chosen
                        // bitrate/resolution exceeds what the route carries.
                        // Throttled: a sustained bad route logs once per 30 s.
                        // Fire only when the route is genuinely failing to
                        // deliver: the client receives well under half the
                        // encode target while frames go incomplete, or loss is
                        // catastrophic. Raw fragment loss alone is NOT evidence
                        // - cellular erasure runs 8-25 % with every frame
                        // repaired (2026-09-08 Safari: the phone received
                        // 9 Mbps at 59 fps through 8 % loss).
                        // The zombie-queue signature (fixed at the sender by
                        // the 256 KB datagram buffer, but still worth naming):
                        // the client receives PLENTY of bytes and decodes
                        // nothing - data is arriving too late to play. Without
                        // this branch the user stares at a frozen frame with a
                        // healthy-looking receive rate and no explanation.
                        // Two DIFFERENT failures get two DIFFERENT messages:
                        // the zombie shape (data arrives, nothing decodes) is
                        // not a bandwidth problem and must not claim one -
                        // the user's 400 Mbps downlink made that message a
                        // lie (2026-09-08).
                        let zombie = decoded_fps <= 0.0 && recv_kbps > 1000.0;
                        let bandwidth_starved = loss_frac > 0.35
                            || (inc_delta > 0
                                && recv_kbps > 0.0
                                && recv_kbps < adapted_kbps as f32 * 0.5);
                        let fire = zombie || bandwidth_starved;
                        if fire && now - last_route_warn > std::time::Duration::from_secs(30) {
                            last_route_warn = now;
                            let detail = if zombie {
                                format!(
                                    "the stream is arriving but nothing is decoding - \
                                     waiting for a keyframe (receiving {:.0} kbps, loss {:.0}%)",
                                    recv_kbps,
                                    loss_frac * 100.0
                                )
                            } else {
                                format!(
                                    "route is losing video data: loss {:.0}%, \
                                     receiving {:.0} of ~{} kbps, {} incomplete frames",
                                    loss_frac * 100.0,
                                    recv_kbps,
                                    adapted_kbps,
                                    inc_delta
                                )
                            };
                            warn!("{detail}");
                            t.push_route_warning(detail);
                        }
                        stats.update_host(|h| {
                            h.route_loss_pct = loss_frac * 100.0;
                            h.incomplete_frames = inc_delta;
                        });
                    }
                }

                let enc_name = venc
                    .as_ref()
                    .and_then(|e| e.factory())
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                stats.update_host(|h| {
                    h.capture_fps = capture_fps;
                    h.encoded_fps = enc_fps;
                    h.raw_queue_frames = raw_level;
                    h.encoder_bitrate_kbps = enc_bitrate;
                    h.encoder_backend = enc_name.clone();
                    h.codec = format!("{:?}", session.codec);
                    h.width = session.width;
                    h.height = session.height;
                    h.target_fps = session.fps;
                });

                if pipeline_gone(&raw_q) {
                    break;
                }
            }
        })
        .ok();
}

/// Ask `enc` for an IDR now.
///
/// Upstream events travel UPSTREAM from a SRC pad. An upstream event on the
/// SINK pad is the wrong direction - GStreamer rejects it outright
/// ("sending custom-upstream event in wrong direction") - and with an
/// effectively-infinite GOP there is no periodic IDR to mask the rejection:
/// the WT path simply never recovers a reference break and the client stays
/// black while it keeps requesting keyframes. Every GstVideoEncoder-based
/// element handles an upstream ForceKeyUnit arriving on its SRC pad
/// (nvd3d11h265enc has NO force-idr property or force-keyframe signal, so
/// the property fallbacks below are only for hypothetical others).
fn force_keyframe_on(enc: &gst::Element) {
    if let Some(src) = enc.static_pad("src") {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if src.send_event(event) {
            info!("keyframe request: ForceKeyUnit sent upstream via encoder src pad");
            return;
        }
        warn!("ForceKeyUnit event rejected by encoder src pad");
    }
    // Fallbacks for hypothetical elements without event handling.
    if enc.find_property("force-keyframe").is_some() {
        enc.emit_by_name::<()>("force-keyframe", &[]);
    } else if enc.find_property("force-idr").is_some() {
        enc.set_property("force-idr", true);
    }
}

/// Retune the capture capsfilter to `rung`, keeping format and framerate.
///
fn pipeline_gone(el: &Option<gst::Element>) -> bool {
    match el {
        Some(e) => e.parent().is_none(),
        None => true,
    }
}

/// Watch the pipeline bus on a dedicated thread (the host runs a tokio runtime,
/// not a glib `MainLoop`, so `bus.add_watch` would never fire). Exits on
/// error/EOS or when the bus is dropped.
fn install_bus_watch(pipeline: &gst::Pipeline, stats: Arc<StatsCollector>) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    std::thread::Builder::new()
        .name("inphase-gst-bus".into())
        .spawn(move || {
            use gst::MessageView;
            loop {
                let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else {
                    continue;
                };
                match msg.view() {
                    MessageView::Error(err) => {
                        let from = err
                            .src()
                            .map(|s| s.path_string().to_string())
                            .unwrap_or_default();
                        // An audio-branch failure must not take video down. The
                        // element names come from `build_audio_branch` /
                        // `attach_audio` (wasapi2src, opusenc, rtpopuspay, …);
                        // gst 1.28 `continue-on-error` should keep wasapi2src
                        // alive, this is the backstop for the rest of the branch.
                        // `wasapi2src` with `continue-on-error` keeps producing;
                        // treat its errors as Degraded. A downstream element
                        // erroring means audio is actually dead for this
                        // session — Failed, but still never touch video.
                        if from.contains("wasapi2src") {
                            warn!(
                                "audio capture degraded (video unaffected): {} ({:?})",
                                err.error(),
                                err.debug()
                            );
                            stats.set_audio_health(crate::stats::AudioHealth::Degraded);
                            continue;
                        }
                        if from.contains("opusenc")
                            || from.contains("rtpopuspay")
                            || from.contains("rtpopus")
                            || from.contains("audioconvert")
                            || from.contains("audioresample")
                            || from.contains("audio-leaky")
                        {
                            warn!(
                                "audio branch FAILED (video unaffected, recovers on reconnect): \
                                 {} from {from} ({:?})",
                                err.error(),
                                err.debug()
                            );
                            stats.set_audio_health(crate::stats::AudioHealth::Failed);
                            continue;
                        }
                        // TODO(§22): classify DXGI-lost vs encoder failure via `stats`.
                        warn!(
                            "pipeline error: {} from {from} ({:?})",
                            err.error(),
                            err.debug()
                        );
                        break;
                    }
                    MessageView::Warning(w) => warn!("pipeline warning: {}", w.error()),
                    MessageView::Eos(_) => {
                        warn!("pipeline EOS");
                        break;
                    }
                    _ => {}
                }
            }
        })
        .ok();
}

/// Apply [`encoder_policy`]'s low-latency properties to a freshly-created
/// encoder element (§6, §18).
pub fn configure_encoder(
    enc: &gst::Element,
    vendor: GpuVendor,
    codec: VideoCodec,
    bitrate_kbps: u32,
) {
    for (name, value) in encoder_policy::low_latency_properties(vendor, codec, bitrate_kbps) {
        if enc.find_property(name).is_none() {
            // Honest logging: a property the element doesn't carry is a
            // policy/element mismatch (typo, version drift, wrong backend)
            // and must be diagnosable from host.log, not silent (§7).
            debug!(
                element = enc.name().as_str(),
                property = name,
                "encoder property not on this element/version - skipped"
            );
            continue;
        }
        // `set_property_from_str` coerces into whatever concrete type the pspec
        // is (gint vs gint64, enum-by-nick, bool) and only logs a warning on a
        // bad value — unlike `set_property`, which panics on a type mismatch.
        enc.set_property_from_str(name, &value.to_prop_string());
    }
    // §4.3: infinite GOP + periodic intra-refresh - a sweeping column of
    // intra macroblocks per P-frame keeps the bandwidth profile flat instead
    // of spiking on periodic keyframes. On-demand IDRs (connect/repair) still
    // work alongside it. Optional: not every GStreamer/NVENC version exposes
    // the property; skip silently-but-visibly when absent.
    if enc.find_property("intra-refresh").is_some() {
        enc.set_property_from_str("intra-refresh", &true.to_string());
    } else {
        debug!(
            element = enc.name().as_str(),
            "intra-refresh not on this encoder/version - skipped"
        );
    }
    info!(
        ?vendor,
        ?codec,
        bitrate_kbps,
        "applied low-latency encoder properties"
    );
}

/// Build the screen-capture source. One element, one mode — the capture API is
/// chosen once from config and never swapped at runtime (a live source swap
/// renegotiates caps against a monitor-sized capsfilter and stalls the graph).
fn build_capture_src(
    cfg: &Config,
    capture_frames: Arc<std::sync::atomic::AtomicU64>,
) -> anyhow::Result<gst::Element> {
    let api = match cfg.capture.api {
        CaptureApi::Dxgi => "dxgi",
        CaptureApi::Wgc => "wgc",
    };
    let monitor_index = resolve_monitor_index(cfg);
    let src = gst::ElementFactory::make("d3d11screencapturesrc")
        .name("capture-src")
        .property_from_str("capture-api", api)
        .property("show-cursor", cfg.capture.show_cursor)
        .property("monitor-index", monitor_index)
        .build()
        .context("d3d11screencapturesrc (is the gstd3d11 plugin present?)")?;

    if let Some(pad) = src.static_pad("src") {
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if info.data.is_some() {
                capture_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }

    info!(
        api,
        monitor_index,
        show_cursor = cfg.capture.show_cursor,
        "configured screen capture"
    );
    Ok(src)
}

/// GStreamer monitor index; `-1` means primary. We always pass an explicit index
/// so monitor selection matches our own enumeration order.
fn resolve_monitor_index(cfg: &Config) -> i32 {
    if let Some(idx) = cfg.capture.monitor_index {
        return idx as i32;
    }
    crate::platform::enumerate_monitors()
        .ok()
        .and_then(|mons| {
            mons.iter()
                .find(|m| m.primary)
                .or_else(|| mons.first())
                .map(|m| m.index as i32)
        })
        .unwrap_or(-1)
}
