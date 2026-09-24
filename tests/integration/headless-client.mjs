// Headless InPhase client — pairs, negotiates, opens the data channels, sends a
// few input packets, reports what came back. Runs on the host box against
// localhost so the media/input path can be verified without a real browser.
//
//   npm --prefix tests/integration i          # once: werift + ws
//   node tests/integration/headless-client.mjs <PIN> [http://127.0.0.1:47800]
//
// Exit 0 if the data channels open and video negotiates; non-zero otherwise.

import { WebSocket } from "ws";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
} from "werift";

const PIN = process.argv[2];
const BASE = process.argv[3] || "http://127.0.0.1:47800";
if (!PIN) {
  console.error("usage: headless-client.mjs <PIN> [base-url]");
  process.exit(2);
}
const WS_BASE = BASE.replace(/^http/, "ws");

const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);
const result = { paired: false, offer: false, answered: false, ice: false, video: false, dcOpen: {}, sessionReady: false, error: null };

// ---- 1. pair -------------------------------------------------------------
const pairRes = await fetch(`${BASE}/api/v1/pair`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ pin: PIN }),
});
if (!pairRes.ok) {
  console.error("pair failed:", pairRes.status, await pairRes.text());
  process.exit(1);
}
const cookie = (pairRes.headers.get("set-cookie") || "").split(";")[0];
result.paired = !!cookie;
log("paired, cookie:", cookie.slice(0, 24) + "…");

// ---- 2. peer connection ------------------------------------------------
const pc = new RTCPeerConnection({
  iceServers: [],
  codecs: {
    video: [
      new RTCRtpCodecParameters({
        mimeType: "video/H264",
        clockRate: 90000,
        rtcpFeedback: [
          { type: "nack" },
          { type: "nack", parameter: "pli" },
          { type: "goog-remb" },
          { type: "ccm", parameter: "fir" },
        ],
        parameters: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
      }),
    ],
    audio: [
      new RTCRtpCodecParameters({ mimeType: "audio/opus", clockRate: 48000, channels: 2 }),
    ],
  },
});

function exerciseChannel(ch) {
  log(`  ${ch.label} OPEN`);
  result.dcOpen[ch.label] = true;
  if (ch.label === "input") {
    ch.send(Buffer.from(inputPacket(1 /*MouseMove*/, seq++, [...enc16(40), ...enc16(0), ...enc16(0), ...enc16(0)])));
    log("  sent MouseMove dx=40");
    ch.send(Buffer.from(inputPacket(3 /*Key*/, seq++, [...enc16(0x11), 1, 0]))); // W (0x11) down
    // Gamepad: A button (bit 0) + left stick pushed up. buttons u32, lx/ly/rx/ry i16, lt/rt u16.
    const gp = [
      1, 0, 0, 0, // buttons = A
      ...enc16(0), ...enc16(-30000), // lx, ly (up)
      ...enc16(0), ...enc16(0),      // rx, ry
      ...enc16(0), ...enc16(0),      // lt, rt
    ];
    ch.send(Buffer.from(inputPacket(4 /*Gamepad*/, seq++, gp)));
    log("  sent Gamepad A + stick");
    setTimeout(() => {
      ch.send(Buffer.from(inputPacket(3, seq++, [...enc16(0x11), 0, 0])));
      ch.send(Buffer.from(inputPacket(4, seq++, [0,0,0,0, ...enc16(0),...enc16(0),...enc16(0),...enc16(0),...enc16(0),...enc16(0)])));
      log("  sent Key W up + Gamepad neutral");
    }, 400);
  }
  if (ch.label === "control") {
    ch.send(JSON.stringify({ type: "ping", at_us: Date.now() * 1000 }));
  }
}

pc.onDataChannel.subscribe((ch) => {
  log("datachannel:", ch.label, "state:", ch.readyState);
  const onMsg = (m) => log(`  ${ch.label} <-`, typeof m === "string" ? m : `${m.byteLength ?? m.length}B`);
  if (ch.message?.subscribe) ch.message.subscribe(onMsg);
  else ch.onmessage = (e) => onMsg(e.data ?? e);
  if (ch.readyState === "open") exerciseChannel(ch);
  else if (ch.stateChanged?.subscribe) ch.stateChanged.subscribe((s) => s === "open" && exerciseChannel(ch));
  else ch.onopen = () => exerciseChannel(ch);
});

pc.onTrack.subscribe((track) => {
  log("track:", track.kind, track.codec?.mimeType);
  result.video = result.video || track.kind === "video";
});

pc.onicecandidate = ({ candidate }) => {
  if (candidate) {
    ws.send(JSON.stringify({
      type: "ice",
      candidate: candidate.candidate,
      sdp_mid: candidate.sdpMid,
      sdp_mline_index: candidate.sdpMLineIndex,
    }));
  }
};
pc.iceConnectionStateChange.subscribe((s) => {
  log("ice:", s);
  if (s === "connected" || s === "completed") result.ice = true;
});

let seq = 0;

// ---- 3. signaling ----------------------------------------------------
const ws = new WebSocket(`${WS_BASE}/api/v1/signal`, {
  headers: { Cookie: cookie, Origin: BASE },
});

ws.on("open", () => {
  log("ws open");
  ws.send(JSON.stringify({
    type: "client_hello",
    protocol_version: 1,
    browser: "headless-test",
    requested_mode: { width: 1920, height: 1080, fps: 60, preset: "low_latency" },
  }));
  ws.send(JSON.stringify({
    type: "client_capabilities",
    rtp_video_codecs: [{ mime_type: "video/H264", clock_rate: 90000 }],
    rtp_audio_codecs: [{ mime_type: "audio/opus", clock_rate: 48000, channels: 2 }],
    decode_hints: [],
    features: {
      secure_context: false, jitter_buffer_target: false, keyboard_lock: false,
      pointer_lock: false, pointer_lock_unadjusted_movement: false,
      request_video_frame_callback: false, gamepad: false,
    },
  }));
});

ws.on("message", async (raw) => {
  const msg = JSON.parse(raw.toString());
  if (msg.type !== "ice") log("ws <-", msg.type);
  switch (msg.type) {
    case "session_config":
      break;
    case "offer": {
      result.offer = true;
      if (/m=application/.test(msg.sdp)) log("  offer HAS m=application (data channel)");
      else log("  offer has NO m=application !!");
      await pc.setRemoteDescription({ type: "offer", sdp: msg.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      result.answered = true;
      ws.send(JSON.stringify({ type: "answer", sdp: pc.localDescription.sdp }));
      break;
    }
    case "ice":
      if (msg.candidate) {
        try {
          await pc.addIceCandidate({ candidate: msg.candidate, sdpMLineIndex: msg.sdp_mline_index ?? 0 });
        } catch (e) { log("  addIceCandidate err", String(e)); }
      }
      break;
    case "session_ready":
      result.sessionReady = true;
      break;
    case "error":
      result.error = msg;
      log("  ERROR", JSON.stringify(msg));
      break;
  }
});

// ---- helpers --------------------------------------------------------
function enc16(n) {
  n = n < 0 ? n + 0x10000 : n;
  return [n & 0xff, (n >>> 8) & 0xff];
}
// 16-byte LE header (version=1, kind, flags u16, seq u32, client_us u64) + payload
function inputPacket(kind, sequence, payload) {
  const b = [1, kind, 0, 0];
  b.push(sequence & 0xff, (sequence >>> 8) & 0xff, (sequence >>> 16) & 0xff, (sequence >>> 24) & 0xff);
  for (let i = 0; i < 8; i++) b.push(0);
  return b.concat(payload);
}

// ---- verdict --------------------------------------------------------
setTimeout(async () => {
  const ok =
    result.paired && result.offer && result.answered &&
    result.dcOpen.input && result.dcOpen.control && !result.error;
  // Un-pair this ephemeral test session first (werift teardown can crash the
  // process on exit, so do network work before touching the peer connection).
  await fetch(`${BASE}/api/v1/logout`, { method: "POST", headers: { Cookie: cookie } }).catch(() => {});
  try { ws.close(); } catch {}
  log("---- result ----");
  console.log(JSON.stringify(result, null, 2));
  console.log(ok ? "PASS" : "FAIL");
  process.exit(ok ? 0 : 1);
}, 8000);
