# 0016 — The canvas spirals on a square lattice

[← docs index](../README.md) · [frontend](../frontend/README.md) · [the canvas](../frontend/canvas.md)

## Context

[C2](../todo.md) asked for an infinite scrolling canvas: the top of the current
sort in the centre, later items spiralling outward, reusing the
aspect-preserving justified tiers so it costs no new thumbnails.

The visual brief is loose; the engineering constraint is not. The canvas is a
virtualized view of a gallery that can hold hundreds of thousands of items, so
whatever arranges them has to answer two questions cheaply, one of them once
per frame:

1. **Which items are inside this rectangle?** Asked every frame the view moves.
   It must cost what the viewport renders, not what the gallery holds, and it
   must not get more expensive the further the user pans from the centre.
2. **Where is item *i*?** Asked for every path the view holds, on every pass of
   the fetch loop (eviction), and again whenever the viewer navigates.

Both grids get these for free, because a reading-order scroller's window is a
contiguous range of indices and a division recovers a row. A spiral has neither
property by default.

## Options considered

**Phyllotaxis — the sunflower arrangement.** Item *i* at angle *i* × the golden
angle, radius proportional to √*i*. It is the arrangement anyone reaching for
"organic spiral" has in mind, it fills the plane at uniform density, and
question 2 is a one-line closed form.

Question 1 is where it fails. There is no inverse: from a rectangle you can
recover the *range of radii* it spans, and therefore a contiguous range of
indices, but a viewport far from the centre intersects a thin arc of a very
large annulus. The candidate range is the whole annulus, so the work per frame
grows linearly with distance from the centre — the cost of browsing would
increase the longer someone browsed. Fixable with a spatial index rebuilt on
every sort, filter and zoom change, which is a large amount of machinery to
carry for a look.

**Ring-packed cells of varying width.** Each ring a justified strip of
aspect-sized boxes, filling the plane with no letterboxing at all — visually
the nicest of the three, and the natural extension of what `justifiedLayout.ts`
already does for rows.

It has a closed form in neither direction: where a ring ends depends on the
aspect ratios of everything before it, so both questions need a precomputed
layout plus a binary search per ring side. That is buildable — the justified
grid precomputes exactly this kind of layout over the whole gallery — but it is
a layout pass on every zoom step and every filter change, and the range query
is four monotone runs per ring rather than an arithmetic expression.

**A square spiral on a uniform lattice.** Cells on an integer grid, walked
outward: right, down, left twice, up twice, and so on. Ring *r* holds indices
`[(2r-1)², (2r+1)²)`, each of its four sides is an arithmetic progression, and
both questions are constant-time closed forms — the second one, `indexOfCell`,
is the inverse the other two designs lack. Items keep their aspect ratio by
being fitted inside a square slot, which letterboxes them.

## Decision

The square lattice.

Question 1 is answered by walking the rectangle's cells and mapping each back
to an index, so a frame costs what it renders and costs the same at any
distance from the centre. Question 2 is `cellOfIndex`. Neither needs a
precomputed layout, so a zoom, a re-sort or a filter change costs nothing but a
re-render — and the geometry is a pure module (`lib/spiralLayout.ts`) that can
be tested without a browser, which is how the ring walk's corner cases were
settled.

The price is the letterboxing. A portrait leaves space at its sides and a
panorama at its top and bottom, so the canvas is less dense than a justified
layout of the same cell size. Aspect ratios are still *preserved* — nothing is
cropped, which is the part the brief cared about, and the part that lets the
view serve the `j`/`jm`/`jh` tiers unchanged.

## Consequences

- The canvas costs the gallery no thumbnails of its own: `views.rs` maps it to
  `ThumbTier::Justified`, so enabling it beside the justified grid pre-warms
  nothing extra, and enabling it alone warms the same tier that grid would.
- The look-ahead window is a border rather than a run of rows, and a border
  grows quadratically — so the buffers are small integers and the resolution
  ladder carries the depth instead. See [`canvas.md`](../frontend/canvas.md).
- The surface is finite and modest: *n* items span `(2⌈√n⌉+1)` cells, about
  484,000 px a side at a million images and a 240px cell, which is well inside
  what browsers lay out. That is what allowed the view to be an ordinary
  two-axis scroller rather than a transformed layer, and so to inherit touch
  panning, momentum and trackpad scrolling rather than reimplement them.
- `loadPriority.pickByPriority` was split into a rank-by-callback core plus the
  range-based case, because a lattice window is several disjoint index runs and
  the range ranker would have called most of the screen stale. That split was
  anticipated in `grid-loading.md` and is otherwise unremarkable.
- If the letterboxing is what people actually complain about, the ring-packed
  design above is the one to reach for, and it should arrive as a precomputed
  layout beside `justifiedLayout.ts` rather than as a change to this geometry.
  Phyllotaxis should not be revisited without a spatial index designed first.
