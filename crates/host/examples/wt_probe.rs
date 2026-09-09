//! Browser-free transport probe: pair, claim a session, dial WebTransport, and
//! write the delivered video to a file.
//!
//! # Why
//!
//! Since the switch to H.265-only the E2E harness has had no opinion about
//! video at all. Headless Chrome on Linux has no HEVC decoder, so it can
//! produce neither pixels nor a decoded-frame count, and the suite printed
//! SKIP-HEVC and asserted nothing. Every video regression after that was found
//! by a person looking at a phone, one round trip per bug — a stale codec
//! description that declared level 0, a config upgrade that killed the decoder
//! on every session, a fragmenter sized past the path MTU.
//!
//! Decoding is not what needs testing here. The transport either delivers the
//! encoder's bitstream intact or it does not, and that question needs no
//! browser: this dials the host exactly as a client does, reads frames off the
//! wire, and writes the Annex-B elementary stream out. `ffmpeg` decodes HEVC in
//! software anywhere, so the assertion lands in the shell.
//!
//! Writing this against the v1 datagram protocol would have meant
//! reimplementing the fragmenter, the parity groups and the NACK planner. On v2
//! a frame is a stream, so the receive path is ten lines.
//!
//! # Usage
//!
//! ```sh
//! # the invite is minted over the loopback admin API (see tools/remote-e2e.sh)
//! cargo run -p inphase-host --example wt_probe -- \
//!     --host 100.127.176.18 --invite <secret> --secs 10 --out /tmp/received.h265
//! ```
//!
//! Exit 0 on success, 1 on a failed assertion, 2 on a setup error.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use inphase_protocol::{SignalMessage, WtClientMessage, WtFrame};

struct Args {
    host: String,
    invite: String,
    secs: u64,
    out: String,
    http_port: u16,
    /// Milliseconds to stall inside each frame read, standing in for a client
    /// whose main thread is busy decoding and painting.
    read_delay_ms: u64,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        host: String::new(),
        invite: String::new(),
        secs: 10,
        out: "/tmp/received.h265".into(),
        http_port: 47_800,
        read_delay_ms: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().context("missing value");
        match k.as_str() {
            "--host" => a.host = v()?,
            // Accept a whole invite URL as well: the secret is its fragment.
            "--invite" => {
                let raw = v()?;
                a.invite = raw.rsplit('#').next().unwrap_or(&raw).to_string();
            }
            "--secs" => a.secs = v()?.parse()?,
            "--out" => a.out = v()?,
            "--http-port" => a.http_port = v()?.parse()?,
            "--read-delay-ms" => a.read_delay_ms = v()?.parse()?,
            other => bail!("unknown argument {other}"),
        }
    }
    if a.host.is_empty() || a.invite.is_empty() {
        bail!("--host and --invite are required");
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("probe: {e:#}");
            std::process::exit(2);
        }
    }
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let base = format!("http://{}:{}", args.host, args.http_port);

    // --- pair -----------------------------------------------------------
    // A device key is mandatory (see http::api::pair_success): a cookie
    // without one produces a client that looks paired and can never stream.
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).context("OS entropy")?;
    let key = SigningKey::from_bytes(&seed);
    let pubkey_hex = hex(key.verifying_key().as_bytes());

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let resp = http
        .post(format!("{base}/api/v1/pair"))
        .json(&serde_json::json!({
            "invite": args.invite,
            "controller_pubkey": pubkey_hex,
            "controller_name": "wt_probe",
        }))
        .send()
        .await
        .context("POST /api/v1/pair")?
        .error_for_status()
        .context("pair rejected")?;
    // Take the session id straight off Set-Cookie: the websocket upgrade below
    // does not go through reqwest's jar.
    let sid = resp
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
        .context("pair returned no session cookie")?;
    eprintln!("probe: paired");

    // --- signaling --------------------------------------------------------
    let cookie = sid;
    let ws_url = format!("ws://{}:{}/api/v1/signal", args.host, args.http_port);
    let req = tungstenite_request(&ws_url, &cookie)?;
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("signaling websocket")?;

    let mut wt_info: Option<(String, u16, String)> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let Some(msg) = ws.next().await else {
            bail!("signaling closed before the session was ready")
        };
        let msg = msg?;
        let Ok(text) = msg.into_text() else { continue };
        let Ok(m) = serde_json::from_str::<SignalMessage>(&text) else {
            continue;
        };
        match m {
            SignalMessage::AuthChallenge { nonce } => {
                // Prove possession of the device key we just enrolled.
                let raw = b64_decode(&nonce)?;
                let sig = key.sign(&raw);
                send(
                    &mut ws,
                    &SignalMessage::AuthResponse {
                        controller_id: pubkey_hex.clone(),
                        signature: b64_encode(&sig.to_bytes()),
                    },
                )
                .await?;
                send(
                    &mut ws,
                    &SignalMessage::ClientHello {
                        protocol_version: inphase_protocol::SIGNALING_PROTOCOL_VERSION,
                        browser: "wt_probe".into(),
                        requested_mode: inphase_protocol::RequestedMode {
                            width: 1920,
                            height: 1080,
                            fps: 60,
                            preset: Default::default(),
                            codec_preference: Some(inphase_protocol::VideoCodec::H265),
                            max_bitrate_kbps: None,
                            stream_target: Default::default(),
                        },
                    },
                )
                .await?;
                send(
                    &mut ws,
                    &SignalMessage::ClientCapabilities {
                        // The host still gates the video session on this
                        // WebRTC-era capability list, even though WebRTC video is
                        // gone - see http::signal `client_has("video/H265")`.
                        // Both codecs, because the host now chooses between them
                        // (media.allow_hevc). Advertising only HEVC made the probe
                        // untestable the moment the host was switched to H.264.
                        rtp_video_codecs: vec![
                            inphase_protocol::RtpCodecCapability {
                                mime_type: "video/H265".into(),
                                clock_rate: 90_000,
                                channels: None,
                                sdp_fmtp_line: None,
                            },
                            inphase_protocol::RtpCodecCapability {
                                mime_type: "video/H264".into(),
                                clock_rate: 90_000,
                                channels: None,
                                sdp_fmtp_line: None,
                            },
                        ],
                        rtp_audio_codecs: vec![inphase_protocol::RtpCodecCapability {
                            mime_type: "audio/opus".into(),
                            clock_rate: 48_000,
                            channels: Some(2),
                            sdp_fmtp_line: None,
                        }],
                        // Claim HEVC: this probe never decodes, but the host only
                        // starts a session for a client that could.
                        decode_hints: vec![
                            inphase_protocol::DecodeHint {
                                codec: inphase_protocol::VideoCodec::H265,
                                width: 1920,
                                height: 1080,
                                framerate: 60,
                                supported: true,
                                smooth: true,
                                power_efficient: true,
                            },
                            inphase_protocol::DecodeHint {
                                codec: inphase_protocol::VideoCodec::H264,
                                width: 1920,
                                height: 1080,
                                framerate: 60,
                                supported: true,
                                smooth: true,
                                power_efficient: true,
                            },
                        ],
                        features: Default::default(),
                    },
                )
                .await?;
            }
            SignalMessage::WtVideoInfo {
                token,
                port,
                cert_sha256,
            } => {
                wt_info = Some((token, port, cert_sha256));
                break;
            }
            SignalMessage::Error(e) => bail!("host refused the session: {e:?}"),
            _ => {}
        }
    }
    let (token, port, cert_hex) = wt_info.context("host never offered a WT video path")?;
    eprintln!("probe: wt offer on port {port}");

    // --- dial the video transport ----------------------------------------
    let mut hash = [0u8; 32];
    for (i, b) in hash.iter_mut().enumerate() {
        *b = u8::from_str_radix(&cert_hex[i * 2..i * 2 + 2], 16)?;
    }
    let cfg = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([wtransport::tls::Sha256Digest::new(hash)])
        .build();
    let conn = wtransport::Endpoint::client(cfg)?
        .connect(format!("https://{}:{}/wt-video", args.host, port))
        .await
        .context("wt dial")?;

    // Token first: a QUIC dial carries no credentials.
    let (mut ctl_tx, _ctl_rx) = conn.open_bi().await?.await?;
    let mut line = serde_json::to_string(&WtClientMessage::Auth { token })?;
    line.push('\n');
    ctl_tx.write_all(line.as_bytes()).await?;

    // --- read frames ------------------------------------------------------
    // One frame per unidirectional stream: read to EOF, that is the frame.
    let mut stream_out = Vec::new();
    let (mut frames, mut keyframes) = (0u32, 0u32);
    let mut arrivals: Vec<(u32, bool, usize)> = Vec::new();
    let until = Instant::now() + Duration::from_secs(args.secs);
    while Instant::now() < until {
        let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_secs(5), conn.accept_uni()).await
        else {
            continue;
        };
        use tokio::io::AsyncReadExt as _;
        if args.read_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(args.read_delay_ms)).await;
        }
        let mut buf = Vec::new();
        if s.read_to_end(&mut buf).await.is_err() {
            continue; // stream reset: the frame was abandoned, keep going
        }
        match WtFrame::decode(&buf) {
            Ok(f) => {
                frames += 1;
                if f.key {
                    keyframes += 1;
                }
                // Arrival ORDER, not frame order. Streams complete
                // independently, so this is the sequence the browser's decoder
                // actually sees - and the input the client's reordering logic
                // has to cope with.
                arrivals.push((f.frame_no, f.key, f.payload.len()));
                stream_out.extend_from_slice(&f.payload);
            }
            Err(e) => eprintln!("probe: undecodable frame: {e}"),
        }
    }

    std::fs::write(&args.out, &stream_out)?;
    // The arrival trace, for replaying through the client's ordering logic.
    let trace: Vec<serde_json::Value> = arrivals
        .iter()
        .map(|(n, k, len)| serde_json::json!({ "frame_no": n, "key": k, "bytes": len }))
        .collect();
    std::fs::write(
        format!("{}.arrivals.json", args.out),
        serde_json::to_string(&trace)?,
    )?;
    println!(
        "{{\"frames\":{frames},\"keyframes\":{keyframes},\"bytes\":{},\"out\":{:?}}}",
        stream_out.len(),
        args.out
    );
    if frames == 0 {
        eprintln!("probe: the transport delivered no frames");
        std::process::exit(1);
    }
    Ok(())
}

async fn send(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    m: &SignalMessage,
) -> Result<()> {
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(m)?.into(),
    ))
    .await?;
    Ok(())
}

fn tungstenite_request(
    url: &str,
    cookie: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    req.headers_mut().insert("cookie", cookie.parse()?);
    Ok(req)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn b64_encode(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn b64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}
