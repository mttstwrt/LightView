// ---------------------------------------------------------------------------
// Canvas 2D grid renderer — draws thumbnails on a single <canvas> element
// ---------------------------------------------------------------------------
//
// Replaces the DOM grid with manual drawing. Eliminates per-cell DOM nodes,
// CSS layout recalculation, and compositor overhead.

import type { GridRenderer, GridLayout, GridItem, HitTestResult } from "./types";

const SKELETON_COLOR = "#1a1a1a";
const SELECTION_COLOR = "rgba(59, 130, 246, 0.35)";
const SELECTION_BORDER_COLOR = "#3b82f6";
const SELECTION_BORDER_WIDTH = 2;
const CHECK_RADIUS = 10;
const CHECK_BG = "#3b82f6";
const BADGE_BG = "rgba(0, 0, 0, 0.5)";
const BADGE_FONT = "bold 11px ui-monospace, monospace";
const BADGE_COLOR = "rgba(255, 255, 255, 0.8)";
const BADGE_PADDING_X = 6;
const BADGE_PADDING_Y = 3;
const BADGE_MARGIN = 6;
const BADGE_RADIUS = 3;

export class Canvas2DRenderer implements GridRenderer {
  readonly canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private dpr = 1;
  private images = new Map<string, HTMLImageElement | ImageBitmap>();

  constructor() {
    this.canvas = document.createElement("canvas");
    this.canvas.style.display = "block";
    this.canvas.style.width = "100%";
    this.canvas.style.height = "100%";
    const ctx = this.canvas.getContext("2d", { alpha: false });
    if (!ctx) throw new Error("Canvas 2D context unavailable");
    this.ctx = ctx;
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
    return true;
  }

  render(layout: GridLayout, items: GridItem[]): void {
    const ctx = this.ctx;
    const dpr = this.dpr;
    const { cols, cellSize, gap, startRow } = layout;

    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    // Clear to background
    ctx.fillStyle = "#0a0a0a";
    ctx.fillRect(0, 0, this.canvas.width / dpr, this.canvas.height / dpr);

    for (const item of items) {
      const localIdx = item.index - startRow * cols;
      const row = Math.floor(localIdx / cols);
      const col = localIdx % cols;
      const x = col * (cellSize + gap);
      const y = row * (cellSize + gap);

      // Skeleton background
      ctx.fillStyle = SKELETON_COLOR;
      ctx.fillRect(x, y, cellSize, cellSize);

      // Draw thumbnail if loaded
      const img = this.images.get(item.path);
      if (img) {
        this.drawCover(ctx, img, x, y, cellSize, cellSize);
      }

      // Selection overlay
      if (item.selected) {
        ctx.fillStyle = SELECTION_COLOR;
        ctx.fillRect(x, y, cellSize, cellSize);

        ctx.strokeStyle = SELECTION_BORDER_COLOR;
        ctx.lineWidth = SELECTION_BORDER_WIDTH;
        ctx.strokeRect(
          x + SELECTION_BORDER_WIDTH / 2,
          y + SELECTION_BORDER_WIDTH / 2,
          cellSize - SELECTION_BORDER_WIDTH,
          cellSize - SELECTION_BORDER_WIDTH,
        );

        // Check mark
        const cx = x + 12;
        const cy = y + 12;
        ctx.beginPath();
        ctx.arc(cx, cy, CHECK_RADIUS, 0, Math.PI * 2);
        ctx.fillStyle = CHECK_BG;
        ctx.fill();

        ctx.fillStyle = "#ffffff";
        ctx.font = "bold 12px sans-serif";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";
        ctx.fillText("\u2713", cx, cy + 1);
      }

      // Badge (VID / GIF)
      if (item.badge) {
        this.drawBadge(ctx, item.badge, x, y, cellSize);
      }
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
    this.images.set(path, img);
  }

  evictImage(path: string): void {
    const img = this.images.get(path);
    if (img && img instanceof ImageBitmap) {
      img.close();
    }
    this.images.delete(path);
  }

  destroy(): void {
    for (const img of this.images.values()) {
      if (img instanceof ImageBitmap) {
        img.close();
      }
    }
    this.images.clear();
  }

  // -------------------------------------------------------------------------
  // Private drawing helpers
  // -------------------------------------------------------------------------

  /** Draw an image with object-fit: cover behavior. */
  private drawCover(
    ctx: CanvasRenderingContext2D,
    img: HTMLImageElement | ImageBitmap,
    dx: number,
    dy: number,
    dw: number,
    dh: number,
  ) {
    const iw = img.width;
    const ih = img.height;
    if (iw === 0 || ih === 0) return;

    const scale = Math.max(dw / iw, dh / ih);
    const sw = dw / scale;
    const sh = dh / scale;
    const sx = (iw - sw) / 2;
    const sy = (ih - sh) / 2;

    ctx.drawImage(img, sx, sy, sw, sh, dx, dy, dw, dh);
  }

  /** Draw a text badge (VID/GIF) at bottom-right of a cell. */
  private drawBadge(
    ctx: CanvasRenderingContext2D,
    text: string,
    cellX: number,
    cellY: number,
    cellSize: number,
  ) {
    ctx.font = BADGE_FONT;
    const metrics = ctx.measureText(text);
    const tw = metrics.width;
    const th = 11; // approximate font height

    const bw = tw + BADGE_PADDING_X * 2;
    const bh = th + BADGE_PADDING_Y * 2;
    const bx = cellX + cellSize - bw - BADGE_MARGIN;
    const by = cellY + cellSize - bh - BADGE_MARGIN;

    // Rounded rect background
    ctx.beginPath();
    ctx.roundRect(bx, by, bw, bh, BADGE_RADIUS);
    ctx.fillStyle = BADGE_BG;
    ctx.fill();

    // Text
    ctx.fillStyle = BADGE_COLOR;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(text, bx + bw / 2, by + bh / 2);
  }
}
