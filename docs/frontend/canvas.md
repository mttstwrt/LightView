# The canvas

[← docs index](../README.md) · [frontend](README.md) · [the two grids](grid-loading.md)

A single pannable surface with the current sort spiralling out from its centre:
item 0 in the middle, each later item one step further round. It is the third
view onto the loading machine the grids share, and the first that is not a
reading-order scroller — which is what makes it worth its own page.

`components/gallery/CanvasView.tsx` is the view; `lib/spiralLayout.ts` is the
geometry it stands on.

## The spiral is a lattice, not a curve

Items sit on a square grid of cells, walked as a square spiral: right, down,
left twice, up twice, right three times, and so on. Ring *r* holds the indices
in `[(2r-1)², (2r+1)²)`, so `cellOfIndex` and `indexOfCell` are both closed
form and both constant time.

That inverse is the whole reason for this shape rather than a prettier one.
Every frame the view has to answer "which items are inside this rectangle?",
and with an inverse it answers by walking the rectangle's cells and mapping
each one back to an index — so a frame costs what it renders, not what the
gallery holds. [Decision 0016](../decisions/0016-a-square-spiral-on-a-lattice.md)
records the two alternatives that were given up for it, and what would justify
revisiting them.

Aspect ratios are preserved rather than cropped: each cell is a square slot
with the image fitted inside it, so a portrait leaves space at its sides and a
panorama at its top and bottom. That is what lets the canvas serve the
justified `j`/`jm`/`jh` tiers unchanged — the reason
[`views.rs`](../../src-tauri/src/views.rs) maps it to `ThumbTier::Justified`
and a gallery that already offers the justified grid pre-warms nothing extra
for it.

## What it inherits, and what it had to write

Inherited whole, constructed and otherwise untouched: `cellSources`,
`thumbQueue`, `urlVersions`, `pathIndex`, `fetchLoop`, `thumbProgress`,
`scrollDynamics`, `galleryControls`, `wheelScroll`, `thumbRegeneration`. This
is the split [`grid-loading.md`](grid-loading.md) proposed before the view
existed, and it held: the primitives own the bookkeeping that fails silently,
and the view owns its geometry and its policy.

Written here, because it *is* the view: the spiral layout, the range
calculation, `evict`'s notion of far away, what a batch drains, and what it
speculates on.

Two things differ from the grids in kind rather than in tuning.

**The window is a rectangle of a lattice.** Everything that took a range of
item indices takes a cell range instead. In index terms the canvas's window is
several disjoint runs — the four sides of each ring it crosses — so the
range-based ranker in `loadPriority.ts` would have called most of what is on
screen stale and dropped it from the queue. `pickByRank` is that function with
the zone→rank mapping lifted into a parameter, exactly as
[`grid-loading.md`](grid-loading.md) predicted would be wanted; the canvas
passes a ranker that measures distance from the viewport centre in lattice
steps, and `pickByPriority` stays as the range-based case both grids use.

**Look-ahead grows quadratically.** A grid that renders *k* rows past the
viewport pays *k* rows. A canvas that renders *k* cells past it on every side
pays the whole border, so the third ring out already costs more than the screen
holds. The buffers here are therefore small integers (two cells ahead, one
behind) where the grids can afford a dozen rows, and the resolution ladder does
the work instead: the cheap `j` tier covers the outer window, and only the
cells on screen — plus one row of border — are upgraded.

The ladder itself is JustifiedGrid's, against simpler numbers. A fitted cell's
longest edge is exactly the slot size, so the thresholds are the tiers' own
edges (512 and 1280) times the same upscale tolerance, rather than a row height
times an assumed aspect. There is no serve-the-original path: that is
JustifiedGrid's one genuine difference and the canvas has no need of it.

The background crawl gets a property for free that is worth knowing. Item order
*is* spiral order, so walking `props.paths` from 0 fills outward from the
centre — the direction the user is going to pan in — with no ordering logic of
its own.

## Panning, and why the scroller is a real one

The surface is finite: *n* items occupy `(2⌈√n⌉+1)` cells a side, which even at
a million images is well inside what a browser will lay out. So the canvas
scrolls an ordinary `overflow: auto` element in both axes rather than
transforming a layer, and gets touch panning, iOS momentum, trackpad
two-axis scrolling and keyboard scrolling without writing any of them.

Three shared primitives grew a second axis for it, each a no-op on a host that
cannot scroll sideways:

- `scrollHost` gained `scrollLeft`, `viewportWidth`, `maxScrollX` and
  `scrollToX`.
- `scrollDynamics` measures velocity as distance travelled over both axes
  rather than the change in `scrollTop`. On the grids' host the horizontal term
  is always zero and the number is exactly what it was; on the canvas,
  measuring only the vertical component would report a fast sideways fling as a
  standstill and hand it a full-resolution window every frame. `direction`
  stays vertical, with `directionX` beside it.
- `wheelScroll` carries deltaX, and treats shift+wheel as sideways where the
  engine has not already done that conversion. It had to: that handler calls
  `preventDefault()` on every wheel event it sees, so a canvas without it would
  swallow every trackpad sideways swipe and pan nowhere.

A mouse still only has one wheel, so dragging the background pans, with a
`grab` cursor. It is mouse-only — touch already pans the scroller natively —
and a drag that starts on a thumbnail is left alone, because that is a
selection gesture or a click.

**Zoom is not a scroll offset.** A cell's position is
`(coordinate + rings) × pitch`, so changing the cell size moves every cell at
once: left alone, a step from 200px to 240px cells throws the viewport a fifth
of the way across the spiral. One effect watches the surface's frame — pitch,
ring count, gap — converts the anchored viewport point to a lattice coordinate
against the old frame and back against the new, and scrolls so it lands where
it was. Ctrl+wheel anchors on the cursor; everything else (the settings slider,
a file added or deleted, which changes the ring count) anchors on the viewport
centre.

Two things the canvas deliberately does not get:

- **The scrollbar.** `ScrollBar` maps one scroll offset onto a position in the
  sort; on a spiral that position is two coordinates and a ring. Its date
  markers would be wrong rather than merely unhelpful.
- **Start-at-bottom.** The foot of this surface is the empty corner of the
  outermost ring. The view opens on the centre, which is where the top of the
  sort is.

Opening on the centre waits for two things that both arrive late: the scroll
host, because Solid creates a child before appending it to its parent and this
view's `onMount` can run before `App` has registered the host, and the first
non-empty item list, because the view is mounted while the gallery is still
loading and the surface is one cell wide until the items land.

## Verifying a change

`tsc` covers none of this. `lib/spiralLayout.ts` is pure and worth checking
directly — compile it and round-trip `cellOfIndex`/`indexOfCell` over a few
thousand indices, which is how the corner cases in the ring walk were settled.
Everything else needs the real SPA against `lightview-headless`; see
[`build-and-verify.md`](../build-and-verify.md).
