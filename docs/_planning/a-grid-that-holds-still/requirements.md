# A grid that holds still

## Background

The grid moves under the reader for reasons the reader cannot see. Five
mechanisms have been identified; two are fixed and three are live.

**Fixed already.** The `h-screen` "Loading…" banner rendered in flow above a
populated grid, displacing every row by exactly one viewport and back — measured
at 390x844, content height 1035 → 1879 → 1035 with `scrollTop` pinned. And
`record_view`, a read, reached every client as `tags-indexed`, which is what
triggered that refetch in the first place.

**Still live, and the subject of this plan:**

1. **A set change is a teleport.** `<For>` is keyed by path and cells are
   absolutely positioned at explicit coordinates, so a change to the item list
   repositions every surviving cell in a single frame. Nothing moves; things are
   simply elsewhere.
2. **A change above the viewport drags the screen with it.** Removing an item
   costs its row height, and every row below shifts up — including all of the
   ones being read. The cause is off-screen, so the motion is inexplicable.
3. **Zoom loses your place, and the error scales with depth.** `onZoom` sets
   `thumbnail_size` and nothing touches `scrollTop`, so the browser holds the
   literal pixel offset while the content height changes underneath. At the top
   there is no error; forty thousand pixels into a gallery that just grew thirty
   percent taller you land twelve thousand pixels early.

The governing sentence, which the requirements below are all readings of:
**nothing moves except where the reader caused it to move.**

## Requirements

### R1 — Items move, they do not jump

Every layout change is animated. A cell that survives the change slides from
where it was to where it is; a cell that arrives fades in; a cell that leaves
fades out, and only then does its space close.

### R2 — A change above the viewport moves nothing on screen

Items added or removed entirely above the scroll position leave every visible
cell exactly where it was. The scroll rail may move; photographs may not.

### R3 — A change inside the viewport is local to itself

An item removed from the middle of the screen closes its own gap from below.
Rows above it do not move at all — there is one motion, and it is where the
reader is looking.

### R4 — A large change is one motion, not many

Above **12 changed items**, the grid crossfades as a whole rather than animating
each cell independently. Two hundred cells flying to new positions reads as a
fault, not as responsiveness.

### R5 — Zoom keeps you where you were

The item nearest the centre of the viewport holds its *fractional* position in
the viewport across the whole zoom. Ctrl+wheel is continuous — twelve percent
per notch — so this must hold across a gesture of twenty notches without
accumulating drift, not merely across one step.

### R6 — A width change behaves like a zoom

Device rotation and a resized desktop window change the column count, which is
the same transformation as zoom by a different cause, and gets the same
treatment.

### R7 — A viewport height change does nothing

The software keyboard shrinks the scroll host on Android (see the measurements
below). The grid must not react: holding the top edge, so the reader sees less
of the same content, is the correct behaviour and it is also what the browser
does unaided.

### R8 — A set change costs what it changed

A client learns about new items without re-fetching the library. Measured at
**235 bytes per item** with thumbhash populated on only 8 of 30 files; warmed,
with real nested paths, roughly 290. A twenty-thousand item gallery is ~6 MB to
every connected client per upload batch — and moving photographs between devices
by tagging them is a primary workflow, so the most-used path is the one paying
most.

## Out of scope

- **Device provenance on events.** Considered and rejected: "my edit versus
  someone else's" is a fiction in a single-owner gallery, and the distinction
  would buy only the deferral below, which is also rejected.
- **Deferred removals.** An item that stops matching a filter disappears at once,
  like any other change. This was weighed against leaving it as a ghost until the
  view is re-run; immediacy won, because the gallery has one owner and a change
  they made on another device is one they are expecting to see.
- **Snapshot filters.** A filter is a live view, not a frozen one. A snapshot has
  no honest expiry, so it forces a "stale — reload" affordance, and that
  affordance is the thing that reads as dated.
- **A JavaScript unit-test runner.** The arithmetic in this plan would be
  pleasant to unit test, but adding `vitest` to test ~40 lines is a permanent
  dependency for one use. `grid.mjs` covers it in the real browser, which is
  where the failure would actually appear.

## What the measurements settled

**The justified layout is a function of width, and takes no viewport height.**
`computeJustifiedLayout` takes `aspects`, `containerWidth`, `targetRowHeight`,
`gap` and `groupStarts`. This is what makes R7 free: a compensation keyed on
*the layout changed* cannot hear the keyboard, because a height change cannot
produce a layout change. The naive implementation — reacting to "the viewport
geometry changed" — is avoided by never subscribing to viewport geometry.

**The chrome overlays the grid; it does not push it.** The scroll host is
`fixed inset-0` and the top bar is `fixed` above it, with no top inset on the
grid that tracks the bar. So the filter bar growing or shrinking covers and
uncovers cells but never moves them, and there is nothing to compensate for. The
question that prompted R7 assumed otherwise and was wrong.

**The keyboard does resize the scroll host, on Android only.** `index.html` sets
`interactive-widget=resizes-content` deliberately, so bottom-anchored UI is not
buried. iOS ignores that entirely and `TopBar` tracks `visualViewport` by hand
for the filter sheet. So R7 is a real requirement rather than a theoretical one
— it simply costs no code.

**The grid is already in the right shape for animation.** `<For>` keyed by path
means a surviving cell keeps its DOM node, and cells carry explicit
`top/left/width/height`. Sliding is therefore a CSS transition on four
properties, with no FLIP measurement, no reparenting and no snapshotting.

**An anchor item is unnecessary for set changes.** For a change entirely above
the viewport, pinning *any* visible item gives `newScrollTop = S - Δ` — the
choice of item cancels out. For a change inside the viewport, a centre anchor is
actively wrong: it scrolls to hold an item below the edit, which slides
everything above the edit downward while the gap closes from below. Compensating
by the height change of rows *strictly above the viewport* is simpler than
anchoring and is correct in all three positions.
