// ---------------------------------------------------------------------------
// WebGL grid renderer — texture atlas, batched draw calls
// ---------------------------------------------------------------------------
//
// Uses a texture atlas to batch all thumbnail draws into a single draw call.
// Each thumbnail is uploaded to a region of a large atlas texture. The vertex
// shader positions quads and the fragment shader samples the atlas with
// per-quad UV offsets.

import type { GridRenderer, GridLayout, GridItem, HitTestResult } from "./types";

// Atlas configuration
const ATLAS_SIZE = 4096; // pixels per side
const TILE_SIZE = 256;   // pixels per tile (thumbnails resized to fit)
const TILES_PER_ROW = Math.floor(ATLAS_SIZE / TILE_SIZE);
const MAX_TILES = TILES_PER_ROW * TILES_PER_ROW; // 256 tiles in a 4096x4096 atlas

// Visual constants (match Canvas2D)
const SELECTION_COLOR = [59 / 255, 130 / 255, 246 / 255, 0.35] as const;

// Shaders
const VERT_SRC = `#version 100
attribute vec2 a_position;
attribute vec2 a_texcoord;
attribute float a_hasImage;
attribute float a_selected;

uniform vec2 u_resolution;

varying vec2 v_texcoord;
varying float v_hasImage;
varying float v_selected;

void main() {
  // Convert pixel coords to clip space
  vec2 clipSpace = (a_position / u_resolution) * 2.0 - 1.0;
  gl_Position = vec4(clipSpace.x, -clipSpace.y, 0.0, 1.0);

  v_texcoord = a_texcoord;
  v_hasImage = a_hasImage;
  v_selected = a_selected;
}
`;

const FRAG_SRC = `#version 100
precision mediump float;

varying vec2 v_texcoord;
varying float v_hasImage;
varying float v_selected;

uniform sampler2D u_atlas;
uniform vec4 u_skeletonColor;
uniform vec4 u_selectionColor;

void main() {
  vec4 color;
  if (v_hasImage > 0.5) {
    color = texture2D(u_atlas, v_texcoord);
  } else {
    color = u_skeletonColor;
  }

  // Blend selection overlay
  if (v_selected > 0.5) {
    color = mix(color, u_selectionColor, u_selectionColor.a);
  }

  gl_FragColor = vec4(color.rgb, 1.0);
}
`;

// Badge overlay shader (separate pass for text overlays)
const BADGE_VERT_SRC = `#version 100
attribute vec2 a_position;
attribute vec4 a_color;

uniform vec2 u_resolution;

varying vec4 v_color;

void main() {
  vec2 clipSpace = (a_position / u_resolution) * 2.0 - 1.0;
  gl_Position = vec4(clipSpace.x, -clipSpace.y, 0.0, 1.0);
  v_color = a_color;
}
`;

const BADGE_FRAG_SRC = `#version 100
precision mediump float;
varying vec4 v_color;
void main() {
  gl_FragColor = v_color;
}
`;

interface TileSlot {
  /** Index in the atlas grid. */
  slotIndex: number;
  /** Normalized UV coordinates [u0, v0, u1, v1]. */
  uv: [number, number, number, number];
}

export class WebGLRenderer implements GridRenderer {
  readonly canvas: HTMLCanvasElement;
  private gl: WebGLRenderingContext;
  private dpr = 1;

  // Main thumbnail program
  private program: WebGLProgram;
  private posLoc: number;
  private texLoc: number;
  private hasImageLoc: number;
  private selectedLoc: number;
  private resolutionLoc: WebGLUniformLocation;
  private skeletonColorLoc: WebGLUniformLocation;
  private selectionColorLoc: WebGLUniformLocation;

  // Badge overlay program
  private badgeProgram: WebGLProgram;
  private badgePosLoc: number;
  private badgeColorLoc: number;
  private badgeResolutionLoc: WebGLUniformLocation;

  // Buffers
  private posBuffer: WebGLBuffer;
  private texBuffer: WebGLBuffer;
  private hasImageBuffer: WebGLBuffer;
  private selectedBuffer: WebGLBuffer;
  private indexBuffer: WebGLBuffer;
  private badgePosBuffer: WebGLBuffer;
  private badgeColorBuffer: WebGLBuffer;
  private badgeIndexBuffer: WebGLBuffer;

  // Atlas
  private atlas: WebGLTexture;
  private tileMap = new Map<string, TileSlot>();
  private freeTiles: number[] = [];
  private lruOrder: string[] = [];

  // Selection border canvas for rendering text badges
  private badgeCanvas: HTMLCanvasElement;
  private badgeCtx: CanvasRenderingContext2D;
  private badgeTexture: WebGLTexture;

  constructor() {
    this.canvas = document.createElement("canvas");
    this.canvas.style.display = "block";
    this.canvas.style.width = "100%";
    this.canvas.style.height = "100%";

    const gl = this.canvas.getContext("webgl", {
      alpha: false,
      antialias: false,
      premultipliedAlpha: false,
      preserveDrawingBuffer: false,
    });
    if (!gl) throw new Error("WebGL context unavailable");
    this.gl = gl;

    // Compile programs
    this.program = this.createProgram(VERT_SRC, FRAG_SRC);
    this.posLoc = gl.getAttribLocation(this.program, "a_position");
    this.texLoc = gl.getAttribLocation(this.program, "a_texcoord");
    this.hasImageLoc = gl.getAttribLocation(this.program, "a_hasImage");
    this.selectedLoc = gl.getAttribLocation(this.program, "a_selected");
    this.resolutionLoc = gl.getUniformLocation(this.program, "u_resolution")!;
    this.skeletonColorLoc = gl.getUniformLocation(this.program, "u_skeletonColor")!;
    this.selectionColorLoc = gl.getUniformLocation(this.program, "u_selectionColor")!;

    this.badgeProgram = this.createProgram(BADGE_VERT_SRC, BADGE_FRAG_SRC);
    this.badgePosLoc = gl.getAttribLocation(this.badgeProgram, "a_position");
    this.badgeColorLoc = gl.getAttribLocation(this.badgeProgram, "a_color");
    this.badgeResolutionLoc = gl.getUniformLocation(this.badgeProgram, "u_resolution")!;

    // Create buffers
    this.posBuffer = gl.createBuffer()!;
    this.texBuffer = gl.createBuffer()!;
    this.hasImageBuffer = gl.createBuffer()!;
    this.selectedBuffer = gl.createBuffer()!;
    this.indexBuffer = gl.createBuffer()!;
    this.badgePosBuffer = gl.createBuffer()!;
    this.badgeColorBuffer = gl.createBuffer()!;
    this.badgeIndexBuffer = gl.createBuffer()!;

    // Create atlas texture
    this.atlas = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.atlas);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, ATLAS_SIZE, ATLAS_SIZE, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);

    // Initialize free tile list
    for (let i = MAX_TILES - 1; i >= 0; i--) {
      this.freeTiles.push(i);
    }

    // Badge overlay canvas (for text rendering)
    this.badgeCanvas = document.createElement("canvas");
    this.badgeCanvas.width = 64;
    this.badgeCanvas.height = 20;
    this.badgeCtx = this.badgeCanvas.getContext("2d")!;
    this.badgeTexture = gl.createTexture()!;

    // Enable blending
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }

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
    const { cols, cellSize, gap, startRow } = layout;
    const n = items.length;
    if (n === 0) {
      gl.clearColor(10 / 255, 10 / 255, 10 / 255, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      return;
    }

    // Build vertex data: 4 vertices per quad, 6 indices per quad
    const positions = new Float32Array(n * 8);   // 4 verts * 2 components
    const texcoords = new Float32Array(n * 8);
    const hasImage = new Float32Array(n * 4);    // 4 verts * 1
    const selected = new Float32Array(n * 4);
    const indices = new Uint16Array(n * 6);

    // Badge overlay data
    const badgeItems: { x: number; y: number; w: number; h: number; selected: boolean }[] = [];
    const checkItems: { cx: number; cy: number }[] = [];

    for (let i = 0; i < n; i++) {
      const item = items[i];
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const x = col * (cellSize + gap);
      const y = row * (cellSize + gap);

      // Quad positions (px)
      const pi = i * 8;
      positions[pi + 0] = x;            positions[pi + 1] = y;             // top-left
      positions[pi + 2] = x + cellSize; positions[pi + 3] = y;             // top-right
      positions[pi + 4] = x + cellSize; positions[pi + 5] = y + cellSize;  // bottom-right
      positions[pi + 6] = x;            positions[pi + 7] = y + cellSize;  // bottom-left

      // UV coordinates from atlas tile
      const tile = this.tileMap.get(item.path);
      const ti = i * 8;
      if (tile) {
        const [u0, v0, u1, v1] = tile.uv;
        texcoords[ti + 0] = u0; texcoords[ti + 1] = v0;
        texcoords[ti + 2] = u1; texcoords[ti + 3] = v0;
        texcoords[ti + 4] = u1; texcoords[ti + 5] = v1;
        texcoords[ti + 6] = u0; texcoords[ti + 7] = v1;
      }

      // Per-vertex flags
      const hi = i * 4;
      const hasImg = tile ? 1 : 0;
      const sel = item.selected ? 1 : 0;
      hasImage[hi] = hasImage[hi + 1] = hasImage[hi + 2] = hasImage[hi + 3] = hasImg;
      selected[hi] = selected[hi + 1] = selected[hi + 2] = selected[hi + 3] = sel;

      // Indices
      const ii = i * 6;
      const vi = i * 4;
      indices[ii + 0] = vi;     indices[ii + 1] = vi + 1; indices[ii + 2] = vi + 2;
      indices[ii + 3] = vi;     indices[ii + 4] = vi + 2; indices[ii + 5] = vi + 3;

      // Collect badge/selection overlays
      if (item.badge) {
        badgeItems.push({ x, y, w: cellSize, h: cellSize, selected: item.selected });
      }
      if (item.selected) {
        checkItems.push({ cx: x + 12, cy: y + 12 });
      }
    }

    // --- Draw thumbnail quads ---
    gl.clearColor(10 / 255, 10 / 255, 10 / 255, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);

    gl.useProgram(this.program);
    gl.uniform2f(this.resolutionLoc, this.canvas.width / this.dpr, this.canvas.height / this.dpr);
    gl.uniform4f(this.skeletonColorLoc, 26 / 255, 26 / 255, 26 / 255, 1);
    gl.uniform4f(this.selectionColorLoc, ...SELECTION_COLOR);

    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.atlas);

    this.uploadAndBind(this.posBuffer, positions, this.posLoc, 2);
    this.uploadAndBind(this.texBuffer, texcoords, this.texLoc, 2);
    this.uploadAndBind(this.hasImageBuffer, hasImage, this.hasImageLoc, 1);
    this.uploadAndBind(this.selectedBuffer, selected, this.selectedLoc, 1);

    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.indexBuffer);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, indices, gl.DYNAMIC_DRAW);

    gl.drawElements(gl.TRIANGLES, n * 6, gl.UNSIGNED_SHORT, 0);

    // --- Draw selection borders and check marks ---
    if (checkItems.length > 0 || badgeItems.length > 0) {
      this.drawOverlays(layout, items, checkItems, badgeItems);
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
    const gl = this.gl;
    let tile = this.tileMap.get(path);

    if (!tile) {
      // Allocate a tile slot
      if (this.freeTiles.length === 0) {
        // Evict LRU tile
        this.evictLRU();
      }
      if (this.freeTiles.length === 0) return; // shouldn't happen

      const slotIndex = this.freeTiles.pop()!;
      const col = slotIndex % TILES_PER_ROW;
      const row = Math.floor(slotIndex / TILES_PER_ROW);
      const u0 = col * TILE_SIZE / ATLAS_SIZE;
      const v0 = row * TILE_SIZE / ATLAS_SIZE;
      const u1 = (col + 1) * TILE_SIZE / ATLAS_SIZE;
      const v1 = (row + 1) * TILE_SIZE / ATLAS_SIZE;

      tile = { slotIndex, uv: [u0, v0, u1, v1] };
      this.tileMap.set(path, tile);
    }

    // Update LRU order
    const lruIdx = this.lruOrder.indexOf(path);
    if (lruIdx >= 0) this.lruOrder.splice(lruIdx, 1);
    this.lruOrder.push(path);

    // Upload image to atlas tile region
    const col = tile.slotIndex % TILES_PER_ROW;
    const row = Math.floor(tile.slotIndex / TILES_PER_ROW);
    const px = col * TILE_SIZE;
    const py = row * TILE_SIZE;

    gl.bindTexture(gl.TEXTURE_2D, this.atlas);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, px, py, gl.RGBA, gl.UNSIGNED_BYTE, img);
  }

  evictImage(path: string): void {
    const tile = this.tileMap.get(path);
    if (tile) {
      this.freeTiles.push(tile.slotIndex);
      this.tileMap.delete(path);
      const lruIdx = this.lruOrder.indexOf(path);
      if (lruIdx >= 0) this.lruOrder.splice(lruIdx, 1);
    }
  }

  destroy(): void {
    const gl = this.gl;
    gl.deleteProgram(this.program);
    gl.deleteProgram(this.badgeProgram);
    gl.deleteBuffer(this.posBuffer);
    gl.deleteBuffer(this.texBuffer);
    gl.deleteBuffer(this.hasImageBuffer);
    gl.deleteBuffer(this.selectedBuffer);
    gl.deleteBuffer(this.indexBuffer);
    gl.deleteBuffer(this.badgePosBuffer);
    gl.deleteBuffer(this.badgeColorBuffer);
    gl.deleteBuffer(this.badgeIndexBuffer);
    gl.deleteTexture(this.atlas);
    gl.deleteTexture(this.badgeTexture);
    this.tileMap.clear();
    this.lruOrder.length = 0;
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private evictLRU() {
    if (this.lruOrder.length === 0) return;
    const oldest = this.lruOrder.shift()!;
    this.evictImage(oldest);
  }

  private uploadAndBind(buffer: WebGLBuffer, data: Float32Array, loc: number, size: number) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.DYNAMIC_DRAW);
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, size, gl.FLOAT, false, 0, 0);
  }

  private drawOverlays(
    layout: GridLayout,
    items: GridItem[],
    checkItems: { cx: number; cy: number }[],
    badgeItems: { x: number; y: number; w: number; h: number }[],
  ) {
    const gl = this.gl;
    const { cols, cellSize, gap, startRow } = layout;

    // Use badge program for colored quads
    gl.useProgram(this.badgeProgram);
    gl.uniform2f(this.badgeResolutionLoc, this.canvas.width / this.dpr, this.canvas.height / this.dpr);

    // Build overlay geometry: selection borders (line quads), check marks (circles as quads), badges
    const positions: number[] = [];
    const colors: number[] = [];
    const indices: number[] = [];
    let vi = 0;

    // Selection borders (4 thin quads per selected item)
    for (const item of items) {
      if (!item.selected) continue;
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const x = col * (cellSize + gap);
      const y = row * (cellSize + gap);
      const bw = 2; // border width

      const borderColor = [59 / 255, 130 / 255, 246 / 255, 1];

      // Top
      this.pushQuad(positions, colors, indices, vi, x, y, cellSize, bw, borderColor);
      vi += 4;
      // Bottom
      this.pushQuad(positions, colors, indices, vi, x, y + cellSize - bw, cellSize, bw, borderColor);
      vi += 4;
      // Left
      this.pushQuad(positions, colors, indices, vi, x, y, bw, cellSize, borderColor);
      vi += 4;
      // Right
      this.pushQuad(positions, colors, indices, vi, x + cellSize - bw, y, bw, cellSize, borderColor);
      vi += 4;

      // Check mark background (square approximation of circle)
      const checkSize = 20;
      const cx = x + 12 - checkSize / 2;
      const cy = y + 12 - checkSize / 2;
      this.pushQuad(positions, colors, indices, vi, cx, cy, checkSize, checkSize, borderColor);
      vi += 4;
    }

    // Badge backgrounds
    for (const item of items) {
      if (!item.badge) continue;
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const x = col * (cellSize + gap);
      const y = row * (cellSize + gap);

      const bw = item.badge === "VID" ? 34 : 30;
      const bh = 18;
      const bx = x + cellSize - bw - 6;
      const by = y + cellSize - bh - 6;

      this.pushQuad(positions, colors, indices, vi, bx, by, bw, bh, [0, 0, 0, 0.5]);
      vi += 4;
    }

    if (positions.length > 0) {
      const posArr = new Float32Array(positions);
      const colArr = new Float32Array(colors);
      const idxArr = new Uint16Array(indices);

      this.uploadAndBind(this.badgePosBuffer, posArr, this.badgePosLoc, 2);
      this.uploadAndBind(this.badgeColorBuffer, colArr, this.badgeColorLoc, 4);

      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.badgeIndexBuffer);
      gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, idxArr, gl.DYNAMIC_DRAW);

      gl.drawElements(gl.TRIANGLES, indices.length, gl.UNSIGNED_SHORT, 0);
    }
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
