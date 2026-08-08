# 0002 — Seven thumbnail tiers, in two families

[← docs index](../README.md) · [pipeline](../pipeline/README.md)

## Context

The gallery renders in two layouts: a fixed grid of square cells, and a
justified layout of aspect-preserving rows. Both are zoomable, so a cell's
displayed size varies by more than an order of magnitude. Serving one cached
size means either upscaling a small thumbnail into a large cell (visibly soft)
or decoding a large one into a small cell (the webview decodes on the main
thread, so this is the expensive direction).

## Options considered

**One cached size, resized in the browser.** Simplest, and wrong at both ends of
the zoom range.

**Resize per request from the original.** Always exactly right, no cache to
bound, and far too slow: thumbnail generation is decode-bound, and a source
decode per cell per scroll is exactly the CPU peg the cache exists to avoid.

**A ladder of cached sizes.** Pick the nearest rung at or above the displayed
size.

**A ladder of square crops only,** with the justified layout letterboxing them.

## Decision

Seven tiers in two families: four square-cropped (Micro 128, Standard 512,
Large 1024, Preview 1600) for the fixed grid, and three aspect-preserving
(Justified 512, JustifiedMid 1280, JustifiedHigh 2560) for the justified layout.

The two families are the substantive half of this decision. A fit tier cannot be
derived from a square one — the crop has already thrown the pixels away — so the
justified layout needs its own source decode even when the square tier is warm.
Letterboxing square crops was rejected because it wastes the cropped pixels and
still shows the crop's composition, which is the thing the justified layout
exists to avoid.

`JustifiedMid` at 1280 is the one rung whose existence needs justifying on its
own. Without it, mid zoom serves `jh` (2560) into cells far smaller than that —
roughly sixteen times the pixels the cell displays, decoded on the main thread.
Its rows are about a quarter the bytes of `jh`'s, so an equal byte budget holds
roughly four times as many, which matches usage: mid zoom shows more cells per
screen than high zoom does.

`Standard` is deliberately the hub. It is the only tier carrying a
`resize_filter` column and the only one carrying the ThumbHash blob, and Micro
is derived from its bytes rather than from the original.

## Consequences

- `ThumbTier::ALL` becomes load-bearing: it is the source of truth that
  propagates a tier to `path_keyed_tables()`, `clear_thumbnails`,
  `get_all_tier_info`, and the per-file delete. Adding a tier is a schema change
  *and* a maintenance change.
- Seven tables of blobs is a lot of disk. Four of the tiers are unbounded
  because they are small and generated for everything; the two zoom tiers are
  bounded — see
  [decision 0004](0004-byte-budgeted-lru-for-zoom-tiers.md).
- Tier selection becomes frontend policy, and the two grids implement it
  differently (cell size × DPR with an upscale tolerance, versus a hysteretic
  zoom level). See [`frontend/grid-loading.md`](../frontend/grid-loading.md).
- Deriving Micro from Standard rather than from the original is worth ~10–20×
  on that path, and is why the invariant "every Standard generation also
  produces its derivations" has to hold: a Standard row without its Micro row
  sends the grid's cheap rung back to a full source decode per 128px thumbnail.
