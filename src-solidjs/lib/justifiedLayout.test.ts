// What the layout does to everything *after* a change, which is the fact a
// scroll-steadiness design has to be built on.
//
// These exist because a plan was written on the opposite assumption — that
// removing an item shifts everything below it by one uniform delta, so holding
// any visible cell in place holds them all. It does not, and the arithmetic
// that followed from it computed a correction of zero. The browser harness
// could not have caught either error; three assertions here would have.

import { describe, it, expect } from "vitest";
import { computeJustifiedLayout } from "./justifiedLayout";

/** The real defaults, so the numbers mean something. */
const layout = (aspects: number[], groupStarts?: number[]) =>
  computeJustifiedLayout({
    aspects,
    containerWidth: 1600,
    targetRowHeight: 240,
    gap: 4,
    groupStarts,
  });

/** A repeatable spread of portrait, square and landscape. */
const aspects = (n: number) =>
  Array.from({ length: n }, (_, i) => [0.75, 1, 1.5, 1.33][i % 4]);

describe("a change reflows everything after it", () => {
  it("does not displace later rows by a single uniform delta", () => {
    const before = layout(aspects(40));
    const after = layout(aspects(40).filter((_, i) => i !== 1));

    // Compare each surviving item's y against where it was. If the content
    // below a removal simply slid up, every one of these would be identical.
    const yOf = (l: ReturnType<typeof layout>, index: number) => {
      for (const row of l.rows) if (row.cells.some((c) => c.index === index)) return row.y;
      throw new Error(`item ${index} is in no row`);
    };
    const shifts = new Set<number>();
    for (let i = 10; i < 30; i++) shifts.add(yOf(before, i) - yOf(after, i - 1));
    expect(shifts.size).toBeGreaterThan(1);
  });

  it("changes the heights of rows it did not touch", () => {
    const before = layout(aspects(40));
    const after = layout(aspects(40).filter((_, i) => i !== 1));
    const heights = (l: ReturnType<typeof layout>) => l.rows.map((r) => Math.round(r.height));
    // Row membership moves, and a row's height is `avail / sumAspect` over the
    // items that landed in it — so a removal near the top re-sizes rows far
    // below it. Anything that assumes otherwise is assuming text does not
    // reflow.
    expect(heights(before).slice(4, 9)).not.toEqual(heights(after).slice(4, 9));
  });

  it("is contained by a forced group break", () => {
    // The one case where the cascade stops. Grouping is off by default, so a
    // design may not rely on this, but a design that *breaks* here is wrong
    // twice.
    const starts = [0, 20];
    const before = layout(aspects(40), starts);
    const after = layout(aspects(40).filter((_, i) => i !== 1), [0, 19]);
    const rowOfItem = (l: ReturnType<typeof layout>, index: number) =>
      l.rows.findIndex((r) => r.cells.some((c) => c.index === index));
    // Item 20 before / 19 after both start the second group, so both open a row.
    expect(before.rows[rowOfItem(before, 20)].cells[0].index).toBe(20);
    expect(after.rows[rowOfItem(after, 19)].cells[0].index).toBe(19);
  });
});

describe("height above an offset carries no information about a change", () => {
  it("is approximately the offset itself, in any layout", () => {
    // The refuted helper: a pure function of (layout, scrollTop) that answers
    // "how much content is above the viewport" can only answer with the top of
    // whichever row straddles that offset — which is within one row height of
    // the offset in *every* layout. Evaluated before and after a change at the
    // same scroll position, the difference is noise, not the correction.
    const S = 1200;
    const topOfStraddlingRow = (l: ReturnType<typeof layout>) => {
      let top = 0;
      for (const row of l.rows) if (row.y <= S) top = row.y;
      return top;
    };
    const before = layout(aspects(40));
    const after = layout(aspects(40).filter((_, i) => i !== 1));

    const tallestRow = Math.max(...before.rows.map((r) => r.height));
    expect(Math.abs(topOfStraddlingRow(before) - S)).toBeLessThanOrEqual(tallestRow);
    expect(Math.abs(topOfStraddlingRow(after) - S)).toBeLessThanOrEqual(tallestRow);

    // Which is the whole point: the two agree with each other far more closely
    // than either agrees with the real displacement, so their difference cannot
    // be the correction. Identity across the change is the missing input.
    const naive = topOfStraddlingRow(before) - topOfStraddlingRow(after);
    expect(Math.abs(naive)).toBeLessThan(tallestRow);
  });
});
