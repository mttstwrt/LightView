# Design

Answering principle 1's five questions, in its order.

## Placement

**The probe branch goes in `services/gallery.rs::probe_and_store`.** That is
already the one place that turns "a path nothing has looked at" into a
`ProbedMedia` row: it resolves the path, batches by `PROBE_BATCH`, commits as it
goes, and marks `exif_read`. It runs from both entry points that matter — the
enrichment backfill and the watcher's ingest of an arriving file — so a clip
dropped into a running gallery is covered by the same code as a clip found at
open. Today its blocking closure calls `exif::read` unconditionally; it gains a
`match media_type_of(&path)` around that one call.

Dependencies point the way they already do: `services` → `pipeline::{exif,
video}` → `cache::meta`. No new edge, and nothing lower learns about anything
higher.

**Capture-time parsing goes in `pipeline/video.rs`, beside `location_from_tags`.**
`services` must not learn ffprobe's JSON shape; it asks for a `VideoInfo` and
gets one.

**The probe does not go in the thumbnailer.** `pipeline/serve.rs` writes
dimensions when it happens to decode a frame, which is lazy and tier-triggered.
A file's facts must not depend on whether someone scrolled past it — that is the
defect A1 fixed for EXIF, and reintroducing it for video would be the same bug
in a new place.

**The presence guard moves down, from `cli/mod.rs` into
`services/gallery.rs::backfill_locations`.** The guard exists so an exit cannot
land between a `modify_companion`'s lock and its rename. It is currently held
around the whole of `enrich_and_index`, whose four phases are: a whole-library
header read, a geocode loop, a companion re-index, and an autocomplete refresh.
Only the second writes a companion. The other three touch the derived cache,
where an abrupt exit costs nothing the next open will not rebuild. The guard
belongs at the write, not around the pass that contains it.

The same argument retires the guard in `spawn_companion_sweep` entirely:
`reindex_companions` reads companions and writes `index::set_state`, and never
calls `modify_companion`.

Every other `modify_companion` caller is a user request arriving over HTTP, and
those are covered already — axum's graceful shutdown waits for in-flight
requests before the listener closes. `backfill_locations` is the only unattended
writer, which is precisely why it is the only one that needs a guard.

## Contract

| what changes | who is on the other side |
|---|---|
| `VideoInfo` gains `date_taken: Option<i64>` | in-crate; Unix seconds UTC, the same units and frame as `exif::Facts::date_taken` |
| `media_meta.duration`, `gps_lat`, `gps_lon` become populated for videos | `sort::sorter::SortedItem`, `services::media::MediaMeta`, the wire, `galleryStore.durationByPath`, `ThumbnailCell` |
| `enrich_and_index` takes `Arc<Presence>` | one caller, `cli/mod.rs` |
| `FORMAT_VERSION` 2 → 3 | every existing derived cache |

Nothing changes shape on the wire. Fields that were always `null` start
carrying values, and the frontend already types every one of them as nullable —
which is why the grid needs no change at all to satisfy R2. The autoplay gate
is written and correct; it has simply never been handed a number.

**The format bump is required, not convenient.** `exif_read` means *looked*,
not *found something*. Under version 2 every video is marked looked-at by a tool
that cannot read it, so without a bump no existing cache would ever probe one.
The alternative — re-probing wherever `duration IS NULL` — is a gate phrased
over the result columns, which AGENTS.md forbids by name: a corrupt clip, or one
ffprobe cannot parse, would be re-probed on every open forever.

## Cost in concepts

One branch, in one function, on an enum that already exists for this purpose.
`media_type_of` is the concept; this is its second use.

**One `except`, named as principle 1 requires it to be: if ffprobe is not
installed at all, a video row is not marked read.** This is not a gate over the
result columns — it asks whether the tool exists, not what it returned — but it
is a second reason a row can stay unprobed, and it is permanent until someone
removes it. It buys R1's last paragraph: a user who installs `ffmpeg` after
first launch gets durations on the next open instead of never.

Everything else in this plan subtracts: eight functions, four dependencies, one
frontend function, two dead settings fields, and two comments that describe a
rendering path that was deleted with Tauri.

## Alternatives

**Delete the two autoplay settings instead of probing.** Cheapest by a wide
margin and principle 2 leans this way. Rejected deliberately: the feature is
wanted, and "it was never wired up" is not a reason to remove a thing, it is a
description of the bug.

**Gate autoplay on file size, which the index already has.** No subprocess at
all. Rejected: size is bitrate × duration, so a fifteen-second 4K clip reads as
long and a five-minute screen recording reads as short. The setting is in
seconds; deciding in bytes would make it lie.

**Probe in the thumbnailer, which already shells out to ffmpeg for a frame.**
Rejected on placement, above.

**Probe during `scan_and_index`.** Rejected on cost: the scan is what the grid
waits for before it can paint. The enrichment pass is background, batched,
committed as it goes, and resumable — `exif_read` is what makes the next open
continue rather than restart.

**Put the guard inside `modify_companion` so every companion write is
structurally protected.** The strongest version of the invariant, and tempting.
Rejected: it needs either an `Arc<Presence>` threaded through every caller —
tags, plugins, geocode, trash — or a process-global singleton, which is hidden
state standing in for one explicit parameter at one call site.

## Assumptions

- **(unmeasured)** ffprobe costs on the order of 30–50 ms per file. To be
  measured on a fixture of a few hundred clips before the commit lands. If it is
  materially worse the batch closure gets rayon, which it can take without any
  structural change since it is already inside `spawn_blocking`.
- **(measured, this session)** `video::probe` caches on `(path, mtime)`
  in-process with a capacity bound, so the thumbnailer's later probe of the same
  clip costs nothing.
- **(unmeasured)** `format.tags.creation_time` is RFC 3339. Cameras and phones
  write it; some transcoders rewrite it to the mux time, so a re-encoded clip can
  carry the date of its re-encode rather than its capture. This is the same trust
  already extended to EXIF `DateTimeOriginal`, and the A2 fallback it replaces —
  mtime — is strictly worse on a library that has been copied between disks.
- Containers that carry no creation time at all (AVI, older MKV) keep falling
  back through `COALESCE(date_taken, mtime)`. Nothing regresses.
- **(unmeasured)** A file still being written when the watcher sees it may probe
  to a missing duration, and nothing re-probes it until a format bump. The same
  is already true of a half-written JPEG's EXIF header; if it proves to matter it
  is one fix for both, not one for video.

## The two checks principle 1 asks for

**Second-implementation test** — not applicable by construction: no
abstraction, interface, or plugin point is introduced. The change is a `match`
on an existing enum.

**Seam test** — passes, and worth recording why. `VideoInfo` already carries
width, height, duration and a `Location` whose doc comment says it uses "the
same convention as the EXIF path, so both feed `media_meta.gps_lat/gps_lon`
without conversion", and `ProbedMedia` already has a field for every one of
them. The seam was cut correctly when the two paths were written. Only the wire
between them was never run — which is also why this lands as a small change
rather than a refactor.

## The work

Five commits, in this order.

**1. Probe videos where photos are probed.** `date_taken` on `VideoInfo`, parsed
from `format.tags.creation_time` in `probe_uncached` — free, because
`-show_format` is already requested and the JSON is already in hand. The branch
in `probe_and_store`. The ffprobe-missing condition on `mark_header_read`.
`FORMAT_VERSION` to 3 with a comment saying what the bump is for. Fix the false
comment at `sort/sorter.rs:83`. Tests: a fixture clip probes to a duration and
dimensions; a clip with a `creation_time` gets a date and one without falls back
to mtime; a video row is not marked read when ffprobe is absent.

**2. Hold the presence guard at the write, not around the pass.** Guard into
`backfill_locations` per `modify_companion`; out of `cli/mod.rs`; out of
`spawn_companion_sweep`. Test: a session with a window closed and a long
enrichment still running exits within the grace period.

**3. Delete what nothing calls.** `write_companion_to`, `read_companion_locked`,
`header_dimensions`, `decode_heic_natural_from_bytes`, `tag_count`,
`tags_in_namespace`, `HardwareProfile::{prefetch_count, lru_cache_size}`;
`currentQuery` and the `preload_count` / `lru_cache_size` settings fields;
`percent-encoding`, `bytemuck`, `tower-http`, and npm `qrcode`. The two comments
that survive their subjects.

**4. Reap what we spawn.** `xdg-open` and **Open with**. `.wait()` is the wrong
fix — `xdg-open` blocks until the browser exits under some handlers — so each
gets a detached thread that waits, or the launch is double-forked.

**5. Docs.** `docs/pipeline/`, `docs/cache/`, `docs/gallery/`,
`docs/frontend/` on both sides of the changed data flow, and the `exif_read`
invariant in AGENTS.md extended to say which tool did the looking.
