// Justified ("flexbox-row") gallery layout — the algorithm Flickr / Google
// Photos use. Items are placed left-to-right in their given order and wrapped
// into rows; each completed row is scaled to fill the container width exactly,
// so heights vary but the right edge stays flush. Sort order is preserved
// exactly (items never move between positions).
//
// This is a pure function: given aspect ratios + geometry it returns row
// rectangles plus a cumulative-offset table for virtual scrolling. No DOM, no
// reactivity — the component memoizes over it.

export interface LayoutCell {
  /** Index into the original ordered items array. */
  index: number;
  /** X offset (px) from the row's left edge. */
  x: number;
  /** Cell width (px). */
  width: number;
  /** Cell height (px) — equal to the row height. */
  height: number;
}

export interface LayoutRow {
  /** Y offset (px) of the row's top from the content top. */
  y: number;
  /** Row height (px). */
  height: number;
  cells: LayoutCell[];
}

export interface JustifiedLayout {
  rows: LayoutRow[];
  /** `rowTops[i]` = `rows[i].y`; sorted ascending for binary search. */
  rowTops: number[];
  /** Total content height (px), excluding the trailing inter-row gap. */
  totalHeight: number;
}

export interface JustifiedLayoutOptions {
  /** Aspect ratio (width / height) per item, in display order. */
  aspects: number[];
  containerWidth: number;
  /** Desired row height before justification (px). */
  targetRowHeight: number;
  /** Gap between cells and between rows (px). */
  gap: number;
  /** Clamp justified row heights to avoid extreme rows. */
  minRowHeight?: number;
  maxRowHeight?: number;
  /** Clamp per-item aspect so a panorama / sliver doesn't dominate a row. */
  minAspect?: number;
  maxAspect?: number;
  /**
   * Boost row height for portrait-skewed rows so vertical images aren't dwarfed
   * by landscapes. At a shared row height a portrait has far less area than a
   * landscape (area = aspect × height²); to compensate, the per-row target
   * height is multiplied by `sqrt(boostRefAspect / avgRowAspect)`, clamped to
   * `[1, orientationBoost]`. The clamp floor of 1 means landscapes are never
   * shrunk — only portrait/square rows grow taller (fewer, larger images).
   * `orientationBoost = 1` disables the boost entirely.
   */
  orientationBoost?: number;
  /**
   * Average row aspect at/above which a row gets its natural (unboosted)
   * target. Set near the typical landscape aspect so landscape rows stay put
   * and only portrait-leaning rows are boosted.
   */
  boostRefAspect?: number;
  /**
   * Indices at which a new group begins — the row is force-broken before each,
   * so group boundaries stay clean. Empty / omitted = no forced breaks.
   */
  groupStarts?: number[];
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * How much taller than its natural height a row ending a group may be stretched
 * to fill the width.
 *
 * A row that ends a group or the content never reached its commit height, so
 * justifying it to the full width always makes it taller than the rows above —
 * the only question is by how much. Up to about half again reads as one of
 * them; beyond that it reads as a mistake, and the row is better left short.
 *
 * Measured at the real defaults (container 1600, gap 4, target 240): a row of
 * four landscapes needs 1.10× and a row of three needs 1.48×, so both fill the
 * width; two squares would need 2.7× and one landscape 4.4×, so both stay put.
 * Rows of three or more close up, which is where the ragged edge was
 * conspicuous, and the cases that cannot close up keep today's rendering
 * rather than becoming double-height *and* still ragged.
 */
export const FINAL_ROW_STRETCH = 1.5;

/** Default cap on the portrait row-height boost (see `orientationBoost`). */
export const DEFAULT_ORIENTATION_BOOST = 1.6;
/** Default aspect at/above which a row is unboosted (see `boostRefAspect`). */
export const DEFAULT_BOOST_REF_ASPECT = 1.5;

/**
 * Multiplier applied to a row's target height given its average aspect, so
 * portrait-leaning rows grow taller (vertical images get more area). Returns
 * `sqrt(boostRefAspect / avgAspect)` clamped to `[1, orientationBoost]`: 1 for
 * landscape rows (`avgAspect >= boostRefAspect`), up to `orientationBoost` for
 * strongly portrait rows. Exported so callers that need to predict a cell's
 * displayed pixel size (e.g. choosing a resize resolution) stay in sync with
 * the layout — passing a single image's aspect approximates its row's boost.
 */
export function portraitRowBoost(
  avgAspect: number,
  orientationBoost = DEFAULT_ORIENTATION_BOOST,
  boostRefAspect = DEFAULT_BOOST_REF_ASPECT,
): number {
  if (orientationBoost <= 1 || avgAspect <= 0 || avgAspect >= boostRefAspect) return 1;
  return Math.min(orientationBoost, Math.sqrt(boostRefAspect / avgAspect));
}

/**
 * Compute a justified layout. Runs in O(n) over the items.
 */
export function computeJustifiedLayout(opts: JustifiedLayoutOptions): JustifiedLayout {
  const {
    aspects,
    containerWidth,
    targetRowHeight,
    gap,
    minRowHeight = targetRowHeight * 0.5,
    maxRowHeight = targetRowHeight * 2,
    minAspect = 0.25,
    maxAspect = 4,
    orientationBoost = DEFAULT_ORIENTATION_BOOST,
    boostRefAspect = DEFAULT_BOOST_REF_ASPECT,
    groupStarts,
  } = opts;

  // Per-row target height, scaled up when the row skews portrait (see
  // `portraitRowBoost`). This is the only lever that gives portraits more space
  // — within a row all heights are equal, so taller rows are the only way to
  // enlarge vertical images.
  const targetFor = (avgAspect: number): number =>
    targetRowHeight * portraitRowBoost(avgAspect, orientationBoost, boostRefAspect);

  const rows: LayoutRow[] = [];
  const rowTops: number[] = [];

  if (containerWidth <= 0 || aspects.length === 0 || targetRowHeight <= 0) {
    return { rows, rowTops, totalHeight: 0 };
  }

  const groupBreak = groupStarts && groupStarts.length > 0 ? new Set(groupStarts) : null;

  let y = 0;
  let rowStart = 0; // first item index of the current row
  let sumAspect = 0;

  // Emit a row covering items [rowStart, end). `isFinal` marks the row that
  // ends the content or a group — the one that never reached its commit
  // height, and so the only one where filling the width is a choice.
  const flush = (end: number, isFinal: boolean) => {
    const n = end - rowStart;
    if (n <= 0) return;
    const totalGap = gap * (n - 1);
    const avail = containerWidth - totalGap;
    const justifiedH = avail / sumAspect;
    // A final row used to sit at its target height unconditionally, which left
    // a ragged edge at **every group boundary** — and with monthly grouping on
    // by default, that is not the end of the library, it is a dozen times down
    // a scroll. It fills the width when doing so costs little height, and
    // otherwise stays short: a lone image stretched to a full-width banner is
    // worse than the gap it closes. `FINAL_ROW_STRETCH` is where that line is.
    //
    // The unstretched fallback keeps the portrait boost, so a leftover
    // portrait row matches the boosted rows above it rather than snapping back
    // to the base target.
    let h = justifiedH;
    if (isFinal) {
      const natural = targetFor(sumAspect / n);
      h = justifiedH <= natural * FINAL_ROW_STRETCH ? justifiedH : natural;
    }
    h = clamp(h, minRowHeight, maxRowHeight);

    const cells: LayoutCell[] = [];
    let x = 0;
    for (let i = rowStart; i < end; i++) {
      const a = clamp(aspects[i] > 0 ? aspects[i] : 1, minAspect, maxAspect);
      const w = a * h;
      cells.push({ index: i, x, width: w, height: h });
      x += w + gap;
    }
    rows.push({ y, height: h, cells });
    rowTops.push(y);
    y += h + gap;

    rowStart = end;
    sumAspect = 0;
  };

  for (let i = 0; i < aspects.length; i++) {
    // Force a break before an item that starts a new group.
    if (groupBreak && groupBreak.has(i) && i > rowStart) {
      flush(i, true);
    }

    const a = clamp(aspects[i] > 0 ? aspects[i] : 1, minAspect, maxAspect);
    sumAspect += a;

    const n = i - rowStart + 1;
    const totalGap = gap * (n - 1);
    const justifiedH = (containerWidth - totalGap) / sumAspect;

    // Once justifying to full width would shrink the row to/under its target,
    // the row is "full" — commit it at that justified height. A portrait-leaning
    // row has a taller target (see `targetFor`), so it commits earlier: fewer,
    // bigger images instead of many narrow slivers.
    if (justifiedH <= targetFor(sumAspect / n)) {
      flush(i + 1, false);
    }
  }

  // Trailing partial row: filled to the width when that is cheap, left short
  // when it is not.
  flush(aspects.length, true);

  const totalHeight = rows.length > 0 ? y - gap : 0;
  return { rows, rowTops, totalHeight };
}

/**
 * Binary search `rowTops` for the index of the last row whose top is `<= scrollY`.
 * Returns 0 if `scrollY` is above the first row.
 */
export function rowIndexAtOffset(rowTops: number[], scrollY: number): number {
  let lo = 0;
  let hi = rowTops.length - 1;
  let ans = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (rowTops[mid] <= scrollY) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return ans;
}

// ---------------------------------------------------------------------------
// Holding the reader's place across a change
// ---------------------------------------------------------------------------
//
// A justified grid reflows like text. Row height is `avail / sumAspect` over the
// items that landed in the row, so removing one item repacks every row after it:
// measured at the real defaults, one removal near the top displaces later items
// by 18, 283, 11 and 29 px while total height moves ten. There is no single
// displacement to subtract, and — the trap that cost a design — no function of a
// layout and a scroll offset can recover one, because its only honest answer to
// "how much content is above the viewport" is the top of whichever row straddles
// the offset, which is within a row height of the offset in *every* layout.
//
// What survives a reflow is identity. These take an item, remembered from the
// old layout, and say where to scroll so that item sits where it sat.
//
// They are index-based because that is all a layout knows. An index is not
// stable across an insertion or a removal, so a caller holding one across a set
// change must carry the item's own identity — the grid converts through its path
// list. See `justifiedLayout.test.ts` for the reflow these exist to survive.

/** The item whose position is kept across a set change, and where it sat. */
export interface TopAnchor {
  index: number;
  /** Its row's top, as pixels below the top of the viewport. Normally ≤ 0. */
  offset: number;
}

/** The item whose position is kept across a scale change, and where it sat. */
export interface ScaleAnchor {
  index: number;
  /** Its row's centre, as a fraction of the viewport height from the top. */
  fraction: number;
}

/** The row containing `index`, by binary search over each row's first item. */
export function rowOfItem(layout: JustifiedLayout, index: number): number {
  const { rows } = layout;
  let lo = 0;
  let hi = rows.length - 1;
  let ans = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (rows[mid].cells[0].index <= index) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return ans;
}

/**
 * The anchor for a **set change**: the item at the top of the viewport.
 *
 * The top edge rather than the centre, because a change inside the viewport has
 * to stay local to itself. An anchor below the edit would be dragged by it, and
 * holding it still would slide everything *above* the edit downward while the
 * gap closed from below — two motions where there should be one. The topmost
 * item is above any edit the reader can see, so it is unmoved by definition.
 */
export function topAnchor(layout: JustifiedLayout, scrollTop: number): TopAnchor | null {
  if (layout.rows.length === 0) return null;
  const row = layout.rows[rowIndexAtOffset(layout.rowTops, scrollTop)];
  return { index: row.cells[0].index, offset: row.y - scrollTop };
}

/**
 * The anchor for a **scale change**: the item nearest the middle of the screen.
 *
 * The centre rather than the top edge, because zoom scales everything at once
 * and there is no unaffected item to hold — so the fixed point should be where
 * the eye is, which is what every map and image viewer does. A *fraction* of
 * the viewport rather than a pixel offset, so a rotation that changes the
 * viewport's height keeps the same item in the same visual place rather than
 * the same number of pixels down a screen of a different size.
 */
export function scaleAnchor(
  layout: JustifiedLayout,
  scrollTop: number,
  viewportHeight: number,
): ScaleAnchor | null {
  if (layout.rows.length === 0 || viewportHeight <= 0) return null;
  const row = layout.rows[rowIndexAtOffset(layout.rowTops, scrollTop + viewportHeight / 2)];
  // The row's first cell. Every cell in a row shares a vertical position, so
  // any of them holds the row equally well — and the first is the one whose
  // index survives a repack most predictably, because rows are packed forward
  // from it. Anchoring the middle cell instead was tried, on the theory that it
  // tracks the photograph in front of the reader more closely as rows shed
  // members; measured over a zoom it drifted 370px where this holds at zero.
  return {
    index: row.cells[0].index,
    fraction: (row.y + row.height / 2 - scrollTop) / viewportHeight,
  };
}

/** Where to scroll so `anchor`'s item sits where it sat, in the new layout. */
export function scrollHolding(
  layout: JustifiedLayout,
  anchor: TopAnchor | ScaleAnchor,
  viewportHeight: number,
): number {
  if (layout.rows.length === 0) return 0;
  const row = layout.rows[rowOfItem(layout, anchor.index)];
  return "offset" in anchor
    ? row.y - anchor.offset
    : row.y + row.height / 2 - anchor.fraction * viewportHeight;
}
