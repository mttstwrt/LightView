# The two grids

[← docs index](../README.md) · [frontend](README.md)

`GalleryGrid` (fixed square cells) and `JustifiedGrid` (aspect-preserving rows)
are the two views of a gallery. They solve the same problem — stream thumbnails
into a virtual scroller without burying the main thread — and they have
converged on the same architecture without yet being one component. The
two-window shape at the centre of it is
[decision 0007](../decisions/0007-two-zone-render-window.md).

`GalleryGrid` (fixed square cells) and `JustifiedGrid` (aspect-preserving rows)
are the two views of a gallery. They solve the same problem — stream thumbnails
into a virtual scroller without burying the main thread — and they have
converged on the same architecture without yet being one component.

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
   tier URL. A cached thumbnail loads instantly; an uncached one 404s, and
   `onError` queues it.
5. **Drain-time prioritization.** Queued paths are ranked against the *current*
   windows, never at enqueue time — by the time a batch drains, the scroll may
   be elsewhere entirely.
6. **Two single-flight slots.** `inFlightFetch` for the visible drain,
   `inFlightWarm` for speculation. See the invariants in
   [`frontend/`](README.md) for why one slot was a session-killing bug.
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

This is the established pattern: shared grid behavior lives in `lib/` as a
factory function taking accessors, and the components stay presentational plus
their own policy.

## What is still duplicated

Roughly 250–300 lines of near-identical logic remain, structurally the same in
both files but differing in small ways that make a naive merge unsafe:

| Concern | Difference between the two |
|---|---|
| `pathToIndex` maintenance + the prune-on-`props.paths` effect | JustifiedGrid additionally prunes `jhPrecached`, the pinned viewer path, and `measuredAspects` |
| `evictFaraway` | index ranges come from `cols()` arithmetic vs. layout row lookups |
| `warmLandingZone` | warms via `precacheThumbnails` vs. `ensureTierThumbnails(_, "j")` |
| `drainQueued` | GalleryGrid's queue is a `Set<string>`; JustifiedGrid's is a `Map<string, ThumbTier>` because a cheap-rung cell must regenerate the tier that actually 404'd, not the level's target |
| `warmBackground` | GalleryGrid crawls at any tier; JustifiedGrid is base-detail only, deliberately (measured: enabling it at high zoom more than doubled time-to-sharp) |
| `scheduleFetch` | JustifiedGrid additionally gates on `blockingCount()` and has a look-ahead stage |
| `onScrollToIndex` | row lookup by division vs. by layout search |
| URL versioning (`urlVersions`, `versionEpoch`, `bumpVersion`) | identical |

### A convergence plan, if this is picked up

Do it in the direction the codebase already moves — extract primitives, do not
merge the components. Suggested order, smallest risk first:

1. **`createUrlVersions()`** — `urlVersions` + `versionEpoch` + `bumpVersion` +
   the `?v=` builder. Verbatim in both; no behavioral surface.
2. **`createPathIndex(paths)`** — the `pathToIndex` map plus a
   `pruneAbsent(...sets)` helper. Each grid keeps its own effect and passes its
   own set list, so the extra sets JustifiedGrid prunes stay local.
3. **`createThumbQueue()`** — the `needsGeneration` / `inFlightSet` /
   `failedSet` / `landingWarmed` group with its queue-and-drain protocol.
   Parameterize the queue value type so GalleryGrid's `Set` and
   JustifiedGrid's tier-keyed `Map` are the same structure with `void` vs
   `ThumbTier` payloads.
4. **`createFetchLoop()`** — the two single-flight slots plus `scheduleFetch`'s
   ordering (drain, then speculate), taking the warm strategies as callbacks.

Steps 1–2 are mechanical. Step 3 is where the real reduction is, and it is also
where the grids' policies genuinely differ, so it needs the differences kept as
parameters rather than flattened.

**Verify it in a browser, not just with `tsc`.** [`build-and-verify.md`](../build-and-verify.md)
describes driving the real SPA against `lightview-headless` with Playwright;
these are exactly the code paths a type check cannot cover.

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
