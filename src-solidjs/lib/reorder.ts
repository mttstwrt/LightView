// Reordering a set in the grid: which gap the pointer is over, and the order a
// drop produces. Pure, so the arithmetic is tested without a DOM.
//
// Coordinates are the layout's own: content pixels from the top-left of the
// grid's cell track, the frame `computeJustifiedLayout` places cells in. The
// grid converts a pointer's client position into that frame before asking.

import { rowIndexAtOffset, type JustifiedLayout } from "./justifiedLayout";

/** A place a dragged block can land, and where to draw its marker. */
export interface Gap {
  /** The index, in the order *before* the move, of the item the block will
   *  stand before; the item count when it goes at the end. */
  index: number;
  /** The marker's x (the edge of the neighbouring cell) and its row's top
   *  and height, in layout coordinates. */
  x: number;
  y: number;
  height: number;
}

/** The gap a point is over, or null for an empty layout.
 *
 *  The row is the one whose band holds `y` — a point between two rows belongs
 *  to the row above, and one past either end to the nearest row — so a drop
 *  always lands somewhere. Within the row, a point left of a cell's midpoint is
 *  the gap before that cell, and one right of the last cell's midpoint is the
 *  gap after it. */
export function gapAt(layout: JustifiedLayout, x: number, y: number): Gap | null {
  if (layout.rows.length === 0) return null;
  const row = layout.rows[rowIndexAtOffset(layout.rowTops, y)];
  for (const cell of row.cells) {
    if (x < cell.x + cell.width / 2) {
      return { index: cell.index, x: cell.x, y: row.y, height: row.height };
    }
  }
  const last = row.cells[row.cells.length - 1];
  return { index: last.index + 1, x: last.x + last.width, y: row.y, height: row.height };
}

/** The order after moving `moving` to stand before the item now at `insertAt`.
 *
 *  `insertAt` indexes `order` as it is before the move, and `order.length` is
 *  the end. The moved items keep their relative order wherever they were, so a
 *  selection gathered from across the set lands as one block; and dropping a
 *  block into a gap inside itself leaves the order as it was. */
export function moveBlock(
  order: readonly string[],
  moving: ReadonlySet<string>,
  insertAt: number,
): string[] {
  const before = order.slice(0, insertAt).filter((p) => !moving.has(p));
  const after = order.slice(insertAt).filter((p) => !moving.has(p));
  const block = order.filter((p) => moving.has(p));
  return [...before, ...block, ...after];
}

/** True when two lists hold the same paths, in any order. */
export function isPermutation(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  const inA = new Set(a);
  return b.every((p) => inA.has(p));
}
