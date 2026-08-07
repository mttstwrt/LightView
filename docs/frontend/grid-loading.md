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
   entry dropped, releasing the decoded bitmap.

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
