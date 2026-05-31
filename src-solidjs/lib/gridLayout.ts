// Grid column math, shared between the settings drawer's column-count picker
// and the grid's pinch-to-density gesture so both agree on the mapping between
// a thumbnail_size (CSS px) and the resulting column count at the current
// viewport width. The grid lays out columns as
// `cols = floor((w + gap) / (size + gap))`.

/** Compute a thumbnail_size in CSS px that lands the grid on `targetCols`
 *  columns, given the current viewport width and grid gap. Picks the middle of
 *  the bucket that yields `targetCols` so a small viewport change doesn't flip
 *  the column count. */
export function thumbSizeForCols(targetCols: number, gap: number): number {
  const w = typeof window !== "undefined" ? window.innerWidth : 400;
  const upper = (w + gap) / targetCols - gap;
  const lower = (w + gap) / (targetCols + 1) - gap;
  return Math.max(20, Math.round((upper + lower) / 2));
}

/** Reverse-derive the current effective column count from a saved
 *  thumbnail_size so the matching preset stays highlighted. */
export function currentColCount(thumbSize: number, gap: number): number {
  const w = typeof window !== "undefined" ? window.innerWidth : 400;
  return Math.max(1, Math.floor((w + gap) / (thumbSize + gap)));
}
