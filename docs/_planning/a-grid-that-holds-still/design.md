# Design

Principle 1's five questions, in order.

## Placement

**The arithmetic goes in `lib/justifiedLayout.ts`; the effects and the gesture
state stay in `JustifiedGrid.tsx`.**

Three pure functions, each taking a `JustifiedLayout` — the type that module
already owns and exports:

```
heightAbove(layout, scrollTop)                  -> number
anchorAt(layout, scrollTop, viewportHeight)     -> { index, fraction }
scrollForAnchor(layout, anchor, viewportHeight) -> number
```

They belong beside the structure they read rather than in a module of their own.
A `lib/scrollAnchor.ts` with one consumer would be a new concept for no second
use (principle 2), and it would put functions that are *about* a
`JustifiedLayout` somewhere other than where `JustifiedLayout` lives. The
dependency direction is unchanged: the grid reads `lib/`, `lib/` knows nothing
about the grid.

What cannot move out of the component: the `createEffect` that fires on a layout
change, the `leaving` map, and the zoom-gesture state. Each is reactive or DOM
state, and hoisting them would mean inventing a store for something one component
owns.

**R8 is a different module and a different commit.** A `sort_positions`-style
command lives in `server/commands.rs` alongside `get_items`, delegating to the
same `sort::sorter` query with a `WHERE path IN (…)` and a rank. No new layer.

## Contract

| what changes | who is on the other side |
|---|---|
| a cell may be in the DOM while absent from `props.paths` | `grid.mjs`, anything counting cells |
| `scrollTop` is written by the grid, not only by the reader | `lib/scrollHost.ts`, the custom scroll rail |
| `JustifiedLayout` gains three pure readers | one caller each, in-bundle |
| **R8 only:** a new command returning sort positions for named paths | `galleryStore`, the command table |

**The first row is the one to watch.** A leaving cell is real DOM with a real
`<img>` for the length of its fade, so any check that counts
`img[src*='/thumb/']` sees a transient over-count. `grid.mjs` counts cells in
four places today. Those counts are all taken at rest, so they are safe, but the
new checks must sample deliberately rather than incidentally.

**The second row has a subtlety.** The rail's thumb and date markers are
positioned from `scrollTop`, so compensation moves the thumb without the reader
touching it. That is accepted and is the point: a marker shifting is
imperceptible next to a photograph shifting.

No wire change and no schema change for R1–R7.

## Cost in concepts

Four, counted honestly:

1. A transition duration, and the rule that it is suppressed on first mount.
2. A `leaving` map: path → its last geometry, swept after the transition.
3. A crossfade threshold. **Not** `PATCH_LIMIT`, despite both being 12 — that
   one is about where N round trips stop beating one payload, and reusing a
   constant because the numbers coincide is how two unrelated things become
   impossible to tune apart.
4. An anchor held for the duration of a zoom gesture rather than recomputed per
   notch.

Checked in the opposite direction, as principle 1 asks. Two candidates for
deletion rather than addition: the 1:1 aspect placeholder is already unreachable
for every format either reader can parse, and could go entirely if RAW and AVIF
were given a header read — but that is a different plan, and compensation covers
their corrections in the meantime. The `loading` prop could be deleted now that
the banner only renders on an empty grid; it is retained because the empty-grid
case is the one it is for.

Nothing here needs the word *except*.

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

**An anchor item for set changes too, for symmetry.** Rejected in the
requirements' measurement note: it is more machinery than `heightAbove` and it is
wrong for a change inside the viewport.

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
  drift. The alternative failure is that the anchor item scrolls out of the
  viewport mid-gesture at extreme zoom, which needs a re-pick and therefore
  reintroduces exactly the drift it avoids.
- **(measured, this session)** The layout takes no viewport height; the chrome
  overlays rather than pushes; `interactive-widget=resizes-content` is set; a
  warmed item is ~235–290 bytes.

## The two checks principle 1 asks for

**Second-implementation test.** No abstraction, interface or plugin point is
introduced. The three helpers are concrete functions with one caller each; if a
second grid ever existed they would still be the same three functions.

**Seam test.** Passes. The helpers sit with the type they read, the effects sit
with the reactive state they depend on, and nothing had to move for either to
fit.

## The work

Three commits, ordered so the *correct* half lands before the *pretty* half and
can be judged on its own.

**1. The grid holds its place.** R2, R3, R5, R6, R7 — `heightAbove`, `anchorAt`,
`scrollForAnchor`, the compensation effect, and the anchor held across a zoom
gesture via the `onSettle` hook `createWheelScroll` already exposes. No animation
yet, so every assertion is a number. `grid.mjs`: a removal above the viewport
leaves the first visible cell's rect unchanged; a removal inside the viewport
leaves the rows above it unchanged; a twenty-notch ctrl+wheel zoom leaves the
centre item within a few pixels of centre; a simulated keyboard-shaped height
change moves nothing. Each verified to fail before the change.

**2. The grid moves rather than jumps.** R1 and R4 — the transition, the
`leaving` map, the crossfade above the threshold. Preceded by the frame-rate
measurement named in the assumptions; if it fails, stop and re-present rather
than reaching for the fallback unasked. `grid.mjs`: after a removal, a surviving
cell's rect is sampled per frame and must take more than one frame to reach its
destination — the assertion that distinguishes a slide from a teleport.

**3. A change costs what it changed.** R8 — the sort-position command and the
client splice, replacing the wholesale refetch.

Docs on completion: `docs/frontend/grid-loading.md` gains the steadiness rules
beside the banner rule it already carries; `docs/server/README.md` gains the new
command in its table for commit 3; this directory is deleted.
