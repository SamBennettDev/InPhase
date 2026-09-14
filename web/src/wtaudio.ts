// WT audio playback (§11-on-WT, ADR-0011 WT-only): Opus datagrams from the
// host are decoded with WebCodecs AudioDecoder and played on an
// AudioContext. A small jitter ring (target ~60 ms) absorbs network jitter;
// under-runs play silence rather than freezing, by design.
export class WtAudio {
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  private decoder: AudioDecoder | null = null;
  private queue: { data: AudioData }[] = [];
  private nextStartUs = 0;
  readonly root: HTMLAudioElement;

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
  }

  /** One Opus packet from a WT audio datagram (payload after the 13-byte header). */
  push(opus: Uint8Array, ptsUs: number): void {
    if (!this.decoder || this.decoder.state !== "configured") return;
    this.decoder.decode(new EncodedAudioChunk({
      type: "key",
      timestamp: ptsUs,
      data: opus,
    }));
  }

  private decoded = 0;

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
      this.nextStartUs = startUs + frames * 1e6 / 48000;
      data.close();
    }
  }
}
