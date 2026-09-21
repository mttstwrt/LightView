# Design

Principle 1's five questions, in order.

## Placement

**Three pure functions in `lib/justifiedLayout.ts`, one new capability on
`lib/scrollHost.ts`, the reactive state in `JustifiedGrid.tsx`.**

The helpers take a `JustifiedLayout` — the type that module already owns — and,
crucially, a **path or index**, because identity across the change is the whole
of the information (see the requirements' note):

```
topVisible(layout, cells, scrollTop)        -> { index, offsetIntoViewport }
centreVisible(layout, scrollTop, height)    -> { index, fraction }
scrollToHold(layout, anchor, height)        -> number
```

They belong beside the structure they read. A module of their own would be a new
concept for one consumer, and they are now unit-testable in `node` — which is
the verification that would have caught the refuted design and that the browser
harness structurally could not.

**`lib/scrollHost.ts` gains `adjustBy(delta)`.** This is the piece the first
draft lacked entirely (R8). It writes `scrollTop` *and* records the adjustment;
`scrollDynamics`, `TopBar` and `ScrollBar` subtract recorded adjustments from the
delta they observe, so a compensating write is invisible to all three. Three real
consumers, so it is not an abstraction built for one.

**What stays in the component:** the effect that fires on a layout change, the
`leaving` map, and the zoom-gesture state. Each is reactive or DOM state that one
component owns.

**`lib/wheelScroll.ts` gains gesture edges.** R5 needs to capture an anchor on
the first notch and release it when the gesture ends. `onSettle` cannot serve:
`wheelScroll.ts:68` returns on a handled zoom *before* `animating`, so during a
zoom it fires zero times — and it is a momentum tail, not a start. A debounce
inside the zoom branch, surfaced as `onZoomStart` / `onZoomEnd`.

## Contract

| what changes | who is on the other side |
|---|---|
| a cell may be in the DOM while absent from `props.paths` | `grid.mjs`, `cellSources.prune`, `geom`, `pathIndex`, `viewerTransition` |
| `scrollHost` gains `adjustBy`; three consumers learn to discount it | `scrollDynamics`, `TopBar`, `ScrollBar` |
| `wheelScroll` gains `onZoomStart` / `onZoomEnd` | `JustifiedGrid`, its only caller |
| `JustifiedLayout` gains three pure readers | one caller each |
| `overflow-anchor: none` on the scroll host | the browser |

**The first row is far more entangled than the first draft admitted**, and every
one of these is a real collision rather than a hypothetical:

- `cells.prune` nulls the URL (`cellSources.ts:149`) the instant a path leaves
  `props.paths`, so without intervention a leaving cell fades out as an **empty
  box** — the exact opposite of R1.
- `geom` is rebuilt from `visibleCells()` and the cell body is gated on
  `<Show when={g()}>`, so a leaving path's wrapper unmounts before any
  transition can run. `leaving` has to feed `geom` too.
- `pathIndex.indexOf(path) ?? -1` for a leaving path is **−1**, and that reaches
  `openViewer(-1)` on click and collapses an in-progress drag-selection range on
  hover. A leaving cell must be inert from the moment it starts leaving.
- `viewerTransition` takes the *first* `[data-vt-path=…]` match, so a path that
  leaves and returns inside the transition window gives the fly-back two
  candidates and it may land on the corpse.
- **`<For>` iterates `visiblePaths()` — the virtualized window, not
  `props.paths`.** Cells leave that array on every scroll step, so a `leaving`
  map driven off `<For>` exits would retain a live `<img>` for every row flung
  past, which is precisely the decoded-bitmap retention `scrollDynamics`
  documents as killing phone tabs. "Left the item list" and "left the render
  window" are different events and only the first may animate.

`overflow-anchor: none` is not optional. The first draft asserted the browser
could not anchor here, on the grounds that the cells are out of flow inside a
`contain: strict` box. That reasoning is shaky — Blink excludes boxes whose
containing block is outside the scroller, and here the containing block is the
positioned track *inside* the host. Nothing in the tree sets `overflow-anchor`,
so the host is at the default `auto`. If Chrome does anchor, it adjusts
`scrollTop` before the effect reads it and the effect applies its correction on
top — overshoot on Chrome and Firefox but not Safari, which is the worst
possible shape of bug. Turning it off makes the question moot rather than
answering it.

No wire change and no schema change.

## Cost in concepts

Seven, counted honestly — up from the four the first draft claimed, and the
increase is the review's doing rather than the design growing ambition:

1. A transition duration, suppressed on first mount.
2. A `leaving` map, distinct from the virtualization window, feeding both the
   render list and `geom`, and inert to pointer events.
3. A crossfade threshold. **Not** `PATCH_LIMIT`, despite both being 12 — that one
   is about where N round trips stop beating one payload. Sharing a constant
   because two numbers coincide is how two unrelated things become impossible to
   tune apart. Its unit is **changed set membership**, not changed geometry;
   under R2's reflow the geometry reading would crossfade on every single
   removal.
4. An anchor by path, captured before a change and restored after.
5. A second anchor rule for scale changes, and the discrimination between the
   two — which the effect cannot infer from `layout()` alone and must derive by
   comparing the previous `props.paths` / `targetRowHeight` / `containerWidth`.
6. `adjustBy`, and the notion of a scroll delta that the reader did not cause.
7. Gesture edges on `wheelScroll`.

Seven is a lot. It is the honest price of "nothing moves except where the reader
caused it to" in a virtualized reflowing grid, and it should be read as an
argument for doing the first commit and then *stopping to look* rather than
running the second on momentum.

Checked in the opposite direction, as principle 1 asks. One deletion is
available and taken: `overflow-anchor: none` removes a browser behaviour from
the picture rather than negotiating with it. One candidate was rejected — the
first draft proposed deleting the `loading` prop, which is wrong: it is what
distinguishes "No media files found" from "Loading…" on an empty grid, and
conflating those is a regression.

## Alternatives

**`solid-transition-group`.** A dependency for what is about thirty lines here,
and it solves the easy half. It has nothing to say about absolute positioning, a
virtualized window, or scroll compensation — the parts that are actually hard.
Rejected on what it fails to remove.

**FLIP via the Web Animations API.** The standard answer when an element's new
position is only knowable by measuring it after layout. Here both positions are
explicit numbers the grid computed itself, so the measure-and-invert half is
pure overhead. Rejected as machinery for a problem this codebase does not have.

**The browser's own `overflow-anchor`.** Would be the correct answer if it could
work. It cannot: scroll anchoring selects an in-flow descendant, and the cells
are absolutely positioned inside a `contain: strict` box, so there is nothing for
it to hold. Rejected on fact rather than on preference.

**The View Transitions API.** One snapshot of the document, no per-element
control inside a virtualized list, and it would fight the recycling window.
Rejected.

**A pure layout-arithmetic correction, with no anchor.** This *was* the design,
and it is refuted rather than merely rejected: a function of a layout and a
scroll offset cannot carry identity across a change, so its correction evaluates
to ~0. `justifiedLayout.test.ts` pins the refutation so nobody rediscovers the
idea and finds it appealing.

**A centre anchor for set changes too, for symmetry with zoom.** Rejected: for an
edit inside the viewport it holds an item *below* the edit, sliding everything
above the edit downward while the gap closes from below — two motions, one of
them inexplicable. The top-visible item is above any in-viewport edit, which is
why R3 needs no mechanism of its own.

**Animating `transform` instead of `top`/`left`.** This is the fallback if the
assumption below fails, not the first choice: it means every cell carries a
fixed base position plus an offset, which is a second coordinate system for
readers of the layout to hold. Deferred until measured, deliberately.

## Assumptions

- **(unmeasured, and the one that could sink R1)** A CSS transition on
  `top`/`left`/`width`/`height` across the forty-odd on-screen cells holds
  60 fps. These are layout-triggering properties, not compositor-only ones;
  `contain: strict` on the parent bounds the recalculation, but it does not make
  it free. **To be measured before committing to the approach**, on the phone
  profile, with a removal that shifts every visible row. If it janks, the
  fallback is the `transform` alternative above and the cost is the second
  coordinate system.
- **(unmeasured)** 12 is the right crossfade threshold. It is a guess borrowed
  from the shape of `PATCH_LIMIT`'s reasoning, not from watching 12 cells move.
  Cheap to change; named here so it is not mistaken for a measurement.
- **(unmeasured)** Holding the zoom anchor across a gesture is enough to prevent
  drift. Two failure modes, and the second is the common one: the anchor item
  scrolling out of view mid-gesture at extreme zoom, and — far more often —
  `scrollToY` clamping to `maxScroll` in the last viewport of any gallery, where
  zooming out shrinks the content by ~12% a notch and every notch clamps. A
  clamped restore destroys the anchor's fraction, so "twenty notches without
  drift" is unmeetable there unless the clamp is handled explicitly. The same
  clamp affects a removal near the bottom, where the browser has already
  clamped before the effect reads `scrollTop`.
- **(measured, this session)** The layout takes no viewport height; the chrome
  overlays rather than pushes; `interactive-widget=resizes-content` is set; one
  removal displaces later items by 18/283/11/29 px while total height moves 10.
- **(extrapolated, not measured — was previously filed as measured)** ~290 bytes
  for a warmed item with real nested paths. The measurement was 235 bytes on
  short paths with thumbhash on 8 of 30 files.

## The two checks principle 1 asks for

**Second-implementation test.** The helpers are concrete functions with one
caller each, not abstractions. `adjustBy` is the one addition that looks like an
interface, and it has **three** real consumers on day one — `scrollDynamics`,
`TopBar`, `ScrollBar` — so it is extracted on demonstrated use rather than
anticipated use. The gesture edges on `wheelScroll` have a single caller and are
deliberately two callbacks rather than a gesture abstraction.

**Seam test.** Passes, with one qualification worth stating. The helpers sit with
the type they read and the effects with the reactive state they depend on, so
nothing had to move. But `adjustBy` exists because three modules independently
infer user intent from a scroll event, and that inference is the actual seam —
each of them is guessing at something no one tells them. Discounting a recorded
adjustment is the cheap fix; giving the host a notion of *who caused this scroll*
would be the honest one, and is worth revisiting if a fourth consumer appears.

## The work

Two commits. What was commit 3 is out of this plan entirely (see the
requirements' out-of-scope note).

**1. The grid holds its place.** R2, R3, R5, R6, R7, R8, R9 — the anchor
helpers, `adjustBy` and its three consumers, the zoom-gesture edges,
`overflow-anchor: none`, and the effect that discriminates a set change from a
scale change. No animation, so every assertion is a number.

*Verified by `npm test`* for the arithmetic: an anchor restored across a
reflowing change holds its offset; a centre anchor demonstrably does not, for an
in-viewport edit; the clamp at the end of the content is handled rather than
silently destroying the anchor's fraction.

*Verified by `grid.mjs`* for the wiring, on a **larger fixture** — the current 12
PNGs are barely a viewport at desktop width, so today the harness has no content
above the fold to remove and no room for twenty notches of zoom without
clamping. Checks: a removal above the viewport leaves the top-visible cell's rect
unchanged; the mobile chrome does not move when the grid compensates; a
twenty-notch ctrl+wheel leaves the centre item within a few pixels of centre.
Each verified to fail before the change.

**2. The grid moves rather than jumps.** R1 and R4 — the transition, the
`leaving` map with all five collisions above resolved, the crossfade above the
threshold. **Preceded by the frame-rate measurement** named in the assumptions;
if it fails, stop and re-present rather than reaching for the `transform`
fallback unasked. `grid.mjs`: a surviving cell's rect sampled per frame must take
more than one frame to reach its destination — the assertion that distinguishes a
slide from a teleport.

Also in commit 1, because it is a doc contradicting code (principle 5):
`JustifiedGrid.tsx:160-166` says a just-added file is inserted with NULL
dimensions "after the frontend has already fetched the sorted items". That is no
longer true — the watcher now runs `probe_and_store` before it sends
`FsChanged`, so dimensions are written before any client hears about the file.
`measuredAspects` still exists for un-reprobed caches and for the formats
`dimensions()` cannot read, which is why R9 exists.

Docs on completion: `docs/frontend/grid-loading.md` gains the steadiness rules
beside the banner rule it already carries; this directory is deleted.
