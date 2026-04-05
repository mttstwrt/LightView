// ---------------------------------------------------------------------------
// WebGL grid renderer — texture pool, instanced drawing (single draw call)
// ---------------------------------------------------------------------------
//
// Architecture (per docs/WebGL.md):
//   1. Texture Pool — pre-allocated fixed-size slots with texSubImage2D uploads
//   2. Instanced Drawing — one quad, one drawElementsInstanced call for all items
//   3. Zero-Copy Loading — ImageBitmap uploaded directly, no intermediate copies
//   4. Float32Array — all coordinate math uses typed arrays to avoid jitter
//   5. Culling — only visible items are sent to the GPU (handled upstream)

import type { GridRenderer, GridLayout, GridItem, HitTestResult } from "./types";

// Pool configuration
const SLOT_SIZE = 512; // pixels per slot (uniform size)

// Visual constants
const SELECTION_COLOR = [59 / 255, 130 / 255, 246 / 255, 0.35] as const;
const SELECTION_BORDER_COLOR = [59 / 255, 130 / 255, 246 / 255, 1.0] as const;

// ---------------------------------------------------------------------------
// Instanced thumbnail shaders
// ---------------------------------------------------------------------------

const VERT_SRC = `#version 100
// Per-vertex: unit quad corner (0,0)→(1,1)
attribute vec2 a_quad;

// Per-instance (divisor = 1)
attribute vec2 a_offset;   // pixel position
attribute vec4 a_uv;       // (u0, v0, u1, v1) in pool texture
attribute vec2 a_flags;    // (hasImage, selected)

uniform vec2 u_resolution;
uniform float u_cellSize;

varying vec2 v_texcoord;
varying float v_hasImage;
varying float v_selected;

void main() {
  vec2 pos = a_offset + a_quad * u_cellSize;
  vec2 clip = (pos / u_resolution) * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);

  v_texcoord = mix(a_uv.xy, a_uv.zw, a_quad);
  v_hasImage = a_flags.x;
  v_selected = a_flags.y;
}
`;

const FRAG_SRC = `#version 100
precision mediump float;

varying vec2 v_texcoord;
varying float v_hasImage;
varying float v_selected;

uniform sampler2D u_pool;
uniform vec4 u_skeletonColor;
uniform vec4 u_selectionColor;

void main() {
  vec4 color;
  if (v_hasImage > 0.5) {
    color = texture2D(u_pool, v_texcoord);
  } else {
    color = u_skeletonColor;
  }

  if (v_selected > 0.5) {
    color = mix(color, u_selectionColor, u_selectionColor.a);
  }

  gl_FragColor = vec4(color.rgb, 1.0);
}
`;

// ---------------------------------------------------------------------------
// Overlay shaders (non-instanced — selection borders, check marks, badges)
// ---------------------------------------------------------------------------

const OVERLAY_VERT_SRC = `#version 100
attribute vec2 a_position;
attribute vec4 a_color;

uniform vec2 u_resolution;

varying vec4 v_color;

void main() {
  vec2 clip = (a_position / u_resolution) * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  v_color = a_color;
}
`;

const OVERLAY_FRAG_SRC = `#version 100
precision mediump float;
varying vec4 v_color;
void main() {
  gl_FragColor = v_color;
}
`;

// ---------------------------------------------------------------------------
// Instance buffer layout: 8 floats (32 bytes) per instance
//   [offsetX, offsetY, u0, v0, u1, v1, hasImage, selected]
// ---------------------------------------------------------------------------

const FLOATS_PER_INSTANCE = 8;
const BYTES_PER_INSTANCE = FLOATS_PER_INSTANCE * 4;

interface PoolSlot {
  index: number;
  u0: number;
  v0: number;
  u1: number;
  v1: number;
}

export class WebGLRenderer implements GridRenderer {
  readonly canvas: HTMLCanvasElement;
  private gl: WebGLRenderingContext;
  private ext: ANGLE_instanced_arrays;
  private dpr = 1;

  // Main instanced program
  private program: WebGLProgram;
  private a_quad: number;
  private a_offset: number;
  private a_uv: number;
  private a_flags: number;
  private u_resolution: WebGLUniformLocation;
  private u_cellSize: WebGLUniformLocation;
  private u_skeletonColor: WebGLUniformLocation;
  private u_selectionColor: WebGLUniformLocation;

  // Overlay program
  private overlayProgram: WebGLProgram;
  private ov_position: number;
  private ov_color: number;
  private ov_resolution: WebGLUniformLocation;

  // Static quad geometry (shared across all instances)
  private quadVBO: WebGLBuffer;
  private quadIBO: WebGLBuffer;

  // Instance buffer (rebuilt each frame)
  private instanceVBO: WebGLBuffer;
  private instanceData: Float32Array;
  private maxInstances: number;

  // Overlay buffers
  private overlayPosBuffer: WebGLBuffer;
  private overlayColorBuffer: WebGLBuffer;
  private overlayIndexBuffer: WebGLBuffer;

  // Texture pool — pre-allocated GPU memory with fixed-size slots
  private poolTexture: WebGLTexture;
  private poolSize: number;
  private slotsPerRow: number;
  private totalSlots: number;
  private slotMap = new Map<string, PoolSlot>();
  private freeSlots: number[] = [];
  private lruOrder: string[] = [];

  constructor() {
    this.canvas = document.createElement("canvas");
    this.canvas.style.display = "block";
    this.canvas.style.width = "100%";
    this.canvas.style.height = "100%";

    const gl = this.canvas.getContext("webgl", {
      alpha: false,
      antialias: false,
      premultipliedAlpha: false,
      preserveDrawingBuffer: true,
    });
    if (!gl) throw new Error("WebGL context unavailable");
    this.gl = gl;

    // Require ANGLE_instanced_arrays for single-call instanced rendering
    const ext = gl.getExtension("ANGLE_instanced_arrays");
    if (!ext) throw new Error("ANGLE_instanced_arrays extension unavailable");
    this.ext = ext;

    // ---------------------------------------------------------------------------
    // Texture pool sizing — use largest texture the GPU supports (capped at 16384)
    // 16384/512 = 32 per row → 1024 slots, well above ImageLoader's 300 cache
    // ---------------------------------------------------------------------------
    const maxTexSize = gl.getParameter(gl.MAX_TEXTURE_SIZE) as number;
    this.poolSize = Math.min(maxTexSize, 16384);
    this.slotsPerRow = Math.floor(this.poolSize / SLOT_SIZE);
    this.totalSlots = this.slotsPerRow * this.slotsPerRow;

    // Pre-allocate instance buffer (grows if needed)
    this.maxInstances = 2000;
    this.instanceData = new Float32Array(this.maxInstances * FLOATS_PER_INSTANCE);

    // ---------------------------------------------------------------------------
    // Compile shader programs
    // ---------------------------------------------------------------------------
    this.program = this.createProgram(VERT_SRC, FRAG_SRC);
    this.a_quad = gl.getAttribLocation(this.program, "a_quad");
    this.a_offset = gl.getAttribLocation(this.program, "a_offset");
    this.a_uv = gl.getAttribLocation(this.program, "a_uv");
    this.a_flags = gl.getAttribLocation(this.program, "a_flags");
    this.u_resolution = gl.getUniformLocation(this.program, "u_resolution")!;
    this.u_cellSize = gl.getUniformLocation(this.program, "u_cellSize")!;
    this.u_skeletonColor = gl.getUniformLocation(this.program, "u_skeletonColor")!;
    this.u_selectionColor = gl.getUniformLocation(this.program, "u_selectionColor")!;

    this.overlayProgram = this.createProgram(OVERLAY_VERT_SRC, OVERLAY_FRAG_SRC);
    this.ov_position = gl.getAttribLocation(this.overlayProgram, "a_position");
    this.ov_color = gl.getAttribLocation(this.overlayProgram, "a_color");
    this.ov_resolution = gl.getUniformLocation(this.overlayProgram, "u_resolution")!;

    // ---------------------------------------------------------------------------
    // Static quad geometry — one unit square, reused for every instance
    // ---------------------------------------------------------------------------
    this.quadVBO = gl.createBuffer()!;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.quadVBO);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([
      0, 0, // top-left
      1, 0, // top-right
      1, 1, // bottom-right
      0, 1, // bottom-left
    ]), gl.STATIC_DRAW);

    this.quadIBO = gl.createBuffer()!;
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.quadIBO);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, new Uint16Array([
      0, 1, 2, 0, 2, 3,
    ]), gl.STATIC_DRAW);

    // Dynamic instance buffer
    this.instanceVBO = gl.createBuffer()!;

    // Overlay buffers
    this.overlayPosBuffer = gl.createBuffer()!;
    this.overlayColorBuffer = gl.createBuffer()!;
    this.overlayIndexBuffer = gl.createBuffer()!;

    // ---------------------------------------------------------------------------
    // Pre-allocate texture pool — reserves GPU memory, no pixel upload
    // ---------------------------------------------------------------------------
    this.poolTexture = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.poolTexture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texImage2D(
      gl.TEXTURE_2D, 0, gl.RGBA,
      this.poolSize, this.poolSize, 0,
      gl.RGBA, gl.UNSIGNED_BYTE, null,
    );

    // Build free-slot stack (pop gives lowest indices first)
    for (let i = this.totalSlots - 1; i >= 0; i--) {
      this.freeSlots.push(i);
    }

    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }

  // -------------------------------------------------------------------------
  // GridRenderer interface
  // -------------------------------------------------------------------------

  resize(width: number, height: number, dpr: number): boolean {
    const pw = Math.round(width * dpr);
    const ph = Math.round(height * dpr);
    if (this.canvas.width === pw && this.canvas.height === ph && this.dpr === dpr) {
      return false;
    }
    this.canvas.width = pw;
    this.canvas.height = ph;
    this.canvas.style.width = `${width}px`;
    this.canvas.style.height = `${height}px`;
    this.dpr = dpr;
    this.gl.viewport(0, 0, pw, ph);
    return true;
  }

  render(layout: GridLayout, items: GridItem[]): void {
    const gl = this.gl;
    const ext = this.ext;
    const { cols, cellSize, gap, startRow } = layout;
    const n = items.length;

    gl.clearColor(10 / 255, 10 / 255, 10 / 255, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);

    if (n === 0) return;

    // Grow instance buffer if the visible set exceeds current capacity
    if (n > this.maxInstances) {
      this.maxInstances = Math.ceil(n * 1.5);
      this.instanceData = new Float32Array(this.maxInstances * FLOATS_PER_INSTANCE);
    }

    // -----------------------------------------------------------------
    // Fill instance data (Float32Array — no jitter during slow scrolls)
    // -----------------------------------------------------------------
    const data = this.instanceData;
    let hasOverlays = false;

    for (let i = 0; i < n; i++) {
      const item = items[i];
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;

      const base = i * FLOATS_PER_INSTANCE;
      data[base] = col * (cellSize + gap);     // offsetX
      data[base + 1] = row * (cellSize + gap); // offsetY

      const slot = this.slotMap.get(item.path);
      if (slot) {
        data[base + 2] = slot.u0;
        data[base + 3] = slot.v0;
        data[base + 4] = slot.u1;
        data[base + 5] = slot.v1;
        data[base + 6] = 1; // hasImage
      } else {
        data[base + 2] = 0;
        data[base + 3] = 0;
        data[base + 4] = 0;
        data[base + 5] = 0;
        data[base + 6] = 0;
      }
      data[base + 7] = item.selected ? 1 : 0;

      if (item.selected || item.badge) hasOverlays = true;
    }

    // -----------------------------------------------------------------
    // Instanced draw — one call renders the entire grid
    // -----------------------------------------------------------------
    gl.useProgram(this.program);

    const resW = this.canvas.width / this.dpr;
    const resH = this.canvas.height / this.dpr;
    gl.uniform2f(this.u_resolution, resW, resH);
    gl.uniform1f(this.u_cellSize, cellSize);
    gl.uniform4f(this.u_skeletonColor, 26 / 255, 26 / 255, 26 / 255, 1);
    gl.uniform4f(this.u_selectionColor, ...SELECTION_COLOR);

    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.poolTexture);

    // Bind static quad (per-vertex, divisor 0)
    gl.bindBuffer(gl.ARRAY_BUFFER, this.quadVBO);
    gl.enableVertexAttribArray(this.a_quad);
    gl.vertexAttribPointer(this.a_quad, 2, gl.FLOAT, false, 0, 0);
    ext.vertexAttribDivisorANGLE(this.a_quad, 0);

    // Upload instance data (sub-array view avoids copying)
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceVBO);
    gl.bufferData(gl.ARRAY_BUFFER, data.subarray(0, n * FLOATS_PER_INSTANCE), gl.DYNAMIC_DRAW);

    const stride = BYTES_PER_INSTANCE;

    // a_offset: vec2 at byte 0
    gl.enableVertexAttribArray(this.a_offset);
    gl.vertexAttribPointer(this.a_offset, 2, gl.FLOAT, false, stride, 0);
    ext.vertexAttribDivisorANGLE(this.a_offset, 1);

    // a_uv: vec4 at byte 8
    gl.enableVertexAttribArray(this.a_uv);
    gl.vertexAttribPointer(this.a_uv, 4, gl.FLOAT, false, stride, 8);
    ext.vertexAttribDivisorANGLE(this.a_uv, 1);

    // a_flags: vec2 at byte 24
    gl.enableVertexAttribArray(this.a_flags);
    gl.vertexAttribPointer(this.a_flags, 2, gl.FLOAT, false, stride, 24);
    ext.vertexAttribDivisorANGLE(this.a_flags, 1);

    // One call to rule them all
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.quadIBO);
    ext.drawElementsInstancedANGLE(gl.TRIANGLES, 6, gl.UNSIGNED_SHORT, 0, n);

    // Reset divisors so overlay drawing isn't affected
    ext.vertexAttribDivisorANGLE(this.a_offset, 0);
    ext.vertexAttribDivisorANGLE(this.a_uv, 0);
    ext.vertexAttribDivisorANGLE(this.a_flags, 0);

    // -----------------------------------------------------------------
    // Overlay pass (selection borders, check marks, badge backgrounds)
    // -----------------------------------------------------------------
    if (hasOverlays) {
      this.drawOverlays(layout, items);
    }
  }

  hitTest(x: number, y: number, layout: GridLayout, items: GridItem[]): HitTestResult {
    const { cols, cellSize, gap, startRow } = layout;

    for (const item of items) {
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const cx = col * (cellSize + gap);
      const cy = row * (cellSize + gap);

      if (x >= cx && x < cx + cellSize && y >= cy && y < cy + cellSize) {
        return { index: item.index, path: item.path };
      }
    }

    return { index: -1, path: null };
  }

  imageLoaded(path: string, img: HTMLImageElement | ImageBitmap): void {
    // Short-circuit: slot already uploaded — skip redundant GPU work
    if (this.slotMap.has(path)) return;

    const gl = this.gl;

    // Allocate a pool slot
    if (this.freeSlots.length === 0) {
      this.evictLRU();
    }
    if (this.freeSlots.length === 0) return;

    const index = this.freeSlots.pop()!;
    const slotCol = index % this.slotsPerRow;
    const slotRow = Math.floor(index / this.slotsPerRow);
    const px = slotCol * SLOT_SIZE;
    const py = slotRow * SLOT_SIZE;

    // Compute UVs from actual image dimensions (not full slot) so the
    // shader never samples uninitialized pixels at the slot edges.
    // Thumbnails are 400x400; slots are 512x512.
    const imgW = Math.min(img.width, SLOT_SIZE);
    const imgH = Math.min(img.height, SLOT_SIZE);

    const slot: PoolSlot = {
      index,
      u0: px / this.poolSize,
      v0: py / this.poolSize,
      u1: (px + imgW) / this.poolSize,
      v1: (py + imgH) / this.poolSize,
    };
    this.slotMap.set(path, slot);

    // Touch LRU
    this.lruOrder.push(path);

    // Non-blocking upload via texSubImage2D — no GPU memory reallocation
    gl.bindTexture(gl.TEXTURE_2D, this.poolTexture);
    gl.texSubImage2D(
      gl.TEXTURE_2D, 0,
      px, py,
      gl.RGBA, gl.UNSIGNED_BYTE,
      img,
    );
  }

  evictImage(path: string): void {
    const slot = this.slotMap.get(path);
    if (slot) {
      this.freeSlots.push(slot.index);
      this.slotMap.delete(path);
      const lruIdx = this.lruOrder.indexOf(path);
      if (lruIdx >= 0) this.lruOrder.splice(lruIdx, 1);
    }
  }

  destroy(): void {
    const gl = this.gl;
    gl.deleteProgram(this.program);
    gl.deleteProgram(this.overlayProgram);
    gl.deleteBuffer(this.quadVBO);
    gl.deleteBuffer(this.quadIBO);
    gl.deleteBuffer(this.instanceVBO);
    gl.deleteBuffer(this.overlayPosBuffer);
    gl.deleteBuffer(this.overlayColorBuffer);
    gl.deleteBuffer(this.overlayIndexBuffer);
    gl.deleteTexture(this.poolTexture);
    this.slotMap.clear();
    this.lruOrder.length = 0;
  }

  // -------------------------------------------------------------------------
  // Private — pool management
  // -------------------------------------------------------------------------

  private evictLRU() {
    if (this.lruOrder.length === 0) return;
    const oldest = this.lruOrder.shift()!;
    this.evictImage(oldest);
  }

  // -------------------------------------------------------------------------
  // Private — overlay rendering (selection borders, check marks, badges)
  // -------------------------------------------------------------------------

  private drawOverlays(layout: GridLayout, items: GridItem[]) {
    const gl = this.gl;
    const { cols, cellSize, gap, startRow } = layout;

    gl.useProgram(this.overlayProgram);
    gl.uniform2f(this.ov_resolution, this.canvas.width / this.dpr, this.canvas.height / this.dpr);

    const positions: number[] = [];
    const colors: number[] = [];
    const indices: number[] = [];
    let vi = 0;

    for (const item of items) {
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const x = col * (cellSize + gap);
      const y = row * (cellSize + gap);

      // Selection: border quads + check mark
      if (item.selected) {
        const bw = 2;
        const bc = [...SELECTION_BORDER_COLOR];

        // Top
        this.pushQuad(positions, colors, indices, vi, x, y, cellSize, bw, bc);
        vi += 4;
        // Bottom
        this.pushQuad(positions, colors, indices, vi, x, y + cellSize - bw, cellSize, bw, bc);
        vi += 4;
        // Left
        this.pushQuad(positions, colors, indices, vi, x, y, bw, cellSize, bc);
        vi += 4;
        // Right
        this.pushQuad(positions, colors, indices, vi, x + cellSize - bw, y, bw, cellSize, bc);
        vi += 4;

        // Check mark background
        const cs = 20;
        const cx = x + 12 - cs / 2;
        const cy = y + 12 - cs / 2;
        this.pushQuad(positions, colors, indices, vi, cx, cy, cs, cs, bc);
        vi += 4;
      }

      // Badge background (VID / GIF)
      if (item.badge) {
        const bw = item.badge === "VID" ? 34 : 30;
        const bh = 18;
        const bx = x + cellSize - bw - 6;
        const by = y + cellSize - bh - 6;
        this.pushQuad(positions, colors, indices, vi, bx, by, bw, bh, [0, 0, 0, 0.5]);
        vi += 4;
      }
    }

    if (positions.length === 0) return;

    const posArr = new Float32Array(positions);
    const colArr = new Float32Array(colors);
    const idxArr = new Uint16Array(indices);

    gl.bindBuffer(gl.ARRAY_BUFFER, this.overlayPosBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, posArr, gl.DYNAMIC_DRAW);
    gl.enableVertexAttribArray(this.ov_position);
    gl.vertexAttribPointer(this.ov_position, 2, gl.FLOAT, false, 0, 0);

    gl.bindBuffer(gl.ARRAY_BUFFER, this.overlayColorBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, colArr, gl.DYNAMIC_DRAW);
    gl.enableVertexAttribArray(this.ov_color);
    gl.vertexAttribPointer(this.ov_color, 4, gl.FLOAT, false, 0, 0);

    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.overlayIndexBuffer);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, idxArr, gl.DYNAMIC_DRAW);

    gl.drawElements(gl.TRIANGLES, indices.length, gl.UNSIGNED_SHORT, 0);
  }

  private pushQuad(
    positions: number[], colors: number[], indices: number[],
    vi: number,
    x: number, y: number, w: number, h: number,
    color: number[],
  ) {
    positions.push(x, y, x + w, y, x + w, y + h, x, y + h);
    for (let i = 0; i < 4; i++) colors.push(...color);
    indices.push(vi, vi + 1, vi + 2, vi, vi + 2, vi + 3);
  }

  // -------------------------------------------------------------------------
  // Private — shader compilation
  // -------------------------------------------------------------------------

  private createProgram(vertSrc: string, fragSrc: string): WebGLProgram {
    const gl = this.gl;

    const vert = gl.createShader(gl.VERTEX_SHADER)!;
    gl.shaderSource(vert, vertSrc);
    gl.compileShader(vert);
    if (!gl.getShaderParameter(vert, gl.COMPILE_STATUS)) {
      throw new Error("Vertex shader error: " + gl.getShaderInfoLog(vert));
    }

    const frag = gl.createShader(gl.FRAGMENT_SHADER)!;
    gl.shaderSource(frag, fragSrc);
    gl.compileShader(frag);
    if (!gl.getShaderParameter(frag, gl.COMPILE_STATUS)) {
      throw new Error("Fragment shader error: " + gl.getShaderInfoLog(frag));
    }

    const prog = gl.createProgram()!;
    gl.attachShader(prog, vert);
    gl.attachShader(prog, frag);
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
      throw new Error("Program link error: " + gl.getProgramInfoLog(prog));
    }

    gl.deleteShader(vert);
    gl.deleteShader(frag);
    return prog;
  }
}
