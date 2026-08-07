# 0007 — Two nested render windows, and degrade instead of stopping

[← docs index](../README.md) · [frontend/grid-loading](../frontend/grid-loading.md)

## Context

Both grids are virtual scrollers over a gallery that may hold tens of thousands
of items. Scrolling has to keep up on two very different engines: WebKitGTK on
the desktop, which decodes images on the main thread, and mobile Safari/Chrome
on a phone, which does not but has far less CPU and a much smaller memory
ceiling.

The original design used a single rendered window with a fast-scroll gate: above
a velocity threshold, thumbnail work stopped entirely and resumed when the
scroll settled. On a fling that meant a screen of empty cells for the whole
gesture, then a burst of work at the end — exactly when the user has stopped and
is looking.

## Options considered

**Widen the single window.** More rows rendered ahead means fewer empty cells,
but every row is rendered at full resolution, so DOM size and decode cost grow
together and the phone runs out of both.

**Keep the gate, prefetch harder before it trips.** Does not help: the gate
trips precisely when the scroll has outrun whatever was prefetched.

**Two nested windows** — a wide outer window rendered at a cheap tier, and a
narrower inner window carrying full resolution, with cells upgrading as they
cross the inner boundary.

**Drop the gate entirely** and let full-resolution work run at any velocity.

## Decision

Two nested windows, and a fast-scroll branch that *degrades* rather than stops.

The insight the single window missed is that "render a row" and "render a row
sharply" are separate costs, and only the second one is expensive. A cheap
128px rung costs a fraction of the decode of a 512px one, so the outer window
can be several times wider than the inner one for less than the cost of
widening a single full-resolution window. During a fling the grid stays
populated with recognisable images; the upgrade to full resolution happens once
cells sit inside the inner window with scrolling settled — usually off-screen,
so the swap is never seen.

Both windows are asymmetric, with more rows ahead of the scroll direction than
behind, and the ahead buffer grows by however many rows a scroll covers in one
measured image-load round-trip — velocity × latency, from an always-on EWMA —
capped so DOM growth stays bounded.

Rather than early-returning during a fling, `scheduleFetch` warms the
*projected landing window*: where the scroll is going to stop, ± one viewport,
deduplicated per fling. Speculative work is issued at a much smaller batch size
than the visible drain, because nothing can preempt a batch once issued, so the
batch size *is* the worst-case delay before a scroll onto cold cells can get CPU
back.

## Consequences

- Two windows means two sets of range signals, two eviction margins, and a
  staged upgrade path — genuinely more complex than one window, and justified by
  the fling behaviour it buys. This is the complexity budget for the grids;
  spend it elsewhere reluctantly.
- Cells that upgrade in place need an underlay `<img>` holding the outgoing
  image during the `src` swap: mobile engines drop the old bitmap for a frame or
  more even when the new URL is pre-decoded. Without it the upgrade flickers.
- `content-visibility` on out-of-window rows is what keeps the wider outer
  window affordable in layout cost.
- Queued paths are ranked at *drain* time against the current windows, never at
  enqueue time — by the time a batch drains, the scroll is usually somewhere
  else entirely (`lib/loadPriority.ts`).
- The two grids implement all of this separately, with ~250–300 near-identical
  lines and small policy differences. The convergence plan is in
  [`frontend/grid-loading.md`](../frontend/grid-loading.md).
- Most of the tuning targets WebKitGTK's main-thread decoder, which no harness
  in this repository can measure. The source marks which wins are demonstrated
  and which are reasoned; that distinction is worth keeping.
