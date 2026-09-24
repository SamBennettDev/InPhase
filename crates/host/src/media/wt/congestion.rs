//! QUIC congestion control for the video connection.
//!
//! quinn's default is Cubic, a loss-based controller built for bulk transfer.
//! It gates *everything* on the connection - datagrams as well as the stream an
//! IDR rides - behind its window, and it shrinks that window on every loss
//! event. That is the wrong law for this connection, for three reasons:
//!
//! * **Wi-Fi erasure is not congestion.** The measured sessions lose 1-5 % of
//!   datagrams at a base rate with the client decoding 60 fps throughout
//!   (parity and NACK repair it). Cubic reads each of those as congestion and
//!   backs the window off by 30 %, once per RTT. At a 60 ms RTT and 2 % loss a
//!   loss-based window sustains single-digit Mbps - far below the 80 Mbps the
//!   user configured - whatever the application's own controller asks for.
//! * **What it throttles is dropped silently.** A window-blocked datagram waits
//!   in quinn's 256 KB send buffer and is discarded from the head when that
//!   fills; `send_datagram` has already returned `Ok`. The host then sees the
//!   loss only as the client's missing receive count, and cannot tell its own
//!   transport's throttling from the network's.
//! * **It fights the controller that is actually responsible for rate.** The
//!   encoder target is set by [`crate::media::bitrate`], which reads the
//!   client's decode rate, tail delay and measured loss, and datagram injection
//!   is paced separately. Two rate authorities on one flow, one of them blind
//!   to whether frames are being decoded, is how the rate stalls below the mark.
//!
//! [`RealtimeWindow`] therefore holds a fixed window large enough never to be
//! the bottleneck at any rate this product negotiates, and ignores loss. Loss
//! recovery (stream retransmission) is unaffected - that is quinn's loss
//! detection, not the congestion controller. quinn's own pacer derives its rate
//! from the window, so a large window also takes it out of the way; datagram
//! spacing is the transport's pace bucket.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use wtransport::quinn::congestion::{Controller, ControllerFactory};

/// Which congestion law the WT QUIC connection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WtCongestion {
    /// Fixed window, loss-blind: the application's bitrate controller and
    /// pacer are the only rate authorities (see the module docs).
    #[default]
    Realtime,
    /// quinn's default Cubic. Kept for A/B comparison on a live route.
    Cubic,
}

/// Bytes in flight the realtime window allows. 80 Mbps x 400 ms: several
/// round trips of the highest negotiated rate even on a relayed route, so the
/// window is never what limits the send rate. Anything that would need more
/// in flight is a queue the bitrate controller cuts on (tail delay) long before
/// it gets here.
pub const REALTIME_WINDOW_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RealtimeWindow {
    window: u64,
}

impl Controller for RealtimeWindow {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        // Deliberately nothing: see the module docs.
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RealtimeWindowFactory {
    pub window: u64,
}

impl ControllerFactory for RealtimeWindowFactory {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(RealtimeWindow {
            window: self.window,
        })
    }
}

/// Install `mode` on a transport config.
pub fn apply(cfg: &mut wtransport::config::QuicTransportConfig, mode: WtCongestion) {
    match mode {
        WtCongestion::Realtime => {
            cfg.congestion_controller_factory(Arc::new(RealtimeWindowFactory {
                window: REALTIME_WINDOW_BYTES,
            }));
        }
        WtCongestion::Cubic => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_never_shrinks_the_realtime_window() {
        let f = Arc::new(RealtimeWindowFactory {
            window: REALTIME_WINDOW_BYTES,
        });
        let now = Instant::now();
        let mut c = f.build(now, 1200);
        assert_eq!(c.initial_window(), REALTIME_WINDOW_BYTES);
        for _ in 0..100 {
            c.on_congestion_event(now, now, false, 1200);
        }
        c.on_congestion_event(now, now, true, 1 << 20);
        assert_eq!(c.window(), REALTIME_WINDOW_BYTES);
    }

    #[test]
    fn realtime_is_the_default() {
        assert_eq!(WtCongestion::default(), WtCongestion::Realtime);
        let parsed: WtCongestion = serde_json::from_str("\"cubic\"").unwrap();
        assert_eq!(parsed, WtCongestion::Cubic);
    }
}
