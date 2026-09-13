# Design

Answering principle 1's five questions, in its order. Revised after an
independent review of the first draft, which found three false premises and one
regression the plan would have shipped; each is marked **[corrected]** where it
changed the design rather than only the prose.

## Three decisions this plan cannot make on its own

Stated up front because the rest only makes sense once they are settled.

**1. Which clock a video's date is on.** **[corrected]** The first draft said
`VideoInfo.date_taken` would be "Unix seconds UTC, the same units and frame as
`exif::Facts::date_taken`". That frame equality does not hold.
`pipeline/exif.rs:50` calls `.and_utc().timestamp()` on a `NaiveDateTime` that
the function below it documents as "the camera's local wall-clock time — EXIF
carries no timezone". So every `date_taken` in the column today is *local wall
clock relabelled UTC*. MP4's `format.tags.creation_time` is a genuine UTC
instant. Mixing the two puts videos on a different clock from every photo beside
them, and `sort/grouper.rs` renders day headers with UTC calendar fields — which
is correct for the existing convention and wrong for a true instant. In UTC+9 a
clip shot at 08:00 would file under the previous day's header and sort ahead of
every photo from the same morning, and `date=2024` would acquire a twelve-hour
wrong edge for videos only.

The recommendation: prefer Apple's `com.apple.quicktime.creationdate`, which
carries a local time *with* its offset, and keep the wall-clock half; when it is
absent, convert `creation_time` from UTC into the host's local zone and keep
that wall clock. The assumption is that the library's owner shot the clip in the
zone they live in — which is exactly the assumption the EXIF path already makes
silently, and it is bounded and stateable, where a guaranteed offset error is
not. The alternative, storing the true instant and teaching the grouper which
rows are on which clock, means two clocks in one column forever.

**2. Whether this earns a `format_version` bump.** **[corrected]** The first
draft called the bump necessary and dismissed the alternative in three lines.
Both halves were too quick.

A bump is not free in the way `cache/db.rs`'s head comment claims. `date_added`
and `last_viewed` survive only where a sidecar already exists, and an untagged
camera roll — the exact library this change targets — has none. So the bump
resets when every file was added and when it was last seen, on top of
re-thumbnailing everything.

The cheaper option the draft did not name: a one-shot, tool-scoped reset —
`UPDATE media_meta SET exif_read = 0 WHERE media_type = 'video'`, guarded by a
key in `gallery_meta`, which `meta_get`/`meta_set` already support. That is a
gate over *which tool last looked*, not over what it found, so it is not the
pattern AGENTS.md forbids; it is structurally the same stamp
`backfill_locations` already keeps for the gazetteer. It costs one key and one
statement at open, and it preserves thumbnails, `date_added` and `last_viewed`.

Against it: AGENTS.md says a `format_version` bump "is the only migration
mechanism", and this is a migration by another name. That invariant is worth
something — it is why there is no migration code to maintain — and a second
mechanism, once it exists, will be reached for again.

**3. Whether geotagged videos should get sidecars.** **[corrected, absent from
the first draft]** `backfill_locations` selects `WHERE gps_lat IS NOT NULL` with
no predicate on media type. The moment videos carry coordinates, the first
enrichment pass writes a new companion into the user's gallery for every
geotagged clip that never had one, and puts place-name tags in it. That is
probably wanted — place tags for videos are a feature, and it is what already
happens for photos — but it is new durable data on the one tree the design
promises is safe to copy around, so it belongs in the contract rather than
arriving as a side effect.

## Placement

**The probe branch goes in `services/gallery.rs::probe_and_store`.** That is
already the one place that turns "a path nothing has looked at" into a
`ProbedMedia` row: it resolves the path, batches by `PROBE_BATCH`, commits as it
goes, and marks `exif_read`. It serves both entry points that matter — the
enrichment backfill and the watcher's ingest — so a clip dropped into a running
gallery is covered by the same code as one found at open. Its blocking closure
gains a `match media_type_of(&path)` around a single call.

Dependencies point the way they already do: `services` → `pipeline::{exif,
video}` → `cache::meta`. No new edge.

**Capture-time parsing goes in `pipeline/video.rs`, beside `location_from_tags`.**
`services` must not learn ffprobe's JSON shape.

**The probe does not go in the thumbnailer.** `pipeline/serve.rs` writes
dimensions when it happens to decode a frame, which is lazy and tier-triggered.
A file's facts must not depend on whether someone scrolled past it — that is the
defect A1 fixed for EXIF.

**The presence guard goes at each unattended `modify_companion` call.**
**[corrected]** The first draft claimed `reindex_companions` never writes a
companion and that the sweep's guard could be retired. It does, at
`services/gallery.rs:405`, via `complete_companions`, which calls
`modify_companion` at `:471` — and the sweep's own doc comment says so in as
many words. Retiring that guard would have left a `.tmp` in the user's gallery
under precisely the scenario it was written for.

The second false premise was that every other caller is an HTTP request covered
by axum's graceful shutdown. `server/commands.rs:439` spawns a **detached** task
for `run_plugin` — its own comment says a run over a thousand files outlives any
request — and `plugin/run.rs:254` writes a companion inside it with no guard at
all. That is a live hole today: closing the last window during a plugin job can
exit mid-write.

So the rule, and it is checkable: **a guard wraps each `modify_companion` call
in unattended work, and nothing else.** Three places qualify —
`backfill_locations`, `complete_companions`, and `plugin::run::run`. The guard
comes off `enrich_and_index`'s spawn in `cli/mod.rs` and off
`spawn_companion_sweep`, because with the writes themselves guarded those
wrappers only extend protection over cache writes, which an abrupt exit costs
nothing.

What that buys: the long phase of a first open — reading every header — stops
holding the session open. That was the point, and it survives the correction.

## Contract

| what changes | who is on the other side |
|---|---|
| `VideoInfo` gains `date_taken: Option<i64>` | in-crate; wall clock relabelled UTC, matching the existing column convention — see decision 1 |
| `media_meta.duration`, `width`, `height`, `gps_lat`, `gps_lon`, `date_taken` populated for videos | `sort::sorter`, `services::media`, the wire, `galleryStore.durationByPath`, `ThumbnailCell` |
| **new sidecars in the user's gallery for geotagged videos** | the companion format; see decision 3 |
| `enrich_and_index` takes `Arc<Presence>`; `plugin::run::run` takes one | `cli/mod.rs`, `server/commands.rs` |
| either `FORMAT_VERSION` 2 → 3, or a `gallery_meta` probe-version key | every existing derived cache; see decision 2 |

Nothing changes shape on the wire. Fields that were always `null` start
carrying values, and the frontend already types every one of them as nullable —
which is why the grid needs no change at all to satisfy R2.

**Why the grid's aspect ratios cannot disagree**, since a reader will wonder and
`set_probed` is COALESCE-first-wins, making any disagreement permanent: the
thumbnailer writes `Frame.src_width/src_height`, which for video come from
`info.width/height` — the same rotation-corrected values the index-time probe
writes. Checked; they cannot differ.

## Cost in concepts

One branch, in one function, on an enum that exists for exactly this.

**One `except`, named as principle 1 requires: if ffprobe is not installed, a
video row is neither probed nor marked read**, so installing `ffmpeg` later
fills the gaps on the next open instead of never. Two things the implementer
must get right, both found by the review:

- **Skip the write, not just the mark.** Otherwise `idx_meta_unprobed` — which
  `cache/db.rs` justifies as "empty on a warm gallery" — holds every video row
  permanently on an ffmpeg-less host, and each open issues a no-op `set_probed`
  UPDATE per video inside a transaction.
- **Ask `video::ffprobe_available()` before probing; never match on `Err`.**
  `video::probe` returns the same `ThumbError::Decode` for "ffprobe not
  available" and for "found no video stream". Branching on the error is the
  result-column gate AGENTS.md forbids, and it would re-probe corrupt clips
  forever.

Everything else in this plan subtracts.

## Alternatives

**Delete the two autoplay settings instead of probing.** Cheapest by a wide
margin. Rejected: the feature is wanted, and "it was never wired up" describes
the bug rather than justifying its removal.

**Gate autoplay on file size, which the index already has.** Rejected: size is
bitrate × duration, so a fifteen-second 4K clip reads as long and a five-minute
screen recording reads as short. The setting is in seconds; deciding in bytes
would make it lie.

**Probe in the thumbnailer, or during `scan_and_index`.** Rejected on placement
and on cost respectively — the scan is what the grid waits for.

**Put the guard inside `modify_companion` itself.** The strongest form of the
invariant. Rejected: it needs an `Arc<Presence>` threaded through every caller
or a process-global singleton. With only three unattended callers, three
explicit guards beat hidden state — but this is the alternative to revisit if a
fourth appears.

**`signal(SIGCHLD, SIG_IGN)` once at startup instead of reaping each child.**
One line, and `libc` is already a dependency. Rejected with a reason worth
recording so the next reader does not re-propose it: it would make every
`wait`/`try_wait` in the crate fail with `ECHILD`, breaking the ffmpeg timeout
loop in `pipeline/video.rs` and the plugin runner.

## Assumptions

- **(measured, this session)** ffprobe costs **60 ms per file** — 60 clips in
  3.6 s on this host, above the 30–50 ms first guessed. Serial over a
  2,000-clip library that is roughly two minutes of background work, batched,
  committed every 256 files and resumable. Left serial deliberately: principle 3
  licenses the complexity only when a measurement makes the trade real, and two
  minutes of resumable background work on a first open does not. The number goes
  in a comment so the next reader knows the threshold rather than re-deriving it.
- **[corrected] Every video is probed twice, and that is accepted.** The first
  draft claimed the thumbnailer's later probe would be free via the memo in
  `video.rs`. The memo's own comment says it "exists to bridge the few
  milliseconds between thumbnailing a file and recording its metadata, not to be
  a long-lived cache", with a 1024 cap and a wholesale `clear()`. Index time and
  scroll time are minutes apart, so the second probe is a real 60 ms. The
  measurement was real; the inference from it was not. The module's "one probe
  per file" claim needs correcting in the same change.
- **(unmeasured)** Some transcoders rewrite `creation_time` to the mux time, so
  a re-encoded clip can carry the date of its re-encode. The same trust is
  already extended to EXIF `DateTimeOriginal`, and the mtime fallback it
  replaces is strictly worse on a library that has been copied between disks.
- Containers with no creation time at all (AVI, older MKV) keep falling back
  through `COALESCE(date_taken, mtime)`.
- **[corrected] The watcher's ingest and the enrichment backfill call the same
  function under different constraints.** `probe_and_store` is awaited inline in
  the watcher loop, which is also what drains the transport, notices the gallery
  disappearing and hot-reloads settings. A phone dumping 200 clips parks it for
  about twelve seconds. Nothing is lost — the channel is unbounded — so this is
  latency, not correctness, and rayon is not the fix for this caller. Named so
  that the first person to see a slow watcher does not think it is new.
- **(unmeasured)** A file still being written when the watcher sees it may probe
  to a missing duration, and nothing re-probes it. The same is already true of a
  half-written JPEG's EXIF header; one fix would serve both.

## The two checks principle 1 asks for

**Second-implementation test** — not applicable: no abstraction, interface or
plugin point is introduced.

**Seam test** — passes, and worth recording why. `VideoInfo` already carries
width, height, duration and a `Location` documented as feeding
`media_meta.gps_lat/gps_lon` without conversion, and `ProbedMedia` already has a
field for every one of them. The seam was cut correctly when the two paths were
written; only the wire between them was never run.

## The work

**[corrected]** Six commits. The first draft's commit 1 was two changes wearing
one hat: duration and dimensions reorder nothing and have no open questions,
while a date changes every video's position and group header and carries all
three decisions above. Split, the safe half can land and be verified while the
other is argued.

**1. Probe videos for duration and dimensions.** The branch in
`probe_and_store`; the ffprobe-availability condition on both the write and the
mark. No date, no cache decision, no reordering. Fixes the false comment at
`sort/sorter.rs:83` and the "one probe per file" claim in `pipeline/video.rs`.
Tests: a fixture clip probes to a duration and rotation-corrected dimensions; a
video row is neither written nor marked when ffprobe is absent.

**2. Give videos a capture date.** `date_taken` on `VideoInfo`, parsed per
decision 1. Whichever of decision 2 is chosen. Tests: a clip with a QuickTime
local-with-offset tag, one with only `creation_time`, one with neither; a
regression that a video and a photo shot in the same hour group under the same
day header.

**3. Hold the guard at each unattended write.** Into `backfill_locations`,
`complete_companions` and `plugin::run::run`; out of `cli/mod.rs` and
`spawn_companion_sweep`. The test that matters is not that an exit happens
during enrichment — that largely inverts the existing
`durable_work_outlasts_the_last_window` — but that each of the three writers
still holds a guard, and that a header-read phase does not.

**4. Delete what nothing calls.** The eight functions, plus the cascade the
review caught: removing `HardwareProfile::lru_cache_size` orphans
`total_ram_mb` and `detect_ram_mb`, which are `pub` on a `pub` struct and so go
quietly dead with no lint. `currentQuery`, the `preload_count` and
`lru_cache_size` settings fields, `percent-encoding`, `bytemuck`, `tower-http`,
npm `qrcode`. Three stale atlas comments, not two — `JustifiedGrid.tsx:225` is
the third. `MediaInfo::duration_seconds` is left alone and gains a sentence
saying why: it is a companion-format field a plugin may write, so removing it is
a durable-format decision, not a prune, and a probed duration must not be
mirrored into it — the companion is the one thing a rebuild cannot regenerate.

**5. Reap what we spawn.** `xdg-open` and **Open with**, each with a detached
waiting thread.

**6. Docs.** `docs/pipeline/`, `docs/cache/`, `docs/gallery/`, `docs/frontend/`,
and — added after review — `docs/duplicates/` and `docs/geocode/`.
`docs/duplicates/` because merging sets the survivor's mtime as "the group's
agreed capture time" and relies on the grid falling back to mtime; once videos
carry a real `date_taken`, `COALESCE` stops reading it and the agreed time
silently stops moving the survivor. The head comment in `cache/db.rs` needs the
correction that a bump loses `date_added` and `last_viewed` wherever no sidecar
exists, and AGENTS.md's `exif_read` invariant needs to say which tool did the
looking.
