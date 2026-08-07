# The cache

[← docs index](../README.md) · [architecture](../architecture.md)

One SQLite file per gallery, at `<gallery>/.lightview/cache.db`. It is a
*cache* in the sense that it can be deleted and rebuilt, but it also holds
state that exists nowhere else until a companion file is written — ratings,
view history, dedup decisions, device pairings — so "just delete it" is not
free. Why per-gallery rather than one global index is
[decision 0001](../decisions/0001-one-cache-per-gallery.md).

**Responsible for:** the schema and its migrations; the connection strategy
(one writer, a read-only pool); every SQL statement in the process; the tier
tables and their byte budget; keeping the stored absolute paths valid when the
gallery directory moves.

**Not responsible for:** deciding *what* to store. It does not generate
thumbnails ([`pipeline/`](../pipeline/README.md)), parse filters
([`query/`](../query/README.md)), or read companion files
([`companion/`](../companion/README.md)) — it stores what those hand it. It
also owns no policy about when to write: callers decide, and callers are
responsible for not holding the writer lock while they do something slow.

**Public interface:** `CacheDb` (the writer), `ThumbProtocolPool` (declared in
`lib.rs`, read-only), `cache::thumbnails::ThumbTier` and its `ALL`, and the
per-area helper modules `index`, `counts`, `duplicates`, `gif_atlas`, and
`coalescer`.

**Depends on:** `rusqlite` (bundled SQLite) and nothing else in the tree except
the `MediaType` vocabulary from `companion/`.

**Depended on by:** effectively everything —
[`pipeline/`](../pipeline/README.md), [`query/`](../query/README.md),
[`remote/`](../remote/README.md), [`duplicates/`](../duplicates/README.md), and
every command handler.

## Invariants callers must uphold

**Never hold the writer lock across an expensive non-DB operation.** The
pattern throughout `commands/media.rs` is: do the work (decode, encode,
`ffprobe`), *then* take the lock and commit. `generate_and_store_tier` spells
this out for `ffprobe` specifically — running a subprocess under the lock kept
every other DB user queued for hundreds of milliseconds per video.

**The thumbnail read path cannot write.** It goes through
`AppState::thumb_protocol_db`, a pool of read-only connections. That is why LRU
access marks are buffered in `AppState::pending_tier_accesses` instead of
written through, and why `enforce_tier_budget` must drain them via
`take_tier_accesses` *immediately before* the eviction pass. Reordering those
two steps makes eviction drop exactly the rows the user is looking at.

**Every path-keyed table must be swept together.** `cache::db::path_keyed_tables()`
is the single source of truth, derived from `ThumbTier::ALL`. Any operation
that removes or relocates a file — trash, fs-watch removal, stale-row pruning
in `populate_media_meta`, `rebase_root` — iterates it. When a site spells the
list out itself, the failure mode is a multi-megabyte blob keyed to a path that
no longer exists and can never be reached again.
`remove_media_rows_clears_every_path_keyed_table` guards this. `not_duplicates`
is deliberately outside the list: its paths live in `path_a`/`path_b`, so
callers handle it separately.

**Adding a tier is a schema change *and* a maintenance change.** Add the
variant to `ThumbTier`, add it to `ThumbTier::ALL`, and add its `CREATE TABLE`
migration. `ALL` then propagates it to `path_keyed_tables()`,
`clear_thumbnails`, `get_all_tier_info`, and the per-file delete.

## Connections

Three kinds, deliberately:

| Handle | Where | Mode | Purpose |
|---|---|---|---|
| `AppState::cache_db` | `Arc<tokio::Mutex<Option<CacheDb>>>` | read/write | every command that writes |
| `AppState::thumb_protocol_db` | `Arc<std::sync::RwLock<Option<Arc<ThumbProtocolPool>>>>` | read-only, N connections | the thumbnail serve hot path |
| per-worker | `lightview-worker` | over HTTP | no direct DB access |

`CacheDb` uses a `Mutex` rather than an `RwLock` because `rusqlite::Connection`
is `Send` but not `Sync`. The read-only pool exists because WAL mode allows
many simultaneous readers but a single `Connection` behind a `Mutex`
serializes them; the pool hands each request one of N (2–6, from
`available_parallelism`) so grid reads fan out.

The pool's connections **block WAL checkpointing**, which is why
`close_gallery` drops them *before* checkpointing the writer.

### PRAGMA choices

Both the writer and each pool connection set `journal_mode=WAL`,
`temp_store=MEMORY` (keeps ORDER BY and IN-list temp B-trees out of a disk temp
file), and `mmap_size=268435456` (thumbnail blobs come from a mapped page
rather than `read()` plus a buffer copy — mmap is per-connection, hence the
repetition). Cache size differs: 64 MB for the writer, 8 MB per pool connection
since N of them exist and the OS page cache backs the WAL underneath.

## The migration contract

`SCHEMA_VERSION` is **derived** from `MIGRATIONS` by a `const fn`, not
maintained by hand. It drifted once (the constant said 14 while a v15 migration
existed), and the drift is silent in the dangerous direction — see below.

To add a migration:

1. Append a `Migration { version: N + 1, sql: "..." }` to `MIGRATIONS`. Versions
   must be strictly increasing; `fresh_db_reaches_the_latest_migration` asserts
   this, because `run_migrations` skips anything `<= version` and an
   out-of-order entry would never run.
2. **Make it idempotent.** `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT
   EXISTS`, and `ALTER TABLE ... ADD COLUMN` (the loop tolerates "duplicate
   column" so a crash mid-migration can be retried). `migrations_are_idempotent`
   asserts a full re-run over a current database is a no-op.
3. Statements are executed one at a time, split on `;`, so a partially-applied
   migration does not abort the rest of the batch.
4. If it adds a thumbnail tier, also add the `ThumbTier` variant and put it in
   `ThumbTier::ALL` — see the invariants at the top of this page.

`SCHEMA_VERSION` needs no edit; it follows.

### Legacy detection, and why it must under-report

`detect_legacy_version` only runs when `gallery_meta.schema_version` is absent —
a database created before the versioning scheme existed. It walks schema
features in version order and returns the last one fully present.

The asymmetry to understand: `run_migrations` **skips every migration at or
below the version returned**. So

- under-reporting is harmless — migrations re-run, and they are idempotent;
- over-reporting silently leaves tables uncreated.

That is precisely the bug the old code had. The ladder's last actual check is
`gif_atlas` (v10), but it returned the `SCHEMA_VERSION` constant, so once that
constant moved past 10 an unstamped database would be stamped 14/15 without
ever creating the justified-tier tables. The ladder now returns
`MAX_DETECTABLE_VERSION` (10), and `legacy_detection_never_over_reports`
guards it. **Extending the ladder is optional; raising its return value beyond
what it verifies is not.**

`run_migrations` also warns if the final version does not equal
`SCHEMA_VERSION`. The only way to land short is a database stamped ahead of
this build — opened by a newer LightView, then downgraded.

## Tables

Path-keyed (all swept together by `path_keyed_tables()`):

- `media_meta` — the index. Path, media type, size, `date_taken`, `date_added`,
  `last_viewed`, `last_rated`, rating, dimensions, duration, GPS. This is what
  sorting and the grid read from; it is populated from the recursive scan at
  gallery open so the grid renders before any tagging or thumbnailing happens.
- `tag_index` — `(path, namespace, tag)`. Rebuilt from companion files.
- `index_state` — companion mtime per path, so re-indexing skips unchanged
  companions.
- `gif_atlas` — pre-rendered GIF frame sprite sheets, keyed `(path, tier)`.
- the seven `thumbnails*` tables, one of which (`thumbnails`) also
  carries the `phash` column duplicate detection reads — see
  [`pipeline/`](../pipeline/README.md).

Not path-keyed:

- `gallery_meta` — key/value. Holds `schema_version`, `gallery_root`, the
  remote-access settings, the default filter, upload config, trash retention.
- `tag_counts` — `(namespace, tag) → count`, rebuilt from `tag_index`; feeds
  autocomplete.
- `not_duplicates` — user-confirmed non-duplicate pairs, stored with
  `path_a < path_b` so lookups are canonical. Handled separately by every sweep
  because its paths are not in a `path` column.
- `remote_devices`, `remote_pairing` — per-device cookie hashes (argon2, so a
  database leak alone grants nothing) and short-lived enrollment codes.

## Paths are absolute, and that is a liability

Every path-keyed row stores an absolute path, so moving the gallery directory
orphans the entire cache. `adopt_gallery_root` handles this at open:

1. Compare the current root against `gallery_meta.gallery_root`. Unchanged is
   the common case and returns immediately.
2. If it changed, `rebase_root` rewrites the prefix across every path-keyed
   table plus both `not_duplicates` columns, in one transaction. The match is
   on `old_root || '/'` so a sibling directory like `/data/photos2` is never
   rewritten when the old root is `/data/photos`. `UPDATE OR REPLACE`, because
   a bare row may already exist under the new root — the relocated row wins,
   since it carries the accumulated history.
3. If the cache predates root tracking, `infer_old_root` matches a few on-disk
   files against cached rows by gallery-relative suffix and requires every
   sample (up to 5) to agree on one foreign prefix. Ambiguity means no rebase —
   a safe no-op.

This must run **before** `populate_media_meta`, which would otherwise insert
bare rows under the new root and shadow the relocated history.
