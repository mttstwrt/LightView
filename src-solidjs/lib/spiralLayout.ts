// ---------------------------------------------------------------------------
// The canvas view's geometry: a square spiral over a uniform lattice of cells,
// item 0 at the centre and each later item one step further round.
//
// The whole module exists to make two questions cheap, because the view asks
// one of them per cell per frame and the other for every path it holds:
//
//   - which items are inside this rectangle? — `cellsInRect` narrows the
//     surface to a lattice range, and `indexOfCell` turns each cell in it back
//     into an item index in constant time. So a frame costs what it renders,
//     not what the gallery holds.
//   - where is item i? — `cellOfIndex`, also constant time, for eviction and
//     for scrolling a specific item into view.
//
// Both directions being closed-form is what picked this spiral over the two
// prettier ones:
//
// **Phyllotaxis** (the sunflower arrangement: angle i × the golden angle,
// radius ∝ √i) is the obvious choice for an organic-looking canvas and has an
// equally trivial forward map. It has no inverse. A viewport far from the
// centre intersects a thin arc of a very large annulus, and the only way to
// find it is to walk every index in that annulus — which grows linearly with
// distance from the centre, so the cost of a frame would grow the further out
// the user browsed. Wrong shape for a virtualized view.
//
// **Ring-packed cells of varying width** — each ring a justified strip of
// aspect-sized boxes, which would fill the plane with no letterboxing — has no
// closed form in either direction, because where a ring ends depends on the
// aspect ratios of everything before it. It needs a precomputed layout like
// `justifiedLayout.ts` builds, plus a per-ring binary search. That is a
// reasonable amount of machinery, and it buys a look; the lattice buys the
// range query. If the letterboxing ever becomes the thing people complain
// about, that is the design to reach for.
//
// Aspect ratios are preserved rather than cropped: a cell is a square slot and
// the item is fitted inside it (`fitBox`), which is why the view can serve the
// justified `j`/`jm`/`jh` tiers unchanged and costs the gallery no thumbnails
// of its own. A portrait leaves space at its sides and a panorama at its top
// and bottom; that space is the price of the lattice.
//
// Coordinates: `cx` grows right and `cy` grows *down*, matching screen space,
// so the spiral reads clockwise. All pixel values are in the surface's own
// coordinate space — the view offsets them by the scroller's origin.
// ---------------------------------------------------------------------------

/** A cell on the spiral's integer lattice. The centre cell is (0, 0). */
export interface SpiralCell {
  cx: number;
  cy: number;
}

/** The pixel dimensions a spiral surface is laid out at. */
export interface SpiralGeometry {
  /** Rings around the centre needed to hold every item. 0 = a single cell. */
  rings: number;
  /** Side of a cell's square slot, in px. */
  cellSize: number;
  /** Gap between adjacent slots, and the margin around the whole surface. */
  gap: number;
  /** Centre-to-centre distance between neighbouring cells. */
  pitch: number;
  /** Side of the whole square surface, in px. */
  extent: number;
}

/**
 * Ring holding item `index`: the number of steps out from the centre, so ring
 * r spans indices [(2r-1)², (2r+1)²).
 */
export function ringOfIndex(index: number): number {
  if (index <= 0) return 0;
  return Math.ceil((Math.sqrt(index + 1) - 1) / 2);
}

/** Rings needed to hold `count` items — the radius of the whole surface. */
export function ringsFor(count: number): number {
  return count <= 1 ? 0 : ringOfIndex(count - 1);
}

/**
 * Where item `index` sits on the lattice.
 *
 * Ring r starts at (r, -r+1) — the top of the right-hand edge — and is walked
 * clockwise: down the right edge, left along the bottom, up the left edge,
 * right along the top, finishing at (r, -r), one step below where ring r+1
 * begins.
 */
export function cellOfIndex(index: number): SpiralCell {
  const r = ringOfIndex(index);
  if (r === 0) return { cx: 0, cy: 0 };
  const off = index - (2 * r - 1) * (2 * r - 1);
  if (off < 2 * r) return { cx: r, cy: -r + 1 + off };
  if (off < 4 * r) return { cx: r - 1 - (off - 2 * r), cy: r };
  if (off < 6 * r) return { cx: -r, cy: r - 1 - (off - 4 * r) };
  return { cx: -r + 1 + (off - 6 * r), cy: -r };
}

/**
 * Which item sits on cell (`cx`, `cy`) — the exact inverse of
 * [`cellOfIndex`], and the reason this spiral was chosen. Every lattice cell
 * holds an item, so the result may be past the end of the gallery; the caller
 * skips those.
 *
 * The four corners each belong to the edge that reaches them first, which is
 * why the tests are ordered rather than independent: (r, r) closes the right
 * edge, (-r, r) the bottom, and so on.
 */
export function indexOfCell(cx: number, cy: number): number {
  const r = Math.max(Math.abs(cx), Math.abs(cy));
  if (r === 0) return 0;
  const base = (2 * r - 1) * (2 * r - 1);
  if (cx === r && cy > -r) return base + cy + r - 1;
  if (cy === r) return base + 2 * r + (r - 1 - cx);
  if (cx === -r) return base + 4 * r + (r - 1 - cy);
  return base + 6 * r + (cx + r - 1);
}

/** The surface `count` items occupy at this cell size. */
export function spiralGeometry(count: number, cellSize: number, gap: number): SpiralGeometry {
  const rings = ringsFor(count);
  const pitch = cellSize + gap;
  return { rings, cellSize, gap, pitch, extent: (2 * rings + 1) * pitch + gap };
}

/** Top-left pixel corner of a cell's square slot within the surface. */
export function cellOrigin(g: SpiralGeometry, cell: SpiralCell): { x: number; y: number } {
  return {
    x: (cell.cx + g.rings) * g.pitch + g.gap,
    y: (cell.cy + g.rings) * g.pitch + g.gap,
  };
}

/** Centre of a cell's slot — what "distance from the viewport centre" is
 *  measured against, and where a scroll-into-view aims. */
export function cellCentre(g: SpiralGeometry, cell: SpiralCell): { x: number; y: number } {
  const o = cellOrigin(g, cell);
  return { x: o.x + g.cellSize / 2, y: o.y + g.cellSize / 2 };
}

/** The lattice cell covering a pixel position, unclamped. */
export function cellAt(g: SpiralGeometry, x: number, y: number): SpiralCell {
  return {
    cx: Math.floor((x - g.gap) / g.pitch) - g.rings,
    cy: Math.floor((y - g.gap) / g.pitch) - g.rings,
  };
}

/** An inclusive lattice range — what a rectangle of the surface covers. */
export interface CellRange {
  cx0: number;
  cy0: number;
  cx1: number;
  cy1: number;
}

/**
 * The cells a surface rectangle covers, clamped to the surface. Bounds are
 * inclusive, and empty ranges are possible in principle but not in practice:
 * clamping a rectangle that misses the surface entirely still yields the
 * nearest edge cell, which is harmless — the view skips indices past the end
 * of the gallery anyway.
 */
export function cellsInRect(
  g: SpiralGeometry,
  x0: number,
  y0: number,
  x1: number,
  y1: number,
): CellRange {
  const lo = cellAt(g, x0, y0);
  const hi = cellAt(g, x1, y1);
  const clamp = (v: number) => Math.max(-g.rings, Math.min(g.rings, v));
  return { cx0: clamp(lo.cx), cy0: clamp(lo.cy), cx1: clamp(hi.cx), cy1: clamp(hi.cy) };
}

/**
 * The box an item of this aspect ratio takes inside a square slot, preserving
 * the ratio (the slot's short axis wins). A missing or nonsensical aspect
 * falls back to a square, which is what an item whose dimensions are not
 * indexed yet gets until its thumbnail loads and reports them.
 */
export function fitBox(aspect: number, cellSize: number): { width: number; height: number } {
  if (!(aspect > 0) || !Number.isFinite(aspect)) return { width: cellSize, height: cellSize };
  return aspect >= 1
    ? { width: cellSize, height: cellSize / aspect }
    : { width: cellSize * aspect, height: cellSize };
}
