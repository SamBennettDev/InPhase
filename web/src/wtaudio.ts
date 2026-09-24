// WT audio playback (§11-on-WT, ADR-0011 WT-only): Opus datagrams from the
// host are decoded with WebCodecs AudioDecoder and played on an
// AudioContext. Under-runs play silence rather than freezing, by design.
//
// Lip sync: each packet is scheduled to be HEARD when the video frame
// captured at the same instant is SEEN (see `syncedStartUs`). Packets carry
// the video's capture clock (host pipeline.rs), and the page knows how old
// frames are when presented, so audio aims at the same age. Chaining packets
// back to back from a fixed 60 ms lead - the old scheme - left audio a steady
// 60 ms plus the output device's latency behind the picture, and every late
// burst or host/client clock drift pushed it further behind, never back.
export class WtAudio {
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  private decoder: AudioDecoder | null = null;
  private queue: { data: AudioData }[] = [];
  private nextStartUs = 0;
  readonly root: HTMLAudioElement;
  /** Host capture clock -> local performance.now() µs offset, and the
   *  video's capture->present age in ms: the two things lip sync needs. */
  private clockOffsetUs: () => number | null = () => null;
  private videoAgeMs: () => number | null = () => null;
  /** Packets dropped or gaps inserted to hold sync (diagnostics). */
  syncDrops = 0;
  syncGaps = 0;

  /** Enable lip sync against the video path. */
  setSync(clockOffsetUs: () => number | null, videoAgeMs: () => number | null): void {
    this.clockOffsetUs = clockOffsetUs;
    this.videoAgeMs = videoAgeMs;
  }

  constructor() {
    const el = document.createElement("audio");
    el.autoplay = true;
    this.root = el;
  }

  /** Call from a user-gesture handler: iOS Safari only creates/resumes an
   *  AudioContext there. Safe to call repeatedly; the context survives
   *  session redials for the life of the page. */
  prime(): void {
    this.start();
    if (!this.ctx) {
      this.ctx = new AudioContext();
      this.gain = this.ctx.createGain();
      this.gain.connect(this.ctx.destination);
      this.gain.gain.value = 1;
      this.decoded = 0;
    }
    void this.ctx.resume();
  }

  setVolume(volume: number, muted: boolean): void {
    if (this.gain) this.gain.gain.value = muted ? 0 : Math.max(0, Math.min(1, volume));
  }

  start(): void {
    // Decoder only. The AudioContext is born in prime() - inside a user
    // gesture - because browsers suspend contexts created outside one, and a
    // suspended context swallows every decoded sample (read: "no audio").
    if (this.decoder) return;
    this.decoder = new AudioDecoder({
      output: (data) => this.enqueue(data),
      error: (e) => console.warn("wt audio decoder error:", e.message),
    });
    this.decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  }

  stop(): void {
    // Keep the context alive across redials - recreating it outside a gesture
    // is exactly the suspended-context trap. Per-session state only.
    try { this.decoder?.close(); } catch { /* already closed */ }
    this.decoder = null;
    for (const q of this.queue) q.data.close();
    this.queue = [];
    this.nextStartUs = 0;
    this.lastPtsUs = null;
  }

  /** One Opus packet from a WT audio datagram (payload after the 13-byte header). */
  push(opus: Uint8Array, ptsUs: number): void {
    if (!this.decoder || this.decoder.state !== "configured") return;
    // A timestamp jump (clock re-base, host restart mid-page) must reach the
    // decoder as a new base: WebCodecs AudioDecoder counts output timestamps
    // on from its first input, so without a reset every later packet keeps
    // the old base and lip sync is scheduled against the wrong clock.
    if (this.lastPtsUs !== null && Math.abs(ptsUs - this.lastPtsUs) > 1_000_000) {
      this.decoder.reset();
      this.decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
      for (const q of this.queue) q.data.close();
      this.queue = [];
      this.nextStartUs = 0;
    }
    this.lastPtsUs = ptsUs;
    this.decoder.decode(new EncodedAudioChunk({
      type: "key",
      timestamp: ptsUs,
      data: opus,
    }));
  }

  private decoded = 0;
  private lastPtsUs: number | null = null;

  private enqueue(data: AudioData): void {
    if (!this.ctx || this.ctx.state !== "running") {
      // Not primed (or suspended): drop rather than queue into the void.
      data.close();
      return;
    }
    this.decoded++;
    if (this.decoded === 1) console.info("wt audio: samples reaching the speakers");
    this.queue.push({ data });
    void this.drain();
  }

  private async drain(): Promise<void> {
    const ctx = this.ctx;
    if (!ctx || ctx.state === "suspended") {
      void ctx?.resume().then(() => this.drain());
      return;
    }
    while (this.queue.length > 0) {
      const { data } = this.queue[0]!;
      const frames = data.numberOfFrames;
      const nowUs = ctx.currentTime * 1e6;
      if (this.nextStartUs === 0 || this.nextStartUs < nowUs - 200_000) {
        this.nextStartUs = nowUs + 60_000; // fresh stream or long gap: 60 ms lead
      }
      const target = this.syncedStartUs(ctx, data.timestamp, nowUs);
      if (target !== null) {
        const lateUs = this.nextStartUs - target;
        if (Math.abs(lateUs) > 40_000) {
          // Far off (start, redial, a long stall): jump straight to sync.
          this.nextStartUs = target;
        } else if (lateUs > 8_000) {
          // Behind the picture: drop this packet (~10 ms) to catch up.
          this.queue.shift();
          data.close();
          this.syncDrops++;
          continue;
        } else if (lateUs < -8_000) {
          // Ahead of the picture: leave a short gap.
          this.nextStartUs = target;
          this.syncGaps++;
        }
      }
      const startUs = Math.max(this.nextStartUs, nowUs);
      if (startUs - nowUs > 250_000) {
        // >250 ms buffered: drop oldest rather than growing the delay forever
        while (this.queue.length > 8) this.queue.shift()!.data.close();
        return;
      }
      this.queue.shift();
      const buf = ctx.createBuffer(data.numberOfChannels, frames, 48000);
      for (let ch = 0; ch < data.numberOfChannels; ch++) {
        const dst = new Float32Array(frames);
        data.copyTo(dst.buffer, { format: "f32-planar", planeIndex: ch, frameCount: frames });
        buf.copyToChannel(dst, ch);
      }
      const src = ctx.createBufferSource();
      src.buffer = buf;
      src.connect(this.gain!);
      src.start(startUs / 1e6);
      this.recordAge(ctx, data.timestamp, startUs);
      this.nextStartUs = startUs + frames * 1e6 / 48000;
      data.close();
    }
  }

  /**
   * When (AudioContext µs) to start the packet captured at `ptsUs` so it is
   * heard as its frame is seen, or null without clock sync.
   *
   * Seen: capture + the video's measured capture->present age + ~half a
   * display refresh of scanout. Heard: the context time `getOutputTimestamp`
   * says is audible at a given moment, which already carries the output
   * device's latency (tens of ms built-in, 150 ms+ on Bluetooth).
   * A target already in the past means audio cannot be as early as the
   * picture; it is clamped to "now" and plays a little late rather than not
   * at all. An answer outside a second either way is a clock not yet
   * synced, not a schedule, and is ignored.
   */
  /** Heard age of the packet just scheduled (ms after capture), next to the
   *  video's seen age: the lip-sync error made measurable. Published on
   *  `window.__inphaseAudio` for the HUD and the test harness. */
  private heardAgeEma = 0;
  private recordAge(ctx: AudioContext, ptsUs: number, startUs: number): void {
    const offset = this.clockOffsetUs();
    const stamp = ctx.getOutputTimestamp?.();
    const g = globalThis as unknown as Record<string, unknown>;
    // Why a sample could not be measured, published instead of silence: an
    // unmeasurable lip-sync path otherwise looks exactly like a synced one.
    const why = (reason: string, extra: Record<string, unknown> = {}) => {
      g.__inphaseAudio = { ...(g.__inphaseAudio as object | undefined), reason, ...extra };
    };
    if (offset === null) return why("no clock sync yet");
    if (!stamp?.performanceTime) return why("no output timestamp");
    const outMs = ((ctx as AudioContext & { outputLatency?: number }).outputLatency || ctx.baseLatency || 0) * 1000;
    // getOutputTimestamp pairs a performance time with the context time
    // being HEARD at that moment, so this mapping already includes the output
    // device's latency - adding outputLatency again double-counted it.
    const heardPerfMs = stamp.performanceTime + (startUs / 1e6 - (stamp.contextTime ?? 0)) * 1000;
    const age = heardPerfMs - (ptsUs + offset) / 1000;
    if (!(age > -1000 && age < 5000)) return why("audio clock not on the video clock", { rawAgeMs: Math.round(age), ptsUs });
    this.heardAgeEma = this.heardAgeEma === 0 ? age : 0.95 * this.heardAgeEma + 0.05 * age;
    const video = this.videoAgeMs();
    g.__inphaseAudio = {
      heardAgeMs: Math.round(this.heardAgeEma * 10) / 10,
      seenAgeMs: video === null ? null : Math.round((video + 8) * 10) / 10,
      outputLatencyMs: Math.round(outMs * 10) / 10,
      syncDrops: this.syncDrops,
      syncGaps: this.syncGaps,
      reason: null,
    };
  }

  private syncedStartUs(ctx: AudioContext, ptsUs: number, nowUs: number): number | null {
    const offset = this.clockOffsetUs();
    const ageMs = this.videoAgeMs();
    const stamp = ctx.getOutputTimestamp?.();
    if (offset === null || ageMs === null || !(ageMs > 0) || !stamp?.performanceTime) return null;
    return lipSyncStartUs({
      ptsUs,
      offsetUs: offset,
      videoAgeMs: ageMs,
      contextTimeS: stamp.contextTime ?? 0,
      performanceTimeMs: stamp.performanceTime,
      nowUs,
    });
  }
}

/** Pure core of {@link WtAudio}'s lip sync: the AudioContext start time (µs)
 *  at which a packet captured at `ptsUs` (host clock) is heard as its frame
 *  is seen, or null when the answer is implausible (clock not synced). */
export function lipSyncStartUs(a: {
  ptsUs: number;
  offsetUs: number;
  videoAgeMs: number;
  /** From getOutputTimestamp: the context time audible at performanceTimeMs. */
  contextTimeS: number;
  performanceTimeMs: number;
  nowUs: number;
}): number | null {
  // ~half a display refresh of scanout after the canvas draw.
  const heardAtPerfMs = (a.ptsUs + a.offsetUs) / 1000 + a.videoAgeMs + 8;
  const startUs = (a.contextTimeS + (heardAtPerfMs - a.performanceTimeMs) / 1000) * 1e6;
  if (Math.abs(startUs - a.nowUs) > 1_000_000) return null;
  return Math.max(startUs, a.nowUs + 5_000);
}
