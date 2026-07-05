# Scroll loading redesign — split the fast-scroll gate, degrade instead of stop

> Status: Phases 1–2 implemented (2026-07-04), including the
> `content-visibility` fix described in Risks. Phase 2 grew two amendments
> from device testing: (a) a two-zone render buffer — a wide outer window
> (12 rows ahead / 4 behind) rendered at the cheap rung with the original
> narrower window (5/2 grid, 4/2 justified) carrying full res, cells
> upgrading as rows cross the inner boundary; and (b) an underlay `<img>` in
> ThumbnailCell that holds the outgoing image during same-cell src swaps,
> because mobile engines drop the old bitmap for a frame+ even when the new
> URL is pre-decoded (the "flicker on upgrade" symptom).
>
> Phase 3 implemented: drain-time priority ordering lives in
> `lib/loadPriority.ts` (full-res window → rendered buffer by distance →
> drop out-of-window leftovers) and is used by both grids' generation
> queues; upgrade-decode cancellation landed with `lib/thumbSwap.ts` in
> Phase 2. The common drain skeleton was left per-grid (streamed events /
> tier branches differ too much to unify cleanly).
>
> Phase 4 implemented (2026-07-04): `projectedLandingY()` in
> `scrollDynamics.ts`; both grids' `scheduleFetch` now warm the landing
> window (± one viewport) during flings instead of just early-returning —
> `precacheThumbnails` (grid) / `ensureTierThumbnails(_, "j")` (justified)
> plus low-priority browser-cache fetches of the cheap-rung URLs on the web
> client, deduped per fling via a `landingWarmed` set. JustifiedGrid gained
> the `VELOCITY_FAST` fling branch it previously lacked (its generation
> drain used to run at any velocity).
>
> Phase 5 implemented (2026-07-04): always-on load-latency EWMA in
> `perfMonitor.ts` (`ewmaImageLoadMs()`, fed by now-unconditional
> `ThumbnailCell` timestamps; the overlay sums stay gated); a
> `bufferAheadRows(base, max)` helper on `scrollDynamics` implementing the
> velocity×latency formula, consumed by both grids' `recalcRange`; and
> `constrainedNetwork()` (Save-Data / 2g via `navigator.connection`), which
> holds web-client cells at the cheap rung and pins the buffer at `base`.
> One amendment vs. the plan below: the formula was written before Phase 2's
> two-zone buffer, so its BASE (5/4) and MAX (12) referred to the old single
> window — as built, the *outer* cheap-rung window adapts with BASE = 12
> (today's `BUFFER_AHEAD`) and `BUFFER_AHEAD_MAX = 20`; eviction margins are
> relative to the rendered range, so no `EVICT_ROWS` change was needed. The
> Tauri gate auto-tune stretch was skipped per its own "only if the static
> default proves wrong" condition.
>
> Server-side findings from device testing (2026-07-04): the request
> pressure these phases add surfaced two backend bugs. (1) The thumbnail
> generation coalescer leaked its key when a generating request future was
> cancelled (browser aborts a fetch → axum drops the handler mid-await →
> `release` never ran), permanently hanging every later request for that
> thumbnail and eventually the browser's whole connection budget — the
> "server stops responding until restart" symptom. Fixed with an RAII
> guard (`cache/coalescer.rs`) plus a retry loop in
> `thumb_serve::get_or_generate` so a woken waiter takes over a cancelled
> generation. (2) A micro ("s") tier miss on the HTTP route ran full
> generation — decoding the multi-megapixel *original* per 128px thumb —
> even when the 512px standard tier was cached; the cheap-rung look-ahead
> flood made this peg the CPU. `generate_and_store_tier` now derives micro
> from cached standard bytes (~10–20× cheaper). Also moved ffprobe out
> from under the cache-DB lock on that path. Known follow-ups: the batch
> paths still call `populate_video_metadata` under the lock (bounded per
> batch), and the `?fit=` resize route has no coalescer.
>
> Follow-up work (2026-07-04): Settings → Thumbnails gained "Generate
> Missing Thumbnails" — a sequential batched pass over the whole gallery
> (standard+micro, then justified base) so cold regions don't
> burst-generate during scrolling; it drives the existing progress
> overlay. Investigating why that overlay "never shows" surfaced that it
> is fed only by frontend-observed 404s, which generate-on-miss routes
> almost never produce — scroll-driven generation is invisible to it by
> design, not broken. The same investigation found `precache_thumbnails`
> and `ensure_tier_thumbnails` missing from the web `/api/invoke`
> allowlist, so the web grids' look-ahead warming had been silently
> failing (masked by generate-on-miss); both are now bridged via `_impl`
> splits, and `precache_thumbnails` now backfills missing micro/thumbhash
> rows so a precache pass leaves the DB fully shaped.

## Problem

Both grids suppress *all* `<img>` src assignment whenever scroll velocity exceeds
`FAST_SCROLL_VELOCITY = 2500` px/s (`GalleryGrid.tsx:48`, `JustifiedGrid.tsx:46`),
resuming 120 ms after the last fast frame. The gate exists to protect the
WebKitGTK webview's main-thread image decoding, but it runs unconditionally —
including on the web client, where decode is async and off-thread. A touch
fling's momentum phase runs 3,000–10,000 px/s, so on a phone essentially every
scroll freezes loading for the whole gesture: rows enter as skeletons and pop in
all at once on settle. The row buffer (`BUFFER_AHEAD`/`BUFFER_BEHIND`) never gets
a chance to do its job because assignment is frozen exactly when it matters.

The gate conflates two different concerns:

| Concern | Real scope | Right mechanism |
|---|---|---|
| **A. Engine decode-throughput protection** — don't bury a main-thread-decoding webview in decodes mid-scroll | WebKitGTK (`isTauri()`) only | A hard gate, expressed in decode-work units (cells/s), applied only where the engine needs it |
| **B. Wasted-work avoidance** — don't fetch/decode full-size images for cells that fly past unseen | Universal | Not a gate: a resolution ladder + settle-time upgrades + prioritized, cancellable loading + landing-zone prefetch |

## Target behavior

| | Desktop app (WebKitGTK) | Web client (desktop + mobile browser) |
|---|---|---|
| Slow scroll | unchanged: target tier assigned as cells enter the buffer | unchanged |
| Fast scroll / fling | hard gate stays (Concern A), threshold in cells/s; optional experiment: assign the tiny rung during the gate | **no hard gate**; new cells immediately get the cheap rung (`s` in grid view, `j` in justified), upgraded to target tier on settle |
| During fling | warm the projected landing zone in the backend | warm landing zone in backend + browser HTTP cache |
| Buffer size | static | grows with velocity × measured image-load latency, capped |

Worst case on the web client becomes *blurry → sharp* instead of *blank → pop-in*.

## Phase 1 — shared scroll dynamics + platform-scoped gate

Fixes the mobile bug on its own; later phases refine.

**New file `src-solidjs/lib/scrollDynamics.ts`** (same extraction pattern as
`lib/galleryControls.ts`): factor out the duplicated scroll machinery from both
grids' `onMount` blocks — the rAF-coalesced `scroll` listener, velocity/direction
tracking, settle timer, and `scrollend` handling.

```ts
createScrollDynamics(opts: {
  rowHeight: () => number;      // current row pitch, for content-relative units
  cellsPerRow: () => number;    // cols (grid) / width-based estimate (justified)
  cellCostPx: () => number;     // approx decoded pixels per cell (tier size²)
  onFrame: (s: ScrollFrame) => void;  // grid runs jump detection + recalcRange
}): {
  velocity: () => number;          // px/s, 0 when stale (>150ms since a frame)
  direction: () => 1 | -1;
  decodeGate: Accessor<boolean>;   // Concern A — see below
  markSettled: () => void;         // scrollend / wheel-animation settled
  dispose: () => void;
  // Phase 2 adds settled(); Phase 4 adds projectedLandingY()
}
```

**Gate semantics (Concern A):**

```ts
const GATE_DECODE_PX_PER_SEC = 18_000_000;
decodePxPerSec = velocity / rowHeight × cellsPerRow × cellCostPx;
decodeGate = isTauri() && decodePxPerSec > GATE_DECODE_PX_PER_SEC;
```

- `isTauri()` (`lib/runtime.ts:16`) scopes it to the engine that needs it. The
  web client never hard-gates.
- Units are decoded pixels/s — decode *work*, not scroll speed. Plain cells/s
  turned out to over-gate dense small-thumbnail layouts (many cells/row, but
  each a tiny 128px decode) and under-gate large-tier ones, so the rate is
  weighted by the tier's decoded pixel count. The threshold is calibrated to
  the old 2500 px/s at default desktop settings (~250px cells, ~7 columns,
  512px tier ⇒ ~18M px/s), so desktop-app behavior at defaults is unchanged.

**Grid changes:** both `GalleryGrid.tsx` and `JustifiedGrid.tsx` delete their
local `FAST_SCROLL_VELOCITY` / `SCROLL_SETTLE_MS` / `fastScroll` plumbing and
their hand-rolled `onScroll`, consuming the module instead. The src-assignment
effects (`GalleryGrid.tsx:294`, `JustifiedGrid.tsx:423`) key on `decodeGate`
instead of `fastScroll`.

`scheduleFetch`'s separate velocity checks stay but move to module accessors:
the `> VELOCITY_FAST` early-return, the `< VELOCITY_SLOW` background-precache
condition, and the justified `< 1500` jh look-ahead condition.

**Acceptance:** phone fling shows images populating *during* the fling
(buffered rows arrive pre-loaded at moderate speeds); desktop app behavior
unchanged; desktop-browser scrollbar-drag no longer blanks.

## Phase 2 — resolution ladder: degrade presence, upgrade on settle

**Rung selection.** Track what each cell was given: change `assignedSet:
Set<string>` to `assignedRung: Map<string, Rung>` in both grids.

- Grid view ladder: `s` (128 px) → target tier from `pickTier()` (`s`/`m`/`l`).
  While `!settled()` (web) or `decodeGate` (Tauri, behind the experiment flag),
  newly-entering cells get `thumbSrcFor(path, "s")`; the target tier waits.
- Justified ladder: `j` (512 px, aspect-preserving) → `jm`/`jh`/`?fit=` original
  per `detailLevel()`/`servesOriginal()`. The square `s`/`m` tiers are wrong
  (object-cover would crop), so `j` is the floor; at base detail there is no
  cheaper rung and behavior is just "no gate".

**Upgrade pass.** A `createEffect` on `settled()` walks the current
visible+buffer range; any cell whose `assignedRung` is below target gets the
target URL via decode-then-swap so the low rung stays on screen (no skeleton
flash). JustifiedGrid's `assignSrc` (`JustifiedGrid.tsx:400`) already implements
exactly this (off-DOM `Image`, `markUrlLoaded` before swap, generation checks) —
extract it to a shared helper (e.g. `lib/thumbSwap.ts`) and use it from both
grids. Add cancellation: keep a `Map<path, HTMLImageElement>` of in-flight
upgrade decodes; on eviction or generation bump, null `onload`/`src` and drop
the entry (today those decodes are only abandoned via the callback guards).

**404s on the cheap rung.** `get_thumbnails_batch` already derives the micro
tier from cached standard bytes, and `ensure_tier_thumbnails` can warm any
single tier. Verify `ThumbTier::from_segment` accepts `"s"` and that
`handleThumbError` → generation resolves a missing `s` (it may need
`ensureTierThumbnails(batch, "s")` when the miss came from the cheap rung —
thread the rung through `needsGeneration`, e.g. `Map<path, Rung>`).

**Bandwidth note:** upgrades are a second fetch per cell, but the `s` tier is a
few KB, responses are cacheable (`max-age=3600`), and upgrades only run for
cells still present after settle — cells that flew past never fetch the big
tier at all. This *reduces* total bytes on fast scrolls vs. today's
settle-then-fetch-everything-at-target-tier.

**Tauri experiment flag:** a const (`CHEAP_RUNG_DURING_GATE`) enabling the `s`
rung while `decodeGate` is active on WebKitGTK. 128 px decodes are ~16× cheaper
than 512 px ones; measure with the debug overlay (fps + imageLoadLatency
sparklines) before defaulting it on. If it janks, the hard gate remains.

## Phase 3 — prioritized, cancellable loading

Make the generation/prefetch queue explicitly priority-ordered and unify the
two grids' fetch loops where practical.

- **Priorities, computed at drain time** (so stale enqueue-time priorities
  can't stick): `visible` > `ahead buffer` > `landing zone (Phase 4)` >
  `background cursor`. GalleryGrid already does a near/far split in
  `scheduleFetch` (`GalleryGrid.tsx:710-734`); JustifiedGrid takes arbitrary
  set-iteration order (`JustifiedGrid.tsx:605-610`) — port the near-viewport
  prioritization there, then extract the common drain skeleton (single-flight
  `inFlightFetch`, generation checks, progress accounting) into a shared
  helper if it falls out cleanly. Don't force the unification if the tier
  differences make it awkward; the priority ordering is the point.
- **Cancellation levels:**
  - Browser fetches: eviction already clears `thumbMap` → `ThumbnailCell`
    clears `src`, which aborts in-flight loads. Verify this actually cancels
    on the web client (network tab) — it's the implicit cancellation path.
  - Upgrade decodes: the cancellation map from Phase 2.
  - IPC generation: batches can't be aborted mid-flight cheaply; rely on
    small batches + drain-time drops (out-of-range `needsGeneration` entries
    are already discarded, `GalleryGrid.tsx:724-727`) + the existing
    generation-counter guards.

## Phase 4 — landing-zone prediction

A fling has a predictable destination. Estimate it in `scrollDynamics`:

```ts
const FLING_PROJECTION_S = 0.35; // tunable ~0.3–0.5; iOS decel ≈ v × 0.5s
projectedLandingY = () => clamp(y + signedVelocity() * FLING_PROJECTION_S, 0, maxScroll);
```

Use it in both `scheduleFetch`s: replace the flat `velocity > VELOCITY_FAST →
return` with "skip near-viewport generation, but warm the landing window":

- Compute the row window around `projectedLandingY()` (± one viewport).
- Backend warm: `precacheThumbnails` (grid) / `ensureTierThumbnails(_, "j")`
  (justified) for uncached paths in that window — reuses the single-flight
  `inFlightFetch` slot, one batch per drain, so it can't pile up.
- Web client only: additionally `fetch(thumbSrc, { priority: "low" })` for the
  window's cheap-rung URLs to warm the browser HTTP cache (responses are
  cacheable for 1 h), so the `<img>`s hit cache when the scroll arrives.
  `priority` is a progressive enhancement (ignored where unsupported).

The projection is recomputed every drain, so a redirected or interrupted fling
self-corrects; wasted warms are bounded to one batch.

## Phase 5 — adapt to measured capability, not device class

- **Always-on load-latency EWMA.** `perfMonitor.recordImageLoad` is gated on
  the debug overlay being open; add an ungated EWMA alongside it
  (`ewma = 0.8·ewma + 0.2·sample`, one multiply per load) and make
  `ThumbnailCell` timestamp loads unconditionally (`loadStart` currently set
  only when `perfActive()` — one `performance.now()` per fresh load is
  negligible; keep the seen-URL skip). Expose `ewmaImageLoadMs()`.
- **Velocity- and latency-aware buffer.** Replace the static `BUFFER_AHEAD`
  with: `bufferAhead = clamp(BASE + ceil(|velocity| × ewmaImageLoadMs/1000 / rowHeight()), BASE, MAX)`
  where `BASE` is today's constant (5 grid / 4 justified) and `MAX ≈ 12` caps
  DOM growth. Slow network or big tiers ⇒ deeper prefetch; instant localhost ⇒
  today's behavior. `EVICT_ROWS` must stay ≥ `MAX` + margin.
- **Network hints (web client, progressive).** If `navigator.connection`
  exists: `saveData` or `effectiveType ∈ {2g, slow-2g}` ⇒ hold cells at the
  cheap rung (skip upgrades), shrink `MAX`. Safari lacks the API; absence
  changes nothing.
- **Stretch (Tauri):** count long rAF gaps (>50 ms) while assignment is active
  and auto-tune `GATE_CELLS_PER_SEC` up/down. Only if the static default
  proves wrong on real hardware.

## Verification (per phase)

- Build the SPA + headless server (`npm run build`; `cargo build --bin
  lightview-headless`), serve a generated gallery, test from a phone on LAN
  and from a desktop browser with DevTools CPU 6× + Fast-4G throttling (an
  honest mobile stand-in).
- Repros: phone fling in both views (was: skeletons until settle); desktop
  scrollbar drag (was: same blanking).
- Debug overlay sparklines (`imageLoadLatency`, `imageLoadRate`, `fps`,
  `cacheMisses`) before/after on both the web client and `cargo tauri dev` —
  the desktop app must show no fps regression at default settings.
- Phase 4: network tab shows landing-window warms during a long fling and
  cache hits when it lands.

## Risks / open questions

- **WebKitGTK regression** is the main risk; Phase 1 is deliberately
  behavior-preserving there (gate stays, recalibrated units), and the cheap
  rung on Tauri ships behind a flag with overlay measurements.
- **`s`-tier coverage**: confirm the miss→generate path produces the micro
  tier when the cheap rung 404s (Phase 2 note). Check
  `ThumbTier::from_segment` and `tier_cached_set` handle `"s"`.
- **`content-visibility: auto`** on cells turned out to be a second,
  velocity-independent cause of the pop-in: mobile browsers defer image
  load/decode inside skipped subtrees until the cell intersects the viewport,
  so buffered cells stayed cold no matter what the gate did. Resolved during
  Phase 1: c-v is now `isTauri()`-only (it was added for WebKitGTK), and on
  the web client cells call `img.decode()` on load so buffered cells are
  paint-ready before reveal (`decoding="async"` otherwise permits deferring
  decode to first paint — i.e. the reveal).
- **Upgrade churn during slow continuous scrolls**: `settled()` flapping could
  re-run the upgrade pass often; debounce upgrades (e.g. 150 ms after settle)
  and skip cells already at target rung, which makes re-runs cheap no-ops.
- Tuning constants introduced: `GATE_DECODE_PX_PER_SEC` (18M),
  `FLING_PROJECTION_S` (0.35), EWMA α (0.2), buffer `MAX` (12), upgrade
  debounce (150 ms). All live in `scrollDynamics.ts` / grid headers with
  rationale comments.

## Suggested order

Each phase is independently shippable. 1 fixes the reported bug; 2 delivers
the Photos-style feel; 3–5 are refinements ordered by payoff-per-risk. Re-test
the phone repro after each.
