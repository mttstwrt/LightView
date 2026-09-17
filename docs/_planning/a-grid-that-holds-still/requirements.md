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

### R2 — A change above the viewport holds the top edge

The item at the top of the viewport stays at the same offset from the top of the
viewport. **Not every visible cell** — that is unachievable and the requirement
used to claim it. A justified grid reflows like text: row height is
`avail / sumAspect` over the items that landed in the row, so removing one item
repacks every row after it. Measured at the real defaults, removing a single
item near the top displaces later items by 18, 283, 11 and 29 px — four
different amounts — while total height moves 2081 → 2071. Cells below the top
edge will move, and the honest promise is that the reader's place is kept, not
that the picture is frozen.

Pinned by `justifiedLayout.test.ts`.

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

### R8 — Compensation is invisible to everything that watches scrolling

A `scrollTop` the grid writes must not read as a scroll the reader performed.
Three consumers currently cannot tell the difference, and all three misbehave at
trivial magnitudes: `scrollDynamics` calls anything over ~21px in a frame a
fling and drops newly revealed cells to the cheap rung; `TopBar` hides or
reveals the mobile chrome over 6px; `ScrollBar` fades the rail in for 1200 ms on
every scroll event. A grid that moves itself in order to hold still, and thereby
moves the chrome, is this plan's own thesis inverted.

### R9 — A layout change from a measured aspect is a layout change like any other

`recordMeasuredAspect` fires once per decoded image whose dimensions the index
did not have, and on a cold cache that is dozens of layout changes in a few
seconds — more frequent than every other cause combined. It gets the same
treatment as the rest, or loading a gallery becomes a shimmer.

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
- **R8, and the whole of what was commit 3.** Pulled into a plan of its own. Its
  justification did not survive review: `tags-indexed` carries no paths, so a
  command answering "where do these sort" has nothing to work with for the
  tagging workflow that motivated it, and would only help uploads. It also
  requires maintaining `groups` across a splice — `setGroups` is called only
  from `refresh()`, so every `start_index` after an insert is wrong. That is
  already a latent bug on the removal path and commit 3 would have made it the
  common case. None of it belongs in a change about motion.

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

**Set changes need an anchor by path, and it is the top-visible item.** This
replaces the opposite claim, which was wrong twice over and is worth recording
because both errors are easy to make again.

The first: "pinning any visible item gives the same answer, so the choice
cancels out." That holds only if everything below the edit moves by one uniform
delta, which a justified layout does not do — see R2. Two visible cells
generally move by different amounts and change size, so the choice does not
cancel and the derivation was void.

The second, worse: "then no anchor is needed — compensate by the change in
height of rows above the viewport." Any pure function of a layout and a scroll
offset can only answer that question with the top of whichever row straddles the
offset, which is within one row height of the offset in *every* layout. The
difference between the old and new answers is therefore ~0, always, while the
real correction is a full row height. Ten rows of 100 px at S=500: delete a row
above, the content that was at y=500 is now at y=400, but row 5 still *starts*
at 500 — it holds different photographs. Computed 0, correct 100.

What carries the information is identity. Record the top-visible item's path and
its row top's offset from the viewport top; after the change, find that path's
new row top and restore the offset. A centre anchor is still wrong for set
changes, for the reason originally given: it holds an item below an in-viewport
edit, sliding everything above the edit downward while the gap closes from
below. The top-visible item is above any in-viewport edit, so R3 falls out of
the same mechanism rather than needing a second one.
