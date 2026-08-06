# Invariants

Rules that are load-bearing across module boundaries. Each one has a failure
mode that is silent — the code compiles, the tests pass, and something degrades
quietly — which is why they are written down rather than left to be inferred.

---

## Cache database

### The single writer connection is the bottleneck for everything

`AppState::cache_db` is one `rusqlite::Connection` behind a `tokio::Mutex`
(a Mutex, not an RwLock, because `Connection` is `Send` but not `Sync`). Every
command that writes queues behind it. Two consequences:

- **Never hold the writer lock across an expensive non-DB operation.** The
  pattern used throughout `commands/media.rs` is: do the work (decode, encode,
  ffprobe), *then* take the lock and commit. `generate_and_store_tier` spells
  this out for ffprobe specifically — running a subprocess under the lock kept
  every other DB user queued for hundreds of milliseconds per video.
- **Reads on the thumbnail hot path do not use it at all.** They go through
  `AppState::thumb_protocol_db`, a pool of read-only WAL connections
  (`ThumbProtocolPool`). That is why the serve path cannot write, and why LRU
  access marks are buffered in memory instead (see below).

### Buffered tier-access marks must be flushed before eviction

`AppState::record_tier_access` accumulates "this row was served" marks in
memory because the serve path holds a read-only connection.
`enforce_tier_budget` drains them via `take_tier_accesses` and writes them
*immediately before* the eviction pass. Reordering those two steps makes
eviction drop exactly the rows the user is currently looking at.

### Every path-keyed table must be swept together

`cache::db::path_keyed_tables()` is the single source of truth, derived from
`ThumbTier::ALL`. Any operation that removes or relocates a file — trash,
fs-watch removal, stale-row pruning in `populate_media_meta`, `rebase_root` —
iterates it. The failure mode when a site spells the list out itself is a
multi-megabyte blob keyed to a path that no longer exists and can never be
reached again. `remove_media_rows_clears_every_path_keyed_table` guards this.

`not_duplicates` is deliberately outside the list: its paths live in
`path_a`/`path_b`, so callers handle it separately.

### Adding a tier is a schema change *and* a maintenance change

Add the variant to `ThumbTier`, add it to `ThumbTier::ALL`, and add its
`CREATE TABLE` migration. `ALL` then propagates it to `path_keyed_tables()`,
`clear_thumbnails`, `get_all_tier_info`, and the per-file delete. See
[`cache-schema.md`](cache-schema.md) for the migration contract.

---

## Thumbnail pipeline

### The Standard tier's format string is a cache key

Four entry points write the Standard tier — `get_thumbnail`,
`get_thumbnails_batch`, `precache_thumbnails_impl`,
`regenerate_thumbnail_impl`. Each compares the requested format against the
stored `format` column to decide hit-versus-regenerate. If one site drifts to a
different format, every one of its lookups misses and regenerates forever, at
full decode cost, with no error anywhere. They now share
`commands::media::standard_tier_params()`; keep it that way.

### Every Standard generation must also produce its derivations

A Standard row implies a ThumbHash blob and a Micro row. All three are written
in one transaction (`store_derived_extras`) so no reader observes a Standard
row without its placeholder. A path that ends up with a Standard row but no
Micro row makes the grid's cheap rung fall through to a full source decode per
128px thumbnail — the exact CPU peg `derive_micro_from_standard` exists to
avoid.

### Placeholder thumbnails must not write source dimensions

`store_source_dims` refuses `0×0`. A placeholder (missing ffmpeg, unreadable
file) carries no source size, and writing zeros both fills the `width IS NULL`
gap that guards the column *and* hands the justified grid a degenerate aspect
ratio to lay out from.

### Video dimensions come from the probe with rotation applied

Phone clips are landscape on disk and portrait on screen. ffmpeg autorotates
ahead of our scale filter, so the dimensions handed to the filter graph must be
the display-matrix-corrected ones. A mismatch is what makes `.MOV` thumbnails
come back sideways or fail outright.

---

## Remote access

### `/api/invoke` is an allowlist, and the second boundary

Auth (device pairing plus optional password) decides *whether* a device may
call; the `dispatch` match in `http_server/api.rs` decides *what* it may do.
Anything not named there is 403 even if the client forges the name. Host-level
operations — file copy/move, plugin execution, render config — stay off it.
Delete-shaped commands sit behind an additional per-gallery
`remote.allow_delete` gate, checked in match guards *before* the arms that
implement them.

### Any route that touches a filesystem path must confine it

`path_in_gallery` canonicalizes the candidate per request and compares against
the root canonicalized once at gallery open. Without it a non-loopback bind
exposes every file on the host. It returns 404, not 403, so "outside the
gallery" is indistinguishable from "missing". `/media`, `/thumb`, and
`/gif-atlas` all call it; `/thumbhash` does not need to, because it can only
read a blob already in the cache DB.

### The reserved worker id

`tagging::local::LOCAL_WORKER_ID` identifies the server's own in-process
executor. `worker_announce` rejects it explicitly so a paired remote device
cannot impersonate the server in the worker registry.

---

## Frontend

### Grid cells are keyed by path, never by index

Both grids render with a path-keyed `<For>`. A cell that stays on screen across
a scroll keeps its exact DOM node and `<img src>`, so the webview never
re-decodes it. An index-keyed `<Index>` rewrites every slot's `src` as the
window shifts, which is what caused per-row scroll flicker.

### Prune per-path state surgically, never wholesale

When `props.paths` changes, both grids drop only the paths that actually
disappeared. The URL-assignment effect has already run for that update and will
not fire again until the visible range moves, so wiping surviving cells blanks
the grid until the next scroll — visible as the whole grid going grey after
deleting one image.

### Speculation is never free

Landing-zone warms, look-ahead precache, and background crawls all land on the
same bounded rayon `thumb_pool` as the cells the user is looking at. Both grids
gate speculation behind "nothing the viewport is waiting on is outstanding",
and both use a much smaller batch for speculation than for the visible drain,
because nothing can preempt a batch once issued — the batch size *is* the
worst-case delay before a scroll onto cold cells can get CPU back.

The measured case against ignoring this: enabling background precache at high
zoom in `JustifiedGrid` more than doubled time-to-sharp for on-screen cells
(6.8 s → 15.2 s mean over four cold runs of a three-viewport scroll).

### Two single-flight slots, not one

`inFlightFetch` carries the visible drain; `inFlightWarm` carries speculation.
They were one slot once, and a single 64-image background batch could hold it
for an entire session, leaving every subsequent visible request to the
one-at-a-time generate-on-serve path. Both slots reset in `finally`, not as a
trailing statement — the early returns for a stale generation would otherwise
wedge the slot permanently.
