// WebGL2 presentation of decoded VideoFrames.
//
// iPhone Safari 27 (2026-09-24, bare page, 120 fps HEVC loop, one draw per
// refresh): a 2D canvas drawImage showed ~105 fps at 1440p and 52 at 4K, and
// the page's own refresh rate fell with it; texImage2D into WebGL showed a
// steady 120 at both with the page refreshing at 120 Hz. The browser does the
// YUV->RGB conversion on upload either way; the 2D path's extra composite is
// what costs.
//
// WebKit only by default. Chromium's 2D canvas already draws VideoFrames
// without the extra cost, and WebGL there froze the page: headless Chromium
// on Linux (RADV) stalled ~30 s into every run with WebGL, never with 2D
// (A/B 2026-09-24).
//
// Only a hardware context is used: with software GL (SwiftShader, llvmpipe)
// every upload is a CPU copy of the whole frame, far slower than the 2D path.

/** Which renderer to start with. `?render=gl` / `?render=2d` override;
 *  otherwise WebGL for WebKit (Safari, and every iOS browser), 2D elsewhere. */
export function preferWebGl(userAgent: string, search: string): boolean {
  const forced = new URLSearchParams(search).get("render");
  if (forced === "gl") return true;
  if (forced === "2d") return false;
  return /AppleWebKit\//.test(userAgent) && !/(Chrome|Chromium|Edg)\//.test(userAgent);
}

const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  vec2 v = vec2(gl_VertexID & 1, gl_VertexID >> 1);
  uv = vec2(v.x, 1.0 - v.y);
  gl_Position = vec4(v * 2.0 - 1.0, 0.0, 1.0);
}`;

const FRAGMENT = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D frame;
out vec4 color;
void main() { color = texture(frame, uv); }`;

export class GlRenderer {
  private lost = false;

  private constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly gl: WebGL2RenderingContext,
  ) {
    canvas.addEventListener("webglcontextlost", (e) => {
      e.preventDefault();
      this.lost = true;
    });
  }

  /** A renderer on `canvas`, or null when WebGL2 is unavailable (the canvas
   *  is then still free for a 2D context). */
  static create(canvas: HTMLCanvasElement): GlRenderer | null {
    const gl = canvas.getContext("webgl2", {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      premultipliedAlpha: false,
      preserveDrawingBuffer: false,
      failIfMajorPerformanceCaveat: true,
    });
    if (gl === null) return null;
    const shader = (type: number, src: string): WebGLShader | null => {
      const s = gl.createShader(type);
      if (s === null) return null;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      return gl.getShaderParameter(s, gl.COMPILE_STATUS) ? s : null;
    };
    const vs = shader(gl.VERTEX_SHADER, VERTEX);
    const fs = shader(gl.FRAGMENT_SHADER, FRAGMENT);
    const program = gl.createProgram();
    if (vs === null || fs === null || program === null) return null;
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) return null;
    gl.useProgram(program);
    gl.bindTexture(gl.TEXTURE_2D, gl.createTexture());
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return new GlRenderer(canvas, gl);
  }

  /** The context is gone (GPU reset, memory pressure); the caller should
   *  move to a fresh 2D canvas. */
  get isLost(): boolean {
    return this.lost || this.gl.isContextLost();
  }

  /** Upload and draw one frame to the full canvas. Throws when the browser
   *  cannot upload a VideoFrame (the caller falls back to 2D). */
  draw(vf: VideoFrame): void {
    const gl = this.gl;
    gl.viewport(0, 0, gl.drawingBufferWidth, gl.drawingBufferHeight);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, vf as unknown as TexImageSource);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  /** Lit pixels among 64 sampled along the middle row of the frame just
   *  drawn (call in the same task as draw; one readback). Zero means a no-op
   *  upload - or a dark frame; the caller tells those apart. */
  litSamples(): number {
    const gl = this.gl;
    const w = gl.drawingBufferWidth;
    const row = new Uint8Array(w * 4);
    gl.readPixels(0, gl.drawingBufferHeight >> 1, w, 1, gl.RGBA, gl.UNSIGNED_BYTE, row);
    let lit = 0;
    for (let i = 0; i < 64; i++) {
      const o = Math.floor(((i + 0.5) * w) / 64) * 4;
      if ((row[o] ?? 0) | (row[o + 1] ?? 0) | (row[o + 2] ?? 0)) lit++;
    }
    return lit;
  }

  get element(): HTMLCanvasElement {
    return this.canvas;
  }
}
