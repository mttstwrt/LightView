// Where a drop lands and what order it makes. The grid draws the marker and
// sends the order these compute; a drag in headless Chromium exercises the
// wiring, and these pin the arithmetic under it.

import { describe, it, expect } from "vitest";
import { computeJustifiedLayout } from "./justifiedLayout";
import { gapAt, isPermutation, moveBlock } from "./reorder";

/** Two rows of three squares: 100px cells, 4px gaps, rows at y=0 and y=104. */
const grid = computeJustifiedLayout({
  aspects: [1, 1, 1, 1, 1, 1],
  containerWidth: 308,
  targetRowHeight: 100,
  gap: 4,
  minRowHeight: 100,
  maxRowHeight: 100,
  orientationBoost: 1,
});

describe("the gap under the pointer", () => {
  it("is laid out the way these cases assume", () => {
    expect(grid.rows.map((r) => r.cells.map((c) => c.index))).toEqual([
      [0, 1, 2],
      [3, 4, 5],
    ]);
  });

  it("is before a cell when left of its midpoint, after it when right", () => {
    const [a, b] = grid.rows[0].cells;
    expect(gapAt(grid, a.x + 10, 50)?.index).toBe(0);
    expect(gapAt(grid, a.x + a.width - 10, 50)?.index).toBe(1);
    expect(gapAt(grid, b.x + 10, 50)?.index).toBe(1);
  });

  it("is after the last cell of a row past that cell's midpoint", () => {
    const last = grid.rows[0].cells[2];
    const gap = gapAt(grid, last.x + last.width - 5, 50)!;
    expect(gap.index).toBe(3);
    // Drawn at the row's right edge, not at the start of the next row.
    expect(gap.x).toBe(last.x + last.width);
    expect(gap.y).toBe(grid.rows[0].y);
  });

  it("belongs to the row above when between rows, and clamps past either end", () => {
    const betweenRows = grid.rows[0].y + grid.rows[0].height + 2;
    expect(gapAt(grid, 5, betweenRows)?.y).toBe(grid.rows[0].y);
    expect(gapAt(grid, 5, -50)?.index).toBe(0);
    expect(gapAt(grid, 5, 10_000)?.index).toBe(3);
    expect(gapAt(grid, 10_000, 10_000)?.index).toBe(6);
  });

  it("does not exist in an empty layout", () => {
    const empty = computeJustifiedLayout({
      aspects: [],
      containerWidth: 300,
      targetRowHeight: 100,
      gap: 4,
    });
    expect(gapAt(empty, 0, 0)).toBeNull();
  });
});

describe("the order a drop makes", () => {
  const order = ["a", "b", "c", "d", "e"];

  it("moves one item forward and back", () => {
    expect(moveBlock(order, new Set(["d"]), 1)).toEqual(["a", "d", "b", "c", "e"]);
    expect(moveBlock(order, new Set(["b"]), 4)).toEqual(["a", "c", "d", "b", "e"]);
  });

  it("moves to either end", () => {
    expect(moveBlock(order, new Set(["c"]), 0)).toEqual(["c", "a", "b", "d", "e"]);
    expect(moveBlock(order, new Set(["c"]), 5)).toEqual(["a", "b", "d", "e", "c"]);
  });

  it("gathers a scattered selection into one block in its own order", () => {
    expect(moveBlock(order, new Set(["e", "b"]), 0)).toEqual(["b", "e", "a", "c", "d"]);
  });

  it("leaves the order alone when a block drops into a gap inside itself", () => {
    expect(moveBlock(order, new Set(["b", "c"]), 2)).toEqual(order);
    expect(moveBlock(order, new Set(["b"]), 1)).toEqual(order);
    expect(moveBlock(order, new Set(["b"]), 2)).toEqual(order);
  });
});

describe("a reorder, as the grid's anchoring tells it apart", () => {
  it("is the same paths in another order", () => {
    expect(isPermutation(["a", "b", "c"], ["c", "a", "b"])).toBe(true);
    expect(isPermutation(["a", "b"], ["a", "b", "c"])).toBe(false);
    expect(isPermutation(["a", "b"], ["a", "c"])).toBe(false);
  });
});
