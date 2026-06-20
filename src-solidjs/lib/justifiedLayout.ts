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
   * Indices at which a new group begins — the row is force-broken before each,
   * so group boundaries stay clean. Empty / omitted = no forced breaks.
   */
  groupStarts?: number[];
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

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
    groupStarts,
  } = opts;

  const rows: LayoutRow[] = [];
  const rowTops: number[] = [];

  if (containerWidth <= 0 || aspects.length === 0 || targetRowHeight <= 0) {
    return { rows, rowTops, totalHeight: 0 };
  }

  const groupBreak = groupStarts && groupStarts.length > 0 ? new Set(groupStarts) : null;

  let y = 0;
  let rowStart = 0; // first item index of the current row
  let sumAspect = 0;

  // Emit a row covering items [rowStart, end). `justify=false` keeps the row at
  // the target height (used for the trailing row of the content and of each
  // group, so 1–2 leftover items aren't stretched grotesquely wide).
  const flush = (end: number, justify: boolean) => {
    const n = end - rowStart;
    if (n <= 0) return;
    const totalGap = gap * (n - 1);
    const avail = containerWidth - totalGap;
    let h = justify ? avail / sumAspect : targetRowHeight;
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
      flush(i, false);
    }

    const a = clamp(aspects[i] > 0 ? aspects[i] : 1, minAspect, maxAspect);
    sumAspect += a;

    const n = i - rowStart + 1;
    const totalGap = gap * (n - 1);
    const justifiedH = (containerWidth - totalGap) / sumAspect;

    // Once justifying to full width would shrink the row to/under the target,
    // the row is "full" — commit it at that justified height.
    if (justifiedH <= targetRowHeight) {
      flush(i + 1, true);
    }
  }

  // Trailing partial row, left-aligned at target height.
  flush(aspects.length, false);

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
