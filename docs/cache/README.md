# cache/

[← docs](../README.md)

**Responsible for** the derived SQLite database: the schema, the connections,
the four thumbnail tier tables, the tag index, the perceptual hashes, and the
maintenance that spans every table.

**Not responsible for** deciding what belongs in it. It takes a connection or a
struct and knows nothing about routes, application state or who asked — a rule
the [layering](../architecture.md#the-layers-and-which-way-they-point) depends
on.

**Depends on** `rusqlite` and [`path/`](../architecture.md). **Depended on by**
every service, [`pipeline/`](../pipeline/README.md), and the idle worker.

## There are no migrations

One `format_version` integer in `gallery_meta`. If it does not match the build's,
the file is **deleted and re-indexed**.

The database is fully derived from the photos and their companions, so migrating
it would be permanent code that runs once — and the migration list this replaces
had already drifted from its own derived version constant, which is the failure
mode of the thing rather than an argument against it in principle.

The cost, stated because it is real: a schema mistake is not a patch later, it is
a version bump that re-thumbnails every library. `date_added` and `last_viewed`
are mirrored into the companion, so on a gallery that has sidecars the two fields
a rebuild could not otherwise recover come back from them.

**That mirror does not reach a gallery with no sidecars**, and an untagged camera
roll is exactly that: the companion sweep skips any file without one, so there is
nowhere for those two fields to have been written. On such a library a bump does
not cost time and nothing else — it resets when every file was added and when it
was last seen, neither of which is recoverable from anything. This page used to
claim otherwise and was wrong.

## A tool change is not a schema change

A bump is the mechanism for the *tables* moving. When instead a **reader** learns
to extract something it used to ignore, nothing about the schema changes; some
rows simply hold less than a fresh read would give them. Deleting the cache to
fix that trades minutes of re-reading for data that cannot be rebuilt, which is
the wrong way round.

So a reader stamps its version into `gallery_meta` — `video_probe_version` is the
first — and on open, a mismatch puts the rows that reader owns back into the
candidate set by clearing their `exif_read`. **It is still a gate over which tool
looked, not over what it found**: the question is asked once about the build and
answered identically for every row, never by inspecting result columns where a
successful-but-empty read is indistinguishable from an absent one.

Two mechanisms, and the line between them is what changed: the schema, or the
reader.

## The schema, and why the indexes are part of it

```
gallery_meta   key/value — format_version and the reader stamps
media_meta     path PK · type · size · mtime · dates · rating · dimensions
               · duration · gps · colour label · thumbhash · exif_read
tag_index      (path, namespace, tag) PK
index_state    path PK · companion_mtime_nanos · companion_size
thumbs_js      path PK · bytes · dimensions            128px
thumbs_j       path PK · bytes · dimensions · phash     512px
thumbs_jm      path PK · bytes · dimensions            1280px
thumbs_jh      path PK · bytes · dimensions            2560px
```

The indexes are written in the schema rather than added afterwards as an
optimization, because **the query language is shaped by them: a field is
filterable only if it is indexed.** Naming them here is what stops them being
discovered by a slow gallery.

Three placements worth their own sentence:

- **`exif_read` records that a header was read, not what was in it.** A photo
  with no GPS and a screenshot with no EXIF block at all leave every result
  column NULL, so a gate phrased over those columns cannot tell "probed, found
  nothing" from "never probed" — it either re-reads them on every open forever
  or excludes them forever. Its partial index (`WHERE exif_read = 0`) holds
  only the rows still owed, so it is empty on a warm gallery and the pass costs
  a lookup rather than a scan of the library.

  The flag says a header was read, not which reader read it — an image's comes
  from EXIF, a video's from what `ffprobe` reports about the container. The one
  case where the index is *not* empty on a warm gallery is a host with no
  `ffmpeg`: its video rows are deliberately left unmarked, since nothing looked
  at them, so an `ffmpeg` installed later fills them in rather than needing the
  cache rebuilt. On such a host the pass walks those rows every open and finds
  nothing to do.

- **`thumbhash` is on `media_meta`, not on a tier.** It is ~25 bytes, and the
  items query would otherwise walk a thumbnail row's overflow pages to reach it.
- **`phash` is a column on `thumbs_j`**, so a perceptual hash is discarded and
  recomputed along with the thumbnail it describes — exactly the lifetime it
  should have.

There are two date indexes and they are not redundant. `idx_meta_date_taken`
serves the `date=` filters, which mean capture time. `idx_meta_sort_date` is an
expression index over `COALESCE(date_taken, mtime)` and serves the grid's own
ordering, which falls back to the file time so that every file has a place —
see [`query/`](../query/README.md) for why the two differ.

### `index_state` stores nanoseconds, not seconds

A gate that truncates to whole seconds and compares for equality was tolerable
for one pass at open. It is not once the companion sweep runs concurrently with
an hours-long stream of writes from another machine: a companion read at T.2 and
rewritten at T.6 has the same second, is skipped forever, and both caches
confidently disagree with the durable file in opposite directions. NFSv3+ and
SMB2 both carry sub-second mtimes.

## Every path-keyed table is swept together

`path_keyed_tables()` is the single source of truth, and it is *derived* from
`ThumbTier::ALL` rather than restating it. A test asserts it matches every table
in the schema that actually has a `path` column, by reading `sqlite_master` and
`pragma_table_info` — so adding a table and forgetting the sweep fails the build
rather than leaking.

What that prevents: a multi-megabyte blob keyed to a path nothing can reach
again. Nothing sits outside the sweep — notably there is no `not_duplicates`
table, because [a set does that job](../query/README.md#sets) and a set is a tag.

A prune refuses to act on an empty scan of a populated gallery. An unreadable
mount reports zero files, and "delete every row" is the wrong response to "I
could not look".

## Connections

**One writer behind a `tokio::Mutex`**, because `rusqlite::Connection` is `Send`
but not `Sync`, and a **read-only pool** (2–6) for the serve path. WAL with
`synchronous = NORMAL`.

The rule that matters more than the shape: **the writer is held for statements
only.** Never across filesystem I/O, an image decode or encode, a subprocess, or
a loop whose length scales with the library. Every one of those was a real hold
in the code this replaces, and each of them blocked every thumbnail the grid was
waiting on. Where a pass needs both — the companion sweep, the hashing loop —
it splits in two: a phase that reads and decodes with no connection at all, and
a phase that takes the writer once and runs batched statements.

## Invariants a caller must uphold

- **Keys are gallery-relative `RelPath`**, in exactly one spelling. The newtype
  is what guarantees that; a raw string key is a second spelling waiting to
  happen.
- **A new path-keyed table goes in `path_keyed_tables()`** in the same change.
  The test will tell you, but the sweep is the reason.
- **Do not hold the writer across anything slow.** If a pass needs to decode,
  read a file or wait on a process, split it.
- **Bump `FORMAT_VERSION` for any schema change**, including an added column.
  There is no other mechanism, and a silently mismatched schema is worse than a
  rebuild.
