# The thumbnail pipeline

[← docs index](../README.md) · [architecture](../architecture.md)

The most intricate subsystem in the codebase, and the one most changes touch.
Seven cached resolutions, four generation entry points, two serving transports,
and a disk budget — this page is the map.

**Responsible for:** decoding a source file, producing thumbnail bytes at a
requested tier, and getting them into the cache. That covers the CPU
decode/resize path, the optional `wgpu` path, `ffmpeg` frame extraction and
probing for video, HEIC transcode, EXIF extraction, and the idle worker that
backfills the queue when nobody is looking.

**Not responsible for:** storage (that is [`cache/`](../cache/README.md)),
deciding *which* tier a cell wants (that is
[`frontend/`](../frontend/README.md)), or transport. `thumb_serve.rs` and
`gif_serve.rs` sit above this module and are shared by the desktop protocol
handler and the HTTP route.

**Public interface:** `pipeline::thumbnailer` (the generate functions),
`pipeline::video` (`probe`, frame extraction), `pipeline::exif`,
`pipeline::heic_cache`, `pipeline::idle::spawn`, and — under the `gpu` feature
— `pipeline::gpu_pipeline::GpuPipeline`.

**Depends on:** [`cache/`](../cache/README.md) for the tier definitions and the
write path, `provider/` for file access, `hardware/` for pool sizing, `views`
for which tiers the idle worker should pre-warm, and the external
`ffmpeg`/`ffprobe` binaries at runtime.

**Depended on by:** `commands/media.rs`, `thumb_serve.rs`, and the idle worker.

**Invariants callers must uphold** — each of these has a silent failure mode:

- *The Standard tier's format string is a cache key.* Four entry points write
  it and each compares the requested format against the stored `format` column
  to decide hit-versus-regenerate. If one site drifts, every one of its lookups
  misses and regenerates forever, at full decode cost, with no error anywhere.
  They share `commands::media::standard_tier_params()`; keep it that way.
- *Every Standard generation must also produce its derivations.* A Standard row
  implies a ThumbHash blob and a Micro row, written in one transaction
  (`store_derived_extras`) so no reader observes a Standard row without its
  placeholder. A path with a Standard row but no Micro row makes the grid's
  cheap rung fall through to a full source decode per 128px thumbnail.
- *Placeholder thumbnails must not write source dimensions.* `store_source_dims`
  refuses `0×0`: a placeholder carries no source size, and writing zeros both
  fills the `width IS NULL` gap that guards the column and hands the justified
  grid a degenerate aspect ratio.
- *Video dimensions come from the probe with rotation applied.* Phone clips are
  landscape on disk and portrait on screen; `ffmpeg` autorotates ahead of our
  scale filter, so the dimensions handed to the filter graph must be the
  display-matrix-corrected ones. A mismatch is what makes `.MOV` thumbnails
  come back sideways or fail outright.
- *A probe with no location must not clear a stored one.* `VideoInfo::location`
  is `None` both when a container carries no coordinates and when the tags could
  not be parsed, so `store_video_meta` writes the GPS columns only when it has
  something to write.
- *Every `ffmpeg`/`ffprobe` invocation is timeout-bounded.* A wedged subprocess
  would otherwise hold a `thumb_pool` thread for the life of the process.

The most intricate subsystem in the codebase, and the one most changes touch.
Seven cached resolutions, four generation entry points, two serving transports,
and a disk budget — this page is the map.

## The tiers

Defined once in `cache::thumbnails::ThumbTier`. `ThumbTier::ALL` is the single
source of truth for "which thumbnail tables exist"; every path-keyed
maintenance operation iterates it rather than repeating the list.

| Tier | Segment | Table | Target | Shape | Format | Bounded? |
|---|---|---|---|---|---|---|
| Micro | `s` | `thumbnails_micro` | 128 | square crop | JPEG | no |
| Standard | `m` | `thumbnails` | 512 | square crop | JPEG | no |
| Large | `l` | `thumbnails_large` | 1024 | square crop | WebP | no |
| Preview | `p` | `thumbnails_preview` | 1600 | square crop | WebP | no |
| Justified | `j` | `thumbnails_justified` | 512 | aspect-preserving | WebP | no |
| JustifiedMid | `jm` | `thumbnails_justified_mid` | 1280 | aspect-preserving | WebP | **LRU** |
| JustifiedHigh | `jh` | `thumbnails_justified_high` | 2560 | aspect-preserving | WebP | **LRU** |

Two families, and the split matters. The **square-cropped** tiers back the
fixed-cell grid; the **aspect-preserving** ("fit") tiers back the justified
layout, which needs true proportions. A fit tier cannot be derived from a
square one — the crop already threw the pixels away — so `j` needs its own
source decode even when `m` is warm. That is exactly what the idle worker's
pre-warm rotation is for.

`Standard` is the hub: it is the only tier with a `resize_filter` column, the
only one carrying the `thumbhash` blob, and the only one other tiers are
derived from rather than decoded for.

## Why `jm` exists

`jm` (1280) sits between `j` (512) and `jh` (2560) purely to avoid an
over-decode. At mid zoom a cell is far smaller than 2560px, so serving `jh`
there decodes roughly sixteen times the pixels the cell displays. Its rows are
about a quarter the bytes of `jh`'s, so an equal byte budget holds roughly four
times as many — which matches usage, since mid zoom shows more cells per
screen.

## Generation entry points

Four, all funnelling into `pipeline::thumbnailer` via the bounded rayon
`thumb_pool`:

1. **`get_thumbnails_batch`** (desktop IPC) — Standard tier for a batch.
   Optionally GPU-accelerated (fused crop+resize via wgpu), CPU otherwise.
   Streams `thumb:streamed` events as each completes.
2. **`precache_thumbnails_impl`** — same tier, no blob returned; for speculative
   warming. Also backfills Micro/ThumbHash for already-cached paths.
3. **`ensure_tier_thumbnails_impl`** — any non-Standard tier, batched. This is
   what the justified grid's look-ahead and drain call.
4. **`generate_and_store_tier`** — one path, one tier, called by the serve path
   on a cache miss. Coalesced (see below).

`regenerate_thumbnail_impl` is a fifth, but it is a maintenance path: it clears
*every* tier for the path first so a re-render at any cell size pulls fresh
bytes, then eagerly regenerates only Standard.

## Serving

Two transports, one shared implementation in `thumb_serve.rs`:

- **Desktop**: the `lightview://thumb/<tier>/<path>` custom protocol, routed in
  `main.rs`. Always answered off the GTK main thread.
- **Web**: `GET /thumb/{tier}/{*rel}` in `http_server/routes.rs`, with ETag
  revalidation so a phone returning after `max-age` expiry refreshes its whole
  cached grid for a few hundred bytes per thumbnail.

Both call `thumb_serve::get_or_generate`, which reads through the read-only
connection pool and, on a miss, generates with coalescing.

### Why `get_or_generate` loops

Concurrent requests for the same `(path, tier)` elect one generator; the rest
wait on a `Notify`. A generator whose request future is cancelled — the browser
aborted the fetch mid-scroll — wakes its waiters without having produced
anything. The woken waiter re-checks the cache, finds the slot free, and
becomes the new generator. Attempts are bounded (3) so a persistently failing
source degrades to a miss rather than spinning.

The waiter enrols in the wake queue (`listener.as_mut().enable()`) *before*
re-checking the cache, to avoid missing a notify that races with the
generator's release.

### The Micro fast path

`generate_and_store_tier` short-circuits Micro: if the Standard row exists, it
derives the 128px thumbnail from those 512px bytes instead of decoding the
multi-megapixel original. Galleries thumbnailed before the Micro tier existed
flood this path via the grids' cheap-rung look-ahead, and paying a full source
decode per 128px thumbnail pegged the CPU during sustained scrolling.

## The disk budget

Only `jm` and `jh` are bounded (`is_lru_capped`). They cache whatever you view
zoomed in, so they would otherwise grow without limit, and their rows are big
enough that the bound has to be real.

- **Budget**: 10% of free disk, floored at 512 MiB, capped at 8 GiB, per tier.
  Override with `LIGHTVIEW_TIER_BUDGET_MB` (per tier, MiB, bypasses the floor)
  to pin the cache on a small box or in a test.
- **Eviction**: byte-budgeted, not row-capped — row size varies by more than an
  order of magnitude across these tiers, so a fixed row count maps to a wildly
  variable footprint. A window function accumulates sizes warmest-first and
  drops everything past the budget.
- **Warmth**: an `accessed_at` column (schema v15). Under the previous rowid
  FIFO, scrolling back over cells you were just looking at could miss them —
  they were *inserted* early, which is what rowid order encodes.
- **Hysteresis**: `EVICT_HIGH_WATER = 1.25`. Evictions only fire past 1.25× the
  budget, then trim all the way back. Without it the two write paths fought:
  on-demand serves grew the table while never evicting, then one batch write
  snapped it back in a single multi-second `DELETE` that held the cache mutex
  and shed the whole working set at once.
- **Freshly written rows are warm.** `write_tier_row` seeds `accessed_at` to
  now for capped tiers. Leaving it at the column default (0) would mark every
  new row maximally cold, so the next eviction would delete exactly what was
  just generated for the current viewport.

Both write paths call `enforce_tier_budget`, not just the batch one.

## The idle backfill worker

`pipeline/idle.rs`. On a headless server with a weak CPU, thumbnails and
perceptual hashes are otherwise computed on demand: the first browse is slow
and the duplicate finder only covers what has been browsed. The worker grinds
the backlog during quiet periods.

"Idle" means no web client is connected (`fs_change_tx` has no SSE subscribers)
**and** no user-driven thumbnail request landed in the last 60 s
(`last_thumb_activity`, touched by the desktop protocol handler and the
frontend batch commands). Both are re-checked between work units, so the worker
yields within one batch of a user showing up.

It walks the backlog newest-first — the same order the default date-descending
sort presents — so the first screen a phone opens onto is the first thing
warmed, rather than whatever the table scan happened upon.

### Which tiers it warms

Whichever the gallery's *enabled views* ask for — `views::prewarm_tiers`, read
fresh each poll so toggling a view takes effect at the next cycle rather than at
the next gallery open. Grid asks for `m`; justified and the canvas both ask for
`j`, since the canvas fits aspect-preserving cells into square slots and so
wants the same tier rather than one of its own; and map asks for nothing: it
draws one micro thumbnail per cluster, a few dozen images that generate on
demand faster than a backlog pass would reach them.

This used to be an unconditional `[Standard, Justified]`, so a gallery browsed
only in the justified layout still paid the full square-tier cost — gigabytes on
a large collection, for cells nobody would render — and the reverse held too.
Disabling a view stops *generation* and nothing else: rows already cached are
kept, because re-enabling a view must not re-decode the library, and the
zoomed-in tiers' LRU byte budget already sheds the genuinely stale ones. See
[`views.rs`](../../src-tauri/src/views.rs) and
[decision 0002](../decisions/0002-two-families-of-thumbnail-tiers.md).

The setting is host configuration, so it is read-only over `/api/invoke` and a
paired browser cannot change it. The desktop app has a toggle in Settings; a
headless server has `lightview-headless views <gallery> [<list>]`, which writes
the same `gallery_meta` key from a second process. Either way the worker re-reads
it each poll, so the change lands without a restart.

## Where the frontend picks a tier

The grids own this; see [`frontend/grid-loading.md`](../frontend/grid-loading.md). Briefly:
`GalleryGrid` maps cell size × DPR to `s`/`m`/`l` with a 1.25× upscale
tolerance; `JustifiedGrid` maps a hysteretic zoom level to `j`/`jm`/`jh`, and
at mid/high detail may bypass the tiers entirely for small native-format
stills, requesting a cell-sized backend resize via `GET /media?fit=<px>`.
