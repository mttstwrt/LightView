# The grid, and how it decides what to load

[← docs](../README.md) · [frontend](README.md)

One justified grid, aspect-preserving rows, virtualized. The square-cell grid
and the map view that used to sit beside it are gone, and with them the view
switcher, the enabled-views setting, and the second copy of everything below.

Its problem: stream thumbnails into a virtual scroller fast enough that a fling
lands on pictures rather than skeletons, without asking the server for a
screenful of full-resolution decodes every frame.

## Rows, and the one that ends a group

Items are placed in order and wrapped into rows; a row commits the moment
justifying it to the full width would shrink it to its target height, so every
committed row fills the width exactly.

The rows that *don't* commit are the last of the content and the last of each
group, and grouping is monthly by default — so a rule that leaves them at their
target height leaves a ragged right edge a dozen times down a scroll, not once
at the end. Such a row is stretched to fill the width when that costs little
height, and left short when it would not: `FINAL_ROW_STRETCH` in
[`justifiedLayout.ts`](../../src-solidjs/lib/justifiedLayout.ts) is where the
line sits, at one and a half times the row's natural height.

The trade is not symmetric, which is why there is a ceiling rather than a plain
"always justify". A group's last row has fewer items than a full one, so filling
the width means growing taller, and the fewer the items the more growth it
takes: at the usual geometry three landscapes need 1.48× and fill, while one
needs 4.4× and does not. Stretching regardless turns a short row into a
double-height row that is *still* short of the width — worse on both counts
than leaving it alone.

## The machine, in seven parts

1. **A virtual range.** A `recalcRange` reads the scroll host's offset each
   frame and updates row-range signals **only when they change** — the
   optimization that stops a reactive recomputation on every scroll pixel.
2. **Two nested windows.** An outer rendered window carrying a cheap tier for
   deep look-ahead, and an inner full-resolution window. Both are asymmetric:
   more rows ahead of the scroll direction than behind, with the ahead buffer
   growing by however many rows a scroll covers in **one measured image-load
   round trip**, capped so DOM growth stays bounded. A slow network or big tiers
   means a deeper prefetch; instant loopback means exactly the base.
3. **A resolution ladder.** A new cell gets the cheap rung while it is outside
   the inner window or a fling is in progress, and upgrades to the target tier
   once it sits in the inner window with scrolling settled — usually off-screen,
   so the swap is never seen.
4. **404-driven generation.** A cell's `<img>` points optimistically at its tier
   URL. A cached thumbnail loads instantly; one the server has not made yet 404s,
   and `onError` queues it for generation. This is the recovery path, not the
   normal one — the idle worker and the look-ahead are what keep it rare.
5. **A bounded fetch loop.** One drain at a time, woken by a miss rather than
   polled, with speculation gated behind everything the user is actually waiting
   on.
6. **Landing-zone warming.** A fling has a predictable destination, so the base
   tier around the projected landing position is warmed — on the server, and
   then in the browser's own HTTP cache at low priority so it cannot compete
   with the visible cells.
7. **ThumbHash placeholders.** Each item's ~25-byte hash is inlined in the items
   payload, so every cell paints a blurry approximation before any thumbnail
   request goes out. Zero extra round trips, which is what makes a phone on the
   far side of a LAN feel instant.

## What a scrub does: nothing

`settled` is false for the whole of a fling and the whole of a scrollbar
gesture; `warping` is true while the view is moving faster than anything could
load.

**While warping, no sources are assigned and no speculation starts.** The window
turns over completely each frame, so assigning would issue a request per cell
for thousands of cells nobody sees. Assignment resumes on the frame the scrub
slows down.

The same reasoning shapes the ahead buffer: the adaptive part is a bet that the
scroll will keep going the way it is going, which is how a fling behaves and how
a scrollbar does not — a warp reports an enormous velocity for a single frame
and then stops dead. So a scrollbar gesture holds the buffer at its base, the
same concession a constrained network makes.

A scrollbar gesture also keeps the view unsettled for its whole duration, so a
burst of scrollbar stops costs **one** tier upgrade at the end rather than one
per stop.

## The staged upgrade

Two passes over the same cells: the rows on screen first, then the look-ahead
rows inside the full-resolution window — and only if the first pass left nothing
outstanding.

A full-rung source costs the server's bounded pool a decode per request, and the
full window is several rows deep, so upgrading it all in one sweep put roughly
seven times more expensive requests in flight than the viewport needed. Cutting
concurrent full-resolution requests from ~17 to ~5 is the difference between the
visible rows queueing behind the look-ahead and not.

Measured neutral for *decode* cost in a browser — the browser decodes off the
main thread and the server's pool already serves in arrival order — and kept for
the other half: each full-resolution source is a decode on a bounded pool, and
over a LAN it is also bytes on the wire.

## What the decode gate was, and why it is gone

WebKitGTK decoded images on the webview's main thread, so assigning a wall of
new `<img>` sources mid-scroll buried the thread and scrolling choked. The gate
deferred assignment while decode-work-per-second was above a threshold.

Its own contract said it was "always false outside WebKitGTK", and outside is now
everywhere. `settled` and `warping` are **not** that gate and stay: they are
about how many cells are turning over, not about the cost of decoding one.

## Where the numbers come from

The look-ahead depth is sized from an exponentially-weighted average of measured
image-load latency, kept in `lib/loadLatency.ts`. It lives in its own module
because neither the cell that feeds it nor the scroll module that reads it owns
the other's concern — it used to sit in the debug overlay's module, and deleting
the overlay would have silently taken adaptive prefetch with it, on exactly the
connection it exists for.

The DPR multiplier a cell sizes its request by is capped at **2**. Taken
literally, a 3× phone triples the linear request and so multiplies an image's
memory by nine, pushing every ordinary phone cell to the top of the ladder.
Measured loading and dropping 1200 thumbnails in a phone viewport: memory climbs
about five times faster per image at 1024px than at 512px, and about twenty
times faster than at 128px. Both plateau eventually — the cache is a fraction of
device memory — but a phone's ceiling is low and iOS enforces it by killing the
tab rather than by pruning, so how fast a tier takes you there is the whole
game. Two is where more stops being visible on a thumbnail and starts being
purely cost; nothing at DPR 2 or below is affected, which is every desktop.

The default cell size on a phone is **a column count in disguise**.
`thumbnail_size` is a size, so the same 200px that gives a desktop six columns
gives a 390px phone exactly one — a grid one photo wide, whose 390px cells then
ask for the largest tier: the most expensive thing the grid can do, on the device
least able to afford it. It is computed against the **short** edge, so a phone
opened in landscape gets a portrait-sensible size rather than two enormous cells
that become one on rotation.

## The scroll host

The gallery scrolls inside a positioned element, not the document.

iOS draws its own scroll indicator over the page, that indicator is interactive
(press and hold to scrub), and it cannot be styled away on the document
scroller — `::-webkit-scrollbar` only reaches element scrollers. Owning the
scroller is what leaves LightView's own bar, the one with the date markers, as
the only one on screen. Being positioned also makes it the grid's `offsetParent`,
so `offsetTop` measurements share an origin with `scrollTop`.

The custom scrollbar sits **outside** that host on purpose. It is `fixed`, so a
fixed element's scroll chain is the viewport either way — being inside would not
give a touch on the rail anything to pan, and would put an overlay inside the
scroller for no gain.

Its indicator labels are cached by hand rather than with a memo: the labels prop
is a getter that re-runs on every read, and each read walks the whole item list.
On a large gallery the first touch of the scrollbar blocked the main thread for
seconds at a stretch, and on a phone that is long enough for the browser to kill
the tab as unresponsive. A memo would instead recompute eagerly on every item
change, for the many sessions that never touch the scrollbar at all.

## Invariants a caller must uphold

- **Assign nothing while warping.** A scrub is thousands of cells nobody sees.
- **Speculation waits on what the user is waiting on.** Cells already pointed at
  a full-resolution source that have not painted are a whole decode each, on the
  same bounded pool.
- **A tier URL is not a cache handle.** An evicted tier regenerates on request,
  so a stale look-ahead record costs one generation and nothing else — which is
  why the grid no longer tracks what the server dropped.
