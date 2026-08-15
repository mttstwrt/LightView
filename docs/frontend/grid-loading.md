# The two grids

[← docs index](../README.md) · [frontend](README.md)

`GalleryGrid` (fixed square cells) and `JustifiedGrid` (aspect-preserving rows)
are the two views of a gallery. They solve the same problem — stream thumbnails
into a virtual scroller without burying the main thread — and they have
converged on the same architecture without yet being one component. The
two-window shape at the centre of it is
[decision 0007](../decisions/0007-two-zone-render-window.md).

## The shared architecture

Both grids run the same seven-part machine:

1. **Virtual range.** A `recalcRange` reads `window.scrollY` each frame and
   updates row-range signals *only when they change* — the key optimization
   that stops reactive recomputation on every scroll pixel.
2. **Two nested windows.** An outer rendered window carrying a cheap tier for
   deep look-ahead, and an inner full-resolution window. Both are asymmetric:
   more rows ahead of the scroll direction than behind, with the ahead buffer
   growing by however many rows a scroll covers in one measured image-load
   round-trip (`dynamics.bufferAheadRows`), capped to bound DOM growth.
3. **A resolution ladder.** New cells get the cheap rung when outside the inner
   window or while a fling is in progress; they upgrade to the target tier once
   they sit in the inner window with scrolling settled — usually off-screen, so
   the swap is not seen.
4. **404-driven generation.** A cell's `<img>` points optimistically at the
   tier URL. A cached thumbnail loads instantly; one the backend cannot produce
   404s, and `onError` queues it. Read this as the recovery path it is — both
   thumbnail routes generate on a miss inside the request, so an uncached
   thumbnail does *not* normally 404. See
   [below](#404-driven-generation-is-a-recovery-path-not-how-a-gallery-fills).
5. **Drain-time prioritization.** Queued paths are ranked against the *current*
   windows, never at enqueue time — by the time a batch drains, the scroll may
   be elsewhere entirely.
6. **Two single-flight slots.** `inFlightFetch` for the visible drain,
   `inFlightWarm` for speculation, both owned by `lib/fetchLoop.ts`. See the
   invariants in [`frontend/`](README.md) for why one slot was a
   session-killing bug.
7. **Eviction.** Paths far outside the rendered range have their `thumbMap`
   entry dropped, so the `<img>` goes away and the DOM stays small. It does
   *not* hand the memory straight back — see below.

## What browsing actually costs, and which half is ours

Measured in Chromium against the built SPA at a phone viewport (390×664, DPR 3)
over a five-thousand-item gallery, with synthetic thumbnails sized to match real
JPEGs. Worth reading before "optimizing" anything here, because two plausible
beliefs about it are wrong.

**Eviction does not release a thumbnail, but retention is not unbounded
either.** Dropping the `thumbMap` entry, removing the `<img>`, clearing its
`src` first, and forcing a GC all reclaim nothing measurable — the browser keeps
the image in a cache of its own that the page cannot address. But loading and
discarding batches of 300 distinct images repeatedly does *not* grow without
limit: it climbs and then flattens, because that cache is a fraction of device
memory. So the client cannot free anything; what it controls is **how fast
browsing walks toward the ceiling**. On a desktop the ceiling is high and far
away. On a phone it is low, and iOS enforces it by killing the tab rather than
by pruning — which is what a blank screen needing a restart is.

**The tier is what sets the pace.** Loading and dropping 1200 distinct
thumbnails costs roughly 28 MB at the 128px `s` tier, 104 MB at 512px `m`, and
409 MB at 1024px `l` — about four times per step of the ladder. So a cell that
asks for one tier more than it can display is not a rounding error, and
`renderScale()` in [`lib/runtime.ts`](../../src-solidjs/lib/runtime.ts) exists
because taking a phone's 3× DPR literally pushed every ordinary cell to the top
of both ladders.

**Most of the rest is not ours.** Scrolling the whole gallery with the server
returning 1×1 images still grows the renderer by hundreds of megabytes, while
the DOM holds steady around 500 nodes and 170 `<img>`s and the JS heap stays
under 12 MB. Virtualization and eviction are doing their job; the remainder is
the browser's own raster and allocator behaviour for a very tall scroller. It is
not a leak in the app and there is no client-side lever for it — the one that
looks like a lever is a trap: removing `will-change: transform` from the slice,
on the theory that a permanent composited layer is expensive, *doubled* the
growth (630 MB → 1232 MB over a full-gallery scroll), because every scroll then
re-rastered. Leave it.

The practical reading: keep the tier honest and keep the number of cells
materialized per landing down, and do not expect the absolute figures above to
be reachable on a phone — they are a desktop browser's ceiling, not one a phone
would ever be allowed.

### Two columns on a phone, and why it is a tier decision

`thumbnail_size` is a cell size, not a column count, so one default has to serve
both surfaces — and the 200px that gives a desktop window six columns gave a
390px phone exactly one. That is a strange gallery (one photo wide), and it was
also the most expensive possible layout: a 390px cell asks for the 1024px tier
in the square grid, and in the justified grid it cleared the top threshold
outright, backing a phone-width thumbnail with a 2560px image or a ~1800px
on-demand resize of the original.

`defaultThumbnailSize` in [`settingsStore`](../../src-solidjs/stores/settingsStore.ts)
therefore derives a mobile default from the viewport's *short* edge — so
rotating does not change the answer — sized for two columns using the same
bucket-midpoint arithmetic as the pinch/Ctrl+wheel column stepper. Combined with
the capped `renderScale()`, a phone's cells then sit on the 512px tier in both
grids.

It is a trade, not a free win: two columns materializes about twice the cells
per screenful, and the look-ahead buffers are counted in *rows*, so they double
in cell terms too. In viewport-distance terms this is what brings mobile in line
with the desktop tuning — 12 rows ahead is ~3.5 viewports at two columns, and
was an unintended ~7 at one — but a scrollbar landing does now build twice the
DOM. Only ever consulted when the client has nothing stored, and only the web
client can be mobile, so no desktop `settings.toml` is ever seeded from it.

### Why the scrollbar gets special treatment

A scrollbar is the fastest way to spend that budget. Every landing reveals a
screenful of cells the grid has never shown, and a person hunting for a spot
lands a few dozen times in a few seconds — a volume of fresh thumbnails that
would take minutes of ordinary scrolling to reach. Left alone, each of those
stops also committed a full-resolution upgrade of the whole inner window, at
the most expensive tier.

`ScrollBar` therefore reports its gesture to `scrollDynamics`
(`markScrollBarActive` / `markScrollBarReleased`), which holds `settled` false
for the gesture and a release tail after it. Two things follow, both in code
that already existed:

- the resolution ladder keeps every new cell on the cheap rung, so an entire
  burst upgrades once at the end instead of once per stop;
- `bufferAheadRows` stops extrapolating. Its adaptive part is a bet that the
  scroll will continue as it is going, which is true of a fling and false of a
  warp — a jump reports an enormous single-frame velocity and then stops dead,
  inflating the look-ahead to its maximum around a position the user is about
  to leave.

Measured over sixty scrollbar landings on a phone-sized viewport, against the
tier and column defaults below: renderer growth falls from +254 MB to +186 MB,
the largest tier leaves the gesture entirely (281 `l` loads → 0), and returning
to positions already visited stops costing anything (+55 MB → +2 MB).

Take the absolute figures as a ratio, not a budget. They come from a desktop
Chromium with a large cache ceiling; the useful part is which tier is being
loaded and how many cells each landing materializes.

The other half of it is that the bar can now be dragged by touch at all.
`ScrollBar` was written with mouse handlers only, so on a phone a drag was never
a scrollbar drag: the browser claimed the gesture and panned the page under the
finger, moving the view by the length of the swipe. The only thing that worked
was tapping the track, so "using the scrollbar" *was* the burst of jumps
described above. The drag lives on pointer events now, and on a grip — an
invisible 44×34px box around the thumb, which on a long gallery is only 24px
tall and unhittable otherwise.

The grip, and nothing else, sets `touch-action: none`. That containment is the
point: claiming the whole track would mean any swipe along the right edge of the
screen warped the gallery instead of scrolling it, which on a phone is exactly
where a thumb rests. Pressing the bare track still jumps, as it always did.

While a drag is running, the thumb is positioned from the pointer and the
scroller is driven from the thumb — not both from the scroll position. On a
desktop those are the same number. On iOS they are not: scrolling is committed
on the compositor and `scrollY` reads back a frame or more stale, so feeding it
into the thumb mid-gesture made the thumb chase the finger and visibly jitter.
`recalc` leaves the thumb alone while `dragging()`, and re-syncs on release.

### The scroll host

The gallery scrolls an element, not the document. `App` renders a fixed,
full-viewport `overflow-y: auto` container around the two grids and registers it
with [`lib/scrollHost.ts`](../../src-solidjs/lib/scrollHost.ts), which everything
else reads instead of touching `window.scrollY`.

The reason is iOS. Its scroll indicator is drawn over the page, it is
interactive, and it cannot be styled away on the *document* scroller —
`::-webkit-scrollbar` only reaches element scrollers, which is exactly why
`.hide-scrollbar` works on the app's panels and never worked on the page. Two
bars were visible at once, one of them ours and one we could not remove. Owning
the scroller removes it and leaves LightView's bar, the one with the date
markers, as the only one.

Three things this constrains:

- **The host must be positioned.** Both grids measure their content with
  `offsetTop`, which is relative to the nearest positioned ancestor, and that
  has to be the same origin `scrollTop` counts from.
- **`scroll` does not bubble.** A listener bound to `window` silently watches a
  scroller that never moves again, so subscriptions go through `onScrollHost`,
  which rebinds them when the host registers. That indirection is not
  decoration: Solid creates a child before appending it to its parent, so a
  grid's `onMount` routinely runs before the host's `ref` has been assigned.
- **With no host registered, everything falls back to the window**, which is
  what the welcome screen and the map view get.

One accepted regression: the custom bar's 10px rail is `fixed`, and a fixed
element's scroll chain is the viewport whichever subtree it sits in — so a swipe
landing on the rail no longer pans the gallery. It still jumps on a tap, which
is how a scrollbar behaves everywhere else, and the rail only accepts input
while it is visible.

### Why the bar does not reach the edges of a phone

On a touch device the track is held clear of the top and bottom of the screen,
and the grip is clamped inside the track rather than centred on the thumb when
that would hang it off an end.

Two problems, both invisible on a desktop and both at the top of a gallery,
which is exactly where a user starts. A phone's screen corners are rounded and
the rail sits two pixels from the right edge — far enough into the curve that
the top of the track is not drawn at all, so the thumb was present but
unseeable, and only appeared after scrolling some way down. And the strip along
the top edge belongs to the system: a finger starting there pulls down
Notification Center rather than taking the thumb.

The inset is `max(calc(env(safe-area-inset-<side>, 0px) + 8px), 44px)`. The
`env()` term handles the status bar and home indicator; the floor handles the
corner radius, which `env()` does not describe — it reports zero in landscape,
where the corner is just as round. It is applied on touch devices only, which
costs a touchscreen laptop a little track for no benefit; that is the cheaper
way to be wrong than leaving a phone's thumb under the curve.

Chromium reports no insets by default, so a test that only loads the page
exercises the 44px floor and nothing else. Drive
`Emulation.setSafeAreaInsetsOverride` over CDP to check the other half — with a
47px top inset the track resolves to 55px, and with a 34px bottom one the floor
still wins at 44px.

### The scrub gate

iOS has a scrollbar of its own, and it is not just an indicator: press and hold
it and you can scrub, crossing the whole gallery in a second or two. It is gone
now — see [the scroll host](#the-scroll-host) — but the gate it forced stays,
because any fast programmatic scroll reaches the same speeds.

A scrub is not a fling, and the cheap rung is not enough for it. At scrub speed
every frame reveals dozens of rows the grid has never shown, so the render
window turns over completely sixty times a second; the cheap rung lowers the
price per cell, and the problem here is the *count*. Measured over three
full-gallery scrubs, the grid asked for 4442 thumbnails and grew the renderer by
528 MB, for content nobody saw a frame of.

So `dynamics.warping` goes true above `WARP_VIEWPORTS_PER_SEC`, and while it is
set the grids assign no sources at all and start no speculation — cells render
as skeletons, which is all a scrub can show anyway, and assignment resumes on
the frame the rate drops. Same three scrubs: 88 thumbnails and +124 MB, with the
landing screen still upgrading to the target tier once it settles.

One trap is worth recording, because it made the gate look like it was working
when it was not. `warping` must **not** be cleared by `markSettled()`. That
function reads like "scrolling has stopped", but `scrollend` fires after every
programmatic scroll, so mid-scrub it arrives between frames — reopening the gate
for one frame in each, which was enough to assign a whole window of cells sixty
times a second. The frame-level state said the gate was up 59 frames out of 60
while thousands of requests went out anyway. Its own timer owns it; if scrolling
really has stopped, that timer expires a few tens of milliseconds later.

## What is already extracted

`src-solidjs/lib/` holds the pieces both grids call rather than reimplement:

| Module | What it owns |
|---|---|
| `scrollDynamics.ts` | velocity/direction tracking, the WebKitGTK decode gate, fling-landing projection, the adaptive ahead-buffer |
| `loadPriority.ts` | `pickByPriority` — drain-time ranking and stale-entry reporting |
| `thumbSwap.ts` | off-DOM decode-then-swap for tier upgrades on cells already showing an image |
| `thumbProgress.ts` | the "Generating N / M" counter |
| `galleryControls.ts` | Ctrl/Cmd-drag range select, click handling, edge-scroll while dragging |
| `wheelScroll.ts` | wheel handling and the Ctrl+wheel zoom hook |
| `thumbRegeneration.ts` | the desktop-event/DOM-event pair that signals "these bytes changed" |
| `justifiedLayout.ts`, `gridLayout.ts` | the geometry each grid needs |
| `urlVersions.ts` | the `?v=` cache-bust counter per path, plus the epoch a rebuild shifts them all by |
| `pathIndex.ts` | path → position, and the pruning a changed item list forces |
| `thumbQueue.ts` | queued / in-flight / failed / warmed, and the protocol between a 404 and the bytes arriving |
| `fetchLoop.ts` | the two single-flight slots and the order one pass runs them in |
| `cellSources.ts` | what each cell shows, which rung it shows it at, and the swapper that changes one without a flash |
| `loadedUrls.ts` | which URLs have been decoded once, so a recycled cell does not re-fade |

This is the established pattern: shared grid behavior lives in `lib/` as a
factory function taking accessors, and the components stay presentational plus
their own policy.

## How the two grids stay different

The four modules at the bottom of that table are the convergence this page used
to propose. They removed the duplication without merging the components, which
was the point: the grids' policies genuinely differ, and the differences are now
arguments rather than parallel code.

| Concern | How the difference is expressed |
|---|---|
| Queue payload | `createThumbQueue()` in GalleryGrid, where every cell wants the same tier; `createThumbQueue<ThumbTier>()` in JustifiedGrid, where a cheap-rung cell must regenerate whichever tier actually 404'd rather than the level's target |
| `evictFaraway` | index ranges from `cols()` arithmetic vs. layout row lookups — each grid passes its own function as `evict` |
| `warmLandingZone` | `precacheThumbnails` vs. `ensureTierThumbnails(_, "j")`, as `warmLanding` |
| Speculation | GalleryGrid's `speculate` is the background crawl; JustifiedGrid's tries the high-tier look-ahead first and falls through to a crawl that is base-detail only, deliberately (measured: enabling it at high zoom more than doubled time-to-sharp) |
| Speculation brake | JustifiedGrid passes `blocked: () => blockingCount() > 0`; GalleryGrid has no equivalent and omits it |
| Prune effect | each grid keeps its own, because JustifiedGrid additionally prunes `jhPrecached`, the pinned viewer path, and `measuredAspects` |
| `onScrollToIndex` | row lookup by division vs. by layout search — untouched, and not worth sharing |

One thing was deleted rather than extracted. GalleryGrid kept a `coalescedPaths`
set alongside its queue, cleared at the top of every pass; a path only ever
entered it together with `needsGeneration` and left it via the drain, so every
membership test it guarded was already covered by the queued or in-flight set.
Its one real effect was a bug: `onInvalidate` cleared the queue but not the
coalesced set, so a path caught in between was blocked from re-queueing until
the next pass.

### What a third view has to write, and what it inherits

The split is meant to be: **the primitives own the bookkeeping that fails
silently; the view owns its geometry and its policy.**

Inherited whole — a new view constructs these and does not reimplement them:
`cellSources` (URL store, rung map, swapper, and their teardown),
`thumbQueue`, `urlVersions`, `pathIndex`, `fetchLoop`, `thumbSwap`,
`thumbProgress`, `loadPriority`, `galleryControls`, `wheelScroll`,
`thumbRegeneration`.

Written per view, because it *is* the view: the layout, the range calculation
that turns a scroll position into rendered/full windows, `evict`'s notion of
"far away", what `drain` batches and at which tier, and what it speculates on.
Those arrive as callbacks precisely so two views can disagree about them.

Two things a new view must get right, both of which fail quietly:

- **`drain` must issue its batch through `loop.fetch`**, or the completion
  re-arm has nothing to hang off and the queue falls back to the poll.
- **Every loop callback must tolerate running before the view has laid out.**
  The poll starts when the loop is constructed — before `onMount` for a
  component-scope call — so `evict` sees an empty layout at least once. Both
  grids return early on a zero-row layout.

One primitive does **not** carry over unchanged, and it is worth knowing before
starting the canvas: `loadPriority.ts` ranks against contiguous *index* ranges,
which is what a reading-order scroller has. A spiral canvas shows an off-centre
2D window — several disjoint index runs — and wants ranking by distance from the
viewport centre. That generalization is deliberately not written yet, since its
shape is a guess until a second kind of view exists; the natural move is to lift
the zone→rank mapping into a parameter and keep the current function as the
range-based case.

**Verify changes here in a browser, not just with `tsc`.**
[`build-and-verify.md`](../build-and-verify.md) describes driving the real SPA
against `lightview-headless` with Playwright; these are exactly the code paths a
type check cannot cover.

### Nothing re-armed the drain

The loop had no continuation. `drainQueued()` issued one batch and returned, the
slot was cleared in a `finally` that called nothing, and the only things that
started another pass were `scrollend`, the settle effect, a generation bump, and
a 500 ms poll. Against a still viewport with a deep queue that is batch, up to
half a second of idle backend, batch — the backend finishing early bought
nothing.

Enqueueing had the mirror image of the problem, and a structural cause:
`handleThumbError` is defined at component scope and `scheduleFetch` was a
closure inside `onMount`, so a 404 *could not* reach the schedule even to ask.
A landing that reveals a screenful of cold cells waited on the poll before
anything was requested.

`createFetchLoop` closes both. Its `fetch` slot pokes the loop when a batch
settles, and `poke` is what a fresh miss calls. Pokes coalesce through a
zero-delay timer rather than a microtask, because each `<img>` error handler is
its own task — a microtask runs at the end of the one that scheduled it and
would merge nothing.

Measured on the loop in isolation, with five synthetic 20 ms batches queued:
106 ms end to end, where the poll alone would have needed more than two
seconds. A hundred pokes issued across two separate tasks collapsed to a single
pass.

**How much this is worth depends on something in the backend, and it is less
than it looks.** See the next section.

### 404-driven generation is a recovery path, not how a gallery fills

Point 4 of the machine above describes the mechanism accurately and is easy to
over-read. A cell does point its `<img>` optimistically at a tier URL, and an
`onError` does queue generation — but a cold thumbnail rarely produces that
error. Both `lightview://thumb` (`main.rs`) and `GET /thumb`
(`http_server/routes.rs`) resolve through `thumb_serve::get_or_generate`, which
looks the tier up, and *on a miss generates it inside the request*, with
coalescing so concurrent requests for the same key decode once. It returns 404
only after generation has genuinely failed, three attempts in.

So a cold gallery fills from two places, and the 404 queue is neither: the
inline generate-on-miss in each request, and the frontend's speculative batch
warms (`precacheThumbnails`, `ensureTierThumbnails`), which exist precisely
because the inline path is one decode per request. What reaches `needsGeneration`
is the residue — sources that could not be decoded at all.

Measured, driving the built SPA against `lightview-headless` over a cold
1200-image gallery in Chromium: **880 thumbnail responses, none of them a 404**.
The same run against the pre-extraction frontend gives the same answer, so this
is a property of the backend, not of any recent change.

Two consequences worth carrying:

- The drain re-arm above bounds how fast the grid recovers from *failed*
  generation. It does not speed up ordinary browsing, and a measurement that
  claims it does is measuring the inline path.
- If you are trying to make a cold gallery fill faster, the levers are
  `get_or_generate`'s coalescer and pool, and the speculative warms. Not this
  queue.

The warm slot is deliberately **not** re-armed. Re-arming it would turn the
background crawl from one batch per poll into a continuous one, which is exactly
the "speculation is never free" invariant in [`frontend/`](README.md): it lands
on the same bounded rayon pool as the cells on screen. The visible drain running
back-to-back while speculation stays on the leash is the intended priority.

## The one thing that is genuinely different

`JustifiedGrid` can bypass the thumbnail tiers entirely. At mid/high zoom, a
native-format still image (`jpg`/`jpeg`/`png`/`webp`) that is neither too large
in bytes nor far larger than the displayed cell is served as a backend resize
of the original — `GET /media/<path>?fit=<px>` — instead of a `jm`/`jh` row.
That is sharper and costs no cache storage, and the requested edge is quantized
to a 256px bucket so the URL stays cache-stable across small layout changes.

Served-original cells are deliberately **not** warmed ahead of the viewport.
Each is an on-demand source decode inside the request, landing on the same
bounded pool as the visible cells; measured, warming them made things worse,
because a look-ahead only pays off if it wins the race and it cannot when each
decode is seconds long.

## A note on measurement honesty

Several decisions in these files are tuned for WebKitGTK, which decodes images
on the main thread — the premise of the decode gate and of every `isTauri()`
branch. No harness available here can measure that platform. Where the source
says a win is "reasoned, not demonstrated", that is accurate and worth
preserving in future edits.
