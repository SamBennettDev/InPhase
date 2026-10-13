//! Actual loopback WebTransport through a UDP delay/jitter/loss proxy.
//! Synthetic bytes only: this does not run Windows capture or input injection.
use inphase_host::media::wt::{
    OutboundFrame, WtClientEvent, WtCongestion, WtIdentity, WtTransportConfig, WtVideoTransport,
    DEFAULT_DATAGRAM_BUDGET, DEFAULT_MAX_QUEUED_FRAMES,
};
use inphase_protocol::{InputEvent, InputPacket, WtClientMessage, WtFrame, WT_VIDEO_HEADER_LEN};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    task::{JoinHandle, JoinSet},
};
use wtransport::tls::Sha256Digest;

struct UdpImpairment {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl UdpImpairment {
    async fn start(host: SocketAddr, delay_ms: u64) -> Self {
        let socket = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
        let address = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut client = None;
            let mut packets = 0u64;
            let mut buffer = [0u8; 65536];
            let mut pending = JoinSet::new();
            loop {
                tokio::select! {
                    received = socket.recv_from(&mut buffer) => {
                        let Ok((len, source)) = received else { break };
                        packets += 1;
                        let destination = if source == host {
                            let Some(peer) = client else { continue };
                            peer
                        } else {
                            client = Some(source);
                            host
                        };
                        // Deterministic 1% bidirectional packet erasure.
                        if packets % 100 == 37 { continue; }
                        let data = buffer[..len].to_vec();
                        let tx = socket.clone();
                        let delay = Duration::from_millis(delay_ms + (packets % 5) * 3);
                        pending.spawn(async move {
                            tokio::time::sleep(delay).await;
                            tx.send_to(&data, destination).await.unwrap();
                        });
                    }
                    result = pending.join_next(), if !pending.is_empty() => {
                        result.unwrap().unwrap();
                    }
                }
            }
        });
        Self { address, task }
    }
}

impl Drop for UdpImpairment {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_event(
    t: &WtVideoTransport,
    predicate: impl Fn(&WtClientEvent) -> bool,
) -> WtClientEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(event) = t.try_next_event() {
                if predicate(&event) {
                    return event;
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("host event deadline")
}

#[tokio::test]
async fn quic_delivers_video_and_input_through_jitter_and_packet_loss() {
    for delay_ms in [5, 50] {
        let id = WtIdentity::self_signed(&["localhost".into(), "::1".into()]).unwrap();
        let transport = Arc::new(
            WtVideoTransport::bind(WtTransportConfig {
                port: 0,
                identity: id.identity,
                cert_not_after: id.not_after,
                datagram_budget: DEFAULT_DATAGRAM_BUDGET,
                max_queued_frames: DEFAULT_MAX_QUEUED_FRAMES,
                congestion: WtCongestion::default(),
            })
            .unwrap(),
        );
        let proxy = UdpImpairment::start(
            format!("[::1]:{}", transport.port()).parse().unwrap(),
            delay_ms,
        )
        .await;
        let endpoint = wtransport::Endpoint::client(
            wtransport::ClientConfig::builder()
                .with_bind_default()
                .with_server_certificate_hashes([Sha256Digest::new(transport.cert_sha256())])
                .build(),
        )
        .unwrap();
        let conn = tokio::time::timeout(
            Duration::from_secs(10),
            endpoint.connect(format!("https://{}/wt-video", proxy.address)),
        )
        .await
        .expect("handshake deadline")
        .expect("QUIC connects through proxy");
        let (mut control, _control_rx) = conn.open_bi().await.unwrap().await.unwrap();
        let token = transport.issue_token(Duration::from_secs(30));
        let mut auth = serde_json::to_vec(&WtClientMessage::Auth { token }).unwrap();
        auth.push(b'\n');
        control.write_all(&auth).await.unwrap();
        wait_event(&transport, |e| matches!(e, WtClientEvent::Connected)).await;
        let (mut video_tx, mut video_rx) = conn.open_bi().await.unwrap().await.unwrap();
        let marker = br#"{"type":"video_channel"}"#;
        video_tx
            .write_all(&(marker.len() as u16).to_be_bytes())
            .await
            .unwrap();
        video_tx.write_all(marker).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while transport.video_sink_is_empty() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let sender = transport.clone();
        let produce = tokio::spawn(async move {
            for no in 0..120u32 {
                sender.send_frame(OutboundFrame {
                    frame_no: no,
                    capture_us: no as u64 * 16667,
                    key: no == 0,
                    payload: vec![no as u8; if no == 0 { 20000 } else { 6000 }],
                    captured_at: Instant::now(),
                    enq_us: 0,
                    capture_host_us: 0,
                });
                tokio::time::sleep(Duration::from_micros(16667)).await;
            }
        });
        let read = tokio::spawn(async move {
            let mut numbers = Vec::new();
            loop {
                let mut header = [0u8; WT_VIDEO_HEADER_LEN];
                match tokio::time::timeout(Duration::from_secs(3), video_rx.read_exact(&mut header))
                    .await
                {
                    Ok(Ok(())) => {}
                    _ => break,
                }
                let len = u32::from_le_bytes(header[14..18].try_into().unwrap()) as usize;
                assert!(len <= 20000);
                let mut bytes = header.to_vec();
                bytes.resize(WT_VIDEO_HEADER_LEN + len, 0);
                tokio::time::timeout(
                    Duration::from_secs(3),
                    video_rx.read_exact(&mut bytes[WT_VIDEO_HEADER_LEN..]),
                )
                .await
                .unwrap()
                .unwrap();
                let frame = WtFrame::decode(&bytes).unwrap();
                assert!(frame.payload.iter().all(|b| *b == frame.frame_no as u8));
                numbers.push(frame.frame_no);
                if frame.frame_no == 119 {
                    break;
                }
            }
            numbers
        });
        // Dedicated input must progress while the video stream is busy.
        let (mut input, _input_rx) = conn.open_bi().await.unwrap().await.unwrap();
        let packet = InputPacket::new(
            1,
            1,
            0,
            InputEvent::MouseMove {
                dx: 7,
                dy: -2,
                wheel_x: 0,
                wheel_y: 0,
            },
        )
        .encode();
        let sent = Instant::now();
        input
            .write_all(&(packet.len() as u16).to_be_bytes())
            .await
            .unwrap();
        input.write_all(&packet).await.unwrap();
        let event = wait_event(&transport, |e| matches!(e, WtClientEvent::Input(_))).await;
        assert!(matches!(event, WtClientEvent::Input(bytes) if bytes == packet));
        let input_ms = sent.elapsed().as_millis();
        produce.await.unwrap();
        let frames = read.await.unwrap();
        assert!(frames.len() >= 60, "sustained video progress: {frames:?}");
        assert_eq!(frames.last(), Some(&119), "no permanent sender stall");
        assert!(frames.windows(2).all(|w| w[1] > w[0]));
        eprintln!("QUIC impairment: one-way delay {delay_ms}ms + 0..12ms jitter, 1% loss; {} frames, input {input_ms}ms", frames.len());
        conn.close(0u32.into(), b"test complete");
        transport.shutdown();
    }
}
