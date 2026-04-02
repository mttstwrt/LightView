// ---------------------------------------------------------------------------
// Grid renderer abstraction — shared types for Canvas2D and WebGL backends
// ---------------------------------------------------------------------------

export interface GridLayout {
  /** Number of columns in the grid. */
  cols: number;
  /** Pixel size of each cell (square). */
  cellSize: number;
  /** Gap between cells in pixels. */
  gap: number;
  /** Total number of items. */
  totalItems: number;
  /** Y offset of the rendered slice within the virtual scroll container. */
  scrollOffsetY: number;
  /** First visible row index. */
  startRow: number;
  /** One past the last visible row index. */
  endRow: number;
}

export interface GridItem {
  /** Absolute index in the full item list. */
  index: number;
  /** File path (unique identifier). */
  path: string;
  /** Protocol URL for the thumbnail image (null if not yet assigned). */
  thumbUrl: string | null;
  /** Whether this item is selected. */
  selected: boolean;
  /** Media badge to display, if any. */
  badge: "VID" | "GIF" | null;
}

export interface HitTestResult {
  /** Index of the item hit, or -1 if none. */
  index: number;
  /** Path of the item hit, or null. */
  path: string | null;
}

/**
 * Common interface for grid renderers. Both Canvas2D and WebGL implement this.
 * The renderer owns the canvas element and handles all drawing.
 */
export interface GridRenderer {
  /** The canvas element managed by this renderer. */
  readonly canvas: HTMLCanvasElement;

  /** Resize the canvas to match its container. Returns true if size changed. */
  resize(width: number, height: number, dpr: number): boolean;

  /**
   * Render the visible grid. Called on every frame where something changed.
   * @param layout Grid geometry
   * @param items Visible items to render (already sliced to viewport)
   */
  render(layout: GridLayout, items: GridItem[]): void;

  /** Hit-test a point in canvas-local coordinates. */
  hitTest(x: number, y: number, layout: GridLayout, items: GridItem[]): HitTestResult;

  /** Notify the renderer that a thumbnail URL has been loaded/updated. */
  imageLoaded(path: string, img: HTMLImageElement | ImageBitmap): void;

  /** Evict a cached image for the given path. */
  evictImage(path: string): void;

  /** Release all GPU/canvas resources. */
  destroy(): void;
}
